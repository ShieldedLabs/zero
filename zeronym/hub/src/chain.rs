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
use futures_util::stream::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use zaino_proto::proto::service::{Empty, LightdInfo, RawTransaction, SendResponse, TxFilter};

use crate::tls::IndexerTls;
use crate::BoxError;

/// Per-request ceiling. An endpoint that hangs must not stall a flush, because
/// the flush is on a block-cadence deadline: late is not merely slow here, it
/// can push a migration past its expiry height.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// How many of a flush's transactions may be in flight at once.
///
/// Each in-flight transaction dials every endpoint, so the real descriptor cost
/// is this times the endpoint count. Bounded because the queue's entry cap
/// (`queue::MAX_QUEUE_ENTRIES`) is an order of magnitude larger, and dialling
/// that many sockets simultaneously fails as fd exhaustion rather than as a
/// slow flush. See `broadcast_batch` for why bounding this does not weaken the
/// simultaneity the anonymity set depends on.
const MAX_PUBLISHES_IN_FLIGHT: usize = 64;

/// gRPC length-prefixed message header: 1 compression flag + 4 big-endian bytes.
const GRPC_PREFIX_LEN: usize = 5;

const SEND_TRANSACTION: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction";
const GET_LIGHTD_INFO: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo";
const GET_TRANSACTION: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTransaction";

/// gRPC NOT_FOUND. The one status a transaction lookup treats as "the indexer
/// answered, and this txid is not one it knows", distinct from the indexer being
/// unreachable. lightwalletd returns exactly this for an unknown txid.
const GRPC_NOT_FOUND: &str = "5";

/// The outcome of a transaction lookup against the indexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxLookup {
    Found { data: Vec<u8>, height: u64 },
    NotFound,
}

/// A non-zero gRPC status from an endpoint, carried as a typed error so a caller
/// can tell a real answer (NOT_FOUND on a lookup, INVALID_ARGUMENT on a publish)
/// from a transport failure. Its `Display` is byte-for-byte the string
/// `round_trip` used to format, so `tip_height`, which only stringifies errors,
/// is unchanged.
#[derive(Debug)]
pub(crate) struct GrpcStatusError {
    pub code: String,
    pub message: Option<String>,
}

impl std::fmt::Display for GrpcStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "grpc-status {}", self.code)?;
        if let Some(message) = &self.message {
            write!(f, ": {message}")?;
        }
        Ok(())
    }
}

impl std::error::Error for GrpcStatusError {}

/// The outcome of publishing one transaction.
///
/// `AlreadyKnown` is a success, and saying so explicitly is load-bearing. Every
/// shim submits every migration to every hub, so the second hub's publish is a
/// duplicate by construction. Treating that as an error would make normal
/// operation look like a fault and could drive a re-submission loop, and
/// re-submissions are a fresh timing signal tied to one transaction.
///
/// `Rejected` and `Retryable` are both failures, and the split between them is
/// what the batcher acts on. `Rejected` is a VERDICT: the indexer took the
/// request and said the transaction is not acceptable, so offering it again
/// would only buy the same answer. `Retryable` means nothing judged the
/// transaction at all: the connection was refused or reset, TLS or the gRPC
/// call timed out, the endpoint said it was unavailable or overloaded, or it
/// answered something this client could not read. Those are the failures a
/// later flush can recover from, and the batcher holds the entry for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Publish {
    Accepted { txid: String },
    AlreadyKnown,
    Rejected { reason: String },
    Retryable { reason: String },
}

