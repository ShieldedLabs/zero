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

    /// Divert Orchard-touching SendTransactions to this hub instead of
    /// forwarding them to the backing indexer. A literal SocketAddr, same
    /// discipline as `--backend`. UNSET means forward-only: the shim classifies
    /// and logs but diverts nothing, which is the merged proof-of-concept
    /// behaviour.
    #[arg(long, env = "ZIS_HUB")]
    pub hub: Option<SocketAddr>,

    /// Verify the hub's certificate as this DNS name, and speak TLS to it. Unset
    /// with `--hub` set means plaintext to the hub. Same split as
    /// `--backend`/`--backend-tls`: the enclave dials an IP, authenticates a
    /// name.
    #[arg(long, env = "ZIS_HUB_TLS")]
    pub hub_tls: Option<String>,

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
    /// Takes an explicit `true`/`false` rather than being a bare flag, which
    /// matters because it is usually set from the environment. A bare flag with
    /// `env` treats the variable as set whenever it EXISTS, so
    /// `ZIS_TLS_PRODUCTION=""` or `=false` would both mean production, and a
    /// deploy meant for staging would quietly spend one of the five weekly
    /// production issuances. Requiring a value makes that unrepresentable.
    #[arg(
        long,
        env = "ZIS_TLS_PRODUCTION",
        action = clap::ArgAction::Set,
        default_value_t = false
    )]
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

#[cfg(test)]
mod production_flag_tests {
    use super::*;

    /// The whole point of `action = Set`: an empty or false-y environment value
    /// must NOT select the production ACME directory. As a bare flag, clap
    /// treats mere presence as true and both of these would have meant
    /// production, silently spending one of five weekly issuances on a deploy
    /// intended for staging.
    #[test]
    fn production_requires_an_explicit_true() {
        let off = Config::parse_from(["zero-indexer-shim", "--tls-production", "false"]);
        assert!(!off.tls_production);

        let on = Config::parse_from(["zero-indexer-shim", "--tls-production", "true"]);
        assert!(on.tls_production);

        let defaulted = Config::parse_from(["zero-indexer-shim"]);
        assert!(
            !defaulted.tls_production,
            "staging must be the default; production is an act, not an omission"
        );
    }
}
