//! Client to the zero-indexer-hub.
//!
//! Two operations, both plain HTTP/1.1 POSTs (the hub is not a gRPC service),
//! optionally over TLS authenticated by name exactly as the backend link is:
//!
//! * [`HubClient::submit`] (`POST /`) diverts an Orchard-touching transaction to
//!   the hub for batched broadcast, instead of handing it to the operator's
//!   indexer, and returns the hub's verdict.
//! * [`HubClient::get_transaction`] (`POST /transaction`) looks a transaction up
//!   by its `TxFilter.hash`, so a wallet's follow-up `GetTransaction` is answered
//!   by the hub (from its queue, or its own indexer) rather than the operator's.
//!   This is what lets the shim keep no per-migration state: it recognises
//!   nothing, and asks the hub every time.
//!
//! The TLS for this hop must advertise ALPN `http/1.1` (`BackendTls::new_http1`,
//! wired in `main.rs`), NOT the `h2` the gRPC backend uses. Offering `h2` to the
//! hub's ALPN-honouring proxy hangs every call; see `tls.rs`.
//!
//! Each call dials fresh. Migrations and their lookups are infrequent, and a
//! persistent multiplexed connection would itself be a standing side channel
//! about this shim's activity.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::client::conn::http1;
use hyper::header::CONTENT_TYPE;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::nym::{NymError, NymHandle};
use crate::tls::BackendTls;
use crate::wire::{LookupReply, WireError};
use crate::BoxError;

/// The hub's lookup path.
const TRANSACTION_PATH: &str = "/transaction";

/// Header carrying the transaction height on a `200` lookup reply.
const TX_HEIGHT_HEADER: &str = "x-tx-height";

/// Ceiling on a lookup: above the hub's own 10 s indexer timeout, so the hub's
/// 404/502 verdict usually arrives before this fires.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Ceiling on a submission. The hub answers a submission as soon as it admits
/// the transaction to a batch, so the only slow part is moving the body: an
/// enclave sustains ~220 KB/s outbound (measured 2026-08-18), so even a
/// maximum-size transaction is ~10 s of upload. Thirty seconds leaves room for
/// that plus a TLS handshake and still fails long before a wallet gives up.
///
/// Without this the wallet's `SendTransaction` inherits a hub stall unbounded:
/// a hub that accepts the connection and never replies would hold the wallet
/// open forever, and the shim would never fall through to its own verdict.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on a hub response body. A mined transaction is at most ~2 MB; this
/// bounds memory against a misbehaving or hostile hub.
const MAX_HUB_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// A connection recipe to the hub. Cheap to clone.
#[derive(Clone)]
pub struct HubClient {
    addr: SocketAddr,
    tls: Option<Arc<BackendTls>>,
    authority: String,
}

/// The outcome of a submission attempt: the hub's verdict, or the one refusal
/// the shim makes on its own behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submit {
    Accepted {
        txid: String,
    },
    AlreadyKnown {
        txid: Option<String>,
    },
    Rejected {
        reason: String,
    },
    /// The transaction does not fit the fixed frame, so this transport cannot
    /// carry it at all.
    ///
    /// Distinct from `Rejected` because it is not a verdict and not a
    /// transient condition: retrying cannot help, and the caller must say so
    /// to the wallet rather than let it read a generic failure as "try again".
    /// `limit` is the frame budget, deliberately NOT the transaction's length,
    /// which must not reach a log (D4).
    TooLarge {
        limit: usize,
    },
}

