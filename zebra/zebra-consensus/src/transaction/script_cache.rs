//! Memoization of successful transparent script verification.
//!
//! Zebra verifies a transaction's transparent input scripts at mempool
//! admission, then again when the transaction arrives in a block; zcashd
//! skips the repeat through its script and signature caches. This module
//! remembers which transactions' input scripts verified, so the repeat
//! becomes a lookup. A hit skips only the per-input script checks;
//! everything else still runs for every block the transaction appears in.
//!
//! The key is the transaction's [`WtxId`], which commits to everything
//! script verification reads: the ZIP-244 txid covers the effecting data,
//! including the consensus branch id and the outpoints naming the immutable
//! prevout scripts and values, and the authorizing-data digest covers the
//! scriptSigs, so a same-txid twin with different signatures
//! (CVE-2026-34377) never hits. Pre-v5 transactions have no id committing
//! to the branch id and are never cached. Only successful script
//! verifications are inserted (an entry whose transaction later fails a
//! proof or contextual check is harmless), so a missing or evicted entry
//! only ever costs a re-verification.
//!
//! Replacement is random: a new key evicts the slot chosen by a keyed
//! siphash of that key, so an adversary who cannot guess the seed cannot
//! target entries, and no access pattern degrades the cache.

use std::{
    collections::HashSet,
    sync::{Mutex, MutexGuard},
};

use once_cell::sync::Lazy;
use rand::Rng;
use siphasher::sip::SipHasher13;

use zebra_chain::transaction::WtxId;

/// The maximum number of remembered transactions: several blocks of history
/// plus a full mempool's worth of churn, a few MiB when full.
const SCRIPT_CACHE_CAPACITY: usize = 30_000;

/// The process-wide cache, shared by the mempool and block verifiers so a
/// mempool admission can answer the block verification that follows it.
static VERIFIED_SCRIPTS: Lazy<VerifiedScripts> = Lazy::new(|| {
    #[cfg(test)]
    let seed: [u8; 16] = [7; 16];
    #[cfg(not(test))]
    let seed: [u8; 16] = rand::thread_rng().gen();

    VerifiedScripts::new(SCRIPT_CACHE_CAPACITY, seed)
});

/// Returns the process-wide transparent script verification cache.
pub(super) fn verified_scripts() -> &'static VerifiedScripts {
    &VERIFIED_SCRIPTS
}

/// The set of remembered script verifications, bounded by random replacement.
pub(super) struct VerifiedScripts {
    /// Chooses the victim slot in [`VerifiedScripts::insert`]; keyed so an
    /// adversary cannot predict which entry a given key evicts.
    siphasher: SipHasher13,
    capacity: usize,
    inner: Mutex<Inner>,
    /// Test-only: hits per key, so a test can observe that a verification was
    /// answered from the cache rather than re-run.
    #[cfg(test)]
    hits_by_key: Mutex<std::collections::HashMap<WtxId, u64>>,
}

/// The collections behind the mutex. `keys` and `slots` always hold the same
/// set of ids: every mutation updates both.
struct Inner {
    /// Answers [`VerifiedScripts::contains`].
    keys: HashSet<WtxId>,
    /// One slot per cached id; a full cache replaces a siphash-chosen slot.
    slots: Vec<WtxId>,
}

