//! The flush cadence: when a batch is published, and what drives the clock.
//!
//! The queue holds; this drives. A flush happens at every multiple of
//! `flush_interval` blocks and at no other time. There is deliberately **no**
//! other trigger: not a queue depth, not an approaching expiry, not an operator
//! command.
//!
//! **Why the cadence is unconditional (REVIEW #2, #8).** Every conditional
//! trigger is a lever someone else can pull. Count-based flushing lets an
//! attacker submit 99 of their own migrations to isolate a target's. An
//! early-expiry trigger lets one cheap junk transaction per block collapse every
//! window network-wide. A stale-tip trigger lets a few minutes of packet
//! interference against the hub's node force a near-empty batch containing the
//! targeted transaction. A deterministic clock nobody can influence is the only
//! shape with no lever on it, and the cost (the schedule is public) is
//! acceptable because simultaneity, not surprise, is what hides intra-batch
//! ordering.
//!
//! **Tip acquisition is adversarial too.** The height is taken as the MAX over
//! all nodes that answer, not the first that answers, because a single lagging
//! or hostile node would otherwise be a second independent lever on the flush
//! clock: a stalled tip freezes flushes, an advanced tip drains the queue.
//! Staleness is a wall-clock fact (no node has advanced for
//! [`TIP_STALE_AFTER`]), not an absence of answers, and it stops *admission*
//! rather than causing a flush. The 15-minute threshold is not arbitrary: block
//! arrivals are Poisson at ~75 s, so a 3-block threshold would fire about 57
//! times a day, while 15 minutes is roughly one false positive every 140 days.

// See queue.rs: a panic here is a fleet-wide privacy event, not a crash.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::chain::{ChainClient, Publish};
use crate::queue::Queue;
use crate::BoxError;

/// Blocks between flushes. Twenty blocks is roughly 25 minutes.
pub const FLUSH_INTERVAL_BLOCKS: u32 = 20;

/// Blocks reserved for the batch to actually get mined after publication.
pub const MINING_MARGIN: u32 = 4;

/// Blocks reserved for wallet-to-shim lag, Nym round trips (measured 9 to 10 s
/// unary), acknowledgement retries and hub failover.
pub const MAX_DELIVERY_LAG: u32 = 6;

/// The tightest wallet expiry the design commits to supporting, in blocks.
///
/// This is librustzcash's default (40), NOT Brave's 20. Brave is out of scope
/// for v1 and the ask to them is to raise their default to 40. If any wallet
/// with an expiry below 40 comes into scope, `FLUSH_INTERVAL_BLOCKS` must come
/// back down and the batch shrinks with it.
pub const MIN_WALLET_EXPIRY: u32 = 40;

/// How far the tip may legitimately move backwards. Real reorgs happen; a drop
/// larger than this is a lagging or hostile node and is ignored.
const REORG_ALLOWANCE: u32 = 10;

/// No node advancing for this long means the tip is stale.
const TIP_STALE_AFTER: Duration = Duration::from_secs(15 * 60);

/// Nominal block interval, used only to free-run the cadence while the tip is
/// stale. During a real stall blocks arrive slower than this, so the free-running
/// clock runs ahead of the true height, which publishes EARLIER relative to true
/// expiry. That is the safe direction to err.
const NOMINAL_BLOCK_SECS: u64 = 75;

/// How often the tip is polled.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// The scheduling parameters, validated against the wallet expiry ceiling.
#[derive(Debug, Clone, Copy)]
pub struct BatchParams {
    pub flush_interval: u32,
    pub mining_margin: u32,
    pub delivery_lag: u32,
    pub min_wallet_expiry: u32,
}

impl Default for BatchParams {
    fn default() -> Self {
        Self {
            flush_interval: FLUSH_INTERVAL_BLOCKS,
            mining_margin: MINING_MARGIN,
            delivery_lag: MAX_DELIVERY_LAG,
            min_wallet_expiry: MIN_WALLET_EXPIRY,
        }
    }
}

