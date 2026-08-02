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

    /// Verify the backend's certificate as this DNS name, and speak TLS to it.
    ///
    /// Deliberately separate from `--backend`, which stays a literal address.
    /// The enclave dials an IP and never resolves DNS (its egress rule is a
    /// single /32 with no port 53), so no poisoned answer can redirect it, but
    /// the connection is still authenticated against a name rather than an
    /// address. Unset means plaintext h2c to the backend.
    #[arg(long, env = "ZIS_BACKEND_TLS")]
    pub backend_tls: Option<String>,

    /// Terminate wallet-facing TLS, obtaining a certificate by ACME for this
    /// domain. Unset means serve plaintext h2c.
    ///
    /// The key is generated inside the process and never leaves it, which in an
    /// enclave is the whole point: a key minted elsewhere would let its holder
    /// impersonate the enclave and make the attestation meaningless.
    #[arg(long, env = "ZIS_TLS_DOMAIN")]
    pub tls_domain: Option<String>,

    /// Contact address for the ACME account. Optional, but without it there is
    /// no expiry warning if renewal ever stops working.
    #[arg(long, env = "ZIS_TLS_EMAIL")]
    pub tls_email: Option<String>,

    /// Use the Let's Encrypt PRODUCTION directory instead of staging.
    ///
    /// Off by default on purpose. An enclave is diskless, so there is no
    /// certificate cache and every restart is a fresh order, against a limit of
    /// 5 duplicate certificates per week. Staging has no such ceiling and is
    /// where a new deployment should prove itself; flip this only when the
    /// deployment is known good.
    #[arg(long, env = "ZIS_TLS_PRODUCTION")]
    pub tls_production: bool,
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
