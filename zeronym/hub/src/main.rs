//! zero-indexer-hub: receive diverted migrations, hold them, and publish each
//! batch on the flush cadence.
//!
//! Two concurrent halves: the serving path admits submissions into the queue,
//! and the cadence publishes what the queue holds at every flush boundary. They
//! share the queue and the tip, and nothing else.

use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use zero_indexer_hub::batcher::{self, BatchParams, TipTracker};
use zero_indexer_hub::chain::ChainClient;
use zero_indexer_hub::config::Config;
use zero_indexer_hub::queue::Queue;
use zero_indexer_hub::server::{self, Hub};
use zero_indexer_hub::BoxError;

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::parse();

    // Refuse to run blind. A hub without indexer TLS lets the enclave's parent
    // host read every batch in the clear moments before it is public, so this is
    // announced loudly rather than left to a config review.
    let tls = config.indexer_tls()?;
    if tls.is_none() {
        tracing::warn!(
            "no --indexer-tls: the hop to the indexer is PLAINTEXT and the host can read every batch"
        );
    }
    let chain = Arc::new(ChainClient::new(config.indexers.clone(), tls)?);

    // The expiry budget is asserted at startup, not trusted. A parameter change
    // that overspends it must fail here rather than be discovered in production
    // as a percentage of real traffic quietly expiring.
    let params = BatchParams::default();
    params.validate()?;

    let queue = Arc::new(Queue::new());
    let tip = Arc::new(TipTracker::new());

    tracing::info!(
        nodes = chain.node_count(),
        flush_interval = params.flush_interval,
        mining_margin = params.mining_margin,
        delivery_lag = params.delivery_lag,
        min_wallet_expiry = params.min_wallet_expiry,
        "zero-indexer-hub starting"
    );

    // The hub cannot admit anything until it knows a height, because both the
    // flush schedule and the expiry check are defined against one. Seed it here
    // so a fresh boot does not spend a whole poll interval refusing.
    match chain.tip_height().await {
        Ok(height) => tip.observe(height),
        Err(err) => tracing::warn!(
            %err,
            "no tip at startup; refusing admissions until a node answers"
        ),
    }

    let listener = tokio::net::TcpListener::bind(config.listen).await?;

    // Shared with the mixnet driver, which fills it in on every (re)build, and
    // read by `GET /nym-address`. Stays empty on a clearnet-only build, where
    // the endpoint then reports that there is no address rather than inventing
    // one.
    let nym_address = server::NymAddress::unknown();

    let cadence = tokio::spawn(batcher::run(
        queue.clone(),
        chain.clone(),
        tip.clone(),
        params,
        shutdown_signal(),
    ));

    // Optionally also accept submissions over the Nym mixnet (M5), sharing the
    // same queue, tip and chain as the clearnet serving path below, so the two
    // ingress paths admit into one queue and cannot drift.
    #[cfg(feature = "mixnet-driver")]
    spawn_nym_listener(&config, &queue, &tip, params, &chain, &nym_address)?;
    #[cfg(not(feature = "mixnet-driver"))]
    if config.nym || config.nym_topology.is_some() {
        return Err(
            "--nym is set but this binary was built WITHOUT the mixnet-driver feature; \
             rebuild with --features mixnet-driver"
                .into(),
        );
    }

    // The clearnet submit path is off unless asked for. Say so at startup: a
    // hub reachable only over the mixnet is the intended shape, and an operator
    // who expected to curl a transaction in should learn that here rather than
    // from a 404.
    if config.http_submit {
        tracing::warn!(
            "--http-submit: accepting UNAUTHENTICATED clearnet submissions on POST /. \
             Intended for a transitional shim or a local demo, not a mixnet deployment"
        );
    }

    let serving = server::serve(
        listener,
        Hub {
            queue,
            tip,
            params,
            chain,
        },
        server::ServeOptions {
            nym_address,
            http_submit: config.http_submit,
        },
    );

    // Either half stopping is fatal: a hub that admits without publishing holds
    // migrations until they expire, and a hub that publishes without admitting
    // is not reachable. Neither is a state to keep running in.
    tokio::select! {
        result = serving => result,
        result = cadence => result.map_err(BoxError::from).and(Ok(())),
    }
}

/// Spawn the mixnet ingress when requested and this binary carries the driver.
///
/// The listener and the driver share the same [`Hub`] cores as the clearnet
/// path, so a migration admitted over the mixnet lands in the same queue and
/// flushes in the same batch. Both tasks are detached and torn down by their
/// channels closing on shutdown; the driver also disconnects cleanly on its own
/// ctrl-c. A no-op unless `--nym` (or `--nym-topology`) is set.
#[cfg(feature = "mixnet-driver")]
fn spawn_nym_listener(
    config: &Config,
    queue: &Arc<Queue>,
    tip: &Arc<TipTracker>,
    params: BatchParams,
    chain: &Arc<ChainClient>,
    nym_address: &server::NymAddress,
) -> Result<(), BoxError> {
    use tokio::sync::mpsc;

    use zero_indexer_hub::nym;
    use zero_indexer_hub::nym_driver::{self, MixnetNetwork};

    if !config.nym && config.nym_topology.is_none() {
        return Ok(());
    }

    let network = match &config.nym_topology {
        None => MixnetNetwork::Default,
        Some(path) => {
            #[cfg(feature = "mixnet-localnet")]
            {
                MixnetNetwork::TopologyFile(path.clone())
            }
            #[cfg(not(feature = "mixnet-localnet"))]
            {
                let _ = path;
                return Err(
                    "--nym-topology requires a build with the mixnet-localnet feature".into(),
                );
            }
        }
    };

    let hub = Hub {
        queue: queue.clone(),
        tip: tip.clone(),
        params,
        chain: chain.clone(),
    };
    // The driver holds the opposite end of each channel: it sends inbound
    // requests and its address, and receives replies.
    let (in_tx, in_rx) = mpsc::channel(64);
    let (out_tx, out_rx) = mpsc::channel(64);
    let (addr_tx, mut addr_rx) = mpsc::channel(4);

    tokio::spawn(nym::run_listener(in_rx, out_tx, hub));
    // Shut the driver's client down on the SAME signal as the rest of the hub
    // (SIGTERM or ctrl-c), not ctrl-c alone: a container stop is SIGTERM, and the
    // driver must run disconnect() to completion then (D12: it is not cancel-safe
    // and a dropped live client leaks its background tasks).
    tokio::spawn(nym_driver::run_driver(
        network,
        config.nym_gateway.clone(),
        in_tx,
        out_rx,
        addr_tx,
        shutdown_signal(),
    ));
    // The driver logs its address, but surfacing it here too keeps it in the
    // startup log the operator reads to configure shims — and, more importantly
    // now, parks it where `GET /nym-address` can answer with it. An ATTESTED
    // enclave has no console, so the endpoint is the only way to read this.
    let nym_address = nym_address.clone();
    tokio::spawn(async move {
        while let Some(address) = addr_rx.recv().await {
            tracing::info!(
                %address,
                "hub reachable over the Nym mixnet; publish this to shims as --hub-nym"
            );
            nym_address.set(address.to_string());
        }
    });
    Ok(())
}

/// Resolves on SIGTERM or ctrl-c, so the cadence can publish what it holds
/// rather than dropping a queue full of migrations that shims believe are on
/// their way to the network.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::warn!(%err, "cannot listen for SIGTERM; ctrl-c only");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
