//! Binary wrapper around [`zero_indexer_shim::serve_with_shutdown`].
//!
//! Plaintext h2c only: no TLS in the proof of concept.

use clap::Parser;
use tokio::net::TcpListener;
use zero_indexer_shim::config::Config;
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

    // Bind before serving so EADDRINUSE / EACCES is a startup failure the
    // operator sees, not a silent death inside a spawned task.
    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(
        listen = %listener.local_addr()?,
        backend = %config.backend,
        "zero-indexer-shim starting (plaintext h2c, non-destructive: migrations are logged, not diverted)"
    );

    zero_indexer_shim::serve_with_shutdown(listener, config.backend, shutdown()).await
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
