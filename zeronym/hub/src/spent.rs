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
//! inside the tip tracker's allowance replaces what it saw.
//!
//! **Unanimity.** A nullifier refuses admission only when EVERY configured
//! indexer reports it. An indexer sees every transaction the hub publishes, so
//! one that lies could otherwise invent a published migration's nullifier and
//! block the wallet's resend. The price is fail-open: an indexer that is down
//! or behind lets copies through, which is the burn set's behaviour before this
//! module, and every honest migration stays admissible.

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
/// tracker allows a 10-block reorg; this matches it.
pub const RESYNC_DEPTH: u32 = 10;

/// Blocks per `GetBlockRangeNullifiers` call. Measured 2026-10-28 against a
/// public lightwalletd: 0.6 s and about 100 KB for 1 000 mainnet blocks
/// filtered to Orchard, well inside the chain client's 10 s budget and 4 MiB
/// body limit even from a server that ignores the filter.
pub const SYNC_CHUNK_BLOCKS: u32 = 1000;

/// Nullifiers one indexer's view may hold. Mainnet had revealed 251,421
/// Orchard nullifiers since NU6.3 on 2026-10-29, read in 67 s from a public
/// lightwalletd, and adds a few hundred a day, so this is four times that: room
/// for years, and a bound on what a hostile indexer can make the enclave hold
/// (about 100 MB at the cap, 30 MB today). A view that reaches it stops
/// recording, and with it the unanimity, so admission falls back to the burn
/// set alone.
pub const MAX_NULLIFIERS_PER_INDEXER: usize = 1 << 20;

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
#[derive(Default)]
struct View {
    by_height: BTreeMap<u32, Vec<[u8; 32]>>,
    nullifiers: HashSet<[u8; 32]>,
    /// The highest height read so far. `None` before the first read.
    synced_to: Option<u32>,
    /// Set when the view reached [`MAX_NULLIFIERS_PER_INDEXER`].
    full: bool,
}

impl View {
    /// Replace everything held from `start` up with `blocks`, read for
    /// `start..=end`. Reads run upward, so anything above `end` is a block a
    /// reorg took back, or one the next read in this round brings back.
    fn replace(&mut self, start: u32, end: u32, blocks: Vec<(u32, Vec<[u8; 32]>)>) {
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
            self.nullifiers.extend(nullifiers.iter().copied());
            self.by_height.insert(height, nullifiers);
        }
        self.synced_to = Some(end);
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

    /// Whether every indexer reports one of `nullifiers` as revealed.
    pub fn any_revealed(&self, nullifiers: &[[u8; 32]]) -> bool {
        if self.views.is_empty() {
            return false;
        }
        nullifiers.iter().any(|nullifier| {
            (0..self.views.len()).all(|endpoint| {
                self.view(endpoint)
                    .is_some_and(|view| view.nullifiers.contains(nullifier))
            })
        })
    }

    /// Record what `endpoint` reported for `start..=end`, replacing whatever it
    /// reported there before.
    pub fn record(&self, endpoint: usize, start: u32, end: u32, blocks: Vec<(u32, Vec<[u8; 32]>)>) {
        if let Some(mut view) = self.view(endpoint) {
            view.replace(start, end, blocks);
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

/// Bring every indexer's view up to its own tip, concurrently. Returns how many
/// views ended the round current.
///
/// An indexer that fails keeps what it had and is retried next round. Logged at
/// warn, by endpoint and reason, never by anything a submission carried.
pub async fn sync_once(spent: &ChainNullifiers, chain: &ChainClient, from: u32) -> usize {
    let rounds = chain
        .endpoints()
        .iter()
        .enumerate()
        .map(|(endpoint, addr)| sync_endpoint(spent, chain, endpoint, *addr, from));
    join_all(rounds).await.into_iter().filter(|ok| *ok).count()
}

async fn sync_endpoint(
    spent: &ChainNullifiers,
    chain: &ChainClient,
    endpoint: usize,
    addr: SocketAddr,
    from: u32,
) -> bool {
    let tip = match chain.tip_height_of(addr).await {
        Ok(tip) => tip,
        Err(err) => {
            tracing::warn!(%addr, reason = %err, "nullifier sync: no tip from this indexer");
            return false;
        }
    };
    let mut start = spent.resume_from(endpoint, from);
    while start <= tip {
        let end = start.saturating_add(SYNC_CHUNK_BLOCKS - 1).min(tip);
        match chain.orchard_nullifiers(addr, start, end).await {
            Ok(blocks) => spent.record(endpoint, start, end, blocks),
            Err(err) => {
                tracing::warn!(%addr, start, end, reason = %err, "nullifier sync: read failed");
                return false;
            }
        }
        start = end.saturating_add(1);
    }
    true
}

/// Keep the set current for the life of the process.
///
/// Waits for the tip tracker's first answer, because the indexers' tips are
/// what bound each read and a hub that knows no height admits nothing yet.
pub async fn run(
    spent: Arc<ChainNullifiers>,
    chain: Arc<ChainClient>,
    tip: Arc<TipTracker>,
    network: Network,
) {
    let from = sync_from(network);
    let mut caught_up = false;
    loop {
        if tip.observed_height() > 0 {
            let started = Instant::now();
            let current = sync_once(&spent, &chain, from).await;
            if !caught_up && current == chain.endpoints().len() {
                caught_up = true;
                tracing::info!(
                    from,
                    synced_to = ?spent.synced_to(),
                    held = ?spent.held(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "nullifier set synced; copies of mined Orchard transactions are refused"
                );
            }
        }
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
            !set.any_revealed(&[nf(1)]),
            "one indexer alone decides nothing"
        );

        set.record(1, 100, 101, vec![(101, vec![nf(1)])]);
        assert!(
            set.any_revealed(&[nf(9), nf(1)]),
            "any revealed nullifier is enough"
        );
        assert!(
            !set.any_revealed(&[nf(2)]),
            "the second indexer never reported it"
        );
    }

    #[test]
    fn no_indexers_reveal_nothing() {
        assert!(!ChainNullifiers::new(0).any_revealed(&[nf(1)]));
    }

    #[test]
    fn a_re_read_replaces_what_a_reorg_took_back() {
        let set = ChainNullifiers::new(1);
        set.record(0, 100, 110, vec![(105, vec![nf(1)]), (110, vec![nf(2)])]);
        assert!(set.any_revealed(&[nf(2)]));

        // Blocks 101.. re-read after a reorg: 110 now holds a different
        // transaction, and 105 stays as it was.
        set.record(0, 101, 111, vec![(105, vec![nf(1)]), (111, vec![nf(3)])]);
        assert!(set.any_revealed(&[nf(1)]));
        assert!(
            !set.any_revealed(&[nf(2)]),
            "the reorged-out nullifier is gone"
        );
        assert!(set.any_revealed(&[nf(3)]));
        assert_eq!(set.synced_to(), vec![Some(111)]);
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
        set.record(0, 1, 1, vec![(1, flood)]);
        let view = set.view(0).unwrap();
        assert!(view.full);
        assert!(
            view.nullifiers.is_empty(),
            "the oversized block is refused whole"
        );
    }

    #[test]
    fn mainnet_reads_from_nu6_3() {
        assert_eq!(sync_from(Network::Mainnet), 3_428_143);
    }
}
