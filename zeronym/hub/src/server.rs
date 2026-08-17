//! The inbound serving path: receive a diverted migration, hold it for the batch.
//!
//! A submission is **admitted, not broadcast**. It joins the queue and is
//! published at the next cadence boundary together with everything else admitted
//! in that window (see [`crate::batcher`]). The acknowledgement carries the txid
//! the hub computes from the bytes, so the wallet gets the txid it expects
//! immediately even though publication is minutes away.
//!
//! Safety rails from `REVIEW.md` that bind on this path:
//!
//! * **Re-parse is telemetry, never a refusal (#5).** A transaction the hub
//!   cannot parse is precisely one the shim deliberately diverted because it
//!   could not read it either, so refusing it would invert the shim's fail-safe
//!   into a leak. `sendrawtransaction` at the node is the only authority on
//!   validity; the hub publishes what it is given.
//! * **Never log a txid or a transaction body (#157).** In an enclave the log
//!   reaches the parent host, and the txid is the one fact this system exists to
//!   withhold. Only counts and dispositions are logged.
//! * **Zeroize the decrypted bytes (#161).** Held bytes live in
//!   [`zeroize::Zeroizing`] for as long as they are queued.
//! * **Never return a queue depth or batch size down this channel.** That would
//!   be a live anonymity-set-size oracle for anyone who can run a shim, and a
//!   real-time "the fleet is unprotected right now" feed for an adversary
//!   choosing when to correlate. The response says admitted or refused, and
//!   nothing about how much company the entry has.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use zeroize::Zeroizing;

use crate::batcher::{BatchParams, TipTracker};
use crate::chain::{ChainClient, TxLookup};
use crate::queue::{Admission, Queue, Refusal};
use crate::BoxError;

/// Ceiling on a submitted transaction. Real Orchard migrations are 2 to 16 KB;
/// 64 KiB is the frame the batching design pads to (`REVIEW.md`). It is a
/// deliberate, tight bound, NOT the shim's 4 MiB HTTP-body limit, which bounds a
/// wallet's request into a shim and is unrelated.
const MAX_TX_BYTES: usize = 64 * 1024;

/// The submission path: `POST /` with a raw transaction body. The transitional
/// clearnet hop, and OFF unless [`ServeOptions::http_submit`] turns it on: with
/// the mixnet path working, nothing legitimate posts here, while the enclave
/// declares `ingress 0.0.0.0/0` and the hub has no submitter ACL by design. Kept
/// reachable by explicit config for tests and local demos (NYM_PLAN M7).
const SUBMIT_PATH: &str = "/";

/// The lookup path: `POST /transaction` with a raw `TxFilter.hash` body.
const TRANSACTION_PATH: &str = "/transaction";

/// The hub's current Nym address: `GET /nym-address`.
///
/// Exists because an ATTESTED enclave has no console. The address is minted
/// inside the enclave and was previously written only to a log the operator
/// cannot read without debug mode — which disables attestation, so reading it
/// and proving it were mutually exclusive. Publishing it costs nothing: it is
/// the one value in this system that is meant to be public, since every shim
/// must know it to submit at all.
const NYM_ADDRESS_PATH: &str = "/nym-address";

/// Liveness: `GET /healthz`. Distinct from Caution's own
/// `/.well-known/caution/health`, which the platform serves itself.
const HEALTH_PATH: &str = "/healthz";

/// Live mixnet status: `GET /nym-status`.
///
/// Neither of the two endpoints above can answer "is this hub reachable over the
/// mixnet right now". `/healthz` is process liveness, and `/nym-address` returns
/// a value that OUTLIVES the client that published it, deliberately — so both
/// answered 200 for hours on a hub that was answering no mixnet traffic at all
/// (measured 2026-08-14). This is the endpoint that distinguishes them.
const NYM_STATUS_PATH: &str = "/nym-status";

/// Ceiling on a lookup body. A `TxFilter.hash` is 32 bytes; 64 leaves slack
/// without letting the lookup path be used to buffer anything meaningful.
const MAX_LOOKUP_BYTES: usize = 64;

