//! The Orchard nullifiers the chain has already revealed, so a copy of a mined
//! transaction buys no place in the queue.
//!
//! A proof check prices forged junk, and leaves one free source of valid
//! proofs: transactions already on chain. Every NU6.3-era Orchard transaction
//! carries proofs that verify, and a ZIP 318 migration expires 30 to 60 days
//! after it is built, so on 2026-10-28 about 1,740 mined ones were still
//! admissible at the tip. Each could fill a proven-lane slot for free. The burn
//! set catches a copy after this hub publishes it once, and a restart empties
//! the burn set. This module refuses the copy before it is ever admitted: a
//! transaction whose Orchard nullifier the chain already holds can never be
//! mined again.
//!
//! The set is read on the hub's own clock, never per submission, so it tells an
//! indexer nothing about what arrives (the same rule REVIEW.md sets against
//! `testmempoolaccept`). Each indexer is read on its own, through
//! `GetBlockRangeNullifiers` filtered to Orchard, from NU6.3 activation (no
//! older proof verifies under the hub's key) to that indexer's tip, about a
//! minute of reads on mainnet, inside the 5-minute tip warm-up. Then every
//! [`SYNC_INTERVAL`] it re-reads from [`RESYNC_DEPTH`] blocks back, so a reorg
//! inside the tip tracker's allowance replaces what it saw. Each indexer runs
//! on its own task, and reads no higher than the tracked tip plus
//! [`RESYNC_DEPTH`].
//!
//! Admission does not wait for the first read. The check is on when at least
//! `min(2, configured endpoints)` views are current. A full view and a view
//! that is not current abstain. The hub logs which state it is in on each
//! change.
//!
//! **Unanimity.** A nullifier refuses admission only when at least
//! `min(2, configured endpoints)` views are current and every one of them
//! reports it. A full view and a view that is not current abstain. Fewer
//! current views, or no views, refuses nothing.
//! One indexer sees every transaction this hub publishes, so a single view
//! does not refuse when another endpoint is configured.

// A panic in a diskless enclave destroys the queue (see `queue.rs`).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use zebra_chain::parameters::constants::activation_heights;

use crate::batcher::TipTracker;
use crate::chain::ChainClient;
use crate::config::Network;

/// How often the set catches up with the tip. A block every 75 s, so a copy
/// of a just-mined transaction passes for at most about a block.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(30);

/// How many blocks below what it last read each sync reads again. The tip
/// tracker allows a 10-block reorg; this matches it. Also how far an indexer's
/// tip may run above the tracked tip, and how far a view may lag it and still
/// count as current.
pub const RESYNC_DEPTH: u32 = 10;

/// How often an indexer's task looks for the tracked tip before its first read.
const FIRST_TIP_POLL: Duration = Duration::from_secs(1);

/// The most blocks per `GetBlockRangeNullifiers` call. Measured 2026-10-28
/// against a public lightwalletd: 0.6 s and about 100 KB for 1 000 mainnet
/// blocks filtered to Orchard, well inside the chain client's 10 s budget and
/// 4 MiB body limit.
///
/// A failed call halves its indexer's chunk, down to one block (at most 2,440
/// actions, under 100 KB), and each read that succeeds doubles it again up to
/// this size.
pub const SYNC_CHUNK_BLOCKS: u32 = 1000;

/// Nullifiers one indexer's view may hold. Mainnet had revealed 251,421
/// Orchard nullifiers since NU6.3 on 2026-10-29 (about 2.35 a block, so about
/// 2,700 a day), read in 67 s from a public lightwalletd. The cap leaves about
/// four years at that rate, and bounds what a hostile indexer can make the
/// enclave hold: about 150 MB at the cap, 9 MB today.
///
/// A view that reaches it stops recording and abstains. The check refuses
/// only while at least `min(2, configured endpoints)` views are not full and
/// current. The hub logs a full view at error, once.
pub const MAX_NULLIFIERS_PER_INDEXER: usize = 1 << 22;

/// A nullifier as a view holds it: its first 16 bytes. Half the memory of the
/// full 32. A false match needs every indexer to report a chain nullifier that
/// shares 128 bits with the submitted one, which is about 2^-106 at the cap.
type Held = u128;

