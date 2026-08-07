//! zero-indexer-hub: receive diverted migrations and broadcast them to the
//! Zcash network. Immediate broadcast (content privacy); the batching layer is
//! not built yet, see `lib.rs`.

use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use zero_indexer_hub::chain::ChainClient;
use zero_indexer_hub::config::Config;
use zero_indexer_hub::{server, BoxError};

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config = Config::parse();
    let chain = Arc::new(ChainClient::new(config.node_endpoints())?);

    tracing::info!(
        nodes = chain.node_count(),
        "zero-indexer-hub starting (immediate broadcast; batching not yet enabled)"
    );

    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    server::serve(listener, chain).await
}