/// Header carrying the transaction's height on a `200` lookup reply. `0` means
/// mempool (a queued, unflushed transaction), matching lightwalletd's sentinel.
const TX_HEIGHT_HEADER: &str = "x-tx-height";

/// The hub's current Nym address, shared between the mixnet driver (which mints
/// it) and the serving path (which publishes it at [`NYM_ADDRESS_PATH`]).
///
/// Deliberately NOT a field on [`Hub`]: that type is constructed in both ingress
/// paths, in four test files and in the localnet probe, and none of those care
/// about a clearnet endpoint. Threading it through [`serve`] keeps the blast
/// radius to the one caller that serves HTTP.
///
/// `None` until the driver's first successful connect, and answered as such
/// rather than as an empty string — an operator pasting `""` into a shim's
/// `--hub-nym` would get a config that fails at the first divert instead of at
/// assemble time.
#[derive(Clone, Default)]
pub struct NymAddress(Arc<NymState>);

#[derive(Default)]
struct NymState {
    address: std::sync::RwLock<Option<String>>,
    /// Whether a client is connected RIGHT NOW, as opposed to whether one ever
    /// was. See [`NymAddress::set_died`] for why the distinction is load-bearing.
    connected: std::sync::atomic::AtomicBool,
    deaths: std::sync::atomic::AtomicU64,
    consecutive_failures: std::sync::atomic::AtomicU64,
}