/// The hub's answer to a transaction lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Found { data: Bytes, height: u64 },
    NotFound,
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

    /// POST the transaction to the hub, padded, and read back its verdict.
    ///
    /// The body is a `SubmitV1` frame -- the same fixed-size, zero-padded frame
    /// the mixnet path uses -- and NOT the bare transaction, because the bare
    /// transaction's LENGTH is a fingerprint. This client dials fresh per
    /// migration (deliberately: a standing connection would itself signal this
    /// shim's activity), so each dial is already a timestamped diversion event.
    /// Letting its size track the transaction turns that into a join key
    /// against public on-chain data, which is the same leak the shim's log was
    /// just cleaned of. TLS is no defence: ciphertext length tracks plaintext
    /// length.
    pub async fn submit(&self, tx_bytes: &[u8]) -> Result<Submit, BoxError> {
        // Minted per submission and never reused. The clearnet path does not
        // correlate on it -- the verdict comes back on this HTTP response --
        // but the frame is one format across both transports, and a constant
        // here would be the one field in an otherwise fixed body that is
        // identical on every submission.
        let mut nonce: crate::wire::Nonce = [0u8; crate::wire::NONCE_BYTES];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);

        // Refused HERE rather than by the hub's 413, so an oversized migration
        // costs no dial at all. The budget is the frame's, not the raw body's:
        // the header takes its bite before the transaction does.
        let frame = match crate::wire::encode_submit(&nonce, tx_bytes) {
            Ok(frame) => frame,
            Err(_) => {
                return Ok(Submit::TooLarge {
                    limit: crate::wire::MAX_NYM_TX_BYTES,
                })
            }
        };

        // The deadline covers the whole exchange -- connect, TLS handshake,
        // request, response body -- not just the read. Each of those can stall
        // independently, and a deadline around only one of them bounds nothing.
        let attempt = async {
            let stream = TcpStream::connect(self.addr).await?;
            stream.set_nodelay(true)?;

            let req = Request::builder()
                .method("POST")
                .uri("/")
                .header(hyper::header::HOST, &self.authority)
                .header(hyper::header::CONTENT_TYPE, "application/octet-stream")
                .body(Full::new(Bytes::copy_from_slice(&frame)))?;

            match &self.tls {
                Some(tls) => {
                    let stream = tls.connect(self.addr, stream).await?;
                    round_trip(stream, req).await
                }
                None => round_trip(stream, req).await,
            }
        };

        let (parts, body) = tokio::time::timeout(SUBMIT_TIMEOUT, attempt)
            .await
            .map_err(|_| -> BoxError { "hub submission timed out".into() })??;

        // The hub caps a clearnet submission body at its frame size and answers
        // `413` with a plain-text body, which would otherwise fail to parse as
        // JSON and become an indistinguishable transport error. Same typed
        // outcome as the mixnet path takes locally: retrying cannot help. The
        // reported limit is the hub's HTTP body cap (`FRAME_BYTES`), NOT the
        // mixnet's `MAX_NYM_TX_BYTES`: this is the clearnet path, whose bound is
        // the hub's frame, which is nine bytes wider than the mixnet tx budget.
        if parts.status == StatusCode::PAYLOAD_TOO_LARGE {
            return Ok(Submit::TooLarge {
                limit: crate::wire::FRAME_BYTES,
            });
        }

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

    /// Look a transaction up by the wallet's `TxFilter.hash` bytes.
    ///
    /// The bytes are posted verbatim (internal, little-endian order); the hub
    /// checks both byte orders against its queue and forwards them unmodified to
    /// its indexer, so behaviour is identical to a direct query. A `200` MUST
    /// carry `application/octet-stream` and a parseable `x-tx-height`, or it is an
    /// error: that is the tripwire against an old hub (which would answer a lookup
    /// POST with a submission's JSON), so a wallet never receives JSON framed as a
    /// transaction. A `404` is `NotFound`; anything else is an error, and the
    /// caller fails closed rather than falling back to the operator.
    pub async fn get_transaction(&self, wire_hash: &[u8]) -> Result<Lookup, BoxError> {
        let attempt = async {
            let stream = TcpStream::connect(self.addr).await?;
            stream.set_nodelay(true)?;

            let req = Request::builder()
                .method("POST")
                .uri(TRANSACTION_PATH)
                .header(hyper::header::HOST, &self.authority)
                .header(CONTENT_TYPE, "application/octet-stream")
                .body(Full::new(Bytes::copy_from_slice(wire_hash)))?;

            match &self.tls {
                Some(tls) => {
                    let stream = tls.connect(self.addr, stream).await?;
                    round_trip(stream, req).await
                }
                None => round_trip(stream, req).await,
            }
        };

        let (parts, body) = tokio::time::timeout(LOOKUP_TIMEOUT, attempt)
            .await
            .map_err(|_| -> BoxError { "hub lookup timed out".into() })??;

        match parts.status {
            StatusCode::OK => {
                let octet_stream = parts
                    .headers
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.starts_with("application/octet-stream"));
                let height = parts
                    .headers
                    .get(TX_HEIGHT_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok());
                match height {
                    Some(height) if octet_stream => Ok(Lookup::Found { data: body, height }),
                    // A 200 that is not shaped like our hub's reply (an old hub, a
                    // proxy error page): refuse rather than hand it to the wallet.
                    _ => Err("hub lookup: 200 without the expected transaction shape".into()),
                }
            }
            StatusCode::NOT_FOUND => Ok(Lookup::NotFound),
            other => Err(format!("hub lookup: unexpected status {other}").into()),
        }
    }
}

