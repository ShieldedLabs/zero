//! The hub's connection to the Zcash network, through an indexer.
//!
//! Two calls and nothing else: `GetLightdInfo` gives the height that drives
//! flush scheduling and expiry admission, and `SendTransaction` publishes each
//! member of a flushed batch. The hub deliberately does NOT run a validator
//! in-enclave: mainnet state is hundreds of gigabytes and an enclave is
//! diskless, so it speaks to infrastructure that already exists.
//!
//! **Why an indexer rather than a node's JSON-RPC.** The indexer endpoint is
//! already published over TLS, which the enclave requires: without TLS on this
//! hop the enclave's parent host reads every batch in the clear moments before
//! it is public. Speaking `CompactTxStreamer` also means the hub broadcasts
//! through exactly the interface wallets use, so nothing about a batched
//! migration looks different from an ordinary submission at the point it enters
//! the network.
//!
//! The honest cost, recorded rather than hidden: an indexer is a single funnel
//! in front of a single node, so the "publish to every node" property in
//! `REVIEW.md` #6 is weaker here than with direct multi-node broadcast. Configure
//! more than one endpoint where you can; a batch that entered only one mempool
//! is one outage away from never being mined.
//!
//! gRPC is spoken directly over hyper rather than through tonic. The surface is
//! two unary calls, and the enclave constraint (dial a literal address, verify a
//! DNS NAME, resolve nothing) is awkward to express through a tonic channel but
//! trivial here.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::future::join_all;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use zaino_proto::proto::service::{Empty, LightdInfo, RawTransaction, SendResponse};

use crate::tls::IndexerTls;
use crate::BoxError;

/// Per-request ceiling. An endpoint that hangs must not stall a flush, because
/// the flush is on a block-cadence deadline: late is not merely slow here, it
/// can push a migration past its expiry height.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// gRPC length-prefixed message header: 1 compression flag + 4 big-endian bytes.
const GRPC_PREFIX_LEN: usize = 5;

const SEND_TRANSACTION: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction";
const GET_LIGHTD_INFO: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo";

/// The outcome of publishing one transaction.
///
/// `AlreadyKnown` is a success, and saying so explicitly is load-bearing. Every
/// shim submits every migration to every hub, so the second hub's publish is a
/// duplicate by construction. Treating that as an error would make normal
/// operation look like a fault and could drive a re-submission loop, and
/// re-submissions are a fresh timing signal tied to one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Publish {
    Accepted { txid: String },
    AlreadyKnown,
    Rejected { reason: String },
}

/// A client over one or more indexer endpoints.
pub struct ChainClient {
    endpoints: Vec<SocketAddr>,
    /// `None` means plaintext h2c, which is correct only for a test or a
    /// trusted local path. A deployed enclave always sets this.
    tls: Option<Arc<IndexerTls>>,
}

impl ChainClient {
    pub fn new(endpoints: Vec<SocketAddr>, tls: Option<IndexerTls>) -> Result<Self, BoxError> {
        if endpoints.is_empty() {
            // Refused at construction rather than at the first flush, when the
            // failure would coincide with transactions being at risk of expiry.
            return Err("at least one indexer endpoint is required".into());
        }
        Ok(Self {
            endpoints,
            tls: tls.map(Arc::new),
        })
    }

    pub fn node_count(&self) -> usize {
        self.endpoints.len()
    }

    /// Current best-chain height: the MAX over every endpoint that answers.
    ///
    /// Not "the first that answers", which would be a second, independent lever
    /// on the flush clock: one lagging or hostile endpoint would otherwise stall
    /// the cadence (freezing flushes) or advance it (draining the queue into a
    /// near-empty batch). Taking the max means an adversary must slow down every
    /// endpoint to slow the clock.
    pub async fn tip_height(&self) -> Result<u32, BoxError> {
        let queries = self.endpoints.iter().map(|addr| async move {
            let info: LightdInfo = self.unary(*addr, GET_LIGHTD_INFO, Empty {}).await?;
            Ok::<u32, BoxError>(info.block_height as u32)
        });

        let results = join_all(queries).await;
        results
            .into_iter()
            .filter_map(|result| match result {
                Ok(height) => Some(height),
                Err(err) => {
                    tracing::debug!(%err, "tip query failed");
                    None
                }
            })
            .max()
            .ok_or_else(|| "no indexer answered a tip query".into())
    }

