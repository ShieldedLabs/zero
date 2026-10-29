//! The hub's connection to the Zcash network, through an indexer.
//!
//! `GetLightdInfo` gives the height that drives flush scheduling and expiry
//! admission, `SendTransaction` publishes each member of a flushed batch,
//! `GetTransaction` answers a lookup that missed the queue, and
//! `GetBlockRangeNullifiers` feeds the set of Orchard nullifiers the chain has
//! revealed ([`crate::spent`]). The hub deliberately does NOT run a validator
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
use http_body_util::{BodyExt, Full, Limited};
use hyper::client::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo};
use percent_encoding::percent_decode_str;
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use zaino_proto::proto::compact_formats::CompactBlock;
use zaino_proto::proto::service::{
    BlockId, BlockRange, Empty, LightdInfo, PoolType, RawTransaction, SendResponse, TxFilter,
};

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
const GET_BLOCK_RANGE_NULLIFIERS: &str =
    "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRangeNullifiers";

/// Orchard actions one block can hold: a 2 MB block over the ~820 bytes of one
/// action is about 2 500. A reply past this is malformed or hostile, and is
/// refused before it can grow the nullifier set.
const MAX_ORCHARD_ACTIONS_PER_BLOCK: usize = 4096;

/// Ceiling on an indexer response body. The three unary calls this client makes
/// are `GetLightdInfo`, a `SendResponse`, and a `RawTransaction` -- a mined
/// transaction is at most ~2 MB and the other two are tiny, so nothing
/// legitimate approaches this. Without it a ~100-byte unauthenticated lookup
/// buys an unbounded allocation in a fixed-size enclave, because the response
/// was collected with no limit at all. Same value and same reasoning as the
/// shim's `MAX_HUB_RESPONSE_BYTES`, which bounds the mirror-image hop.
const MAX_INDEXER_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

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
/// `round_trip` used to format, so `tip_heights`, which only stringifies errors,
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

/// gRPC INTERNAL, which zaino answers every node rejection with.
const GRPC_INTERNAL: &str = "13";

/// The code zcashd and zebrad answer `sendrawtransaction` with when the bytes do
/// not deserialize (`RPC_DESERIALIZATION_ERROR`). lightwalletd relays it as the
/// `SendResponse` code.
const NODE_DESERIALIZATION_ERROR: i32 = -22;

/// What zaino puts in a gRPC INTERNAL message when its node answered
/// [`NODE_DESERIALIZATION_ERROR`]: the `Display` of its `Malformed` rejection
/// (`zaino-source/src/send_raw_transaction.rs`).
const ZAINO_MALFORMED: &str = "malformed transaction:";

/// A client over one or more indexer endpoints.
pub struct ChainClient {
    endpoints: Vec<SocketAddr>,
    /// `None` means plaintext h2c, which is correct only for a test or a
    /// trusted local path. A deployed enclave always sets this.
    tls: Option<Arc<IndexerTls>>,
}

impl ChainClient {
    /// How many indexers a batch is published through.
    ///
    /// Read by the flush to decide whether a mixed outcome is explicable. With
    /// several endpoints a partial failure is ordinary; with ONE it is not.
    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

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

    /// Every endpoint's best-chain height, one per endpoint that answered. An
    /// error only when none did.
    ///
    /// All of them, for the tracker to take the MAX of those it trusts
    /// (`TipTracker::observe`). The MAX means an adversary must slow down every
    /// endpoint to slow the flush clock. It favours whoever reports HIGHEST,
    /// though, so the tracker judges each answer before the MAX, and an honest
    /// answer survives beside a dropped one.
    pub async fn tip_heights(&self) -> Result<Vec<u32>, BoxError> {
        let queries = self.endpoints.iter().map(|addr| async move {
            let info: LightdInfo = self.unary(*addr, GET_LIGHTD_INFO, Empty {}).await?;
            // Checked, not `as`: a wrapped u64 is still a height, just a wrong one.
            u32::try_from(info.block_height).map_err(|_| -> BoxError {
                format!("indexer reported height {} beyond u32", info.block_height).into()
            })
        });

        let heights: Vec<u32> = join_all(queries)
            .await
            .into_iter()
            .filter_map(|result| match result {
                Ok(height) => Some(height),
                Err(err) => {
                    tracing::debug!(%err, "tip query failed");
                    None
                }
            })
            .collect();
        if heights.is_empty() {
            return Err("no indexer answered a tip query".into());
        }
        Ok(heights)
    }