impl NymAddress {
    /// A handle with no address yet.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Record the address the driver just built, and mark the client live.
    /// Called on every (re)build, so it is idempotent for the common case where
    /// the address did not change.
    pub fn set(&self, address: String) {
        if let Ok(mut slot) = self.0.address.write() {
            *slot = Some(address);
        }
        self.0
            .connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.0
            .consecutive_failures
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// The client died. The address is deliberately KEPT — shims are baked
    /// against it and it is still the right value to hand out once the client
    /// comes back with the same identity — but the hub is no longer reachable
    /// over the mixnet until it does.
    ///
    /// This distinction is why the endpoint below exists. Measured 2026-08-14:
    /// the attested hub answered `/nym-address` 200 and `/healthz` 200 for hours
    /// while answering no mixnet traffic at all, because a published address
    /// survives the client that published it. "It has an address" was mistaken
    /// for "it is working", and a whole afternoon went into suspecting the
    /// mixnet, which a local pair then round-tripped in 5.6 s.
    pub fn set_died(&self) {
        self.0
            .connected
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.0
            .deaths
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// A rebuild attempt failed: still down, and the run of failures grows.
    pub fn set_rebuild_failed(&self) {
        self.0
            .connected
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.0
            .consecutive_failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// The current address, if the mixnet client has connected at least once.
    ///
    /// A poisoned lock reads as `None`: the endpoint then reports "not yet
    /// known", which is the honest answer and cannot mislead a shim's config.
    pub fn get(&self) -> Option<String> {
        self.0.address.read().ok().and_then(|slot| slot.clone())
    }

    /// The live mixnet status, as [`NYM_STATUS_PATH`] serves it.
    ///
    /// Same discipline as the address endpoint: this is CLIENT LIFECYCLE only.
    /// No queue depth, no admission counts, no txids — those would be an oracle
    /// for how many migrations are in flight, which is the anonymity-set size.
    pub fn status_json(&self) -> serde_json::Value {
        use std::sync::atomic::Ordering;
        serde_json::json!({
            "mixnet_connected": self.0.connected.load(Ordering::Relaxed),
            "address_published": self.get().is_some(),
            "client_deaths": self.0.deaths.load(Ordering::Relaxed),
            "consecutive_rebuild_failures": self.0.consecutive_failures.load(Ordering::Relaxed),
        })
    }
}

/// Serving-path configuration that is not part of the batching core.
#[derive(Clone, Default)]
pub struct ServeOptions {
    /// The hub's Nym address, published at [`NYM_ADDRESS_PATH`].
    pub nym_address: NymAddress,
    /// Accept clearnet submissions at [`SUBMIT_PATH`]. **Off by default**: the
    /// mixnet is the submit path now, and an open unauthenticated `POST /` on a
    /// `0.0.0.0/0` ingress is attack surface with no legitimate user. Turn it on
    /// only for a transitional clearnet shim, a local demo, or a test.
    pub http_submit: bool,
}

/// Everything the serving path needs.
///
/// It reaches the network in exactly one way, through [`ChainClient`]: to
/// publish a flushed batch and to answer a transaction lookup that missed the
/// queue. Admission itself never touches a node (calling `testmempoolaccept` or
/// any per-submission query would leak each transaction individually at arrival,
/// the timing signal the batch exists to destroy).
#[derive(Clone)]
pub struct Hub {
    pub queue: Arc<Queue>,
    pub tip: Arc<TipTracker>,
    pub params: BatchParams,
    pub chain: Arc<ChainClient>,
}

/// The outcome of the transport-independent lookup core, rendered by each
/// transport in its own wire shape (HTTP status plus header, or a
/// `LookupReplyV1` disposition).
pub enum LookupOutcome {
    /// The transaction, at `height` (`0` = mempool, the sentinel a wallet sees
    /// for an unmined transaction). [`Zeroizing`] because a queue hit is a
    /// diverted, not-yet-published migration.
    Found { data: Zeroizing<Vec<u8>>, height: u64 },
    /// Neither the queue nor the indexer knows it.
    NotFound,
    /// The indexer could not answer. Every transport renders this closed
    /// (HTTP 502, wire `error`); the caller must never fall back to the
    /// operator's indexer.
    Unavailable,
}

impl Hub {
    /// Admit one migration's bytes into the batch, or refuse it. This is the
    /// transport-independent core of the serving path: the stale-tip gate, the
    /// queue admission, and the counts-only logging, with no HTTP and no framing
    /// in it, so the mixnet listener admits through exactly this call and cannot
    /// drift from the HTTP path.
    ///
    /// `Ok(txid)` means the hub holds these bytes and will publish them; the txid
    /// is computed from the bytes and is `None` when they did not parse (queued
    /// and published regardless, REVIEW #5). `Err(refusal)` is a typed refusal
    /// the caller renders in its own wire shape. It must never be answered by
    /// handing the migration to the operator.
    pub fn admit(&self, tx_bytes: &[u8]) -> Result<Option<String>, Refusal> {
        // A stale tip means neither the flush schedule nor the expiry check can
        // be trusted, so admission stops. Fail-closed: the shim holds and
        // retries, or tries another hub.
        if self.tip.is_stale() {
            return Err(Refusal::TipStale);
        }

        match self.queue.admit(
            tx_bytes,
            self.tip.observed_height(),
            self.params.flush_interval,
            self.params.mining_margin,
        ) {
            // Both are success: the hub holds these bytes and will publish them.
            // Duplicate is not an error, because honest resends and cross-hub
            // submission are the designed behaviour, and identical bytes collapse.
            Admission::Admitted { txid } | Admission::Duplicate { txid } => {
                // No txid, no body reaches the log (#157) -- and, since 2026-08-18,
                // no per-event TIMESTAMP at the default level either. A line per
                // admission carries no content, but in an enclave the parent host
                // reads the log, and "a migration arrived at T" is the batch's
                // arrival-time distribution -- below the "aggregates only" bar
                // even though it names nothing. In clearnet mode it adds nothing
                // the host does not already see; in Nym mode it is the one thing
                // the mixnet was hiding from the host. The aggregate the design
                // wants -- how many were admitted -- is logged once per flush by
                // the batcher. Whether this one parsed stays as the single
                // telemetry bit, at debug.
                tracing::debug!(parseable = txid.is_some(), "migration admitted to the batch");
                Ok(txid)
            }
            Admission::Refused(refusal) => {
                // A refusal is operationally urgent (TipStale means the hub cannot
                // see the chain; QueueFull means it is being flooded) and rare, so
                // it stays at warn, by reason. Be honest that this IS still a
                // per-event timestamped line: it leaks "someone was refused at T"
                // to the parent host. That is accepted, because a refusal is the
                // signal an operator must act on now, and because the successful
                // path -- the steady-state signal an observer would actually
                // count -- is what carried the arrival-time distribution and is
                // now silent above debug.
                tracing::warn!(reason = refusal.as_str(), "submission refused at admission");
                Err(refusal)
            }
        }
    }

    /// Answer a transaction lookup: the queue first (a diverted, not-yet-flushed
    /// migration exists nowhere else; height 0 is the mempool sentinel), then
    /// the hub's indexer. This is the transport-independent core of the lookup
    /// path, factored the way [`Hub::admit`] was, so the mixnet listener answers
    /// through exactly this call and cannot drift from the HTTP path.
    ///
    /// Disposition only in every log arm: an indexer's error message can echo
    /// the txid, so nothing but the outcome word reaches the log (#157).
    ///
    /// Note the flush-in-flight gap: `flush()` drains the queue before
    /// `broadcast_batch` has reached the indexer, so a lookup in that window
    /// gets a queue miss then an indexer NOT_FOUND for a transaction it was
    /// told height-0 about seconds earlier. Wallets poll on multi-second
    /// intervals and tolerate a transient NOT_FOUND; a resubmit is harmless
    /// (deduped pre-flush, already-known post-flush). Holding entries until
    /// broadcast returns would extend how long the hub remembers a txid, which
    /// is the wrong trade.
    pub async fn lookup(&self, wire_hash: &[u8]) -> LookupOutcome {
        if let Some(bytes) = self.queue.find_by_txid(wire_hash) {
            tracing::debug!(source = "queue", "transaction lookup answered");
            return LookupOutcome::Found {
                data: bytes,
                height: 0,
            };
        }

        match self.chain.get_transaction(wire_hash).await {
            Ok(TxLookup::Found { data, height }) => {
                tracing::debug!(source = "indexer", "transaction lookup answered");
                LookupOutcome::Found {
                    data: Zeroizing::new(data),
                    height,
                }
            }
            Ok(TxLookup::NotFound) => {
                tracing::debug!(source = "miss", "transaction lookup: not found");
                LookupOutcome::NotFound
            }
            Err(_) => {
                tracing::debug!(source = "indexer_error", "transaction lookup failed");
                LookupOutcome::Unavailable
            }
        }
    }
}

/// How long the accept loop pauses when the process is out of descriptors,
/// before trying again. Short, because the point is to stop spinning on EMFILE
/// (which returns instantly and would otherwise be a hot loop), not to wait for
/// the condition to clear -- that happens when connections close.
const ACCEPT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

/// The accept() errors that say something about ONE connection, not the
/// listener: the peer went away between the SYN and the accept, or the call was
/// interrupted. Continuing is the only correct answer.
fn is_transient_accept_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::Interrupted
    )
}

/// EMFILE (24, this process is out of descriptors) and ENFILE (23, the system
/// is). Both are transient, and both are self-inflicted denial of service if
/// the loop treats them as fatal, because on this hub "fatal" means the held
/// batch is lost.
fn is_fd_exhaustion(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(23) | Some(24))
}

