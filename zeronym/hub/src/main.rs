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

    let cadence = tokio::spawn(batcher::run(
        queue.clone(),
        chain.clone(),
        tip.clone(),
        params,
        shutdown_signal(),
    ));

    let serving = server::serve(listener, Hub { queue, tip, params });

    // Either half stopping is fatal: a hub that admits without publishing holds
    // migrations until they expire, and a hub that publishes without admitting
    // is not reachable. Neither is a state to keep running in.
    tokio::select! {
        result = serving => result,
        result = cadence => result.map_err(BoxError::from).and(Ok(())),
    }
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