impl BatchParams {
    /// The budget inequality, asserted at startup rather than trusted.
    ///
    /// `flush_interval + mining_margin + delivery_lag <= min_wallet_expiry`.
    /// The expiry ceiling is real and every mitigation in this space silently
    /// spends it, so a future parameter change must fail loudly at boot instead
    /// of quietly pushing a percentage of real traffic past its expiry and into
    /// the shim's last-resort path.
    pub fn validate(&self) -> Result<(), BoxError> {
        let spent = self
            .flush_interval
            .saturating_add(self.mining_margin)
            .saturating_add(self.delivery_lag);
        if spent > self.min_wallet_expiry {
            return Err(format!(
                "expiry budget exceeded: flush_interval ({}) + mining_margin ({}) + delivery_lag ({}) = {} > min_wallet_expiry ({}). \
                 Lower the flush interval or raise the supported wallet expiry; do not ship this.",
                self.flush_interval, self.mining_margin, self.delivery_lag, spent, self.min_wallet_expiry
            )
            .into());
        }
        if self.flush_interval == 0 {
            return Err("flush_interval must be at least one block".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct TipState {
    height: u32,
    last_advance: Instant,
    /// False until the first successful tip query. Before that the hub cannot
    /// schedule or expiry-check anything, so it admits nothing.
    observed: bool,
}

/// The hub's view of the chain tip, and whether it can still be trusted.
pub struct TipTracker {
    inner: RwLock<TipState>,
}

impl TipTracker {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(TipState {
                height: 0,
                last_advance: Instant::now(),
                observed: false,
            }),
        }
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, TipState> {
        match self.inner.write() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, TipState> {
        match self.inner.read() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        }
    }

    /// Record a height observed from the network (already the max over nodes).
    pub fn observe(&self, height: u32) {
        let mut state = self.write();

        if !state.observed {
            state.height = height;
            state.last_advance = Instant::now();
            state.observed = true;
            return;
        }

        if height > state.height {
            state.height = height;
            state.last_advance = Instant::now();
            return;
        }

        if height < state.height {
            let drop = state.height - height;
            if drop <= REORG_ALLOWANCE {
                // A real reorg. Follow it, and say so: this is rare enough that
                // a quiet regression would be the wrong default.
                tracing::warn!(
                    drop,
                    "chain tip moved backwards within the reorg allowance; following it"
                );
                state.height = height;
                state.last_advance = Instant::now();
            } else {
                // Beyond any plausible reorg: a lagging or hostile node won the
                // max, or a node is lying. Do not follow it.
                tracing::warn!(
                    drop,
                    "ignoring a tip regression larger than the reorg allowance"
                );
            }
        }
    }

    /// True once a tip has ever been observed.
    pub fn is_ready(&self) -> bool {
        self.read().observed
    }

    /// No node has advanced for [`TIP_STALE_AFTER`].
    pub fn is_stale(&self) -> bool {
        let state = self.read();
        !state.observed || state.last_advance.elapsed() > TIP_STALE_AFTER
    }

    /// The last observed height, without extrapolation. This is what admission
    /// checks expiry against, because admission must never be more optimistic
    /// than the chain actually is.
    pub fn observed_height(&self) -> u32 {
        self.read().height
    }

    /// The height the cadence runs on: the observed height, or, while the tip is
    /// stale, a free-running estimate from the last known good height.
    fn cadence_height(&self) -> u32 {
        let state = self.read();
        if !state.observed {
            return 0;
        }
        let elapsed = state.last_advance.elapsed();
        if elapsed <= TIP_STALE_AFTER {
            return state.height;
        }
        let estimated_blocks = (elapsed.as_secs() / NOMINAL_BLOCK_SECS) as u32;
        state.height.saturating_add(estimated_blocks)
    }
}

impl Default for TipTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Run the flush cadence until `shutdown` resolves.
///
/// Polls the tip, and whenever the height crosses into a new flush epoch
/// (`height / flush_interval`), publishes everything the queue holds. On
/// shutdown, finishes any flush in progress, then flushes once more.
pub async fn run(
    queue: Arc<Queue>,
    chain: Arc<ChainClient>,
    tip: Arc<TipTracker>,
    params: BatchParams,
    shutdown: impl std::future::Future<Output = ()>,
) {
    run_with_poll_interval(queue, chain, tip, params, shutdown, POLL_INTERVAL).await
}

/// [`run`] with the poll interval as a parameter, so a test can drive the loop
/// through a real flush-then-shutdown sequence in milliseconds. Production has
/// exactly one interval and never reaches for this directly.
async fn run_with_poll_interval(
    queue: Arc<Queue>,
    chain: Arc<ChainClient>,
    tip: Arc<TipTracker>,
    params: BatchParams,
    shutdown: impl std::future::Future<Output = ()>,
    poll_interval: Duration,
) {
    tracing::info!(
        flush_interval = params.flush_interval,
        mining_margin = params.mining_margin,
        "batching cadence started"
    );

    let mut shutdown = std::pin::pin!(shutdown);
    let mut last_flush_epoch: Option<u32> = None;
    let mut next_poll = tokio::time::Instant::now();
    loop {
        // The shutdown signal is observed here, between iterations, and nowhere
        // else. It used to race the whole ticker in a `select!`, which meant a
        // SIGTERM landing during a flush dropped the ticker future mid-await:
        // the batch had already been drained out of the queue, so the shutdown
        // flush that followed found nothing to publish and the batch was simply
        // gone. Now a flush in progress always runs to its end, its transport
        // failures go back into the queue, and only then is shutdown noticed.
        // `biased` so the signal is polled (and its handler registered) before
        // the sleep is looked at, on the very first pass.
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            _ = tokio::time::sleep_until(next_poll) => {}
        }

        match chain.tip_height().await {
            Ok(height) => tip.observe(height),
            Err(err) => {
                // Not an error condition on its own: staleness is decided by
                // elapsed time since the last advance, not by one failure.
                tracing::debug!(%err, "tip query failed on every node");
            }
        }

        if tip.is_ready() {
            let epoch = tip.cadence_height() / params.flush_interval.max(1);
            match last_flush_epoch {
                // First observation: adopt the current epoch without
                // flushing, so a restart does not publish a partial batch
                // off-cadence.
                None => last_flush_epoch = Some(epoch),
                Some(previous) if epoch > previous => {
                    flush(&queue, &chain, &tip, params).await;
                    last_flush_epoch = Some(epoch);
                }
                _ => {}
            }
        }

        next_poll = tokio::time::Instant::now() + poll_interval;
    }

    tracing::info!("cadence shutting down; publishing what is held rather than dropping it");
    flush(&queue, &chain, &tip, params).await;

    // What the shutdown flush could not place is still in the queue, and the
    // queue is about to die with the process. The enclave is diskless by
    // design, so there is nowhere to put it; the honest thing left is to say
    // how much was lost, as a count and nothing more.
    let unpublished = queue.len();
    if unpublished > 0 {
        tracing::error!(
            unpublished,
            "shutting down with migrations the indexer could not be reached for; they are lost"
        );
    }
}

/// Publish everything held, all at once, in an unpredictable order.
///
/// Returns the achieved batch size, which is the honest measure of the privacy
/// the flush actually delivered. Entries the indexer could not be reached for
/// are back in the queue when this returns; entries the indexer refused are
/// gone. Public so the cadence is not the only thing that can exercise it: a
/// flush is the security-critical operation here and it must be testable
/// directly.
pub async fn flush(
    queue: &Arc<Queue>,
    chain: &Arc<ChainClient>,
    tip: &Arc<TipTracker>,
    params: BatchParams,
) -> usize {
    let batch = queue.drain_shuffled();
    let size = batch.len();

    if size == 0 {
        // Still a flush event, and worth recording: a run of empty flushes is
        // what "the anonymity claim does not hold at this adoption level" looks
        // like in the metrics.
        tracing::info!(flush_size = 0, "flush: nothing held");
        return 0;
    }

    let started = Instant::now();
    let payloads: Vec<Vec<u8>> = batch.iter().map(|entry| entry.tx_bytes.to_vec()).collect();
    let outcomes = chain.broadcast_batch(&payloads).await;

    // Verdicts are positional, one per input transaction. Achieved is counted as
    // node-accepted OR already-known: with every shim submitting to every hub,
    // the second hub's publish is already-known by construction, so counting
    // only Accepted would report zero on one side of every honest batch.
    let mut achieved = 0usize;
    let mut rejected = 0usize;
    let mut sample_failure: Option<String> = None;
    let mut unplaced = Vec::new();
    for (i, entry) in batch.into_iter().enumerate() {
        match outcomes.get(i) {
            Some(Publish::Accepted { .. }) | Some(Publish::AlreadyKnown) => achieved += 1,
            Some(Publish::Rejected { .. }) => rejected += 1,
            Some(Publish::Retryable { reason }) => {
                sample_failure.get_or_insert_with(|| reason.clone());
                unplaced.push(entry);
            }
            // No verdict at all for this position cannot happen (join_all is
            // positional), but if it ever did, "nothing judged it" is the truthful
            // reading and the safe one.
            None => unplaced.push(entry),
        }
    }

    // A transport failure goes back into the queue for the next cadence. This is
    // the only place such a failure can be recovered: the shim answered the
    // wallet error_code 0 the moment the frame reached the mixnet and keeps no
    // record, so once the entry left this queue there is no other copy anywhere
    // that anyone will retry. Dropping it because the indexer restarted during
    // the flush window would lose the migration outright while the wallet
    // believes it was sent. The entry keeps its original expiry; when the
    // indexer answers again a stale one gets the node's verdict and leaves. A
    // Rejected verdict is not put back: the node said no, and re-offering the
    // same bytes buys the same answer every flush until expiry.
    //
    // `observed_height`, the same tip admission uses, so an entry is judged
    // against the same clock on the way back in as on the way in. Using the
    // cadence height here would test it against a different notion of "now" and
    // could hold an entry admission would already have refused.
    let requeued = queue.requeue(
        unplaced,
        tip.observed_height(),
        params.flush_interval,
        params.mining_margin,
    );

    // Aggregates only. Never a txid, never a body, never a per-entry
    // identifier: in a Nitro enclave the tracing output reaches the parent host,
    // which is exactly who this system withholds the txid from. The sample
    // reason names an endpoint and an error, not an entry.
    tracing::info!(
        flush_size = size,
        achieved_batch_size = achieved,
        rejected,
        requeued = requeued.held,
        dropped_expired = requeued.dropped_expired,
        dropped_exhausted = requeued.dropped_exhausted,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "flush published"
    );
    if requeued.held > 0 {
        tracing::warn!(
            requeued = requeued.held,
            reason = sample_failure.as_deref().unwrap_or("no verdict"),
            "indexer could not be reached for part of the batch; held for the next flush"
        );
    }
    // Louder than the requeue, because this is where a migration stops existing.
    // A wallet was told these were on their way and nothing else holds a copy,
    // so a non-zero count here is the signal that someone has lost a migration,
    // not routine housekeeping.
    if requeued.dropped_expired > 0 || requeued.dropped_exhausted > 0 {
        tracing::error!(
            dropped_expired = requeued.dropped_expired,
            dropped_exhausted = requeued.dropped_exhausted,
            reason = sample_failure.as_deref().unwrap_or("no verdict"),
            "gave up on migrations that could not be placed; they exist nowhere else"
        );
    }

    if achieved <= 1 {
        // Honest telemetry, not an error. At batch size 1 the anonymity set is
        // the transaction itself and the shuffle, the simultaneous publish, Nym
        // and the enclave are all irrelevant to it.
        tracing::warn!(
            achieved_batch_size = achieved,
            "batch provides no batching anonymity at this size"
        );
    }

    achieved
}

#[cfg(test)]
// The module-level deny above is about PRODUCTION paths: a panic in a diskless
// enclave is a fleet-wide privacy event. In tests, panicking IS the failure
// report, so the assertion macros are allowed here and nowhere else.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_parameters_satisfy_the_expiry_budget() {
        // 20 + 4 + 6 = 30 <= 40.
        BatchParams::default()
            .validate()
            .expect("the shipped parameters must fit the expiry ceiling");
    }