/// Accept and serve submissions on an already-bound listener until it errors.
/// Taking the listener rather than an address lets the caller (and tests) choose
/// and observe the bound port.
pub async fn serve(
    listener: TcpListener,
    hub: Hub,
    options: ServeOptions,
) -> Result<(), BoxError> {
    tracing::info!(
        local = ?listener.local_addr().ok(),
        flush_interval = hub.params.flush_interval,
        http_submit = options.http_submit,
        "hub listening: submissions are queued and published on the flush cadence"
    );

    loop {
        // A transient accept() error must not take the hub down. This is the
        // ingress on 0.0.0.0/0, so ECONNABORTED (the peer left between SYN and
        // accept), EINTR, and descriptor exhaustion are all reachable, and none
        // of them says the listener is unusable. Before this, `accept().await?`
        // returned from serve() on ANY of them, main's select! then exited the
        // process, and the RAM-only queue -- every migration already acked to a
        // wallet and waiting for the next flush -- went with it. One EMFILE was
        // a total loss of the held batch. Same classification as the shim's loop.
        let (stream, _peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) if is_transient_accept_error(&err) => {
                tracing::debug!(%err, "transient accept error, continuing");
                continue;
            }
            Err(err) if is_fd_exhaustion(&err) => {
                tracing::warn!(%err, "out of file descriptors, pausing the accept loop");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        let hub = hub.clone();
        let options = options.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            // `.timer()` is load-bearing, not decoration. hyper 1.x defaults
            // header_read_timeout to 30 s but silently DISABLES it (a warn!, no
            // error) when no timer is installed, so without this line a client
            // that opens a connection and never sends headers holds a task and a
            // descriptor forever, and enough of them reach the EMFILE arm above.
            if let Err(err) = http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(
                    io,
                    service_fn(move |req| handle(req, hub.clone(), options.clone())),
                )
                .await
            {
                tracing::debug!(%err, "connection closed with error");
            }
        });
    }
}