/// Views that must be voting before a nullifier can refuse admission.
fn voters_required(endpoints: usize) -> usize {
    endpoints.min(2)
}

fn held(nullifier: &[u8; 32]) -> Held {
    let mut prefix = [0; 16];
    prefix.copy_from_slice(&nullifier[..16]);
    Held::from_le_bytes(prefix)
}

/// The first height whose Orchard proofs verify under the hub's key.
pub fn sync_from(network: Network) -> u32 {
    match network {
        Network::Mainnet => activation_heights::mainnet::NU6_3.0,
        Network::Testnet => activation_heights::testnet::NU6_3.0,
        // A regtest chain is short, and its upgrades activate where its config
        // says, so it is read from the start.
        Network::Regtest => 1,
    }
}

/// One indexer's account of the chain's Orchard nullifiers.
struct View {
    /// The blocks a later read can replace: those inside [`RESYNC_DEPTH`] of
    /// `synced_to`. Older blocks live in `nullifiers` alone.
    by_height: BTreeMap<u32, Vec<Held>>,
    nullifiers: HashSet<Held>,
    /// The highest height read so far. `None` before the first read.
    synced_to: Option<u32>,
    /// Set when the view reached [`MAX_NULLIFIERS_PER_INDEXER`].
    full: bool,
    /// Whether this indexer's last read succeeded. A view whose indexer
    /// failed, or fell back, holds what it saw before and may hold a block a
    /// reorg has since taken back.
    fresh: bool,
    /// Blocks in this indexer's next read. See [`SYNC_CHUNK_BLOCKS`].
    chunk: u32,
}

impl Default for View {
    fn default() -> Self {
        Self {
            by_height: BTreeMap::new(),
            nullifiers: HashSet::new(),
            synced_to: None,
            full: false,
            fresh: false,
            chunk: SYNC_CHUNK_BLOCKS,
        }
    }
}

impl View {
    /// Current at `tip`: within [`RESYNC_DEPTH`] of it, last read
    /// successfully, and not full.
    fn is_current(&self, tip: u32) -> bool {
        self.fresh
            && !self.full
            && self
                .synced_to
                .is_some_and(|synced| synced >= tip.saturating_sub(RESYNC_DEPTH))
    }

    /// Replace everything held from `start` up with `blocks`, read for
    /// `start..=end`. Reads run upward, so anything above `end` is a block a
    /// reorg took back, or one the next read in this round brings back.
    ///
    /// Returns whether this read made the view full.
    fn replace(&mut self, start: u32, end: u32, blocks: Vec<(u32, Vec<[u8; 32]>)>) -> bool {
        let was_full = self.full;
        let stale: Vec<u32> = self.by_height.range(start..).map(|(h, _)| *h).collect();
        for height in stale {
            if let Some(old) = self.by_height.remove(&height) {
                for nullifier in old {
                    self.nullifiers.remove(&nullifier);
                }
            }
        }
        for (height, nullifiers) in blocks {
            if nullifiers.is_empty() {
                continue;
            }
            if self.full
                || self.nullifiers.len().saturating_add(nullifiers.len())
                    > MAX_NULLIFIERS_PER_INDEXER
            {
                self.full = true;
                continue;
            }
            let nullifiers: Vec<Held> = nullifiers.iter().map(held).collect();
            self.nullifiers.extend(nullifiers.iter().copied());
            self.by_height.insert(height, nullifiers);
        }
        self.synced_to = Some(end);
        self.fresh = true;
        // A later read starts no lower than `end - RESYNC_DEPTH + 1`.
        self.by_height = self
            .by_height
            .split_off(&end.saturating_sub(RESYNC_DEPTH).saturating_add(1));
        self.full && !was_full
    }
}

/// Whether the nullifier check is refusing copies, and why it is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NullifierCheck {
    On,
    /// Fewer than `min(2, configured endpoints)` views are current.
    Quorum,
}

impl NullifierCheck {
    fn reason(self) -> Option<&'static str> {
        match self {
            Self::On => None,
            Self::Quorum => Some("quorum"),
        }
    }
}

/// The Orchard nullifiers every indexer has seen revealed.
pub struct ChainNullifiers {
    /// One per endpoint, in [`ChainClient::endpoints`] order.
    views: Vec<Mutex<View>>,
}

