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
        let from = from_height.map_or(0, |h| u64::from(h));
        // Cloned out of the lock before the callback runs: `with_block` is free to call
        // back into the wallet, and holding the cache lock across it would be a deadlock
        // waiting for a reason.
        let blocks: Vec<CompactBlock> = self
            .lock()
            .range(from..)
            .take(limit.unwrap_or(usize::MAX))
            .map(|(_, block)| block.clone())
            .collect();

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
        Ok(self
            .lock()
            .range(u64::from(r.start)..u64::from(r.end))
            .map(|(_, block)| block.clone())
            .collect())
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
