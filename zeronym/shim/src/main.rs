//! Binary wrapper around [`zero_indexer_shim::serve_with_shutdown`].
//!
//! Both hops are optionally TLS, and independently so: the wallet-facing link
//! is terminated here when `--tls-domain` is set, and the backend link is
//! originated here when `--backend-tls` is set. With neither, the shim is
//! plaintext h2c end to end, which is what a local demo and the tests use.

use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;
use zero_indexer_shim::config::{Config, HubSelection};
use zero_indexer_shim::hub::HubClient;
use zero_indexer_shim::intercept::Diversion;
use zero_indexer_shim::proxy::Backend;
use zero_indexer_shim::tls::{BackendTls, ServerTls};
use zero_indexer_shim::BoxError;

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    // `info` deliberately does NOT include the per-request `zis::proxy` line:
    // that line names the method each wallet called, which is a metadata source
    // this component exists to deny the operator, and it would live in a log
    // file on the operator's box. `RUST_LOG=zis::proxy=debug,info` turns it on
    // when someone is debugging or demoing. `zis::classify` stays at info.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::parse();

    // Built before binding, so a malformed TLS name is a startup failure the
    // operator sees rather than a per-request one they have to infer.
    let backend_tls = match config.backend_tls.as_deref() {
        Some(name) => Some(BackendTls::new(name)?),
        None => None,
    };
    let backend = Backend {
        addr: config.backend,
        tls: backend_tls,
    };

    let server_tls = config.tls_domain.as_deref().map(|domain| {
        Arc::new(ServerTls::start(
            domain,
            config.tls_email.as_deref(),
            config.tls_production,
        ))
    });

    // Bind before serving so EADDRINUSE / EACCES is a startup failure the
    // operator sees, not a silent death inside a spawned task.
    let listener = TcpListener::bind(config.listen).await?;
    let local = listener.local_addr()?;

    match &server_tls {
        Some(tls) => tracing::info!(
            listen = %local,
            backend = %backend,
            domain = tls.domain(),
            acme = if config.tls_production {
                "letsencrypt-production"
            } else {
                "letsencrypt-staging"
            },
            "zero-indexer-shim starting (TLS terminated here)"
        ),
        None => tracing::info!(
            listen = %local,
            backend = %backend,
            "zero-indexer-shim starting (plaintext h2c)"
        ),
    }

    // Said once and loudly at startup, rather than left for someone to work out
    // from the absence of a flag. A listener with no certificate serves wallet
    // queries in the clear, and those queries are exactly the metadata this
    // component exists to keep from the operator.
    if server_tls.is_none() {
        tracing::warn!(
            "no --tls-domain: wallet traffic is PLAINTEXT. Reasonable on loopback \
             beside an indexer, not across a network."
        );
    }
    if backend.tls.is_none() {
        tracing::warn!("no --backend-tls: the hop to the backing indexer is PLAINTEXT.");
    }

    // Diversion is on iff a hub is configured. Built here so a malformed hub TLS
    // name is a startup failure the operator sees, warned about loudly when the
    // hop is plaintext, and stated plainly when absent so nobody mistakes
    // forward-only for private.
    // `to_string`, not `?` on the typed error: main renders a BoxError with
    // Debug, and this message is the whole reason the check runs at startup
    // rather than at the first divert.
    let selection = config.hub_selection().map_err(|err| err.to_string())?;
    let diversion = match selection {
        HubSelection::Http(hub_addr) => {
            // new_http1, NOT new: the hub's submission endpoint is a plain
            // HTTP/1.1 POST, while the backing indexer above is gRPC. Offering
            // `h2` here makes an ALPN-honouring server agree to HTTP/2 and then
            // wait forever for a preface this client never sends.
            let hub_tls = match config.hub_tls.as_deref() {
                Some(name) => Some(BackendTls::new_http1(name)?),
                None => None,
            };
            if hub_tls.is_none() {
                tracing::warn!("no --hub-tls: the hop to the hub is PLAINTEXT.");
            }
            tracing::info!(
                hub = %hub_addr,
                "diversion ENABLED: Orchard-touching sends and all GetTransaction go to the hub, \
                 not the operator"
            );
            Some(Arc::new(Diversion {
                hub: HubClient::new(hub_addr, hub_tls).into(),
            }))
        }
        HubSelection::Nym(addresses) => nym_diversion(&config, addresses)?,
        HubSelection::ForwardOnly => {
            tracing::warn!(
                "no --hub: FORWARD-ONLY. Migrations are classified and logged but forwarded to \
                 the operator's indexer. No privacy until a hub is set."
            );
            None
        }
    };

    zero_indexer_shim::serve_with_shutdown(listener, backend, server_tls, diversion, shutdown())
        .await
}

