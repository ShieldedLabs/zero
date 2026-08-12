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

    /// Divert over the Nym mixnet instead of the clearnet HTTP hop: a
    /// comma-separated LIST of hub Nym addresses. Mutually exclusive with
    /// `--hub`; setting both is a startup error, because which transport is in
    /// use decides whether the operator can see a divert at all.
    ///
    /// A LIST, never a single value, for three independent reasons (D10). A Nym
    /// address embeds its gateway and dies with it, so one hub is hosted at
    /// several gateways for uptime; a diskless hub mints a new address on every
    /// restart, so shims carry the current and the just-rotated one at once;
    /// and send-to-all-hubs (REVIEW #6) is a config change on top of this shape,
    /// not a schema break.
    #[arg(long, env = "ZIS_HUB_NYM", value_delimiter = ',')]
    pub hub_nym: Vec<String>,

    /// Rotate the mixnet client's identity every N seconds (D11): the window
    /// within which the hub can link one shim's submissions under a single
    /// sender tag. Unset means NEVER rotate, which leaves that window at the
    /// whole process uptime. Only meaningful with `--hub-nym`, and the period is
    /// a deployment decision, which is why there is no default.
    #[arg(long, env = "ZIS_NYM_ROTATION_SECS")]
    pub nym_rotation_secs: Option<u64>,

    /// Localnet end-to-end tests only: load the mixnet topology from this file
    /// instead of connecting to the default network. Requires a build with the
    /// `mixnet-localnet` feature; set on a production binary it is a startup
    /// error, not a silent ignore.
    #[arg(long, env = "ZIS_NYM_TOPOLOGY")]
    pub nym_topology: Option<std::path::PathBuf>,

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

/// Which transport carries diversions, decided once at startup.
///
/// The set is closed and the choice is explicit: a shim is forward-only, or it
/// diverts over the transitional clearnet hop, or it diverts over the mixnet.
/// There is no default and no fallback between them, because silently
/// answering "the other way" is exactly the leak the hub exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubSelection {
    /// No hub: classify and log, forward everything. No privacy.
    ForwardOnly,
    /// The transitional clearnet path.
    Http(SocketAddr),
    /// The mixnet path, over one or more gateway-bound hub addresses.
    Nym(Vec<String>),
}