/// gRPC INVALID_ARGUMENT and FAILED_PRECONDITION: the two statuses under which
/// a non-OK gRPC reply is still a verdict on the transaction rather than a
/// failure to reach one. Every other status is treated as transport (see
/// [`classify_publish_failure`]).
const GRPC_INVALID_ARGUMENT: &str = "3";
const GRPC_FAILED_PRECONDITION: &str = "9";

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
                    Err(err) => classify_publish_failure(&err),
                }
            }
        });

        best_of(join_all(calls).await)
    }

    /// Publish a whole flushed batch, every transaction to every endpoint,
    /// concurrently.
    ///
    /// Simultaneity is the property: the batch is the anonymity set only if its
    /// members hit the network together. Publishing them one after another would
    /// re-expose exactly the arrival ordering the shuffle just destroyed, so the
    /// (transaction x endpoint) product is issued concurrently and the verdicts
    /// are positional, one per input transaction.
    ///
    /// Concurrency is bounded, though, because "all at once" and "unbounded" are
    /// not the same requirement. Each in-flight publish dials its own connection,
    /// so an unbounded product exhausts file descriptors before it finishes, and
    /// a flush that dies of fd exhaustion publishes nothing at all. The bound is
    /// chosen so it does not weaken the property: what the anonymity set needs is
    /// that the batch lands in the same BLOCK, not the same microsecond, and
    /// `MAX_PUBLISHES_IN_FLIGHT` transactions clear in well under a second
    /// against a healthy indexer -- against a whole queue at its entry cap, still
    /// far inside one block interval, let alone the flush interval.
    ///
    /// `buffered`, not `buffer_unordered`: the verdicts are positional and the
    /// caller zips them against the batch by index.
    pub async fn broadcast_batch(&self, txs: &[Vec<u8>]) -> Vec<Publish> {
        // Collected into a `Vec` before streaming, not mapped lazily. A lazy
        // `iter().map(|tx| self.broadcast(tx))` makes the stream's item type
        // borrow `self` under a higher-ranked lifetime, and the resulting future
        // fails `tokio::spawn`'s `Send` bound ("implementation of `Send` is not
        // general enough") at the CALLER, several layers away. Collecting first
        // pins one concrete lifetime and keeps the error where it belongs.
        let calls: Vec<_> = txs.iter().map(|tx| self.broadcast(tx)).collect();
        futures_util::stream::iter(calls)
            .buffered(MAX_PUBLISHES_IN_FLIGHT)
            .collect::<Vec<_>>()
            .await
    }

    /// Fetch one transaction by the wallet's `TxFilter.hash` bytes.
    ///
    /// The bytes are passed through unmodified (the indexer reverses them to a
    /// display txid itself, exactly as it would for a wallet talking to it
    /// directly), so behaviour is identical to a direct query by construction.
    /// Queried against every endpoint concurrently, folded like `broadcast`: any
    /// `Found` wins, `NotFound` only if every endpoint that answered said so, and
    /// an error only if none answered. A single endpoint returning NOT_FOUND does
    /// not mask another that has the transaction.
    pub async fn get_transaction(&self, wire_hash: &[u8]) -> Result<TxLookup, BoxError> {
        let calls = self.endpoints.iter().map(|addr| {
            let filter = TxFilter {
                block: None,
                index: 0,
                hash: wire_hash.to_vec(),
            };
            async move {
                match self
                    .unary::<_, RawTransaction>(*addr, GET_TRANSACTION, filter)
                    .await
                {
                    Ok(raw) => Ok(TxLookup::Found {
                        data: raw.data,
                        height: raw.height,
                    }),
                    // NOT_FOUND is a real answer, not a fault; anything else is a
                    // transport or protocol failure the caller must surface as 502.
                    Err(err)
                        if err
                            .downcast_ref::<GrpcStatusError>()
                            .is_some_and(|status| status.code == GRPC_NOT_FOUND) =>
                    {
                        Ok(TxLookup::NotFound)
                    }
                    Err(err) => Err(err),
                }
            }
        });

        let mut saw_not_found = false;
        let mut last_error: Option<BoxError> = None;
        for outcome in join_all(calls).await {
            match outcome {
                Ok(found @ TxLookup::Found { .. }) => return Ok(found),
                Ok(TxLookup::NotFound) => saw_not_found = true,
                Err(err) => last_error = Some(err),
            }
        }

        if saw_not_found {
            Ok(TxLookup::NotFound)
        } else {
            Err(last_error.unwrap_or_else(|| "no indexer answered the lookup".into()))
        }
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
        // ONE deadline over the whole call -- connect, TLS handshake, h2
        // handshake, request, and body -- not a timer on the connect alone.
        // Before this only TcpStream::connect was inside RPC_TIMEOUT; the TLS
        // handshake (a bare tokio-rustls await with no timeout of its own) and
        // the h2 handshake were not, so an endpoint that ACKed the TCP connect
        // and then went silent before ServerHello hung this future forever. And
        // this future is awaited INLINE by the flush cadence and by tip_height,
        // so one such endpoint stopped every future flush; fifteen minutes later
        // is_stale() refused every submission as TipStale. Worse after the
        // requeue-on-transport-failure change: a hung flush never returns, so
        // nothing requeues either. The whole call now fails closed inside the
        // budget, which is what "per-call budget" was always meant to mean.
        tokio::time::timeout(RPC_TIMEOUT, self.unary_inner(addr, path, message))
            .await
            .map_err(|_| -> BoxError {
                format!("{path} to {addr} exceeded the {RPC_TIMEOUT:?} call budget").into()
            })?
    }

    /// The body of [`Self::unary`], without the deadline; kept separate so the
    /// deadline wraps EVERY await below, including the handshakes.
    async fn unary_inner<Req, Resp>(
        &self,
        addr: SocketAddr,
        path: &str,
        message: Req,
    ) -> Result<Resp, BoxError>
    where
        Req: Message,
        Resp: Message + Default,
    {
        let stream = TcpStream::connect(addr).await?;
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
        // Typed so a lookup can distinguish NOT_FOUND from a transport fault. The
        // Display of this error is the same string this arm used to build.
        Some(code) => Err(Box::new(GrpcStatusError {
            code: code.to_owned(),
            message,
        })),
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

/// Fold one verdict per endpoint into the verdict for the transaction.
///
/// Any acceptance wins: one endpoint taking it is enough for the transaction to
/// reach the network. Already-known beats everything else: the network has it.
/// A rejection beats a transport failure, and that ordering is deliberate
/// rather than a tie-break. The batcher holds a `Retryable` entry for the next
/// flush, so if an unreachable endpoint could outvote a live one's verdict, a
/// single dead endpoint in the list would keep every doomed transaction resident
/// until it expired, and an unparseable payload never expires (queue.rs, REVIEW
/// #5): junk plus one dead endpoint would fill the byte budget for everyone. A
/// verdict from any endpoint that answered is final, exactly as it is today
/// with one endpoint.
fn best_of(outcomes: Vec<Publish>) -> Publish {
    fn rank(outcome: &Publish) -> u8 {
        match outcome {
            Publish::Accepted { .. } => 3,
            Publish::AlreadyKnown => 2,
            Publish::Rejected { .. } => 1,
            Publish::Retryable { .. } => 0,
        }
    }
    outcomes
        .into_iter()
        .max_by_key(rank)
        .unwrap_or(Publish::Rejected {
            reason: "no endpoints configured".into(),
        })
}

/// Map a failed `SendTransaction` call (no `SendResponse` came back) onto
/// [`Publish`].
///
/// This is the seam between "the indexer judged the transaction" and "the
/// indexer was never really asked", and the batcher's requeue depends on it
/// being drawn honestly. Only INVALID_ARGUMENT and FAILED_PRECONDITION are
/// verdicts here: they are what a gRPC service returns when it read the request
/// and refuses its content. Everything else, a refused or reset connection, a
/// TLS failure, any of the three timeouts, UNAVAILABLE, DEADLINE_EXCEEDED,
/// RESOURCE_EXHAUSTED, an unframeable reply, is a failure to obtain a verdict,
/// and is classed retryable on purpose even where it might be permanent (an
/// UNIMPLEMENTED or UNAUTHENTICATED endpoint): re-offering an entry costs one
/// call per flush and stops at its expiry, while dropping a valid migration on
/// a misread error is unrecoverable, because the shim has already told the
/// wallet it was sent.
fn classify_publish_failure(err: &BoxError) -> Publish {
    let reason = err.to_string();
    match err.downcast_ref::<GrpcStatusError>() {
        Some(status)
            if status.code == GRPC_INVALID_ARGUMENT || status.code == GRPC_FAILED_PRECONDITION =>
        {
            Publish::Rejected { reason }
        }
        _ => Publish::Retryable { reason },
    }
}

/// Map a node's rejection message onto [`Publish`].
///
/// Matched on text because the error codes for these cases are not consistent
/// between zebrad and zcashd, nor through an indexer that relays them. Kept in
/// one place and deliberately conservative: anything unrecognised is a
/// rejection, never a silent success. Note that a rejection here is a VERDICT
/// and is dropped by the batcher, not retried: the indexer answered OK with a
/// non-zero code, which is exactly how lightwalletd and zaino relay the node
/// saying no, and offering the same bytes again would buy the same answer every
/// flush until expiry (or forever, for an unparseable payload).
/// Node rejections that CANNOT become acceptances by waiting, so re-offering the
/// same bytes buys the same answer at every flush.
///
/// Matched against the message lowercased with hyphens folded to spaces, so both
/// the hyphenated reject reasons and the prose forms hit.
///
/// Deliberately short, and every entry has to be a property of the SIGNED BYTES
/// themselves. Anything time-dependent is excluded on purpose and retries: a
/// missing parent may arrive, a full mempool may drain, a syncing node may catch
/// up. Adding a time-dependent pattern here converts a recoverable failure back
/// into a lost migration, which is the mistake this list exists to prevent.
const PERMANENT_REJECTIONS: &[&str] = &[
    // The consensus-failure family: signatures, bindings, spent inputs, values.
    "bad txns",
    // Size and fee are fixed by the bytes; waiting changes neither.
    "tx size",
    "oversize",
    "insufficient fee",
    "min relay fee",
    "absurdly high fee",
    "fee out of range",
    "dust",
    // Past its expiry height, and it can only get further past it.
    "expired",
    // The inputs are gone. A retry re-offers bytes that can never be valid.
    "conflict",
    "already spent",
];

fn classify_publish_error(message: &str) -> Publish {
    // Hyphens folded to spaces before matching. Bitcoin-derived nodes report
    // these as hyphenated reject reasons (`txn-already-known`) while the longer
    // prose forms use spaces, and matching only one shape silently misses the
    // other. That mistake is not cosmetic: an already-known transaction
    // classified as a rejection would be re-submitted forever, and the retries
    // would be a fresh timing signal tied to one transaction, which is exactly
    // what this component exists to avoid emitting.
    let m = message.to_ascii_lowercase().replace('-', " ");

    // A NAMED CONSENSUS FAILURE IS CHECKED FIRST, and the order is load-bearing.
    // `bad-txns-inputs-duplicate` is a consensus rejection that happens to
    // contain the substring "duplicate", so with already-known matched first it
    // was classified `AlreadyKnown` -- which the batcher counts as ACHIEVED and
    // does not hold. A transaction the network refused was being recorded as
    // successfully published, and the wallet had already been told it was sent.
    // No already-known message contains any of these patterns, so this ordering
    // costs nothing and closes that.
    if PERMANENT_REJECTIONS
        .iter()
        .any(|pattern| m.contains(pattern))
    {
        Publish::Rejected {
            reason: message.to_string(),
        }
    } else if m.contains("already in block chain")
        || m.contains("already known")
        || m.contains("already in mempool")
        || m.contains("duplicate")
    {
        Publish::AlreadyKnown
    } else {
        // UNRECOGNISED IS RETRYABLE, not rejected.
        //
        // This default used to be the other way round, which put this function
        // in direct contradiction with `classify_publish_failure` above it: the
        // same trade, resolved oppositely, in two adjacent functions.
        //
        // The costs are not symmetric. A transient error misread as a rejection
        // drops the migration for good -- the shim has already told the wallet
        // it was sent and keeps no copy, so nothing anywhere retries it. A
        // permanent error misread as retryable costs one indexer call per flush
        // and stops at the entry's expiry.
        //
        // It also makes the verdict independent of WHICH INDEXER answered, which
        // it never was: lightwalletd relays zebra's transient errors as
        // `SendResponse` with a non-zero code, and they arrived here and were
        // dropped as rejections; zaino maps everything to gRPC INTERNAL, which
        // never reaches this function and was retried instead. The same node
        // failure had opposite outcomes decided by deployment. Biasing toward
        // retry, and bounding the retry at the queue, removes the need to tell
        // the two apart at all.
        Publish::Retryable {
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
    fn an_unrecognised_node_error_is_retried_not_dropped() {
        // INVERTED DELIBERATELY. This used to require a rejection, on the
        // reasoning that the node answered and said no, so the entry should not
        // be held. The reasoning is right and the conclusion was still wrong,
        // because it ignored which way the mistake costs.
        //
        // We cannot tell, from words we have never seen, whether the node is
        // refusing these bytes forever or reporting something momentary. Guess
        // "permanent" and a recoverable migration is dropped for good: the shim
        // already told the wallet it was sent and keeps no copy, so nothing
        // anywhere retries it. Guess "transient" and we spend one indexer call
        // per flush until the entry expires.
        //
        // Unrecoverable versus bounded and cheap. So the default is to retry,
        // and `PERMANENT_REJECTIONS` carries the cases we can actually name.
        match classify_publish_error("some node we have never seen says no") {
            Publish::Retryable { .. } => {}
            other => panic!("an unrecognised node error must be retried: {other:?}"),
        }
    }

    #[test]
    fn a_named_consensus_failure_is_still_a_rejection() {
        // The other half of the same decision: biasing toward retry must not
        // turn into retrying everything forever. Anything we can positively
        // identify as a property of the signed bytes is still a verdict, and
        // the batcher drops it.
        for permanent in [
            "16: bad-txns-sapling-binding-signature-invalid",
            "bad-txns-inputs-duplicate",
            "tx-size",
            "insufficient fee, rejecting replacement",
            "absurdly-high-fee",
            "dust",
            "tx expired",
            "txn-mempool-conflict",
        ] {
            match classify_publish_error(permanent) {
                Publish::Rejected { .. } => {}
                other => panic!("{permanent:?} must be a rejection, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_time_dependent_failure_is_not_treated_as_permanent() {
        // The list must stay free of anything that waiting could fix. These are
        // the ones most likely to be added to it by mistake: each looks like a
        // hard failure and each can resolve on its own.
        for transient in [
            "mempool full",
            "node is still syncing",
            "missing inputs",
            "too many unconfirmed ancestors",
        ] {
            match classify_publish_error(transient) {
                Publish::Retryable { .. } => {}
                other => panic!("{transient:?} must be retried, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_failure_to_reach_a_verdict_is_retryable() {
        // The shapes `unary` produces when nothing judged the transaction: the
        // three timeouts and a raw I/O error carry no gRPC status at all, and
        // UNAVAILABLE / DEADLINE_EXCEEDED are the transport-flavoured statuses.
        let plain: BoxError = "connect to 127.0.0.1:9 timed out".into();
        assert!(matches!(
            classify_publish_failure(&plain),
            Publish::Retryable { .. }
        ));
        for code in ["14", "4", "8", "2", "13"] {
            let status: BoxError = Box::new(GrpcStatusError {
                code: code.into(),
                message: None,
            });
            assert!(
                matches!(classify_publish_failure(&status), Publish::Retryable { .. }),
                "grpc-status {code} must be retryable"
            );
        }
    }

    #[test]
    fn invalid_argument_and_failed_precondition_are_verdicts() {
        for code in [GRPC_INVALID_ARGUMENT, GRPC_FAILED_PRECONDITION] {
            let status: BoxError = Box::new(GrpcStatusError {
                code: code.into(),
                message: Some("bad anchor".into()),
            });
            assert!(
                matches!(classify_publish_failure(&status), Publish::Rejected { .. }),
                "grpc-status {code} is the indexer refusing the transaction"
            );
        }
    }

    #[test]
    fn a_verdict_from_one_endpoint_beats_a_transport_failure_from_another() {
        // Otherwise one dead endpoint would keep every doomed transaction
        // resident until expiry, and unparseable ones forever.
        let outcomes = vec![
            Publish::Retryable {
                reason: "connection refused".into(),
            },
            Publish::Rejected {
                reason: "bad-txns".into(),
            },
        ];
        assert!(matches!(best_of(outcomes), Publish::Rejected { .. }));

        // And any success beats both.
        let outcomes = vec![
            Publish::Retryable {
                reason: "connection refused".into(),
            },
            Publish::AlreadyKnown,
            Publish::Rejected {
                reason: "bad-txns".into(),
            },
        ];
        assert_eq!(best_of(outcomes), Publish::AlreadyKnown);

        // Only transport failures means only transport failures.
        let outcomes = vec![
            Publish::Retryable {
                reason: "connection refused".into(),
            },
            Publish::Retryable {
                reason: "gRPC call timed out".into(),
            },
        ];
        assert!(matches!(best_of(outcomes), Publish::Retryable { .. }));
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