impl ChainNullifiers {
    pub fn new(endpoints: usize) -> Self {
        Self {
            views: (0..endpoints)
                .map(|_| Mutex::new(View::default()))
                .collect(),
        }
    }

    fn view(&self, endpoint: usize) -> Option<MutexGuard<'_, View>> {
        self.views.get(endpoint).map(|view| match view.lock() {
            Ok(view) => view,
            Err(poison) => poison.into_inner(),
        })
    }

    /// Every view, locked in endpoint order, for one decision. The guards stay
    /// held together so a view cannot become full after it has been counted.
    fn locked(&self) -> Vec<MutexGuard<'_, View>> {
        self.views
            .iter()
            .map(|view| match view.lock() {
                Ok(view) => view,
                Err(poison) => poison.into_inner(),
            })
            .collect()
    }

    /// Whether enough current views report one of `nullifiers` as revealed.
    ///
    /// A view that is not current at `tip` abstains, and so does a full view.
    /// At least `min(2, configured endpoints)` views must be voting, and every
    /// voting view must contain the candidate. Fewer voting views, or no views,
    /// returns false.
    pub fn any_revealed(&self, nullifiers: &[[u8; 32]], tip: u32) -> bool {
        if self.views.is_empty() {
            return false;
        }
        let views = self.locked();
        let mut candidates: Vec<Held> = nullifiers.iter().map(held).collect();
        let mut voting = 0usize;
        for view in &views {
            if !view.is_current(tip) {
                continue;
            }
            voting += 1;
            candidates.retain(|nullifier| view.nullifiers.contains(nullifier));
            if candidates.is_empty() {
                return false;
            }
        }
        voting >= voters_required(views.len())
    }

    /// On, or `Quorum` when fewer than `min(2, configured endpoints)` views
    /// are current.
    fn nullifier_check(&self, tip: u32) -> NullifierCheck {
        if self.views.is_empty() {
            return NullifierCheck::Quorum;
        }
        if self.current(tip) >= voters_required(self.views.len()) {
            NullifierCheck::On
        } else {
            NullifierCheck::Quorum
        }
    }

    /// How many views are current at `tip`: within [`RESYNC_DEPTH`] of it,
    /// last read successfully, and not full.
    fn current(&self, tip: u32) -> usize {
        self.locked()
            .iter()
            .filter(|view| view.is_current(tip))
            .count()
    }

    /// Record what `endpoint` reported for `start..=end`, replacing whatever it
    /// reported there before. Returns whether this read made its view full.
    pub fn record(
        &self,
        endpoint: usize,
        start: u32,
        end: u32,
        blocks: Vec<(u32, Vec<[u8; 32]>)>,
    ) -> bool {
        self.view(endpoint)
            .is_some_and(|mut view| view.replace(start, end, blocks))
    }

    /// Whether `endpoint`'s tip fell more than [`RESYNC_DEPTH`] below what its
    /// view already read. Blocks that deep have left `by_height`, so no read
    /// can replace them, and their nullifiers may be a reorg's orphans. The
    /// view is then emptied, and the next round re-reads it from `from`;
    /// until that read is current it refuses nothing.
    fn retreated(&self, endpoint: usize, from: u32, tip: u32) -> bool {
        let synced = self.view(endpoint).and_then(|view| view.synced_to);
        let retreated = synced.is_some() && self.resume_from(endpoint, from) > tip;
        if retreated {
            if let Some(mut view) = self.view(endpoint) {
                let chunk = view.chunk;
                *view = View {
                    chunk,
                    ..View::default()
                };
            }
        }
        retreated
    }

    /// Mark `endpoint`'s view not current until its next successful read.
    fn mark_stale(&self, endpoint: usize) {
        if let Some(mut view) = self.view(endpoint) {
            view.fresh = false;
        }
    }

    fn chunk(&self, endpoint: usize) -> u32 {
        self.view(endpoint)
            .map_or(SYNC_CHUNK_BLOCKS, |view| view.chunk)
    }

    /// Halve `endpoint`'s chunk after a failed read, or double it after one
    /// that succeeded, within `1..=SYNC_CHUNK_BLOCKS`.
    fn adapt_chunk(&self, endpoint: usize, read_ok: bool) {
        if let Some(mut view) = self.view(endpoint) {
            view.chunk = if read_ok {
                view.chunk.saturating_mul(2).min(SYNC_CHUNK_BLOCKS)
            } else {
                (view.chunk / 2).max(1)
            };
        }
    }

    /// How many nullifiers each indexer's view holds, for the operator's log.
    pub fn held(&self) -> Vec<usize> {
        (0..self.views.len())
            .map(|endpoint| self.view(endpoint).map_or(0, |view| view.nullifiers.len()))
            .collect()
    }

    /// The height each indexer's view reaches, for the operator's log.
    pub fn synced_to(&self) -> Vec<Option<u32>> {
        (0..self.views.len())
            .map(|endpoint| self.view(endpoint).and_then(|view| view.synced_to))
            .collect()
    }

    /// Where `endpoint`'s next read starts.
    fn resume_from(&self, endpoint: usize, from: u32) -> u32 {
        self.view(endpoint)
            .and_then(|view| view.synced_to)
            .map_or(from, |synced| {
                synced
                    .saturating_sub(RESYNC_DEPTH)
                    .saturating_add(1)
                    .max(from)
            })
    }
}

