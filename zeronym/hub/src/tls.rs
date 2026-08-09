//! TLS for the hub's outbound connections to full nodes.
//!
//! **Why this is not optional.** The hub holds every migration in plaintext and
//! then hands whole batches to the indexer. Without TLS on that hop the
//! enclave's PARENT host reads every request body in the clear, which means it
//! reads the entire batch a few seconds before the batch becomes public, plus
//! every tip query that drives the flush clock. An attested enclave whose
//! egress is plaintext is only attested about the part nobody was reading.
//!
//! **Dial an address, verify a NAME.** Exactly the split the shim's `BackendTls`
//! uses, and for the same reason: the enclave has no DNS egress (no port 53 in
//! its network policy), so it cannot resolve a hostname. `ZIH_INDEXERS`
//! therefore carries literal `IPv4:port` addresses, while `ZIH_INDEXER_TLS`
//! names what the certificate must say. That combination is stronger than either half alone: a
//! poisoned DNS answer has nothing to poison because no lookup happens, and a
//! hijacked address cannot present a valid certificate for the name.
//!
//! ALPN is `h2`. The hub broadcasts through an indexer's `CompactTxStreamer`,
//! and gRPC is HTTP/2 by definition, so an ALPN mismatch here does not degrade
//! gracefully: a server that honours ALPN would negotiate HTTP/1.1 and every
//! call would fail on a connection that looks perfectly healthy.
//!
//! The roots are compiled in (`webpki-roots`) rather than read from the
//! filesystem. An enclave has no system trust store to read, and a store the
//! operator could edit would let them substitute their own CA and quietly
//! terminate this hop themselves, which is the whole exposure this closes.

use std::net::SocketAddr;
use std::sync::Arc;

use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::BoxError;

/// ALPN for gRPC, which is HTTP/2 only.
const ALPN_H2: &[u8] = b"h2";

/// Install the process-wide rustls crypto provider. Idempotent, and safe to
/// call from every constructor.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A TLS client for the hub's indexer connections.
pub struct IndexerTls {
    connector: TlsConnector,
    /// The name verified against the server certificate. Deliberately distinct
    /// from the address dialled; see the module docs.
    server_name: ServerName<'static>,
    /// The `Host` header a request should carry: the verified name, not the
    /// dialled address. Any ingress in front of the node routes on this, so
    /// getting it wrong yields a 404 over a perfectly healthy TLS connection.
    authority: String,
}

impl IndexerTls {
    /// Verify every indexer against `sni_name` using the compiled-in WebPKI
    /// roots.
    ///
    /// One name for all endpoints, because they are expected to sit behind one
    /// ingress. If they ever need distinct names this becomes per-endpoint.
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
            .map_err(|_| -> BoxError { format!("invalid indexer TLS name {sni_name:?}").into() })?;

        Ok(Self {
            connector: TlsConnector::from(Arc::new(config)),
            server_name,
            authority: sni_name.to_owned(),
        })
    }

    /// The `Host` header value for requests over this connection.
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// Wrap an established TCP stream in TLS, verifying the configured name.
    pub async fn connect(
        &self,
        addr: SocketAddr,
        stream: TcpStream,
    ) -> Result<tokio_rustls::client::TlsStream<TcpStream>, BoxError> {
        let _ = addr;
        Ok(self
            .connector
            .connect(self.server_name.clone(), stream)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dns_name_is_accepted() {
        assert!(IndexerTls::new("zebrad-rpc.shieldedinfra.net").is_ok());
    }

    #[test]
    fn a_garbage_name_is_refused_at_construction() {
        // Refused here, where the error is readable, rather than inside an
        // enclave with no console.
        assert!(IndexerTls::new("not a name").is_err());
    }

    #[test]
    fn the_authority_is_the_verified_name_not_an_address() {
        // The ingress in front of the node routes on this. An address here
        // matches no host rule and answers 404 over a healthy connection.
        let tls = IndexerTls::new("zebrad-rpc.shieldedinfra.net").expect("valid name");
        assert_eq!(tls.authority(), "zebrad-rpc.shieldedinfra.net");
    }
}