    /// The configured endpoints, in order, for a caller that reads each one on
    /// its own ([`crate::spent`]).
    pub fn endpoints(&self) -> &[SocketAddr] {
        &self.endpoints
    }

    /// One endpoint's best-chain height.
    pub async fn tip_height_of(&self, addr: SocketAddr) -> Result<u32, BoxError> {
        let info: LightdInfo = self.unary(addr, GET_LIGHTD_INFO, Empty {}).await?;
        u32::try_from(info.block_height).map_err(|_| -> BoxError {
            format!("indexer reported height {} beyond u32", info.block_height).into()
        })
    }

    /// The Orchard nullifiers of blocks `start..=end` as one endpoint reports
    /// them, one entry per block it returned, in height order.
    ///
    /// Orchard only: a transaction admitted to the proven lane carries an
    /// Orchard bundle, so its Orchard nullifiers alone say whether the chain
    /// already has it. The request filters to the Orchard pool; a server that
    /// ignores the filter still returns the Orchard actions, and the rest is
    /// skipped.
    pub async fn orchard_nullifiers(
        &self,
        addr: SocketAddr,
        start: u32,
        end: u32,
    ) -> Result<Vec<(u32, Vec<[u8; 32]>)>, BoxError> {
        let range = BlockRange {
            start: Some(BlockId {
                height: u64::from(start),
                hash: Vec::new(),
            }),
            end: Some(BlockId {
                height: u64::from(end),
                hash: Vec::new(),
            }),
            pool_types: vec![PoolType::Orchard as i32],
        };
        let body = self.call(addr, GET_BLOCK_RANGE_NULLIFIERS, range).await?;
        orchard_nullifiers_of(&unframe_all::<CompactBlock>(&body)?, start, end)
    }