/// Bring every indexer's view up to its own tip, capped at `max_tip`, all at
/// once. Returns how many views ended the round current. [`run`] reads each
/// indexer on its own task instead; this is one round of all of them.
pub async fn sync_once(
    spent: &ChainNullifiers,
    chain: &ChainClient,
    from: u32,
    max_tip: u32,
) -> usize {
    let rounds = chain
        .endpoints()
        .iter()
        .enumerate()
        .map(|(endpoint, addr)| sync_endpoint(spent, chain, endpoint, *addr, from, max_tip));
    join_all(rounds).await.into_iter().filter(|ok| *ok).count()
}

/// Bring one indexer's view up to its tip, capped at `max_tip`.
///
/// An indexer that fails keeps what it had and is retried next round. Logged at
/// warn, by endpoint and reason, never by anything a submission carried.
async fn sync_endpoint(
    spent: &ChainNullifiers,
    chain: &ChainClient,
    endpoint: usize,
    addr: SocketAddr,
    from: u32,
    max_tip: u32,
) -> bool {
    let tip = match chain.tip_height_of(addr).await {
        Ok(tip) => tip.min(max_tip),
        Err(err) => {
            spent.mark_stale(endpoint);
            tracing::warn!(%addr, reason = %err, "nullifier sync: no tip from this indexer");
            return false;
        }
    };
    if spent.retreated(endpoint, from, tip) {
        tracing::warn!(
            %addr,
            tip,
            "nullifier sync: this indexer's tip fell past the reorg window; its view is \
             emptied and re-read from the start"
        );
        return false;
    }
    let mut start = spent.resume_from(endpoint, from);
    while start <= tip {
        let end = start
            .saturating_add(spent.chunk(endpoint).saturating_sub(1))
            .min(tip);
        match chain.orchard_nullifiers(addr, start, end).await {
            Ok(blocks) => {
                if spent.record(endpoint, start, end, blocks) {
                    tracing::error!(
                        %addr,
                        cap = MAX_NULLIFIERS_PER_INDEXER,
                        height = end,
                        "nullifier set full for this indexer; this view abstains"
                    );
                }
                spent.adapt_chunk(endpoint, true);
            }
            Err(err) => {
                // Retried from the same start next round, with half the blocks.
                spent.mark_stale(endpoint);
                spent.adapt_chunk(endpoint, false);
                tracing::warn!(
                    %addr,
                    start,
                    end,
                    next_chunk = spent.chunk(endpoint),
                    reason = %err,
                    "nullifier sync: read failed"
                );
                return false;
            }
        }
        start = end.saturating_add(1);
    }
    true
}