    /// Publish one raw transaction to every configured endpoint.
    pub async fn broadcast(&self, tx_bytes: &[u8]) -> Publish {
        // Concurrent, not sequential. A flush publishes k transactions across n
        // endpoints with a 10 s per-call ceiling; done in sequence, one hung
        // endpoint pushes the total past a block interval, which would
        // reintroduce the very ordering the shuffle exists to remove.
        let calls = self.endpoints.iter().map(|addr| {
            let raw = RawTransaction {
                data: tx_bytes.to_vec(),
                height: 0,
            };
            async move {
                match self
                    .unary::<_, SendResponse>(*addr, SEND_TRANSACTION, raw)
                    .await
                {
                    Ok(resp) => classify_send_response(&resp),
                    Err(err) => Publish::Rejected {
                        reason: err.to_string(),
                    },
                }
            }
        });

        join_all(calls)
            .await
            .into_iter()
            .fold(None, |best: Option<Publish>, outcome| {
                Some(match (best, outcome) {
                    // Any acceptance wins: one endpoint taking it is enough for
                    // the transaction to reach the network.
                    (Some(Publish::Accepted { txid }), _) => Publish::Accepted { txid },
                    (_, Publish::Accepted { txid }) => Publish::Accepted { txid },
                    // Already-known beats a rejection: the network has it.
                    (Some(Publish::AlreadyKnown), _) | (_, Publish::AlreadyKnown) => {
                        Publish::AlreadyKnown
                    }
                    (_, other) => other,
                })
            })
            .unwrap_or(Publish::Rejected {
                reason: "no endpoints configured".into(),
            })
    }

    /// Publish a whole flushed batch, every transaction to every endpoint, all
    /// at once.
    ///
    /// Simultaneity is the property: the batch is the anonymity set only if its
    /// members hit the network together. Publishing them one after another would
    /// re-expose exactly the arrival ordering the shuffle just destroyed, so the
    /// full (transaction x endpoint) product is issued concurrently and the
    /// verdicts are positional, one per input transaction.
    pub async fn broadcast_batch(&self, txs: &[Vec<u8>]) -> Vec<Publish> {
        join_all(txs.iter().map(|tx| self.broadcast(tx))).await
    }

    /// One unary gRPC call: dial, optionally wrap in TLS, frame, send, unframe.
    async fn unary<Req, Resp>(
        &self,
        addr: SocketAddr,
        path: &str,
        message: Req,
    ) -> Result<Resp, BoxError>
    where
        Req: Message,
        Resp: Message + Default,
    {
        let stream = tokio::time::timeout(RPC_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| -> BoxError { format!("connect to {addr} timed out").into() })??;
        stream.set_nodelay(true)?;

        // The :authority is the VERIFIED NAME, not the dialled address. Any
        // ingress in front of the indexer routes on it, so an address here
        // matches no host rule and answers 404 over a healthy connection.
        let authority = match &self.tls {
            Some(tls) => tls.authority().to_owned(),
            None => addr.to_string(),
        };

        let request = hyper::Request::builder()
            .method("POST")
            .uri(format!("http://{authority}{path}"))
            .header(hyper::header::CONTENT_TYPE, "application/grpc")
            .header("te", "trailers")
            .body(Full::new(frame(&message)))?;

        let response = match &self.tls {
            Some(tls) => {
                let stream = tls.connect(addr, stream).await?;
                round_trip(stream, request).await?
            }
            None => round_trip(stream, request).await?,
        };

        unframe(&response)
    }
}

/// One HTTP/2 request/response over an already-connected stream, TLS or not.
///
/// Returns the response body only after checking `grpc-status`, so a call that
/// failed at the gRPC layer is an `Err` here rather than an empty success.
async fn round_trip<IO>(stream: IO, request: hyper::Request<Full<Bytes>>) -> Result<Bytes, BoxError>
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let response = tokio::time::timeout(RPC_TIMEOUT, sender.send_request(request))
        .await
        .map_err(|_| -> BoxError { "gRPC call timed out".into() })??;

    // A gRPC error can arrive in the HEADERS (a trailers-only response) or in
    // the trailers after the body. Capture the header form before consuming the
    // body, because collecting it discards the parts.
    let header_status = response
        .headers()
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_owned());
    let header_message = response
        .headers()
        .get("grpc-message")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_owned());

    let collected = tokio::time::timeout(RPC_TIMEOUT, response.into_body().collect())
        .await
        .map_err(|_| -> BoxError { "reading the gRPC response timed out".into() })??;
    let trailers = collected.trailers().cloned();
    let body = collected.to_bytes();

    let status = trailers
        .as_ref()
        .and_then(|map| map.get("grpc-status"))
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_owned())
        .or(header_status);
    let message = trailers
        .as_ref()
        .and_then(|map| map.get("grpc-message"))
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_owned())
        .or(header_message);

    match status.as_deref() {
        // Absent is tolerated: some servers omit it on a successful unary reply.
        None | Some("0") => Ok(body),
        Some(code) => Err(format!(
            "grpc-status {code}{}",
            message.map(|m| format!(": {m}")).unwrap_or_default()
        )
        .into()),
    }
}

/// Wrap a protobuf message in the gRPC length prefix.
fn frame<M: Message>(message: &M) -> Bytes {
    let encoded = message.encode_to_vec();
    let mut framed = BytesMut::with_capacity(GRPC_PREFIX_LEN + encoded.len());
    framed.extend_from_slice(&[0]); // not compressed
    framed.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
    framed.extend_from_slice(&encoded);
    framed.freeze()
}