/// Route one request. Never returns `Err`: a bad request is a response, not a
/// connection fault.
///
/// Dispatch on the PATH first, then the method within it, which keeps the two
/// answers distinct: a path that exists but was asked with the wrong verb is
/// `405`, and a path that does not exist is `404`. The write paths stay `POST`;
/// the two read-only endpoints are `GET`, because their callers are a human with
/// `curl` and an uptime monitor, and a POST-only health check is one most
/// checkers cannot be pointed at.
///
/// An unknown path is `404`, a deliberate narrowing kept from the original: the
/// old hub treated EVERY POST as a submission, so a path typo (or a shim posting
/// a lookup to the wrong URL) silently queued garbage.
///
/// `SUBMIT_PATH` falls through to that `404` arm unless
/// [`ServeOptions::http_submit`] is set — so a disabled submit path is
/// indistinguishable from one that never existed, and a scanner learns nothing
/// about whether this hub could have accepted a submission.
async fn handle(
    req: Request<Incoming>,
    hub: Hub,
    options: ServeOptions,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Cloned up front: the handlers below consume `req`, so the method cannot
    // be borrowed from it at the same time. `Method` is cheap to clone.
    let method = req.method().clone();
    match req.uri().path() {
        SUBMIT_PATH if options.http_submit => match method {
            Method::POST => submit(req, hub).await,
            _ => Ok(text(StatusCode::METHOD_NOT_ALLOWED, "POST only")),
        },
        TRANSACTION_PATH => match method {
            Method::POST => lookup(req, hub).await,
            _ => Ok(text(StatusCode::METHOD_NOT_ALLOWED, "POST only")),
        },
        NYM_ADDRESS_PATH => match method {
            Method::GET => Ok(nym_address(&options.nym_address)),
            _ => Ok(text(StatusCode::METHOD_NOT_ALLOWED, "GET only")),
        },
        HEALTH_PATH => match method {
            Method::GET => Ok(text(StatusCode::OK, "ok")),
            _ => Ok(text(StatusCode::METHOD_NOT_ALLOWED, "GET only")),
        },
        NYM_STATUS_PATH => match method {
            Method::GET => Ok(json(StatusCode::OK, &options.nym_address.status_json())),
            _ => Ok(text(StatusCode::METHOD_NOT_ALLOWED, "GET only")),
        },
        _ => Ok(text(StatusCode::NOT_FOUND, "unknown path")),
    }
}

/// Publish the hub's Nym address, and nothing else.
///
/// The response body is the bare address so `curl` output can be pasted
/// straight into a shim's `--hub-nym`. It carries no queue depth, no batch
/// size, no counts: those would be a live anonymity-set-size oracle for anyone
/// who can reach the hub (see this module's header), and this endpoint is
/// reachable by everyone.
fn nym_address(address: &NymAddress) -> Response<Full<Bytes>> {
    match address.get() {
        Some(address) => text(StatusCode::OK, &address),
        // Not an empty 200: an operator pasting "" into a shim config would get
        // a deployment that fails at the first divert rather than at assemble.
        None => text(
            StatusCode::SERVICE_UNAVAILABLE,
            "the mixnet client has not connected yet; no address to publish",
        ),
    }
}

