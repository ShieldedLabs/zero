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
/// (`height / flush_interval`), publishes everything the queue holds.
pub async fn run(
    queue: Arc<Queue>,
    chain: Arc<ChainClient>,
    tip: Arc<TipTracker>,
    params: BatchParams,
    shutdown: impl std::future::Future<Output = ()>,
) {
    tracing::info!(
        flush_interval = params.flush_interval,
        mining_margin = params.mining_margin,
        "batching cadence started"
    );

    let mut last_flush_epoch: Option<u32> = None;
    let ticker = async {
        loop {
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
                        flush(&queue, &chain).await;
                        last_flush_epoch = Some(epoch);
                    }
                    _ => {}
                }
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    };

    tokio::select! {
        _ = ticker => {}
        _ = shutdown => {
            tracing::info!("cadence shutting down; publishing what is held rather than dropping it");
            flush(&queue, &chain).await;
        }
    }
}

/// Publish everything held, all at once, in an unpredictable order.
///
/// Returns the achieved batch size, which is the honest measure of the privacy
/// the flush actually delivered. Public so the cadence is not the only thing
/// that can exercise it: a flush is the security-critical operation here and it
/// must be testable directly.
pub async fn flush(queue: &Arc<Queue>, chain: &Arc<ChainClient>) -> usize {
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

    // Counted as node-accepted OR already-known. With every shim submitting to
    // every hub, the second hub's publish is already-known by construction, so
    // counting only Accepted would report zero on one side of every honest
    // batch.
    let achieved = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, Publish::Accepted { .. } | Publish::AlreadyKnown))
        .count();
    let rejected = size.saturating_sub(achieved);

    // Aggregates only. Never a txid, never a body, never a per-entry
    // identifier: in a Nitro enclave the tracing output reaches the parent host,
    // which is exactly who this system withholds the txid from.
    tracing::info!(
        flush_size = size,
        achieved_batch_size = achieved,
        rejected,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "flush published"
    );

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
}
