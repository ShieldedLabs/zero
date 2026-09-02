//! [zero] @claude An in-memory [`BlockCache`].
//!
//! `zcash_client_backend` ships no `BlockCache` implementation — the trait documentation
//! carries an example and every consumer writes its own. This one exists so the wasm
//! harness can instantiate `sync::run`, and because in-memory is the right shape for a
//! browser: compact blocks are scanned and discarded, so there is nothing worth persisting
//! and nothing to reconcile with the wallet database on restart.
//!
//! The trait requires `Send + Sync`, which rules out a cache holding JS values directly.
//! That is not a limitation in practice — a `Vec` behind a `Mutex` satisfies it, and any
//! IndexedDB-backed cache would need a `Send` shim regardless.

use std::{
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

/// A [`BlockCache`] holding compact blocks in memory.
///
/// Cloning shares the underlying storage.
#[derive(Clone, Default)]
pub struct MemoryBlockCache {
    blocks: Arc<Mutex<Vec<CompactBlock>>>,
}

impl MemoryBlockCache {
    /// Creates an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of blocks currently held.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Returns whether the cache holds no blocks.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<CompactBlock>> {
        // A poisoned lock means another thread panicked while holding it. There is no
        // other thread here — wasm is single-threaded and this cache is not shared across
        // workers — so this cannot happen, and recovering would hide a real bug.
        self.blocks
            .lock()
            .expect("the cache lock is never poisoned")
    }
}

fn height_of(block: &CompactBlock) -> BlockHeight {
    BlockHeight::from_u32(block.height as u32)
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
        // Blocks are inserted in whatever order the caller supplies them, so sort rather
        // than assume; `scan_cached_blocks` requires them in ascending height order.
        let mut blocks = self.lock().clone();
        blocks.sort_unstable_by_key(height_of);

        let selected = blocks
            .into_iter()
            .filter(|block| from_height.is_none_or(|from| height_of(block) >= from))
            .take(limit.unwrap_or(usize::MAX));

        for block in selected {
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
        Ok(self
            .lock()
            .iter()
            .map(height_of)
            .filter(|height| range.is_none_or(|r| r.block_range().contains(height)))
            .max())
    }

    async fn read(&self, range: &ScanRange) -> Result<Vec<CompactBlock>, Self::Error> {
        let mut blocks: Vec<CompactBlock> = self
            .lock()
            .iter()
            .filter(|block| range.block_range().contains(&height_of(block)))
            .cloned()
            .collect();
        // The contract is that returned blocks are contiguous from the start of the range,
        // which callers rely on; ordering is this cache's responsibility, not theirs.
        blocks.sort_unstable_by_key(height_of);
        Ok(blocks)
    }

    async fn insert(&self, mut compact_blocks: Vec<CompactBlock>) -> Result<(), Self::Error> {
        self.lock().append(&mut compact_blocks);
        Ok(())
    }

    async fn delete(&self, range: ScanRange) -> Result<(), Self::Error> {
        self.lock()
            .retain(|block| !range.block_range().contains(&height_of(block)));
        Ok(())
    }
}