/// Keep the set current for the life of the process: one task per indexer,
/// and this one logging whether the check is on, at each change.
pub async fn run(
    spent: Arc<ChainNullifiers>,
    chain: Arc<ChainClient>,
    tip: Arc<TipTracker>,
    network: Network,
) {
    let from = sync_from(network);
    for (endpoint, addr) in chain.endpoints().iter().copied().enumerate() {
        tokio::spawn(sync_forever(
            spent.clone(),
            chain.clone(),
            tip.clone(),
            endpoint,
            addr,
            from,
        ));
    }

    let started = Instant::now();
    let mut state: Option<NullifierCheck> = None;
    loop {
        let observed = tip.observed_height();
        if observed > 0 {
            let current = spent.current(observed);
            let now = spent.nullifier_check(observed);
            if state != Some(now) {
                state = Some(now);
                if now == NullifierCheck::On {
                    tracing::info!(
                        synced_to = ?spent.synced_to(),
                        held = ?spent.held(),
                        elapsed_s = started.elapsed().as_secs(),
                        "nullifier set synced; copies of mined Orchard transactions are refused"
                    );
                } else if let Some(reason) = now.reason() {
                    tracing::warn!(
                        current,
                        indexers = spent.views.len(),
                        tip = observed,
                        reason,
                        synced_to = ?spent.synced_to(),
                        held = ?spent.held(),
                        "nullifier check is off; copies of mined Orchard transactions pass"
                    );
                }
            }
        }
        tokio::time::sleep(SYNC_INTERVAL).await;
    }
}