/// Why a hub selection is unusable. Every variant aborts startup: a shim that
/// guessed here would divert somewhere its operator did not intend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// Both transports were configured. Which one carries a divert decides
    /// whether the operator can observe it, so this is never inferred.
    BothTransports,
    /// A hub Nym address is not of the form `identity.encryption@gateway`.
    /// Checked structurally here; the authoritative parse is the SDK's, in the
    /// driver.
    MalformedNymAddress(String),
    /// The same hub address appears twice. Harmless to send to, but it means
    /// the operator believes there is redundancy that does not exist.
    DuplicateNymAddress(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::BothTransports => f.write_str(
                "--hub and --hub-nym are mutually exclusive: set exactly one transport",
            ),
            ConfigError::MalformedNymAddress(addr) => write!(
                f,
                "--hub-nym entry is not a Nym address of the form identity.encryption@gateway: {addr}"
            ),
            ConfigError::DuplicateNymAddress(addr) => {
                write!(f, "--hub-nym lists the same address twice: {addr}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Resolve the configured transport, rejecting anything ambiguous.
    pub fn hub_selection(&self) -> Result<HubSelection, ConfigError> {
        // Empty entries are dropped, not diagnosed. `ZIS_HUB_NYM=` reaches
        // clap as one EMPTY value rather than as no value at all, because with
        // a delimiter clap splits whatever the variable holds and an unset
        // variable is not the same thing as an empty one. Without this an
        // existing clearnet deployment that templates the new variable in as
        // empty would stop booting, either because both transports look set or
        // because "" looks like a malformed address. (The same trap is
        // documented above for `--tls-production`; it is the environment's,
        // not clap's.)
        let addresses: Vec<&str> = self
            .hub_nym
            .iter()
            .map(|addr| addr.trim())
            .filter(|addr| !addr.is_empty())
            .collect();

        match (self.hub, addresses.is_empty()) {
            (Some(_), false) => Err(ConfigError::BothTransports),
            (Some(addr), true) => Ok(HubSelection::Http(addr)),
            (None, true) => Ok(HubSelection::ForwardOnly),
            (None, false) => {
                let mut seen: Vec<&str> = Vec::new();
                for addr in &addresses {
                    if !is_nym_address(addr) {
                        return Err(ConfigError::MalformedNymAddress((*addr).to_owned()));
                    }
                    if seen.contains(addr) {
                        return Err(ConfigError::DuplicateNymAddress((*addr).to_owned()));
                    }
                    seen.push(addr);
                }
                Ok(HubSelection::Nym(
                    addresses.iter().map(|addr| (*addr).to_owned()).collect(),
                ))
            }
        }
    }
}

/// A structural check for `identity.encryption@gateway`, the form a Nym address
/// takes. Deliberately shallow: it catches a truncated or empty entry at
/// startup rather than at the first divert, and leaves the real parse (base58,
/// key lengths) to the SDK in the driver, so this cannot reject an address the
/// SDK would have accepted.
fn is_nym_address(addr: &str) -> bool {
    let Some((keys, gateway)) = addr.split_once('@') else {
        return false;
    };
    let Some((identity, encryption)) = keys.split_once('.') else {
        return false;
    };
    !gateway.is_empty()
        && !identity.is_empty()
        && !encryption.is_empty()
        && !gateway.contains('@')
        && !encryption.contains('.')
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
mod hub_selection_tests {
    use super::*;

    /// Two syntactically valid hub addresses, in the shape the SDK prints.
    const HUB_A: &str = "8HUf4wdaTTmjBZTWY2QpzHxxPGaSfvyg7QHfGYLD6Sea.FhXtgW892fPF2PBQKh22op36fRpJv5aJSmhRnRL63hWV@HdneFpALdZYPhjJ7KVc2MPmsGosUoHWdP4dCVbvr4Kzg";
    const HUB_B: &str = "E9eGFHtTXiwNLFvTxXmBNcDocud3ZUbt7rq8WQJcmx1z.FjjFUxWiZ7pLjSkQdoJzuzH9ia2y1TfJgN8XpvTUBPgp@GdneFpALdZYPhjJ7KVc2MPmsGosUoHWdP4dCVbvr4Kzg";

    fn parse(args: &[&str]) -> Config {
        let mut argv = vec!["zero-indexer-shim"];
        argv.extend_from_slice(args);
        Config::parse_from(argv)
    }

    #[test]
    fn no_hub_is_forward_only() {
        assert_eq!(
            parse(&[]).hub_selection().unwrap(),
            HubSelection::ForwardOnly
        );
    }

    #[test]
    fn a_hub_address_selects_the_http_transport() {
        assert_eq!(
            parse(&["--hub", "10.0.0.5:9069"]).hub_selection().unwrap(),
            HubSelection::Http("10.0.0.5:9069".parse().unwrap())
        );
    }

    #[test]
    fn a_nym_address_list_selects_the_mixnet_transport() {
        let selection = parse(&["--hub-nym", &format!("{HUB_A},{HUB_B}")])
            .hub_selection()
            .unwrap();
        assert_eq!(
            selection,
            HubSelection::Nym(vec![HUB_A.to_owned(), HUB_B.to_owned()]),
            "the list keeps its order: the first is tried first"
        );
    }

    #[test]
    fn setting_both_transports_is_an_error() {
        // Which transport carries a divert decides whether the operator can see
        // it, so this must never be inferred from precedence.
        let config = parse(&["--hub", "10.0.0.5:9069", "--hub-nym", HUB_A]);
        assert_eq!(config.hub_selection(), Err(ConfigError::BothTransports));
    }

    #[test]
    fn a_malformed_nym_address_is_rejected_at_startup() {
        // Each of these would otherwise fail at the first divert, long after
        // the operator has walked away from the deploy.
        for bad in [
            "not-an-address",
            "identity.encryption",        // no gateway
            "identityencryption@gateway", // no key separator
            "@gateway",
            "identity.@gateway",
            ".encryption@gateway",
            "identity.encryption@",
        ] {
            assert_eq!(
                parse(&["--hub-nym", bad]).hub_selection(),
                Err(ConfigError::MalformedNymAddress(bad.to_owned())),
                "{bad} should not parse as a hub address"
            );
        }
    }

    #[test]
    fn a_duplicated_address_is_rejected() {
        // Not harmful to send to, but it means the operator believes there is
        // gateway redundancy that does not exist.
        let config = parse(&["--hub-nym", &format!("{HUB_A},{HUB_A}")]);
        assert_eq!(
            config.hub_selection(),
            Err(ConfigError::DuplicateNymAddress(HUB_A.to_owned()))
        );
    }

    #[test]
    fn an_empty_hub_nym_is_the_same_as_an_unset_one() {
        // `ZIS_HUB_NYM=` arrives as one empty value, not as no value: with a
        // delimiter clap splits whatever the variable holds. Both cases below
        // used to abort startup, the first of them breaking a working clearnet
        // deployment that merely templated the new variable in as empty.
        let empty = parse(&["--hub-nym", ""]);
        assert_eq!(
            empty.hub_nym,
            vec![String::new()],
            "the field really does hold one empty entry"
        );
        assert_eq!(empty.hub_selection().unwrap(), HubSelection::ForwardOnly);

        let with_http = parse(&["--hub", "10.0.0.5:9069", "--hub-nym", ""]);
        assert_eq!(
            with_http.hub_selection().unwrap(),
            HubSelection::Http("10.0.0.5:9069".parse().unwrap()),
            "an empty mixnet list must not make a clearnet deployment ambiguous"
        );

        // Whitespace and stray separators are empty too.
        assert_eq!(
            parse(&["--hub-nym", " , ,"]).hub_selection().unwrap(),
            HubSelection::ForwardOnly
        );
    }

    #[test]
    fn a_stray_empty_entry_does_not_invalidate_a_real_list() {
        let selection = parse(&["--hub-nym", &format!("{HUB_A},,{HUB_B}")])
            .hub_selection()
            .unwrap();
        assert_eq!(
            selection,
            HubSelection::Nym(vec![HUB_A.to_owned(), HUB_B.to_owned()])
        );
    }

    #[test]
    fn surrounding_whitespace_in_a_list_is_tolerated() {
        // A hand-edited environment variable with a space after the comma is
        // an operator typo that costs nothing to accept.
        let selection = parse(&["--hub-nym", &format!("{HUB_A} , {HUB_B}")])
            .hub_selection()
            .unwrap();
        assert_eq!(
            selection,
            HubSelection::Nym(vec![HUB_A.to_owned(), HUB_B.to_owned()])
        );
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
