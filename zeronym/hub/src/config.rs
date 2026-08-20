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

    /// Run with an UNAUTHENTICATED hop to the indexer. Off by default, and it
    /// must stay that way in any real deployment.
    ///
    /// Without `--indexer-tls` the hub speaks cleartext h2c to the indexer, so
    /// the enclave's parent host reads every flushed batch minutes before it is
    /// public -- which is the whole exposure the hub exists to remove. That case
    /// used to fail OPEN behind a `warn!` the operator of an attested enclave
    /// cannot even read, there being no console (Hornby review, 2026-08-19).
    ///
    /// It is a flag rather than a hard refusal because a local run against a
    /// plaintext lightwalletd is legitimate. It is safe as a flag because the
    /// manifest's `unit.env` -- every `ZIH_*` value AND their absence -- is
    /// measured into PCR0/PCR1, so a deployment that sets this cannot hide it
    /// from anyone running `caution verify`.
    /// Takes an explicit `true`/`false` for the same reason `--http-submit`
    /// does: it is usually set from the environment, where a bare flag would
    /// read as set whenever the variable merely exists -- which for THIS flag
    /// would silently turn the protection off.
    #[arg(
        long = "allow-plaintext-indexer",
        env = "ZIH_ALLOW_PLAINTEXT_INDEXER",
        action = clap::ArgAction::Set,
        default_value_t = false
    )]
    pub allow_plaintext_indexer: bool,

    /// Also accept submissions over the Nym mixnet (M5), alongside the clearnet
    /// `--listen` socket. The hub's mixnet address is logged at startup; publish
    /// it to shims as `--hub-nym`. Requires a build with the `mixnet-driver`
    /// feature; set on a binary without it, it is a startup error.
    ///
    /// Takes an explicit `true`/`false` rather than being a bare flag, for the
    /// same reason `--indexer-tls`-adjacent booleans do: it is usually set from
    /// the environment, where a bare flag would read as set whenever the
    /// variable merely exists.
    #[arg(long, env = "ZIH_NYM", action = clap::ArgAction::Set, default_value_t = false)]
    pub nym: bool,

    /// Localnet end-to-end tests only: load the mixnet topology from this file
    /// instead of connecting to the default network. Implies `--nym`. Requires a
    /// build with the `mixnet-localnet` feature.
    #[arg(long, env = "ZIH_NYM_TOPOLOGY")]
    pub nym_topology: Option<std::path::PathBuf>,

    /// Pin the hub's ENTRY gateway by identity key. A SINGLE value, not a list
    /// like the shim's: the hub's Nym address embeds its gateway and must stay
    /// stable (D10), so it holds ONE gateway and does not rotate. Unset = the SDK
    /// picks (and the address is then whatever gateway it lands on). The enclave's
    /// egress rule must allow this gateway's IP, or connect fails closed with no
    /// console. Only meaningful with `--nym`.
    #[arg(long, env = "ZIH_NYM_GATEWAY")]
    pub nym_gateway: Option<String>,

    /// Accept clearnet submissions on `POST /`, the transitional shim-to-hub
    /// hop. **Off by default** (NYM_PLAN M7): once the mixnet path works,
    /// nothing legitimate posts there, while the enclave declares an
    /// `0.0.0.0/0` ingress and the hub has no submitter ACL by design — the
    /// mixnet address IS the credential. Leaving it on ships an open,
    /// unauthenticated submit endpoint.
    ///
    /// Turn it on for a transitional clearnet shim, a local demo, or a test.
    /// The lookup path (`POST /transaction`) is unaffected; so are
    /// `/nym-address` and `/healthz`, which are read-only.
    ///
    /// Takes an explicit `true`/`false` for the same reason `--nym` does: it is
    /// usually set from the environment, where a bare flag would read as set
    /// whenever the variable merely exists.
    #[arg(long = "http-submit", env = "ZIH_HTTP_SUBMIT", action = clap::ArgAction::Set, default_value_t = false)]
    pub http_submit: bool,
}

impl Config {
    /// The TLS verifier for indexer connections, if one is configured.
    pub fn indexer_tls(&self) -> Result<Option<IndexerTls>, BoxError> {
        match &self.indexer_tls {
            Some(name) => Ok(Some(IndexerTls::new(name)?)),
            None => Ok(None),
        }
    }

    /// Refuse to start with an unauthenticated hop to the indexer unless it was
    /// asked for by name.
    ///
    /// This used to be a `warn!`, which fails OPEN twice over: the deployment
    /// runs, and the warning goes to a log the operator of an ATTESTED enclave
    /// cannot read, because attestation is what closes the console. So the one
    /// signal that every flushed batch was readable by the parent host reached
    /// nobody (Hornby review, 2026-08-19).
    ///
    /// Whitespace counts as unset. `ZIH_INDEXER_TLS=` templated in by a deploy
    /// script is the shape this is most likely to arrive in, and treating it as
    /// "configured" would reintroduce exactly the silence being removed.
    pub fn check_indexer_tls(&self) -> Result<(), BoxError> {
        let configured = self
            .indexer_tls
            .as_deref()
            .map(str::trim)
            .is_some_and(|name| !name.is_empty());

        match (configured, self.allow_plaintext_indexer) {
            (true, _) => Ok(()),
            (false, true) => {
                tracing::warn!(
                    "--allow-plaintext-indexer: the hop to the indexer is PLAINTEXT and the \
                     parent host can read every batch before it is public. Never deploy this."
                );
                Ok(())
            }
            (false, false) => Err("refusing to start: no --indexer-tls, so the hop to the \
                 indexer would be plaintext h2c and the enclave's parent host would read \
                 every flushed batch minutes before it is public. Set --indexer-tls to the \
                 name the indexer's certificate carries, or pass --allow-plaintext-indexer \
                 to say you meant it."
                .into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a config with only the fields these tests care about.
    fn cfg(indexer_tls: Option<&str>, allow_plaintext: bool) -> Config {
        let mut c = Config::parse_from(["zero-indexer-hub", "--indexer", "127.0.0.1:1"]);
        c.indexer_tls = indexer_tls.map(str::to_owned);
        c.allow_plaintext_indexer = allow_plaintext;
        c
    }

    #[test]
    fn an_unset_indexer_tls_refuses_to_start() {
        // It used to warn and continue, into a console attestation has closed,
        // so nobody learned the parent host could read every batch.
        assert!(cfg(None, false).check_indexer_tls().is_err());
    }

    #[test]
    fn an_empty_or_whitespace_indexer_tls_counts_as_unset() {
        // `ZIH_INDEXER_TLS=` templated in by a deploy script is the shape this
        // arrives in most often. Treating it as configured would reintroduce
        // exactly the silence being removed.
        assert!(cfg(Some(""), false).check_indexer_tls().is_err());
        assert!(cfg(Some("   "), false).check_indexer_tls().is_err());
    }

    #[test]
    fn a_configured_indexer_tls_starts() {
        assert!(cfg(Some("na.zec.rocks"), false).check_indexer_tls().is_ok());
    }

    #[test]
    fn plaintext_is_allowed_only_when_asked_for_by_name() {
        assert!(cfg(None, true).check_indexer_tls().is_ok());
    }

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