    /// Publish one raw transaction to every configured endpoint.
    pub async fn broadcast(&self, tx_bytes: &[u8]) -> Publish {
        // Concurrent, not sequential. A flush publishes k transactions across n
        // endpoints with a 10 s per-call ceiling; done in sequence, one hung
        // endpoint pushes the total past a block interval, which would
        // reintroduce the very ordering the shuffle exists to remove.
        let calls = self.endpoints.iter().map(|addr| {
            let raw = RawTransaction {
                data: tx_bytes.to_vec().into(),
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
    /// not mask another that has the transaction. An OK answer with no body, at
    /// any height, counts as that endpoint failing, not as `Found`.
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
                    // Found with no bytes is the hub's own queue-hit reply, and
                    // only the queue may say it. An indexer that answers OK with
                    // nothing in it has not answered.
                    Ok(raw) if raw.data.is_empty() => {
                        Err("an indexer answered a lookup with an empty transaction".into())
                    }
                    Ok(raw) => Ok(TxLookup::Found {
                        // `RawTransaction.data` is `Bytes` since zaino-proto 0.4.0;
                        // copy it out because `TxLookup` feeds `Zeroizing`, which
                        // needs a buffer it owns exclusively.
                        data: raw.data.to_vec(),
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
        unframe(&self.call(addr, path, message).await?)
    }

    /// One gRPC call, unary or server-streaming, returning the response body
    /// with its frames still on.
    async fn call<Req: Message>(
        &self,
        addr: SocketAddr,
        path: &str,
        message: Req,
    ) -> Result<Bytes, BoxError> {
        // ONE deadline over the whole call -- connect, TLS handshake, h2
        // handshake, request, and body -- not a timer on the connect alone.
        // Before this only TcpStream::connect was inside RPC_TIMEOUT; the TLS
        // handshake (a bare tokio-rustls await with no timeout of its own) and
        // the h2 handshake were not, so an endpoint that ACKed the TCP connect
        // and then went silent before ServerHello hung this future forever. And
        // this future is awaited INLINE by the flush cadence and by tip_heights,
        // so one such endpoint stopped every future flush; fifteen minutes later
        // is_stale() refused every submission as TipStale. Worse after the
        // requeue-on-transport-failure change: a hung flush never returns, so
        // nothing requeues either. The whole call now fails closed inside the
        // budget, which is what "per-call budget" was always meant to mean.
        tokio::time::timeout(RPC_TIMEOUT, self.call_inner(addr, path, message))
            .await
            .map_err(|_| -> BoxError {
                format!("{path} to {addr} exceeded the {RPC_TIMEOUT:?} call budget").into()
            })?
    }

    /// The body of [`Self::call`], without the deadline; kept separate so the
    /// deadline wraps EVERY await below, including the handshakes.
    async fn call_inner<Req: Message>(
        &self,
        addr: SocketAddr,
        path: &str,
        message: Req,
    ) -> Result<Bytes, BoxError> {
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

        match &self.tls {
            Some(tls) => {
                let stream = tls.connect(addr, stream).await?;
                round_trip(stream, request).await
            }
            None => round_trip(stream, request).await,
        }
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

    let collected = tokio::time::timeout(
        RPC_TIMEOUT,
        Limited::new(response.into_body(), MAX_INDEXER_RESPONSE_BYTES).collect(),
    )
    .await
    .map_err(|_| -> BoxError { "reading the gRPC response timed out".into() })?
    .map_err(|_| -> BoxError { "indexer response exceeded the size limit".into() })?;
    let trailers = collected.trailers().cloned();
    let body = collected.to_bytes();

    let status = trailers
        .as_ref()
        .and_then(|map| map.get("grpc-status"))
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_owned())
        .or(header_status);
    // Percent-encoded on the wire, so a space arrives as `%20`. Decoded here so
    // the publish classifier reads the indexer's words.
    let message = trailers
        .as_ref()
        .and_then(|map| map.get("grpc-message"))
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_owned())
        .or(header_message)
        .map(|value| percent_decode_str(&value).decode_utf8_lossy().into_owned());

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

/// Every message of a server-streaming response body, in order.
fn unframe_all<M: Message + Default>(mut body: &[u8]) -> Result<Vec<M>, BoxError> {
    let mut messages = Vec::new();
    while !body.is_empty() {
        if body.len() < GRPC_PREFIX_LEN {
            return Err("gRPC response ends inside a frame header".into());
        }
        let declared = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
        let end = GRPC_PREFIX_LEN
            .checked_add(declared)
            .filter(|end| *end <= body.len())
            .ok_or_else(|| -> BoxError { "gRPC frame length overruns the body".into() })?;
        messages.push(unframe(&body[..end])?);
        body = &body[end..];
    }
    Ok(messages)
}

/// The Orchard nullifiers of `blocks`, checked against the range they answer.
///
/// Refused whole on any block outside `start..=end`, out of height order, with
/// a nullifier that is not 32 bytes, or with more actions than a block can hold:
/// every one of those is an indexer that cannot be read, and recording part of
/// its answer would be guessing.
fn orchard_nullifiers_of(
    blocks: &[CompactBlock],
    start: u32,
    end: u32,
) -> Result<Vec<(u32, Vec<[u8; 32]>)>, BoxError> {
    let mut out: Vec<(u32, Vec<[u8; 32]>)> = Vec::with_capacity(blocks.len());
    for block in blocks {
        let height = u32::try_from(block.height)
            .ok()
            .filter(|height| (start..=end).contains(height))
            .ok_or_else(|| -> BoxError {
                format!(
                    "indexer returned block {} for {start}..={end}",
                    block.height
                )
                .into()
            })?;
        if out.last().is_some_and(|(last, _)| *last >= height) {
            return Err("indexer returned blocks out of height order".into());
        }
        let actions = block.vtx.iter().flat_map(|tx| &tx.actions);
        if actions.clone().count() > MAX_ORCHARD_ACTIONS_PER_BLOCK {
            return Err(format!("indexer returned too many Orchard actions at {height}").into());
        }
        let nullifiers = actions
            .map(|action| {
                <[u8; 32]>::try_from(action.nullifier.as_slice())
                    .map_err(|_| -> BoxError { "an Orchard nullifier is not 32 bytes".into() })
            })
            .collect::<Result<Vec<_>, _>>()?;
        out.push((height, nullifiers));
    }
    Ok(out)
}

/// Map a `SendResponse` onto [`Publish`].
///
/// lightwalletd's convention, which zaino follows: `error_code == 0` means
/// success and `error_message` carries the txid. A non-zero code carries the
/// node's rejection, read by [`classify_publish_error`].
///
/// A node that could not deserialize the bytes has judged them: the bytes stay
/// unreadable at every later flush. Retrying that answer let one burst of junk,
/// which has no expiry, hold the queue's entry cap for `MAX_REQUEUE_ATTEMPTS`
/// more flushes and reach every indexer that many more times. A node too old to
/// read a new transaction version answers the same, and a few more flushes do
/// not upgrade it.
fn classify_send_response(resp: &SendResponse) -> Publish {
    if resp.error_code == 0 {
        return Publish::Accepted {
            txid: resp.error_message.clone(),
        };
    }
    if resp.error_code == NODE_DESERIALIZATION_ERROR {
        return Publish::Rejected {
            reason: resp.error_message.clone(),
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
/// being drawn honestly. INVALID_ARGUMENT and FAILED_PRECONDITION are verdicts
/// here: they are what a gRPC service returns when it read the request and
/// refuses its content. So is an INTERNAL carrying zaino's `Malformed`
/// rejection, its relay of the node's deserialization failure (see
/// [`classify_send_response`]). Everything else, a refused or reset connection,
/// a TLS failure, any of the three timeouts, UNAVAILABLE, DEADLINE_EXCEEDED,
/// RESOURCE_EXHAUSTED, an unframeable reply, is a failure to obtain a verdict,
/// and is classed retryable on purpose even where it might be permanent (an
/// UNIMPLEMENTED or UNAUTHENTICATED endpoint): re-offering an entry costs one
/// call per flush until its expiry or `MAX_REQUEUE_ATTEMPTS`, while dropping a
/// valid migration on a misread error is unrecoverable, because the shim has
/// already told the wallet it was sent.
fn classify_publish_failure(err: &BoxError) -> Publish {
    let reason = err.to_string();
    match err.downcast_ref::<GrpcStatusError>() {
        Some(status)
            if status.code == GRPC_INVALID_ARGUMENT || status.code == GRPC_FAILED_PRECONDITION =>
        {
            Publish::Rejected { reason }
        }
        Some(status)
            if status.code == GRPC_INTERNAL
                && status
                    .message
                    .as_deref()
                    .is_some_and(|message| message.contains(ZAINO_MALFORMED)) =>
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
/// one place; anything unrecognised is retryable, never a silent success, and
/// never a rejection. Note that a rejection here is a VERDICT
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
        // ZEBRA'S VOCABULARY, which shares none of zcashd's spellings. Note in
        // particular that "already exists in mempool" does NOT contain the
        // substring "already in mempool" -- the two look alike and match
        // differently, which is how this was missed.
        //
        // These had to land with the Retryable default, not after it. The
        // review is explicit: "Applying (1) without (2) converts silent
        // destruction into permanent republication." We shipped (1) in 8fb91b9
        // and left this, so behind a zebra-backed indexer an already-published
        // transaction fell through to Retryable and was re-broadcast on every
        // flush -- bounded to MAX_REQUEUE_ATTEMPTS by d700e90, then recorded as
        // a migration that "exists nowhere else", which it does. Two costs: the
        // repeated per-transaction timing signal this branch exists to prevent,
        // and an error-level report of loss that never happened.
        || m.contains("already exists in mempool")
        || m.contains("already queued for download")
        || m.contains("committed to the best chain")
        || m.contains("already in state")
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
        // for up to `MAX_REQUEUE_ATTEMPTS` flushes. Expiry rarely ends it first:
        // a ZIP 318 migration expires a month or more out.
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
    fn zebras_already_known_vocabulary_is_recognised_too() {
        // The two vocabularies share NO spellings, and one pair looks alike
        // while matching differently: "already exists in mempool" does not
        // contain "already in mempool". That near-miss is how a zebra-backed
        // hub fell through to Retryable.
        //
        // This had to land with the Retryable default rather than after it.
        // With the default inverted and these missing, an already-published
        // transaction was re-broadcast on every flush until the attempt cap and
        // then reported as lost -- the review's "converts silent destruction
        // into permanent republication", bounded but not avoided.
        for message in [
            "transaction already exists in mempool",
            "already queued for download",
            "transaction committed to the best chain",
            "already in state",
        ] {
            assert_eq!(
                classify_publish_error(message),
                Publish::AlreadyKnown,
                "zebra reports success as {message:?}; treating it as anything else                  republishes a transaction that is already published"
            );
        }
    }

    #[test]
    fn a_consensus_rejection_still_wins_over_the_already_known_patterns() {
        // Rejections are matched FIRST, deliberately: `bad-txns-inputs-duplicate`
        // contains "duplicate" and was once counted as ACHIEVED. Adding four more
        // already-known patterns must not reopen that.
        match classify_publish_error("bad-txns-inputs-duplicate") {
            Publish::Rejected { .. } => {}
            other => panic!("a consensus rejection must not read as already-known: {other:?}"),
        }
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
    fn a_child_published_before_its_parent_is_retried() {
        // The shim diverts the second leg of a TEX payment, whose only input is
        // the first leg's output. If the second leg reaches an earlier flush
        // than the first, zebra answers with its missing-input error, and the
        // entry must be requeued to go out with its parent next flush. Dropping
        // it loses a payment the wallet was told was sent.
        //
        // zebra's wording, from `zebra-consensus/src/error.rs`
        // (`TransparentInputNotFound`), bare and as the mempool's
        // `TransactionDownloadVerifyError::Invalid` wraps it. lightwalletd
        // relays either as a `SendResponse` with a non-zero code.
        for message in [
            "could not find transparent input UTXO in the best chain or mempool",
            "transaction did not pass consensus validation: could not find transparent input \
             UTXO in the best chain or mempool",
        ] {
            match classify_send_response(&SendResponse {
                error_code: -25,
                error_message: message.into(),
            }) {
                Publish::Retryable { .. } => {}
                other => panic!("{message:?} must be retried, got {other:?}"),
            }
        }

        // zaino relays the same node error as gRPC INTERNAL.
        let status: BoxError = Box::new(GrpcStatusError {
            code: "13".into(),
            message: Some(
                "RPC error: could not find transparent input UTXO in the best chain or mempool"
                    .into(),
            ),
        });
        assert!(matches!(
            classify_publish_failure(&status),
            Publish::Retryable { .. }
        ));
    }

    #[test]
    fn a_node_that_cannot_parse_the_bytes_has_rejected_them() {
        // zebrad and zcashd, each through lightwalletd, which relays the node's
        // own code.
        for message in [
            "parse error: bad tx header",
            "io error: failed to fill whole buffer",
            "TX decode failed",
        ] {
            match classify_send_response(&SendResponse {
                error_code: NODE_DESERIALIZATION_ERROR,
                error_message: message.into(),
            }) {
                Publish::Rejected { .. } => {}
                other => panic!("{message:?} must be a rejection, got {other:?}"),
            }
        }

        // zaino names the same answer as its own `Malformed` rejection, inside
        // an INTERNAL.
        let status: BoxError = Box::new(GrpcStatusError {
            code: GRPC_INTERNAL.into(),
            message: Some(
                "InternalServerError: error receiving data from backing node: critical error \
                 in backing block source: validator rejected the query: malformed transaction: \
                 parse error: bad tx header"
                    .into(),
            ),
        });
        assert!(matches!(
            classify_publish_failure(&status),
            Publish::Rejected { .. }
        ));
    }

    #[test]
    fn zainos_other_internal_answers_stay_retryable() {
        for message in [
            // Its own transport failure.
            "IO error: connection reset by peer",
            "InternalServerError: error receiving data from backing node: critical error in \
             backing block source: validator unreachable: connection refused",
            // A node verdict the hub does not read as permanent.
            "InternalServerError: error receiving data from backing node: critical error in \
             backing block source: validator rejected the query: rejected by validator: \
             transaction did not pass consensus validation",
        ] {
            let status: BoxError = Box::new(GrpcStatusError {
                code: GRPC_INTERNAL.into(),
                message: Some(message.into()),
            });
            assert!(
                matches!(classify_publish_failure(&status), Publish::Retryable { .. }),
                "{message:?} must be retried"
            );
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

    fn block(height: u64, orchard: &[[u8; 32]], ironwood: &[[u8; 32]]) -> CompactBlock {
        use zaino_proto::proto::compact_formats::{CompactOrchardAction, CompactTx};
        let actions = |nullifiers: &[[u8; 32]]| {
            nullifiers
                .iter()
                .map(|nf| CompactOrchardAction {
                    nullifier: nf.to_vec(),
                    ..Default::default()
                })
                .collect()
        };
        CompactBlock {
            height,
            vtx: vec![CompactTx {
                actions: actions(orchard),
                ironwood_actions: actions(ironwood),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_streamed_reply_unframes_every_message_in_order() {
        let mut body = Vec::new();
        for height in [7, 8, 9] {
            body.extend_from_slice(&frame(&block(height, &[], &[])));
        }
        let blocks = unframe_all::<CompactBlock>(&body).unwrap();
        assert_eq!(
            blocks.iter().map(|b| b.height).collect::<Vec<_>>(),
            [7, 8, 9]
        );
        assert!(unframe_all::<CompactBlock>(&[]).unwrap().is_empty());
        body.extend_from_slice(&[0, 0, 0]);
        assert!(
            unframe_all::<CompactBlock>(&body).is_err(),
            "a cut-off frame"
        );
    }

    #[test]
    fn only_orchard_nullifiers_are_read() {
        let blocks = [
            block(10, &[[1; 32]], &[[2; 32]]),
            block(11, &[], &[[3; 32]]),
        ];
        assert_eq!(
            orchard_nullifiers_of(&blocks, 10, 11).unwrap(),
            vec![(10, vec![[1; 32]]), (11, vec![])]
        );
    }

    #[test]
    fn a_reply_that_does_not_answer_the_range_is_refused_whole() {
        let outside = [block(9, &[[1; 32]], &[])];
        assert!(orchard_nullifiers_of(&outside, 10, 11).is_err());
        let backwards = [block(11, &[], &[]), block(10, &[], &[])];
        assert!(orchard_nullifiers_of(&backwards, 10, 11).is_err());
        let mut short = block(10, &[[1; 32]], &[]);
        short.vtx[0].actions[0].nullifier.pop();
        assert!(orchard_nullifiers_of(&[short], 10, 11).is_err());
        let flood = vec![[1; 32]; MAX_ORCHARD_ACTIONS_PER_BLOCK + 1];
        assert!(orchard_nullifiers_of(&[block(10, &flood, &[])], 10, 11).is_err());
    }
}
