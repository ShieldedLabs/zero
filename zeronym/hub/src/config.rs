//! Command-line and environment configuration.
//!
//! Small on purpose: an address to accept submissions on, and the indexers to
//! broadcast through. Everything the batching design would add (flush cadence,
//! expiry margins) belongs to a layer that does not exist yet.

use std::net::SocketAddr;

use clap::Parser;

use crate::tls::IndexerTls;
use crate::BoxError;

/// The hub's whole configuration surface (env prefix `ZIH_`).
#[derive(Parser, Debug, Clone)]
#[command(
    name = "zero-indexer-hub",
    version,
    about = "Receives diverted migration transactions and broadcasts them to the Zcash network"
)]
pub struct Config {
    /// Address to accept shim submissions on. Plaintext HTTP: on Caution the
    /// platform terminates wallet-facing TLS and forwards here, exactly as for
    /// the shim, so the shim-to-hub hop is protected by the transport around it,
    /// not by this socket.
    #[arg(long, env = "ZIH_LISTEN", default_value = "0.0.0.0:8090")]
    pub listen: SocketAddr,

    /// An indexer's `CompactTxStreamer` address, `IPv4:port`. Repeatable, and at
    /// least one is required.
    ///
    /// A LITERAL address: the enclave has no DNS egress, so a hostname does not
    /// degrade, it fails to parse and the enclave never starts. The certificate
    /// is verified against `--indexer-tls` instead, so a hijacked address cannot
    /// present a valid certificate for the name.
    ///
    /// Every batch member is published to EVERY endpoint: a migration that only
    /// ever entered one mempool is one outage away from never being mined.
    #[arg(
        long = "indexer",
        env = "ZIH_INDEXERS",
        value_delimiter = ',',
        required = true
    )]
    pub indexers: Vec<SocketAddr>,

    /// The DNS name the indexer's certificate must carry.
    ///
    /// Unset means PLAINTEXT h2c, which is correct only for a test or a trusted
    /// local path. A deployed enclave must set this: without it the enclave's
    /// parent host reads every batch in the clear moments before it is public.
    #[arg(long = "indexer-tls", env = "ZIH_INDEXER_TLS")]
    pub indexer_tls: Option<String>,
}

impl Config {
    /// The TLS verifier for indexer connections, if one is configured.
    pub fn indexer_tls(&self) -> Result<Option<IndexerTls>, BoxError> {
        match &self.indexer_tls {
            Some(name) => Ok(Some(IndexerTls::new(name)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_comma_separated_indexer_list_splits() {
        let cfg = Config::parse_from(["zero-indexer-hub", "--indexer", "1.2.3.4:443,5.6.7.8:443"]);
        assert_eq!(cfg.indexers.len(), 2);
    }

    #[test]
    fn repeating_the_flag_accumulates_endpoints() {
        let cfg = Config::parse_from([
            "zero-indexer-hub",
            "--indexer",
            "1.2.3.4:443",
            "--indexer",
            "5.6.7.8:443",
        ]);
        assert_eq!(cfg.indexers.len(), 2);
    }

    #[test]
    fn a_hostname_is_refused_because_the_enclave_resolves_no_dns() {
        // Caught by clap's SocketAddr parse, where the error is readable, rather
        // than inside an enclave with no console.
        assert!(
            Config::try_parse_from(["zero-indexer-hub", "--indexer", "example.net:443"]).is_err()
        );
    }

    #[test]
    fn tls_is_optional_but_a_bad_name_is_refused() {
        let cfg = Config::parse_from(["zero-indexer-hub", "--indexer", "1.2.3.4:443"]);
        assert!(cfg.indexer_tls().expect("no tls configured").is_none());

        let cfg = Config::parse_from([
            "zero-indexer-hub",
            "--indexer",
            "1.2.3.4:443",
            "--indexer-tls",
            "lwd.shieldedinfra.net",
        ]);
        assert!(cfg.indexer_tls().expect("a valid name").is_some());

        let cfg = Config::parse_from([
            "zero-indexer-hub",
            "--indexer",
            "1.2.3.4:443",
            "--indexer-tls",
            "not a name",
        ]);
        assert!(cfg.indexer_tls().is_err());
    }
}