/// Answer a transaction lookup over HTTP. The queue-then-indexer core is
/// [`Hub::lookup`], shared with the mixnet listener; this handler only buffers
/// the key and shapes the outcome as a status code plus the height header.
///
/// This is why the shim can be stateless: it holds nothing about the migrations
/// it diverted, and routes every `GetTransaction` here instead.
async fn lookup(req: Request<Incoming>, hub: Hub) -> Result<Response<Full<Bytes>>, Infallible> {
    let collected = match Limited::new(req.into_body(), MAX_LOOKUP_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected,
        Err(_) => return Ok(text(StatusCode::PAYLOAD_TOO_LARGE, "lookup key too large")),
    };
    let wire_hash = collected.to_bytes();
    if wire_hash.is_empty() {
        return Ok(text(StatusCode::BAD_REQUEST, "empty lookup key"));
    }

    match hub.lookup(&wire_hash).await {
        LookupOutcome::Found { data, height } => Ok(found(&data, height)),
        LookupOutcome::NotFound => Ok(text(StatusCode::NOT_FOUND, "transaction not found")),
        LookupOutcome::Unavailable => Ok(text(StatusCode::BAD_GATEWAY, "indexer unavailable")),
    }
}

/// A `200` lookup reply carrying the raw transaction and its height.
fn found(tx_bytes: &[u8], height: u64) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::copy_from_slice(tx_bytes)));
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    resp.headers_mut()
        .insert(TX_HEIGHT_HEADER, HeaderValue::from(height));
    resp
}

/// Admit one transaction into the batch. Never returns `Err`: a bad request is a
/// response, not a connection fault.
async fn submit(req: Request<Incoming>, hub: Hub) -> Result<Response<Full<Bytes>>, Infallible> {
    // Buffer the body under a hard cap, refused strictly before anything else.
    let collected = match Limited::new(req.into_body(), MAX_TX_BYTES).collect().await {
        Ok(collected) => collected,
        Err(_) => {
            return Ok(text(
                StatusCode::PAYLOAD_TOO_LARGE,
                "transaction exceeds the frame size",
            ))
        }
    };
    let tx_bytes = Zeroizing::new(collected.to_bytes().to_vec());

    if tx_bytes.is_empty() {
        return Ok(text(StatusCode::BAD_REQUEST, "empty body"));
    }

    // The transport-independent core does the stale-tip gate, the queue
    // admission and the counts-only logging; this HTTP handler only shapes the
    // result as JSON. The mixnet listener admits through the same `Hub::admit`.
    match hub.admit(tx_bytes.as_slice()) {
        // `accepted` because the hub has taken responsibility for it, which is
        // what the shim needs in order to answer the wallet. The txid is computed
        // from the bytes, so it is correct now even though the transaction will
        // not reach a node until the next flush.
        Ok(txid) => Ok(json(
            StatusCode::OK,
            &serde_json::json!({
                "disposition": "accepted",
                "txid": txid,
                "reason": serde_json::Value::Null,
            }),
        )),
        Err(refusal) => Ok(refused(refusal)),
    }
}

/// A typed refusal. The reason is a stable machine-readable token so the shim
/// can tell "hold and retry" from "try another hub", and it carries nothing
/// about the entry or the queue.
fn refused(refusal: Refusal) -> Response<Full<Bytes>> {
    json(
        StatusCode::OK,
        &serde_json::json!({
            "disposition": "rejected",
            "txid": serde_json::Value::Null,
            "reason": refusal.as_str(),
        }),
    )
}

/// A `text/plain` response, built without a fallible builder so no serving path
/// can panic.
fn text(code: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(msg.to_owned())));
    *resp.status_mut() = code;
    resp
}

/// A `application/json` response, likewise panic-free.
fn json(code: StatusCode, value: &serde_json::Value) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(value.to_string())));
    *resp.status_mut() = code;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}
