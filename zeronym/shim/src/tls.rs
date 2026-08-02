//! TLS for both of the shim's hops, and the reason each one is shaped the way
//! it is.
//!
//! The shim sits between a wallet and an indexer, so there are two independent
//! links and they have different threat models:
//!
//! * **Wallet to shim** ([`ServerTls`]). Terminated here. The certificate is
//!   obtained by ACME **inside the enclave**, so the private key is generated
//!   in the enclave and never exists anywhere else. That is not a convenience
//!   choice. A key minted outside and injected would let whoever minted it
//!   impersonate the enclave, which would make the attestation worthless: an
//!   auditor could confirm the running code and still be talking to a proxy in
//!   front of it. Key-born-in-enclave is what makes "the code you read is the
//!   code serving you" survive contact with the network.
//!
//! * **Shim to indexer** ([`BackendTls`]). Originated here, verifying an
//!   ordinary WebPKI chain. Note the deliberate split between *who we dial* and
//!   *who we verify*: the address stays a literal `SocketAddr` while the name
//!   checked against the certificate is configured separately. The enclave
//!   therefore never resolves DNS (its egress rule is one `/32`, with no port
//!   53), and a poisoned DNS answer cannot redirect it, yet the connection is
//!   still authenticated against a name rather than an address.
//!
//! Both directions negotiate ALPN `h2`, because the payload is gRPC and gRPC is
//! HTTP/2. Getting this wrong does not fail loudly; it fails as a peer that
//! speaks HTTP/1.1 at an HTTP/2 client.
//!
//! ## The rate limit, which is an operational property of a diskless enclave
//!
//! A Nitro enclave has no persistent storage, so there is nowhere to cache an
//! issued certificate: [`NoCache`] is the only honest choice, and every restart
//! is a fresh ACME order. Let's Encrypt permits 5 duplicate certificates per
//! week for an identical name set, so more than five restarts in a rolling week
//! will start failing issuance. Redeploys are therefore tracked deliberately;
//! see `deploy/caution/RESTARTS.md`.

use std::net::SocketAddr;
use std::sync::Arc;

use rustls::{ClientConfig, RootCertStore, ServerConfig};
use rustls_acme::caches::NoCache;
use rustls_acme::{is_tls_alpn_challenge, AcmeConfig};
use rustls_pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_rustls::{LazyConfigAcceptor, TlsConnector};

use crate::BoxError;

/// ALPN for gRPC. HTTP/2 only: the shim has no HTTP/1.1 path, so offering
/// `http/1.1` here would advertise a protocol it cannot then speak.
const ALPN_H2: &[u8] = b"h2";

/// Install the `ring` crypto provider as the process default.
///
/// rustls requires exactly one process-wide default provider, and with
/// `default-features = false` there is no automatic one to fall back on: the
/// first `ServerConfig::builder()` would panic with "no process-level
/// CryptoProvider available". Calling this twice is not an error, which is why
/// the result is discarded rather than unwrapped: a second call losing the race
/// means the provider is already installed, which is the desired state.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Server-side TLS: an ACME-managed certificate for the wallet-facing port.
pub struct ServerTls {
    /// Serves wallet traffic: the ACME-issued certificate, ALPN h2.
    serving: Arc<ServerConfig>,
    /// Answers TLS-ALPN-01 validation. A separate config with its own ALPN,
    /// supplied by rustls-acme; it must never be used for wallet traffic and
    /// wallet traffic must never be served with it.
    challenge: Arc<ServerConfig>,
    domain: String,
}