/// Resolves on the first ctrl-c, which stops the accept loop and drains.
async fn shutdown() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => tracing::info!("ctrl-c received"),
        // Without a working signal handler the shim would exit immediately, so
        // report it and keep serving instead.
        Err(err) => {
            tracing::error!(%err, "cannot listen for ctrl-c, shutdown must be a kill signal");
            std::future::pending::<()>().await
        }
    }
}

/// Build the mixnet diversion when this binary carries the driver.
///
/// Spins up the transport correlator, the client supervisor and the driver that
/// owns the SDK client, then hands back a [`Diversion`] over a [`HubTransport`]
/// whose `Nym` variant is the sender end of that correlator. The three tasks are
/// detached: they are torn down by their channels closing when the process ends,
/// and the supervisor also disconnects cleanly on its own ctrl-c.
#[cfg(feature = "mixnet-driver")]
fn nym_diversion(
    config: &Config,
    addresses: Vec<String>,
) -> Result<Option<Arc<Diversion>>, BoxError> {
    let transport = build_nym_transport(config, addresses)?;
    tracing::info!(
        "diversion ENABLED over the Nym mixnet: Orchard-touching sends and all GetTransaction \
         go to the hub, not the operator"
    );
    Ok(Some(Arc::new(Diversion { hub: transport })))
}

/// Without the driver compiled in, `--hub-nym` cannot be honoured, and the only
/// honest response is to refuse: forwarding migrations to the operator or
/// falling back to clearnet are both exactly what setting `--hub-nym` rejected.
#[cfg(not(feature = "mixnet-driver"))]
fn nym_diversion(_config: &Config, addresses: Vec<String>) -> Result<Option<Arc<Diversion>>, BoxError> {
    Err(format!(
        "--hub-nym is set ({} address(es)) but this binary was built WITHOUT the mixnet-driver \
         feature; rebuild with --features mixnet-driver, or use --hub for the transitional \
         clearnet path",
        addresses.len()
    )
    .into())
}

/// Wire the mixnet transport: parse the hub addresses, choose the network, and
/// spawn the correlator, supervisor and driver over the channels that connect
/// them. Returns the [`HubTransport`] the intercept path submits through.
#[cfg(feature = "mixnet-driver")]
fn build_nym_transport(
    config: &Config,
    addresses: Vec<String>,
) -> Result<zero_indexer_shim::hub::HubTransport, BoxError> {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use tokio::sync::mpsc;

    use zero_indexer_shim::hub::HubTransport;
    use zero_indexer_shim::nym::{self, NymHandle, RotationPolicy, REQUEST_TIMEOUT};
    use zero_indexer_shim::nym_driver::{self, MixnetNetwork};

    // The SDK's authoritative parse; config only did a shallow structural check
    // (identity.encryption@gateway), so a base58 or key-length error surfaces
    // here, still at startup.
    let recipients = addresses
        .iter()
        .map(|addr| nym_driver::parse_address(addr))
        .collect::<Result<Vec<_>, _>>()
        .map_err(BoxError::from)?;

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

    let rotation = match config.nym_rotation_secs {
        Some(secs) => RotationPolicy::every(Duration::from_secs(secs)),
        None => RotationPolicy::never(),
    };

    // One channel per edge of the driver boundary. The driver holds the opposite
    // end of each: it receives OutFrames and commands, and sends inbound bytes
    // and events.
    let targets: nym::TargetCount = Arc::new(AtomicUsize::new(0));
    let inflight: nym::InflightCount = Arc::new(AtomicUsize::new(0));
    let (req_tx, req_rx) = mpsc::channel(32);
    let (out_tx, out_rx) = mpsc::channel(8);
    let (in_tx, in_rx) = mpsc::channel(32);
    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (evt_tx, evt_rx) = mpsc::channel(8);

    tokio::spawn(nym::run_transport(req_rx, out_tx, in_rx, inflight.clone()));
    tokio::spawn(nym::run_supervisor(rotation, evt_rx, cmd_tx, inflight, async {
        let _ = tokio::signal::ctrl_c().await;
    }));
    tokio::spawn(nym_driver::run_driver(
        network,
        recipients,
        targets.clone(),
        out_rx,
        in_tx,
        cmd_rx,
        evt_tx,
    ));

    Ok(HubTransport::from(NymHandle::new(
        req_tx,
        REQUEST_TIMEOUT,
        targets,
    )))
}
