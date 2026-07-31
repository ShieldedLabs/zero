//! Runtime configuration: where the shim listens, and which indexer it fronts.
//!
//! Two addresses is the whole surface. The proof of concept is plaintext h2c,
//! so there is no TLS, no ACME, and no domain here; production adds those (see
//! the book's `components.md`).

use std::net::SocketAddr;

use clap::Parser;

/// The shim's listen address. Wallets point at this instead of the indexer.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:9068";

/// The backing indexer's address. 9067 is the conventional lightwalletd and
/// Zaino gRPC port, so the operator's existing node keeps its usual address and
/// the shim takes the new one.
pub const DEFAULT_BACKEND: &str = "127.0.0.1:9067";

/// Command line and environment configuration.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "zero-indexer-shim",
    version,
    about = "Transparent CompactTxStreamer reverse proxy that classifies SendTransaction"
)]
pub struct Config {
    /// Address the shim listens on for wallet traffic (plaintext h2c, no TLS).
    #[arg(long, env = "ZIS_LISTEN", default_value = DEFAULT_LISTEN)]
    pub listen: SocketAddr,

    /// Address of the backing indexer, lightwalletd or Zaino (plaintext h2c).
    #[arg(long, env = "ZIS_BACKEND", default_value = DEFAULT_BACKEND)]
    pub backend: SocketAddr,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_differ() {
        let config = Config::parse_from(["zero-indexer-shim"]);
        assert_eq!(config.listen.to_string(), DEFAULT_LISTEN);
        assert_eq!(config.backend.to_string(), DEFAULT_BACKEND);
        // Fronting the indexer on its own address would be a loop.
        assert_ne!(config.listen, config.backend);
    }

    #[test]
    fn flags_override_defaults() {
        let config = Config::parse_from([
            "zero-indexer-shim",
            "--listen",
            "0.0.0.0:443",
            "--backend",
            "10.0.0.5:9067",
        ]);
        assert_eq!(config.listen.to_string(), "0.0.0.0:443");
        assert_eq!(config.backend.to_string(), "10.0.0.5:9067");
    }
}
