//! Client to the zero-indexer-hub.
//!
//! When a `SendTransaction` carries Orchard actions it is diverted here instead
//! of going to the operator's indexer: the raw bytes are POSTed to the hub,
//! which broadcasts them immediately and returns the txid. It is a plain
//! HTTP/1.1 POST (the hub is not a gRPC service), optionally over TLS,
//! authenticated by name exactly as the backend connection is.
//!
//! Each `submit` dials fresh. Migrations are infrequent, and a persistent
//! multiplexed connection to the hub would itself be a standing side channel
//! about when this shim last diverted.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::tls::BackendTls;
use crate::BoxError;

/// A connection recipe to the hub. Cheap to clone.
#[derive(Clone)]
pub struct HubClient {
    addr: SocketAddr,
    tls: Option<Arc<BackendTls>>,
    authority: String,
}

/// The hub's verdict on a submitted migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submit {
    Accepted { txid: String },
    AlreadyKnown { txid: Option<String> },
    Rejected { reason: String },
}

#[derive(Deserialize)]
struct HubResponse {
    disposition: String,
    txid: Option<String>,
    reason: Option<String>,
}

impl HubClient {
    pub fn new(addr: SocketAddr, tls: Option<BackendTls>) -> Self {
        let authority = match &tls {
            Some(t) => t.authority(addr.port()),
            None => addr.to_string(),
        };
        HubClient {
            addr,
            tls: tls.map(Arc::new),
            authority,
        }
    }

    /// POST the raw transaction bytes to the hub and read back its verdict.
    pub async fn submit(&self, tx_bytes: &[u8]) -> Result<Submit, BoxError> {
        let stream = TcpStream::connect(self.addr).await?;
        stream.set_nodelay(true)?;

        let req = Request::builder()
            .method("POST")
            .uri("/")
            .header(hyper::header::HOST, &self.authority)
            .header(hyper::header::CONTENT_TYPE, "application/octet-stream")
            .body(Full::new(Bytes::copy_from_slice(tx_bytes)))?;

        let body = match &self.tls {
            Some(tls) => {
                let stream = tls.connect(self.addr, stream).await?;
                round_trip(stream, req).await?
            }
            None => round_trip(stream, req).await?,
        };

        let parsed: HubResponse = serde_json::from_slice(&body)?;
        Ok(match parsed.disposition.as_str() {
            "accepted" => Submit::Accepted {
                txid: parsed.txid.unwrap_or_default(),
            },
            "already_known" => Submit::AlreadyKnown { txid: parsed.txid },
            _ => Submit::Rejected {
                reason: parsed.reason.unwrap_or(parsed.disposition),
            },
        })
    }
}

/// One HTTP/1.1 request/response over an already-connected stream, TLS or not.
async fn round_trip<IO>(stream: IO, req: Request<Full<Bytes>>) -> Result<Bytes, BoxError>
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let resp = sender.send_request(req).await?;
    let body = resp.into_body().collect().await?.to_bytes();
    Ok(body)
}