impl VerifiedScripts {
    /// Creates an empty cache holding at most `capacity` keys, with `seed`
    /// keying victim selection. Panics if `capacity` is zero.
    pub(super) fn new(capacity: usize, seed: [u8; 16]) -> Self {
        assert!(capacity > 0, "cache capacity must be greater than zero");

        Self {
            siphasher: SipHasher13::new_with_key(&seed),
            capacity,
            inner: Mutex::new(Inner {
                keys: HashSet::with_capacity(capacity),
                slots: Vec::with_capacity(capacity),
            }),
            #[cfg(test)]
            hits_by_key: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .expect("no code can panic while holding the script cache lock")
    }

    /// Returns whether `key` was recorded by [`VerifiedScripts::insert`].
    pub(super) fn contains(&self, key: &WtxId) -> bool {
        let hit = {
            let inner = self.lock();
            debug_assert!(inner.holds_invariants());
            inner.keys.contains(key)
        };

        #[cfg(test)]
        if hit {
            *self
                .hits_by_key
                .lock()
                .expect("no code can panic while holding the hit counter lock")
                .entry(*key)
                .or_default() += 1;
        }

        // Metrics are reported after the lock is released: every non-coinbase
        // v5+ transaction the node verifies takes this lock.
        metrics::counter!(
            "zebra.consensus.transaction.script_cache.lookups",
            "outcome" => if hit { "hit" } else { "miss" },
        )
        .increment(1);

        hit
    }

    /// Records that `key`'s transaction passed script verification. Only
    /// successful verifications may be recorded.
    pub(super) fn insert(&self, key: WtxId) {
        let (inserted, evicted, size) = {
            let mut inner = self.lock();
            debug_assert!(inner.holds_invariants());

            if !inner.keys.insert(key) {
                // Concurrent verifications of one transaction can both miss;
                // the second insert must not occupy a second slot.
                (false, false, inner.keys.len())
            } else if inner.slots.len() < self.capacity {
                inner.slots.push(key);
                (true, false, inner.keys.len())
            } else {
                let victim_index = self.victim_index(&key);
                let victim = std::mem::replace(&mut inner.slots[victim_index], key);
                inner.keys.remove(&victim);
                (true, true, inner.keys.len())
            }
        };

        if inserted {
            metrics::counter!("zebra.consensus.transaction.script_cache.inserts").increment(1);
            if evicted {
                metrics::counter!("zebra.consensus.transaction.script_cache.evictions")
                    .increment(1);
            }
            metrics::gauge!("zebra.consensus.transaction.script_cache.entries").set(size as f64);
        }
    }

    /// The slot a full cache replaces when inserting `key`.
    fn victim_index(&self, key: &WtxId) -> usize {
        // Casts are lossless: `capacity` is a usize, and the modulus keeps
        // the result below it.
        (self.siphasher.hash(&key.as_bytes()) % self.capacity as u64) as usize
    }

    /// Test-only: how many times `key` was answered from the cache.
    #[cfg(test)]
    pub(super) fn hits_for(&self, key: &WtxId) -> u64 {
        self.hits_by_key
            .lock()
            .expect("no code can panic while holding the hit counter lock")
            .get(key)
            .copied()
            .unwrap_or_default()
    }

    /// Test-only: forgets one key, so a test can force re-verification of its
    /// own transaction without touching other tests' entries in the global.
    #[cfg(test)]
    pub(super) fn remove(&self, key: &WtxId) {
        let mut inner = self.lock();
        inner.keys.remove(key);
        inner.slots.retain(|slot| slot != key);
    }
}

impl Inner {
    /// O(len) set comparison; call only from `debug_assert!`.
    fn holds_invariants(&self) -> bool {
        self.keys.len() == self.slots.len()
            && self.keys == self.slots.iter().copied().collect::<HashSet<_>>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use zebra_chain::transaction::{AuthDigest, Hash};

    const SEED: [u8; 16] = [7; 16];

    fn wtx_id(n: u8) -> WtxId {
        WtxId {
            id: Hash([n; 32]),
            auth_digest: AuthDigest([n; 32]),
        }
    }

    #[test]
    fn insert_then_contains() {
        let cache = VerifiedScripts::new(4, SEED);

        assert!(!cache.contains(&wtx_id(1)));
        cache.insert(wtx_id(1));
        assert!(cache.contains(&wtx_id(1)));
    }

    #[test]
    fn duplicate_insert_is_a_no_op() {
        let cache = VerifiedScripts::new(2, SEED);

        cache.insert(wtx_id(1));
        cache.insert(wtx_id(1));

        let inner = cache.lock();
        assert_eq!(inner.keys.len(), 1);
        assert_eq!(inner.slots.len(), 1);
    }

    #[test]
    fn bounded_at_capacity() {
        let cache = VerifiedScripts::new(4, SEED);

        for n in 0..100 {
            cache.insert(wtx_id(n));
            let inner = cache.lock();
            assert!(inner.holds_invariants());
            assert!(inner.keys.len() <= 4);
        }

        assert_eq!(cache.lock().keys.len(), 4);
    }

    #[test]
    fn replacement_is_deterministic_with_a_fixed_seed() {
        let cache = VerifiedScripts::new(4, SEED);

        for n in 1..=4 {
            cache.insert(wtx_id(n));
        }

        let new_key = wtx_id(5);
        let victim = cache.lock().slots[cache.victim_index(&new_key)];
        cache.insert(new_key);

        assert!(!cache.contains(&victim), "the predicted victim is evicted");
        assert!(cache.contains(&new_key));
        for n in 1..=4 {
            let key = wtx_id(n);
            assert_eq!(cache.contains(&key), key != victim);
        }
    }

    #[test]
    fn remove_forgets_one_key_and_keeps_the_collections_in_lockstep() {
        let cache = VerifiedScripts::new(4, SEED);

        cache.insert(wtx_id(1));
        cache.insert(wtx_id(2));
        cache.remove(&wtx_id(1));

        assert!(!cache.contains(&wtx_id(1)));
        assert!(cache.contains(&wtx_id(2)));
        assert!(cache.lock().holds_invariants());
    }

    #[test]
    #[should_panic(expected = "capacity must be greater than zero")]
    fn zero_capacity_panics() {
        VerifiedScripts::new(0, SEED);
    }
}