impl ServerTls {
    /// Start ACME for `domain` and spawn the task that drives it.
    ///
    /// Issuance happens in the background: this returns as soon as the state
    /// machine is running, not when a certificate exists. That is deliberate.
    /// Blocking startup on issuance would make the shim unable to accept the
    /// TLS-ALPN-01 challenge that issuance itself depends on, which deadlocks.
    /// Until the order completes, handshakes fail; they do not fall back to
    /// plaintext.
    pub fn start(domain: &str, contact_email: Option<&str>, production: bool) -> Self {
        install_crypto_provider();

        // `new`/`challenge_rustls_config` without a provider argument exist
        // only when a default-provider feature is on. This crate pins `ring`
        // explicitly (aws-lc-rs wants cmake, which the StageX image lacks), so
        // the provider is threaded through by hand.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = AcmeConfig::new_with_provider([domain], provider.clone())
            // Nowhere to persist: an enclave is diskless. Every restart
            // re-orders, which is what makes the weekly duplicate-certificate
            // limit an operational concern rather than a footnote.
            // Turbofished because NoCache is generic over its (never
            // constructed) error types and nothing else in the chain pins them.
            .cache(NoCache::<std::io::Error, std::io::Error>::default())
            .directory_lets_encrypt(production);
        if let Some(email) = contact_email {
            config = config.contact_push(format!("mailto:{email}"));
        }

        let mut state = config.state();
        // All three handles must be taken BEFORE the state moves into the task
        // below, which consumes it. Reordering this is the kind of edit that
        // looks harmless and does not compile.
        let resolver = state.resolver();
        let challenge = state.challenge_rustls_config_with_provider(provider.clone());

        // rustls-acme's state IS the ACME client: ordering, challenge
        // provisioning and renewal only advance while this stream is polled.
        // Without this task the resolver never receives a certificate and every
        // handshake fails forever, with no error to say why.
        tokio::spawn(async move {
            use futures_util::StreamExt;
            while let Some(event) = state.next().await {
                match event {
                    Ok(ok) => tracing::info!(?ok, "acme event"),
                    // Not fatal. Issuance is retried by the state machine, and
                    // the common cause is transient: DNS not yet pointing at
                    // this enclave, or the weekly duplicate-certificate limit.
                    Err(err) => tracing::error!(%err, "acme error"),
                }
            }
            tracing::error!("acme state machine ended; certificates will not renew");
        });

        // Built here rather than using `state.default_rustls_config()`, which
        // returns a ready-made config with no ALPN. gRPC needs h2 advertised at
        // the handshake, and an Arc cannot be mutated after the fact.
        let mut serving = ServerConfig::builder_with_provider(provider)
            // Cannot fail for a provider we just constructed; a failure here
            // would mean `ring` supports no protocol versions at all.
            .with_safe_default_protocol_versions()
            .expect("ring provider supports the default protocol versions")
            .with_no_client_auth()
            .with_cert_resolver(resolver);
        serving.alpn_protocols = vec![ALPN_H2.to_vec()];

        Self {
            serving: Arc::new(serving),
            challenge,
            domain: domain.to_owned(),
        }
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// Complete a TLS handshake, or absorb an ACME challenge.
    ///
    /// Returns `Ok(None)` when the connection was a TLS-ALPN-01 challenge from
    /// the ACME server rather than a client. Those are not wallet traffic and
    /// must not be handed to the proxy; the acceptor has already answered them.
    pub async fn accept<IO>(&self, io: IO) -> Result<Option<tokio_rustls::server::TlsStream<IO>>, BoxError>
    where
        IO: AsyncRead + AsyncWrite + Unpin,
    {
        // Lazy so the ClientHello can be inspected before a config is chosen:
        // which certificate to present depends on whether this is a wallet or
        // the ACME server validating us.
        let start = LazyConfigAcceptor::new(Default::default(), io).await?;

        if is_tls_alpn_challenge(&start.client_hello()) {
            // Validation, not traffic. Complete the handshake with the
            // challenge config (that IS the proof of control) and close. It
            // must not reach the proxy: it carries no gRPC and the peer is
            // Let's Encrypt, not a wallet.
            let mut tls = start.into_stream(self.challenge.clone()).await?;
            tls.shutdown().await?;
            tracing::info!("answered a TLS-ALPN-01 validation request");
            return Ok(None);
        }

        Ok(Some(start.into_stream(self.serving.clone()).await?))
    }
}

/// Client-side TLS to the backing indexer.
#[derive(Clone)]
pub struct BackendTls {
    connector: TlsConnector,
    /// The name verified against the server certificate. Distinct from the
    /// address dialled, on purpose: see the module docs.
    server_name: ServerName<'static>,
}

impl BackendTls {
    /// Verify the backend against `sni_name` using the WebPKI roots.
    ///
    /// `sni_name` must be a DNS name, not an address. An IP literal is
    /// accepted by the parser (rustls has an `IpAddress` variant) but no public
    /// CA issues IP SANs, so it would fail at the handshake rather than here.
    ///
    /// The roots are compiled in (`webpki-roots`) rather than read from the
    /// filesystem. In an enclave there is no system trust store to read, and a
    /// store the operator could edit would let them substitute their own CA and
    /// silently terminate this hop themselves.
    pub fn new(sni_name: &str) -> Result<Self, BoxError> {
        install_crypto_provider();

        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let mut config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![ALPN_H2.to_vec()];

        let server_name = ServerName::try_from(sni_name.to_owned())
            .map_err(|_| -> BoxError { format!("invalid backend TLS name {sni_name:?}").into() })?;

        Ok(Self {
            connector: TlsConnector::from(Arc::new(config)),
            server_name,
        })
    }