/// The shim's link to the hub, as a tagged union over the transports the shim
/// can speak. Closed at compile time: the transitional clearnet HTTP path and
/// the Nym mixnet transport. A match is the whole dispatch; an async method
/// behind a trait object would need `async_trait` or hand-boxed futures for
/// nothing.
pub enum HubTransport {
    /// The transitional clearnet path: a fresh HTTP/1.1 POST per operation.
    Http(HubClient),
    /// The mixnet path: fixed-size frames through a persistent Nym client.
    Nym(NymHandle),
}

impl HubTransport {
    /// Divert a transaction to the hub and read back its verdict.
    pub async fn submit(&self, tx_bytes: &[u8]) -> Result<Submit, BoxError> {
        match self {
            HubTransport::Http(client) => client.submit(tx_bytes).await,
            HubTransport::Nym(handle) => match handle.submit(tx_bytes).await {
                // Best-effort dispatch: a successful hand-off to the mixnet is the
                // wallet's success, carrying the locally-computed txid (the ack
                // never carried one, D5, and is no longer awaited). There is no
                // Refused arm: the hub's verdict is a full round trip away and is
                // deliberately not waited for (see `NymHandle::submit`), so a
                // refusal is never surfaced here.
                Ok(()) => Ok(Submit::Accepted {
                    txid: crate::nym::local_txid(tx_bytes),
                }),
                // A transaction that cannot be framed is a typed outcome, not
                // an error: `?` here would flatten it into the same opaque
                // failure a dead mixnet produces, and the caller would tell
                // the wallet to retry something that can never succeed.
                Err(NymError::Encode(WireError::TxTooLarge { budget, .. })) => {
                    Ok(Submit::TooLarge { limit: budget })
                }
                Err(err) => Err(err.into()),
            },
        }
    }

    /// Look a transaction up on the hub by the wallet's `TxFilter.hash` bytes.
    pub async fn get_transaction(&self, wire_hash: &[u8]) -> Result<Lookup, BoxError> {
        match self {
            HubTransport::Http(client) => client.get_transaction(wire_hash).await,
            HubTransport::Nym(handle) => match handle.get_transaction(wire_hash).await? {
                LookupReply::Found { height, tx } => Ok(Lookup::Found {
                    data: Bytes::copy_from_slice(&tx),
                    height,
                }),
                LookupReply::NotFound => Ok(Lookup::NotFound),
                // The hub could not answer (its indexer failed, or it could not
                // frame the reply). An error, never a NotFound: the caller must
                // fail closed rather than tell a wallet its transaction is gone.
                LookupReply::Error => Err("hub could not answer the lookup".into()),
            },
        }
    }
}

impl From<HubClient> for HubTransport {
    fn from(client: HubClient) -> Self {
        HubTransport::Http(client)
    }
}

impl From<NymHandle> for HubTransport {
    fn from(handle: NymHandle) -> Self {
        HubTransport::Nym(handle)
    }
}

/// One HTTP/1.1 request/response over an already-connected stream, TLS or not.
/// Returns the response head (status, headers) with the body, so a caller can
/// branch on the status and read a typed header; the body is bounded.
async fn round_trip<IO>(
    stream: IO,
    req: Request<Full<Bytes>>,
) -> Result<(http::response::Parts, Bytes), BoxError>
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let resp = sender.send_request(req).await?;
    let (parts, body) = resp.into_parts();
    let bytes = Limited::new(body, MAX_HUB_RESPONSE_BYTES)
        .collect()
        .await
        .map_err(|_| -> BoxError { "hub response exceeded the size limit".into() })?
        .to_bytes();
    Ok((parts, bytes))
}