/// One indexer's reads, for the life of the process. Waits for the tip
/// tracker's first answer, which caps each read.
async fn sync_forever(
    spent: Arc<ChainNullifiers>,
    chain: Arc<ChainClient>,
    tip: Arc<TipTracker>,
    endpoint: usize,
    addr: SocketAddr,
    from: u32,
) {
    loop {
        let observed = tip.observed_height();
        if observed == 0 {
            tokio::time::sleep(FIRST_TIP_POLL).await;
            continue;
        }
        let max_tip = observed.saturating_add(RESYNC_DEPTH);
        sync_endpoint(&spent, &chain, endpoint, addr, from, max_tip).await;
        tokio::time::sleep(SYNC_INTERVAL).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn nf(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    #[test]
    fn a_nullifier_counts_only_when_every_indexer_reports_it() {
        let set = ChainNullifiers::new(2);
        set.record(0, 100, 101, vec![(100, vec![nf(1), nf(2)])]);
        assert!(
            !set.any_revealed(&[nf(1)], 101),
            "one indexer alone decides nothing"
        );

        set.record(1, 100, 101, vec![(101, vec![nf(1)])]);
        assert!(
            set.any_revealed(&[nf(9), nf(1)], 101),
            "any revealed nullifier is enough"
        );
        assert!(
            !set.any_revealed(&[nf(2)], 101),
            "the second indexer never reported it"
        );
    }

    #[test]
    fn no_indexers_reveal_nothing() {
        assert!(!ChainNullifiers::new(0).any_revealed(&[nf(1)], 100));
    }

    #[test]
    fn a_re_read_replaces_what_a_reorg_took_back() {
        let set = ChainNullifiers::new(1);
        set.record(0, 100, 110, vec![(105, vec![nf(1)]), (110, vec![nf(2)])]);
        assert!(set.any_revealed(&[nf(2)], 110));

        // Blocks 101.. re-read after a reorg: 110 now holds a different
        // transaction, and 105 stays as it was.
        set.record(0, 101, 111, vec![(105, vec![nf(1)]), (111, vec![nf(3)])]);
        assert!(set.any_revealed(&[nf(1)], 111));
        assert!(
            !set.any_revealed(&[nf(2)], 111),
            "the reorged-out nullifier is gone"
        );
        assert!(set.any_revealed(&[nf(3)], 111));
        assert_eq!(set.synced_to(), vec![Some(111)]);
    }

    #[test]
    fn a_view_that_is_not_current_refuses_nothing() {
        let set = ChainNullifiers::new(2);
        set.record(0, 100, 110, vec![(105, vec![nf(1)])]);
        set.record(1, 100, 110, vec![(105, vec![nf(1)])]);
        assert!(set.any_revealed(&[nf(1)], 110));

        // Behind the hub's tip by more than the reorg window.
        assert!(!set.any_revealed(&[nf(1)], 110 + RESYNC_DEPTH + 1));

        // Its last read failed: it may hold a block a reorg took back.
        set.mark_stale(1);
        assert!(!set.any_revealed(&[nf(1)], 110));
        set.record(1, 101, 110, vec![(105, vec![nf(1)])]);
        assert!(set.any_revealed(&[nf(1)], 110));
    }

    #[test]
    fn a_tip_that_falls_past_the_reorg_window_makes_the_view_stale() {
        let set = ChainNullifiers::new(1);
        set.record(0, 900, 1000, vec![(950, vec![nf(1)])]);
        assert!(set.any_revealed(&[nf(1)], 1000));

        // Inside the window a read replaces the blocks the indexer lost.
        assert!(!set.retreated(0, 900, 1000 - RESYNC_DEPTH + 1));
        assert!(set.any_revealed(&[nf(1)], 1000));

        // Past it, nothing can: the view starts over from `from`, and
        // refuses nothing until that read is current.
        assert!(set.retreated(0, 900, 900));
        assert!(!set.any_revealed(&[nf(1)], 900));
        assert_eq!(set.synced_to(), vec![None]);
        assert_eq!(set.held(), vec![0]);
        assert_eq!(set.resume_from(0, 900), 900);
    }

    #[test]
    fn a_resumed_sync_re_reads_the_reorg_window() {
        let set = ChainNullifiers::new(1);
        assert_eq!(
            set.resume_from(0, 1000),
            1000,
            "a fresh view starts at NU6.3"
        );
        set.record(0, 1000, 1500, Vec::new());
        assert_eq!(set.resume_from(0, 1000), 1500 - RESYNC_DEPTH + 1);
    }

    #[test]
    fn a_full_view_stops_recording() {
        let set = ChainNullifiers::new(1);
        let flood: Vec<[u8; 32]> = (0..=MAX_NULLIFIERS_PER_INDEXER as u32)
            .map(|i| {
                let mut nullifier = [0; 32];
                nullifier[..4].copy_from_slice(&i.to_le_bytes());
                nullifier
            })
            .collect();
        assert!(set.record(0, 1, 1, vec![(1, flood)]), "reported once");
        assert!(
            !set.record(0, 2, 2, vec![(2, vec![nf(1)])]),
            "and only once"
        );
        let view = set.view(0).unwrap();
        assert!(view.full);
        assert!(
            view.nullifiers.is_empty(),
            "the oversized block is refused whole"
        );
    }

    #[test]
    fn only_the_reorg_window_is_kept_by_height() {
        let set = ChainNullifiers::new(1);
        set.record(0, 1, 1000, vec![(500, vec![nf(1)]), (995, vec![nf(2)])]);
        let view = set.view(0).unwrap();
        assert_eq!(
            view.by_height.keys().copied().collect::<Vec<_>>(),
            vec![995]
        );
        assert_eq!(view.nullifiers.len(), 2, "older blocks stay in the set");
    }

    #[test]
    fn a_failed_read_halves_the_chunk_and_a_good_one_doubles_it() {
        let set = ChainNullifiers::new(1);
        for _ in 0..20 {
            set.adapt_chunk(0, false);
        }
        assert_eq!(set.chunk(0), 1, "never below one block");
        set.adapt_chunk(0, true);
        assert_eq!(set.chunk(0), 2);
        for _ in 0..20 {
            set.adapt_chunk(0, true);
        }
        assert_eq!(set.chunk(0), SYNC_CHUNK_BLOCKS);
    }

    #[test]
    fn a_view_is_current_within_the_reorg_window_and_not_full() {
        let set = ChainNullifiers::new(2);
        set.record(0, 1, 100, Vec::new());
        set.record(1, 1, 80, Vec::new());
        assert_eq!(set.current(100 + RESYNC_DEPTH), 1);
        set.view(0).unwrap().full = true;
        assert_eq!(set.current(100), 0);
    }

    #[test]
    fn a_full_view_abstains() {
        let set = ChainNullifiers::new(3);
        set.record(0, 100, 101, vec![(101, vec![nf(1)])]);
        set.record(1, 100, 101, vec![(101, vec![nf(1)])]);
        set.record(2, 100, 101, vec![(101, vec![nf(1)])]);
        set.view(0).unwrap().full = true;
        assert!(
            set.any_revealed(&[nf(1)], 101),
            "one full view plus two current views that hold the candidate"
        );

        let one_voter = ChainNullifiers::new(2);
        one_voter.record(0, 100, 101, vec![(101, vec![nf(1)])]);
        one_voter.record(1, 100, 101, vec![(101, vec![nf(1)])]);
        one_voter.view(0).unwrap().full = true;
        assert!(!one_voter.any_revealed(&[nf(1)], 101));

        let missing = ChainNullifiers::new(3);
        missing.record(0, 100, 101, vec![(101, vec![nf(1)])]);
        missing.record(1, 100, 101, vec![(101, vec![nf(1)])]);
        missing.record(2, 100, 101, vec![(101, vec![nf(2)])]);
        missing.view(0).unwrap().full = true;
        assert!(!missing.any_revealed(&[nf(1)], 101));

        let stale = ChainNullifiers::new(3);
        stale.record(0, 100, 101, vec![(101, vec![nf(1)])]);
        stale.record(1, 100, 101, vec![(101, vec![nf(1)])]);
        stale.record(2, 100, 101, vec![(101, vec![nf(1)])]);
        stale.view(0).unwrap().full = true;
        stale.mark_stale(1);
        assert!(!stale.any_revealed(&[nf(1)], 101));

        let one_stale = ChainNullifiers::new(3);
        one_stale.record(0, 100, 101, vec![(101, vec![nf(1)])]);
        one_stale.record(1, 100, 101, vec![(101, vec![nf(1)])]);
        one_stale.record(2, 100, 101, vec![(101, vec![nf(1)])]);
        one_stale.mark_stale(0);
        assert!(
            one_stale.any_revealed(&[nf(1)], 101),
            "one stale view plus two current views that hold the candidate"
        );
        assert_eq!(one_stale.nullifier_check(101), NullifierCheck::On);

        let stale_and_missing = ChainNullifiers::new(3);
        stale_and_missing.record(0, 100, 101, vec![(101, vec![nf(1)])]);
        stale_and_missing.record(1, 100, 101, vec![(101, vec![nf(1)])]);
        stale_and_missing.record(2, 100, 101, vec![(101, vec![nf(2)])]);
        stale_and_missing.mark_stale(0);
        assert!(!stale_and_missing.any_revealed(&[nf(1)], 101));

        let lone = ChainNullifiers::new(1);
        lone.record(0, 100, 101, vec![(101, vec![nf(1)])]);
        lone.view(0).unwrap().full = true;
        assert!(!lone.any_revealed(&[nf(1)], 101));

        let only = ChainNullifiers::new(1);
        only.record(0, 100, 101, vec![(101, vec![nf(1)])]);
        assert!(only.any_revealed(&[nf(1)], 101));

        let two_full = ChainNullifiers::new(3);
        two_full.record(0, 100, 101, vec![(101, vec![nf(1)])]);
        two_full.record(1, 100, 101, vec![(101, vec![nf(1)])]);
        two_full.record(2, 100, 101, vec![(101, vec![nf(1)])]);
        two_full.view(0).unwrap().full = true;
        two_full.view(1).unwrap().full = true;
        assert!(!two_full.any_revealed(&[nf(1)], 101));
    }

    #[test]
    fn nullifier_check_follows_the_quorum() {
        let unread = ChainNullifiers::new(3);
        assert_eq!(unread.nullifier_check(101), NullifierCheck::Quorum);

        let set = ChainNullifiers::new(3);
        set.record(0, 100, 101, vec![]);
        set.record(1, 100, 101, vec![]);
        set.record(2, 100, 101, vec![]);
        assert_eq!(set.nullifier_check(101), NullifierCheck::On);

        set.view(0).unwrap().full = true;
        set.view(1).unwrap().full = true;
        assert_eq!(set.nullifier_check(101), NullifierCheck::Quorum);

        let stale = ChainNullifiers::new(2);
        stale.record(0, 100, 101, vec![]);
        stale.record(1, 100, 101, vec![]);
        stale.mark_stale(1);
        assert_eq!(stale.nullifier_check(101), NullifierCheck::Quorum);

        let only = ChainNullifiers::new(1);
        only.record(0, 100, 101, vec![]);
        assert_eq!(only.nullifier_check(101), NullifierCheck::On);

        assert_eq!(
            ChainNullifiers::new(0).nullifier_check(101),
            NullifierCheck::Quorum
        );
    }

    #[test]
    fn mainnet_reads_from_nu6_3() {
        assert_eq!(sync_from(Network::Mainnet), 3_428_143);
    }
}