    /// The HTTP `:authority` a request to this backend should carry.
    ///
    /// The verified name, not the dialled address, and the default port is
    /// elided as HTTP requires. A backend that routes by host (any ingress
    /// controller) matches on this, so getting it wrong produces a 404 from a
    /// connection that is otherwise perfectly healthy.
    pub fn authority(&self, port: u16) -> String {
        let name = match &self.server_name {
            ServerName::DnsName(name) => name.as_ref().to_owned(),
            other => format!("{other:?}"),
        };
        if port == 443 {
            name
        } else {
            format!("{name}:{port}")
        }
    }

    /// Dial `addr` and authenticate it as the configured name.
    pub async fn connect(
        &self,
        addr: SocketAddr,
        stream: TcpStream,
    ) -> Result<tokio_rustls::client::TlsStream<TcpStream>, BoxError> {
        let _ = addr;
        Ok(self.connector.connect(self.server_name.clone(), stream).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpn_is_h2_only() {
        // gRPC is HTTP/2. Advertising http/1.1 would promise a protocol the
        // proxy cannot speak, and the failure would appear at the first
        // request rather than at the handshake.
        assert_eq!(ALPN_H2, b"h2");
    }

    #[test]
    fn backend_tls_accepts_a_dns_name() {
        assert!(BackendTls::new("zaino.shieldedinfra.net").is_ok());
    }

    #[test]
    fn authority_is_the_verified_name_not_the_address() {
        // Regression test for a real failure: with the dialled address as the
        // :authority, our Traefik ingress matched no host rule and returned
        // 404 over a TLS connection that had succeeded.
        let tls = BackendTls::new("lwd.shieldedinfra.net").unwrap();
        assert_eq!(tls.authority(443), "lwd.shieldedinfra.net");
        // Non-default ports are kept, as HTTP requires.
        assert_eq!(tls.authority(8443), "lwd.shieldedinfra.net:8443");
    }

    #[test]
    fn an_ip_literal_parses_but_is_the_wrong_thing_to_configure() {
        // Worth pinning because it is a trap rather than an obvious error.
        // rustls parses an IP into ServerName::IpAddress, so this does NOT
        // fail here; it fails later, at the handshake, because a public CA
        // will not put an IP in a SAN. Configuring --backend-tls with an
        // address therefore looks fine at startup and breaks on the first
        // wallet request. The name must be the DNS name the backend's
        // certificate was issued for, even though the address dialled is a
        // literal.
        assert!(BackendTls::new("66.42.124.202").is_ok());
    }

    #[test]
    fn crypto_provider_installs_idempotently() {
        install_crypto_provider();
        install_crypto_provider();
    }
}