/// Strip the gRPC length prefix and decode the message.
fn unframe<M: Message + Default>(body: &[u8]) -> Result<M, BoxError> {
    if body.len() < GRPC_PREFIX_LEN {
        return Err("gRPC response shorter than its frame header".into());
    }
    if body[0] != 0 {
        // The hub never advertises compression, so a compressed reply means the
        // peer ignored that. Refuse rather than guess at an encoding.
        return Err("gRPC response is compressed, which was not negotiated".into());
    }
    let declared = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    let message = GRPC_PREFIX_LEN
        .checked_add(declared)
        .and_then(|end| body.get(GRPC_PREFIX_LEN..end))
        .ok_or_else(|| -> BoxError { "gRPC frame length overruns the body".into() })?;
    Ok(M::decode(message)?)
}

/// Map a `SendResponse` onto [`Publish`].
///
/// lightwalletd's convention, which zaino follows: `error_code == 0` means
/// success and `error_message` carries the txid. A non-zero code carries the
/// node's rejection text, which is matched conservatively so anything
/// unrecognised counts as a rejection rather than a silent success.
fn classify_send_response(resp: &SendResponse) -> Publish {
    if resp.error_code == 0 {
        return Publish::Accepted {
            txid: resp.error_message.clone(),
        };
    }
    classify_publish_error(&resp.error_message)
}

/// Map a node's rejection message onto [`Publish`].
///
/// Matched on text because the error codes for these cases are not consistent
/// between zebrad and zcashd, nor through an indexer that relays them. Kept in
/// one place and deliberately conservative: anything unrecognised is a
/// rejection, so a message we have not seen before is retried rather than
/// silently dropping a migration.
fn classify_publish_error(message: &str) -> Publish {
    // Hyphens folded to spaces before matching. Bitcoin-derived nodes report
    // these as hyphenated reject reasons (`txn-already-known`) while the longer
    // prose forms use spaces, and matching only one shape silently misses the
    // other. That mistake is not cosmetic: an already-known transaction
    // classified as a rejection would be re-submitted forever, and the retries
    // would be a fresh timing signal tied to one transaction, which is exactly
    // what this component exists to avoid emitting.
    let m = message.to_ascii_lowercase().replace('-', " ");
    if m.contains("already in block chain")
        || m.contains("already known")
        || m.contains("already in mempool")
        || m.contains("duplicate")
    {
        Publish::AlreadyKnown
    } else {
        Publish::Rejected {
            reason: message.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_endpoint_list_is_refused_at_construction() {
        assert!(ChainClient::new(vec![], None).is_err());
    }

    #[test]
    fn a_zero_error_code_is_an_acceptance_carrying_the_txid() {
        // lightwalletd's convention: the txid rides in error_message on success.
        let resp = SendResponse {
            error_code: 0,
            error_message: "aabbcc".into(),
        };
        assert_eq!(
            classify_send_response(&resp),
            Publish::Accepted {
                txid: "aabbcc".into()
            }
        );
    }

    #[test]
    fn duplicate_submissions_are_success_not_failure() {
        // Every shim submits to every hub, so duplicates are normal operation.
        let resp = SendResponse {
            error_code: -25,
            error_message: "txn-already-known".into(),
        };
        assert_eq!(classify_send_response(&resp), Publish::AlreadyKnown);
        assert_eq!(
            classify_publish_error("transaction already in block chain"),
            Publish::AlreadyKnown
        );
        assert_eq!(
            classify_publish_error("txn-already-in-mempool"),
            Publish::AlreadyKnown
        );
    }

    #[test]
    fn unrecognised_errors_are_rejections_so_the_hub_retries() {
        match classify_publish_error("some node we have never seen says no") {
            Publish::Rejected { .. } => {}
            other => panic!("unrecognised errors must not be treated as success: {other:?}"),
        }
    }

    #[test]
    fn a_framed_message_round_trips() {
        let original = SendResponse {
            error_code: 0,
            error_message: "deadbeef".into(),
        };
        let framed = frame(&original);
        assert_eq!(framed[0], 0, "compression flag clear");
        let decoded: SendResponse = unframe(&framed).expect("round trip");
        assert_eq!(decoded, original);
    }

    #[test]
    fn a_truncated_or_compressed_frame_is_an_error_not_a_panic() {
        assert!(unframe::<SendResponse>(&[]).is_err());
        assert!(unframe::<SendResponse>(&[0, 0, 0, 0]).is_err());
        // Declares 99 bytes, carries none.
        assert!(unframe::<SendResponse>(&[0, 0, 0, 0, 99]).is_err());
        // Compression flag set.
        assert!(unframe::<SendResponse>(&[1, 0, 0, 0, 0]).is_err());
    }
}
