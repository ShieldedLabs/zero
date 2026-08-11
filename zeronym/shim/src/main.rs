//! Binary wrapper around [`zero_indexer_shim::serve_with_shutdown`].
//!
//! Both hops are optionally TLS, and independently so: the wallet-facing link
//! is terminated here when `--tls-domain` is set, and the backend link is
//! originated here when `--backend-tls` is set. With neither, the shim is
//! plaintext h2c end to end, which is what a local demo and the tests use.

use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;
use zero_indexer_shim::config::Config;
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
    let diversion = match config.hub {
        Some(hub_addr) => {
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
                hub: HubClient::new(hub_addr, hub_tls),
            }))
        }
        None => {
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
