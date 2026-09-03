//! [zero] @claude An in-memory [`BlockCache`].
//!
//! `zcash_client_backend` ships no `BlockCache` implementation; the trait documentation
//! carries an example and every consumer writes its own. In-memory is the right shape for
//! a browser regardless: compact blocks are scanned and then discarded, so there is
//! nothing worth persisting and nothing to reconcile with the wallet database on restart.
//!
//! The trait requires `Send + Sync`, which rules out a cache holding JS values directly.

use std::{
    collections::BTreeMap,
    convert::Infallible,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use zcash_client_backend::{
    data_api::{
        chain::{error, BlockCache, BlockSource},
        scanning::ScanRange,
    },
    proto::compact_formats::CompactBlock,
};
use zcash_protocol::consensus::BlockHeight;

/// A [`BlockCache`] holding compact blocks in memory. Cloning shares the storage.
///
/// Keyed by height rather than held in a `Vec`. That is not tidiness: `scan_cached_blocks`
/// calls `with_blocks` repeatedly as it works through a range, and a `Vec` implementation
/// has to clone and re-sort the whole cache on each call to guarantee ascending order —
/// quadratic in the number of cached blocks, over data that includes full transaction
/// bodies. A hundred blocks of mainnet took minutes that way. A `BTreeMap` is sorted by
/// construction, so a call clones only the blocks it actually yields.
#[derive(Clone, Default)]
pub struct MemoryBlockCache {
    blocks: Arc<Mutex<BTreeMap<u64, CompactBlock>>>,
}

impl MemoryBlockCache {
    /// Creates an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops every cached block.
    ///
    /// Compact blocks are the bulk of a sync's memory use and are worthless once scanned,
    /// so a browser wallet wants this after each pass rather than at the end of the day.
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Blocks from `start` upwards, stopping at the first gap.
    ///
    /// Contiguity is a contract, not a nicety. `BlockCache::read` is documented to return
    /// blocks that "are contiguous and start from `range.block_range().start`", and
    /// `scan_cached_blocks` relies on it: handed a set that begins past the height it asked
    /// for, or that skips a height, it cannot advance the scanned range — and a caller that
    /// loops until the range is scanned then loops forever, burning CPU re-scanning. An
    /// implementation that merely filters by range looks correct and produces exactly that.
    fn contiguous_from(&self, start: u64, limit: Option<usize>) -> Vec<CompactBlock> {
        let blocks = self.lock();
        let mut out = Vec::new();
        let mut expected = start;
        for (height, block) in blocks.range(start..) {
            if *height != expected {
                break;
            }
            out.push(block.clone());
            expected += 1;
            if limit.is_some_and(|n| out.len() >= n) {
                break;
            }
        }
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, CompactBlock>> {
        // Poisoning means another thread panicked holding the lock. wasm is
        // single-threaded and this cache is not shared across workers, so it cannot
        // happen, and recovering would hide a real bug.
        self.blocks
            .lock()
            .expect("the cache lock is never poisoned")
    }
}

impl BlockSource for MemoryBlockCache {
    type Error = Infallible;

    fn with_blocks<F, WalletErrT>(
        &self,
        from_height: Option<BlockHeight>,
        limit: Option<usize>,
        mut with_block: F,
    ) -> Result<(), error::Error<WalletErrT, Self::Error>>
    where
        F: FnMut(CompactBlock) -> Result<(), error::Error<WalletErrT, Self::Error>>,
    {
        let from = from_height.map_or(0, u64::from);
        // Cloned out of the lock before the callback runs: `with_block` is free to call
        // back into the wallet, and holding the cache lock across it would be a deadlock
        // waiting for a reason.
        let blocks = self.contiguous_from(from, limit);

        for block in blocks {
            with_block(block)?;
        }
        Ok(())
    }
}

#[async_trait]
impl BlockCache for MemoryBlockCache {
    fn get_tip_height(
        &self,
        range: Option<&ScanRange>,
    ) -> Result<Option<BlockHeight>, Self::Error> {
        Ok(match range {
            Some(range) => {
                let r = range.block_range();
                self.lock()
                    .range(u64::from(r.start)..u64::from(r.end))
                    .next_back()
                    .map(|(height, _)| BlockHeight::from_u32(*height as u32))
            }
            None => self
                .lock()
                .last_key_value()
                .map(|(height, _)| BlockHeight::from_u32(*height as u32)),
        })
    }

    async fn read(&self, range: &ScanRange) -> Result<Vec<CompactBlock>, Self::Error> {
        let r = range.block_range();
        let start = u64::from(r.start);
        let limit = usize::try_from(u64::from(r.end).saturating_sub(start)).unwrap_or(usize::MAX);
        Ok(self.contiguous_from(start, Some(limit)))
    }

    async fn insert(&self, compact_blocks: Vec<CompactBlock>) -> Result<(), Self::Error> {
        let mut blocks = self.lock();
        for block in compact_blocks {
            blocks.insert(block.height, block);
        }
        Ok(())
    }

    async fn delete(&self, range: ScanRange) -> Result<(), Self::Error> {
        let r = range.block_range();
        let mut blocks = self.lock();
        let doomed: Vec<u64> = blocks
            .range(u64::from(r.start)..u64::from(r.end))
            .map(|(height, _)| *height)
            .collect();
        for height in doomed {
            blocks.remove(&height);
        }
        Ok(())
    }
}