    #[test]
    fn a_parameter_change_that_overspends_the_budget_fails_at_startup() {
        // The whole point of the assertion: this must not be discoverable in
        // production as transactions quietly expiring.
        let bad = BatchParams {
            flush_interval: 40,
            ..BatchParams::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn a_zero_flush_interval_is_refused() {
        let bad = BatchParams {
            flush_interval: 0,
            ..BatchParams::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn brave_s_twenty_block_expiry_would_not_fit_and_the_assertion_says_so() {
        // Recorded because it is the reason MIN_WALLET_EXPIRY is 40 and not 20:
        // at Brave's default the shipped cadence does not fit, which is exactly
        // what the ask to Brave is about.
        let brave = BatchParams {
            min_wallet_expiry: 20,
            ..BatchParams::default()
        };
        assert!(brave.validate().is_err());
    }

    #[test]
    fn a_fresh_tracker_admits_nothing_because_it_knows_no_height() {
        let tip = TipTracker::new();
        assert!(!tip.is_ready());
        assert!(
            tip.is_stale(),
            "an unobserved tip must count as stale, not as height 0"
        );
    }

    #[test]
    fn the_tip_follows_advances() {
        let tip = TipTracker::new();
        tip.observe(1000);
        assert_eq!(tip.observed_height(), 1000);
        tip.observe(1001);
        assert_eq!(tip.observed_height(), 1001);
        assert!(tip.is_ready() && !tip.is_stale());
    }

    #[test]
    fn a_small_regression_is_followed_as_a_reorg() {
        let tip = TipTracker::new();
        tip.observe(1000);
        tip.observe(995);
        assert_eq!(tip.observed_height(), 995, "a real reorg must be followed");
    }

    #[test]
    fn a_large_regression_is_ignored_rather_than_followed() {
        // Otherwise one lagging or hostile node winning the max would be a lever
        // on the flush clock.
        let tip = TipTracker::new();
        tip.observe(1000);
        tip.observe(500);
        assert_eq!(tip.observed_height(), 1000);
    }

    #[test]
    fn the_cadence_height_equals_the_observed_height_while_fresh() {
        let tip = TipTracker::new();
        tip.observe(1000);
        assert_eq!(tip.cadence_height(), 1000);
    }

    #[test]
    fn flush_epochs_advance_once_per_interval_not_once_per_block() {
        // The scheduling arithmetic the cadence loop runs on.
        let n = FLUSH_INTERVAL_BLOCKS;
        assert_eq!(1000 / n, 50);
        assert_eq!(1019 / n, 50, "still the same epoch nineteen blocks later");
        assert_eq!(1020 / n, 51, "a new epoch exactly at the multiple");
    }

    // ---- flush against a real ChainClient ------------------------------------
    //
    // `ChainClient` is not a trait, so it is faked the way the integration tests
    // fake it: with an in-process gRPC responder on an ephemeral port. This one
    // is smaller than `tests/common`: it answers `GetLightdInfo` from a shared
    // height (so the cadence loop can be driven) and `SendTransaction` from a
    // script, so a test can say "fail this call, then accept the next".

    use std::collections::VecDeque;
    use std::convert::Infallible;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    use bytes::Bytes;
    use http_body_util::combinators::BoxBody;
    use http_body_util::{BodyExt, Full};
    use hyper::body::Incoming;
    use hyper::server::conn::http2;
    use hyper::service::service_fn;
    use hyper::{HeaderMap, Request, Response};
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use prost::Message;
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use zaino_proto::proto::service::{LightdInfo, RawTransaction, SendResponse};

    const GET_LIGHTD_INFO_PATH: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo";
    const TIP: u32 = 1000;
    const N: u32 = FLUSH_INTERVAL_BLOCKS;
    const MARGIN: u32 = MINING_MARGIN;

    /// A payload that will not deserialize as a transaction, so it has no expiry
    /// and is admissible at any tip.
    fn junk(seed: u8) -> Vec<u8> {
        vec![seed; 64]
    }

    /// A tip tracker parked at [`TIP`], which is the height `queue_holding`
    /// admits against. `flush` needs one because the requeue is bounded by the
    /// same expiry test admission uses.
    fn test_tip() -> Arc<TipTracker> {
        let tip = Arc::new(TipTracker::new());
        tip.observe(TIP);
        tip
    }

    /// How the mock answers one `SendTransaction`.
    #[derive(Clone, Copy)]
    enum Answer {
        /// gRPC OK carrying `SendResponse { code, message }`: the indexer's
        /// verdict, in the shape lightwalletd and zaino relay a node's answer.
        Send(i32, &'static str),
        /// A trailers-only non-zero gRPC status with no `SendResponse` at all.
        Status(&'static str),
    }

    struct MockIndexer {
        addr: SocketAddr,
        /// What `GetLightdInfo` reports; a test moves it to advance the epoch.
        height: Arc<AtomicU32>,
        /// Every raw transaction the mock was asked to broadcast, in arrival
        /// order, whatever it answered.
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        /// Fires once per `SendTransaction` received, before it is answered.
        on_send: Arc<Notify>,
    }

    /// Answers `SendTransaction` from `script` in order, repeating the last entry
    /// once the script is exhausted, after `answer_delay`.
    async fn spawn_mock_indexer(script: Vec<Answer>, answer_delay: Duration) -> MockIndexer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let height = Arc::new(AtomicU32::new(TIP));
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let on_send = Arc::new(Notify::new());
        let script = Arc::new(Mutex::new(VecDeque::from(script)));

        let (height_s, sent_s, on_send_s) = (height.clone(), sent.clone(), on_send.clone());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (height, sent, on_send, script) = (
                    height_s.clone(),
                    sent_s.clone(),
                    on_send_s.clone(),
                    script.clone(),
                );
                tokio::spawn(async move {
                    let _ = http2::Builder::new(TokioExecutor::new())
                        .serve_connection(
                            TokioIo::new(stream),
                            service_fn(move |req: Request<Incoming>| {
                                let (height, sent, on_send, script) = (
                                    height.clone(),
                                    sent.clone(),
                                    on_send.clone(),
                                    script.clone(),
                                );
                                async move {
                                    let path = req.uri().path().to_owned();
                                    let body = req.into_body().collect().await.unwrap().to_bytes();
                                    let message = if body.len() > 5 { &body[5..] } else { &[][..] };
                                    if path == GET_LIGHTD_INFO_PATH {
                                        let info = LightdInfo {
                                            block_height: height.load(Ordering::SeqCst) as u64,
                                            ..Default::default()
                                        };
                                        return Ok::<_, Infallible>(grpc_ok(info.encode_to_vec()));
                                    }
                                    if let Ok(raw) = RawTransaction::decode(message) {
                                        sent.lock().unwrap().push(raw.data);
                                    }
                                    on_send.notify_one();
                                    let answer = {
                                        let mut script = script.lock().unwrap();
                                        if script.len() > 1 {
                                            script.pop_front().unwrap()
                                        } else {
                                            *script.front().expect("script must not be empty")
                                        }
                                    };
                                    tokio::time::sleep(answer_delay).await;
                                    Ok(match answer {
                                        Answer::Send(code, text) => grpc_ok(
                                            SendResponse {
                                                error_code: code,
                                                error_message: text.to_owned(),
                                            }
                                            .encode_to_vec(),
                                        ),
                                        Answer::Status(code) => grpc_status(code),
                                    })
                                }
                            }),
                        )
                        .await;
                });
            }
        });

        MockIndexer {
            addr,
            height,
            sent,
            on_send,
        }
    }

    /// Frame `payload` as a gRPC unary body with a `grpc-status: 0` trailer.
    fn grpc_ok(payload: Vec<u8>) -> Response<BoxBody<Bytes, Infallible>> {
        let mut framed = Vec::with_capacity(5 + payload.len());
        framed.push(0);
        framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        framed.extend_from_slice(&payload);
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", "0".parse().unwrap());
        Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .body(
                Full::new(Bytes::from(framed))
                    .with_trailers(async move { Some(Ok(trailers)) })
                    .boxed(),
            )
            .unwrap()
    }

    /// A trailers-only gRPC error: status in the headers, empty body.
    fn grpc_status(code: &str) -> Response<BoxBody<Bytes, Infallible>> {
        Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .header("grpc-status", code)
            .body(Full::new(Bytes::new()).boxed())
            .unwrap()
    }

    /// An address nothing listens on: bound, read, released. A connection to it
    /// is refused at once, which is the cheapest real transport failure there
    /// is.
    async fn unreachable_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    }

    fn client(addr: SocketAddr) -> Arc<ChainClient> {
        Arc::new(ChainClient::new(vec![addr], None).unwrap())
    }

    fn queue_holding(payloads: &[Vec<u8>]) -> Arc<Queue> {
        let queue = Arc::new(Queue::new());
        for payload in payloads {
            assert!(matches!(
                queue.admit(payload, TIP, N, MARGIN),
                crate::queue::Admission::Admitted { .. }
            ));
        }
        queue
    }

    #[tokio::test]
    async fn a_transport_failed_publish_is_held_and_published_on_the_next_flush() {
        // The property this whole change exists for. The shim has already told
        // the wallet the migration was sent, so an entry that leaves the queue
        // without a verdict must come back to it, and the next cadence must
        // actually publish it.
        let queue = queue_holding(&[junk(1), junk(2)]);

        let dead = client(unreachable_addr().await);
        assert_eq!(
            flush(&queue, &dead, &test_tip(), BatchParams::default()).await,
            0
        );
        assert_eq!(
            queue.len(),
            2,
            "nothing judged these transactions, so both must still be held"
        );

        let mock = spawn_mock_indexer(vec![Answer::Send(0, "txid")], Duration::ZERO).await;
        let live = client(mock.addr);
        assert_eq!(
            flush(&queue, &live, &test_tip(), BatchParams::default()).await,
            2,
            "the held entries are the next batch"
        );
        assert!(queue.is_empty());
        assert_eq!(mock.sent.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_indexer_verdict_is_dropped_not_requeued() {
        // The indexer answered OK with a non-zero code: the node said no. Holding
        // it would cost an indexer call every flush for the same answer.
        let mock = spawn_mock_indexer(
            vec![Answer::Send(
                -26,
                "16: bad-txns-sapling-binding-signature-invalid",
            )],
            Duration::ZERO,
        )
        .await;
        let chain = client(mock.addr);
        let queue = queue_holding(&[junk(1)]);

        assert_eq!(
            flush(&queue, &chain, &test_tip(), BatchParams::default()).await,
            0
        );
        assert!(queue.is_empty(), "a rejected transaction is not held");
        assert_eq!(
            flush(&queue, &chain, &test_tip(), BatchParams::default()).await,
            0
        );
        assert_eq!(
            mock.sent.lock().unwrap().len(),
            1,
            "the second flush must not have offered it again"
        );
    }

    #[tokio::test]
    async fn a_transport_flavoured_grpc_status_is_held_but_invalid_argument_is_not() {
        // The same wire (a non-OK gRPC status) carries both kinds of failure;
        // the split is by status code and this pins where it falls.
        let unavailable = spawn_mock_indexer(vec![Answer::Status("14")], Duration::ZERO).await;
        let queue = queue_holding(&[junk(1)]);
        assert_eq!(
            flush(
                &queue,
                &client(unavailable.addr),
                &test_tip(),
                BatchParams::default()
            )
            .await,
            0
        );
        assert_eq!(queue.len(), 1, "UNAVAILABLE is no verdict; hold it");

        let refusing = spawn_mock_indexer(vec![Answer::Status("3")], Duration::ZERO).await;
        assert_eq!(
            flush(
                &queue,
                &client(refusing.addr),
                &test_tip(),
                BatchParams::default()
            )
            .await,
            0
        );
        assert!(
            queue.is_empty(),
            "INVALID_ARGUMENT is the indexer refusing it"
        );
    }

    #[tokio::test]
    async fn a_dead_endpoint_beside_a_live_one_does_not_hold_a_placed_batch() {
        // Two entries, one endpoint that accepts and one that is unreachable.
        // Any acceptance wins per transaction, so both are achieved and nothing
        // is held; the dead endpoint must not turn a placed batch into a
        // re-offered one.
        let mock = spawn_mock_indexer(vec![Answer::Send(0, "txid")], Duration::ZERO).await;
        let chain =
            Arc::new(ChainClient::new(vec![mock.addr, unreachable_addr().await], None).unwrap());
        let queue = queue_holding(&[junk(1), junk(2)]);
        assert_eq!(
            flush(&queue, &chain, &test_tip(), BatchParams::default()).await,
            2
        );
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn a_shutdown_during_an_in_flight_flush_does_not_drop_the_batch() {
        // The mock fires the shutdown signal the moment the first publish
        // reaches it, then answers UNAVAILABLE after a delay: shutdown lands
        // squarely inside a flush whose batch has already left the queue. The
        // old `select!` dropped the ticker there and the batch with it, and the
        // shutdown flush then found an empty queue. Now the in-flight flush must
        // finish, put the entry back, and the shutdown flush must publish it,
        // which the mock sees as a SECOND SendTransaction that it accepts.
        let mock = spawn_mock_indexer(
            vec![Answer::Status("14"), Answer::Send(0, "txid")],
            Duration::from_millis(200),
        )
        .await;
        let chain = client(mock.addr);
        let queue = queue_holding(&[junk(1)]);
        let tip = Arc::new(TipTracker::new());

        let signal = mock.on_send.clone();
        let cadence = tokio::spawn(run_with_poll_interval(
            queue.clone(),
            chain,
            tip.clone(),
            BatchParams::default(),
            async move { signal.notified().await },
            Duration::from_millis(20),
        ));

        // First tick adopts the epoch at TIP without flushing; only then does
        // moving the height into the next epoch make the following tick flush.
        let deadline = Instant::now() + Duration::from_secs(10);
        while tip.observed_height() != TIP {
            assert!(
                Instant::now() < deadline,
                "the cadence never observed the tip"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        mock.height.store(TIP + N, Ordering::SeqCst);

        tokio::time::timeout(Duration::from_secs(30), cadence)
            .await
            .expect("the cadence must exit once shutdown is observed")
            .expect("the cadence must not panic");

        assert_eq!(
            mock.sent.lock().unwrap().len(),
            2,
            "the batch drained by the interrupted flush must be offered again by the shutdown flush"
        );
        assert!(queue.is_empty(), "and the shutdown flush placed it");
    }
}
