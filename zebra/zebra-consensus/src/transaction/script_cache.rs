//! Memoization of successful transparent script verification, per transaction.
//!
//! Zebra verifies every transaction's transparent input scripts at least twice
//! on the common path: once at mempool admission, and again when the
//! transaction arrives in a block (a `getblocktemplate` proposal check pays a
//! third time). zcashd skips the repeat through its script execution and
//! signature caches, so a zcashd miner revalidates a block built from its own
//! mempool in well under a second, while a Zebra miner pays for every
//! signature again. This module removes that asymmetry for the script half of
//! the work.
//!
//! # What is remembered
//!
//! One entry means: "every transparent input script of the transaction named
//! by this key verified successfully under this network upgrade". A hit lets
//! the transaction verifier skip re-running the script interpreter and its
//! signature checks for that transaction. Nothing else is skipped on a hit:
//! structure checks, lock time, expiry height, fees, sigop limits, shielded
//! proof verification, and the state service's spentness and double-spend
//! checks all still run for every block the transaction appears in.
//!
//! # Why the key is complete
//!
//! A hit replaces a verification, so the key must determine every input that
//! verification reads. Transparent script verification of one transaction
//! reads the transaction bytes (scriptSigs, outpoints, and for v5+ sighashes
//! the whole effecting data), the spent outputs' scriptPubKeys and values,
//! and the consensus branch semantics active at the verification height.
//!
//! * The transaction bytes are committed by [`UnminedTxId`]:
//!   - a legacy (v1-v4) id is the double-SHA256 of the whole serialized
//!     transaction, which contains the scriptSigs directly;
//!   - a witnessed (v5+) id pairs the ZIP-244 txid, which commits to all
//!     effecting data, with the ZIP-244 authorizing-data digest, which
//!     commits to the scriptSigs. The txid alone would not be enough: it
//!     deliberately excludes authorizing data, and answering a same-txid twin
//!     from the cached verification of a differently-signed transaction is
//!     exactly CVE-2026-34377 (GHSA-3vmh-33xr-9cqh).
//! * The spent outputs (each input's scriptPubKey and value) are committed
//!   directly, as a digest over their serialization in input order. On a real
//!   chain this is already implied: each input names an outpoint (creating
//!   txid, output index), outpoints are effecting data, and a txid commits to
//!   its transaction's outputs, so one outpoint denotes exactly one
//!   scriptPubKey and value everywhere. The digest makes that property
//!   structural instead of argued: even a caller that presents divergent data
//!   for an outpoint (mocks and tests can; production cannot) gets a distinct
//!   key, never a reused verdict. Whether the output exists and is unspent on
//!   this chain at this height stays the state service's contextual check,
//!   which never consults this cache.
//! * The branch semantics are the [`NetworkUpgrade`] in the key: it is the
//!   same value the verifier hands to `zebra_script`, and it selects the
//!   sighash algorithm and interpreter flags. Distinct upgrades never share
//!   an entry, so an upgrade boundary re-verifies instead of reusing.
//!
//! # Why a stale or missing entry is always safe
//!
//! Only successful verifications are inserted, so the cache can turn repeated
//! work into a hit but can never turn a failure into an acceptance. Evicting
//! or removing an entry only costs a re-verification.

use std::{
    collections::{HashSet, VecDeque},
    sync::Mutex,
};

use once_cell::sync::Lazy;

use zebra_chain::{
    parameters::NetworkUpgrade,
    serialization::{sha256d, ZcashSerialize},
    transaction::UnminedTxId,
    transparent,
};

/// The maximum number of remembered transactions.
///
/// Sized to hold several blocks of history plus a full mempool's worth of
/// churn. Each entry is stored twice (lookup set and eviction queue) at under
/// 100 bytes per copy, so a full cache costs a few MiB.
const SCRIPT_CACHE_CAPACITY: usize = 30_000;

/// The process-wide transparent script verification cache.
///
/// Process-wide for the same reason the signature batch verifiers in
/// [`crate::primitives`] are: the proposition it stores does not depend on
/// which service verified it (see the module docs), and the mempool and block
/// verifiers must share it for mempool-to-block reuse to happen.
static VERIFIED_SCRIPTS: Lazy<VerifiedScripts> =
    Lazy::new(|| VerifiedScripts::new(SCRIPT_CACHE_CAPACITY));

/// Returns the process-wide transparent script verification cache.
pub(super) fn verified_scripts() -> &'static VerifiedScripts {
    &VERIFIED_SCRIPTS
}

/// A key naming one proposition: "every transparent input script of this
/// transaction verifies against these spent outputs under this network
/// upgrade".
///
/// # Correctness
///
/// The caller constructing a key promises that `tx_id` is derived from the
/// transaction whose scripts are verified (the hash half from the bytes the
/// block or mempool actually carried, and, for v5+, the authorizing-data
/// digest recomputed from that same transaction), and that `spent_outputs`
/// are the outputs the script interpreter actually reads, in input order. An
/// id that does not determine the transaction's authorizing data would let a
/// differently-signed twin be answered from this transaction's verification
/// (CVE-2026-34377); see the module docs for the full derivation.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ScriptCacheKey {
    /// The unmined id of the verified transaction.
    tx_id: UnminedTxId,
    /// The network upgrade whose branch id and interpreter semantics the
    /// scripts verified under.
    nu: NetworkUpgrade,
    /// A double-SHA256 digest of the spent outputs' serializations, in input
    /// order. Output serialization is self-delimiting (value, then a
    /// length-prefixed script), so the concatenation is unambiguous.
    spent_outputs_digest: [u8; 32],
}

impl ScriptCacheKey {
    /// Builds the key for `tx_id`'s script verification against
    /// `spent_outputs` under `nu`.
    pub(super) fn new(
        tx_id: UnminedTxId,
        nu: NetworkUpgrade,
        spent_outputs: &[transparent::Output],
    ) -> Self {
        let mut writer = sha256d::Writer::default();
        for output in spent_outputs {
            output.zcash_serialize(&mut writer).expect(
                "output serialization only fails if the lock script length exceeds MAX_PROTOCOL_MESSAGE_LEN (2 MiB); \
                 spent outputs come from blocks bounded by MAX_BLOCK_BYTES (2,000,000), so serialization is infallible here",
            );
        }

        Self {
            tx_id,
            nu,
            spent_outputs_digest: writer.finish(),
        }
    }
}

/// The set of remembered script verifications, bounded by FIFO eviction.
///
/// # Correctness
///
/// `lookup` and `eviction_order` always hold the same keys: only
/// [`VerifiedScripts::insert`] and the test-only [`VerifiedScripts::remove`]
/// mutate them, and each mutates both, so no caller can leave them holding
/// different keys.
///
/// Eviction is FIFO rather than LRU: the working set is the mempool, which
/// turns over roughly in arrival order, and FIFO keeps the read path free of
/// bookkeeping ([`VerifiedScripts::contains`] never mutates). Evicting an
/// entry only costs a re-verification, never correctness.
pub(super) struct VerifiedScripts {
    inner: Mutex<VerifiedScriptsInner>,
    /// Test-only: hits per key, so a test can observe that a specific
    /// verification was answered from the cache rather than re-run.
    ///
    /// Grows with every distinct key hit in the test process and is never
    /// reset (counts survive [`VerifiedScripts::remove`]); both are fine for a
    /// test binary's lifetime and keep hit history observable.
    #[cfg(test)]
    hits_by_key: Mutex<std::collections::HashMap<ScriptCacheKey, u64>>,
}

/// The collections behind [`VerifiedScripts`]' mutex.
struct VerifiedScriptsInner {
    /// Answers [`VerifiedScripts::contains`].
    lookup: HashSet<ScriptCacheKey>,
    /// Chooses which key to drop when the cache is full.
    eviction_order: VecDeque<ScriptCacheKey>,
    /// The maximum number of keys kept.
    capacity: usize,
}

impl VerifiedScripts {
    /// Creates an empty cache holding at most `capacity` keys.
    ///
    /// A capacity of zero degenerates to a capacity of one: the eviction loop
    /// empties the queue and the new key is still pushed.
    fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VerifiedScriptsInner {
                lookup: HashSet::with_capacity(capacity),
                eviction_order: VecDeque::with_capacity(capacity),
                capacity,
            }),
            #[cfg(test)]
            hits_by_key: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Returns whether `key` was previously recorded by [`VerifiedScripts::insert`].
    ///
    /// The hit/miss metric is reported after the lock is released: the metrics
    /// macros allocate their label sets, and this lock is taken by every
    /// non-coinbase transaction the node verifies.
    pub(super) fn contains(&self, key: &ScriptCacheKey) -> bool {
        let hit = self
            .inner
            .lock()
            .expect("the script cache lock only guards infallible collection operations, so no panic can poison it")
            .lookup
            .contains(key);

        #[cfg(test)]
        if hit {
            *self
                .hits_by_key
                .lock()
                .expect("the hit counter lock only guards infallible map operations, so no panic can poison it")
                .entry(*key)
                .or_default() += 1;
        }

        metrics::counter!(
            "zebra.consensus.transaction.script_cache.lookups",
            "outcome" => if hit { "hit" } else { "miss" },
        )
        .increment(1);

        hit
    }

    /// Records that `key`'s proposition was verified.
    ///
    /// The caller promises the named transaction's scripts actually verified
    /// under the named upgrade; only successes may be recorded (module docs).
    pub(super) fn insert(&self, key: ScriptCacheKey) {
        let (inserted, evicted, size) = {
            let mut inner = self
                .inner
                .lock()
                .expect("the script cache lock only guards infallible collection operations, so no panic can poison it");

            // Concurrent verifications of one transaction can both miss and
            // both insert; the second insert must not push a duplicate into
            // the eviction queue, or the two collections would drift apart.
            if !inner.lookup.insert(key) {
                (false, 0, inner.lookup.len())
            } else {
                // Evict before pushing: pushing first would grow the queue
                // past `capacity`, doubling its allocation for the rest of
                // the process.
                let mut evicted: u64 = 0;
                while inner.eviction_order.len() >= inner.capacity {
                    // `break` rather than unwrap, so a zero capacity cannot
                    // spin or panic.
                    let Some(oldest) = inner.eviction_order.pop_front() else {
                        break;
                    };
                    inner.lookup.remove(&oldest);
                    evicted += 1;
                }
                inner.eviction_order.push_back(key);

                (true, evicted, inner.lookup.len())
            }
        };

        // Metrics after the lock is released; see `contains`.
        if inserted {
            metrics::counter!("zebra.consensus.transaction.script_cache.inserts").increment(1);
            if evicted > 0 {
                metrics::counter!("zebra.consensus.transaction.script_cache.evictions")
                    .increment(evicted);
            }
            metrics::gauge!("zebra.consensus.transaction.script_cache.entries").set(size as f64);
        }
    }

    /// Test-only: how many times `key` was answered from the cache.
    #[cfg(test)]
    pub(super) fn hits_for(&self, key: &ScriptCacheKey) -> u64 {
        self.hits_by_key
            .lock()
            .expect("the hit counter lock only guards infallible map operations, so no panic can poison it")
            .get(key)
            .copied()
            .unwrap_or_default()
    }

    /// Forgets one remembered verification.
    ///
    /// Always safe: forgetting a key costs a re-verification and can never
    /// accept scripts that were not verified. Test-only and key-scoped, so a
    /// test can force re-verification of its own transaction without racing
    /// other tests' entries in the process-global cache.
    #[cfg(test)]
    pub(super) fn remove(&self, key: &ScriptCacheKey) {
        let mut inner = self
            .inner
            .lock()
            .expect("the script cache lock only guards infallible collection operations, so no panic can poison it");
        inner.lookup.remove(key);
        inner.eviction_order.retain(|queued| queued != key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use zebra_chain::transaction::Hash;

    /// A distinct legacy-id key for tests.
    fn key(n: u8, nu: NetworkUpgrade) -> ScriptCacheKey {
        ScriptCacheKey::new(UnminedTxId::Legacy(Hash([n; 32])), nu, &[])
    }

    #[test]
    fn distinct_spent_outputs_are_distinct_entries() {
        let cache = VerifiedScripts::new(4);
        let tx_id = UnminedTxId::Legacy(Hash([1; 32]));

        let output = transparent::Output {
            value: zebra_chain::amount::Amount::try_from(1).expect("valid amount"),
            lock_script: transparent::Script::new(&[0x51]),
        };
        let other_output = transparent::Output {
            value: zebra_chain::amount::Amount::try_from(2).expect("valid amount"),
            lock_script: transparent::Script::new(&[0x51]),
        };

        cache.insert(ScriptCacheKey::new(
            tx_id,
            NetworkUpgrade::Nu5,
            std::slice::from_ref(&output),
        ));

        assert!(cache.contains(&ScriptCacheKey::new(
            tx_id,
            NetworkUpgrade::Nu5,
            std::slice::from_ref(&output),
        )));
        assert!(
            !cache.contains(&ScriptCacheKey::new(
                tx_id,
                NetworkUpgrade::Nu5,
                std::slice::from_ref(&other_output),
            )),
            "a verification against one spent-output dataset must not answer another"
        );

        // A script-only difference must also produce a distinct key, so the
        // digest cannot degenerate to covering values alone.
        let other_script_output = transparent::Output {
            value: output.value,
            lock_script: transparent::Script::new(&[0x52]),
        };
        assert!(
            !cache.contains(&ScriptCacheKey::new(
                tx_id,
                NetworkUpgrade::Nu5,
                std::slice::from_ref(&other_script_output),
            )),
            "a spent-output set differing only in scriptPubKey must not answer another"
        );
    }

    #[test]
    fn insert_then_contains() {
        let cache = VerifiedScripts::new(4);

        assert!(!cache.contains(&key(1, NetworkUpgrade::Nu5)));
        cache.insert(key(1, NetworkUpgrade::Nu5));
        assert!(cache.contains(&key(1, NetworkUpgrade::Nu5)));
    }

    #[test]
    fn distinct_network_upgrades_are_distinct_entries() {
        let cache = VerifiedScripts::new(4);

        cache.insert(key(1, NetworkUpgrade::Nu5));

        assert!(
            !cache.contains(&key(1, NetworkUpgrade::Nu6)),
            "a verification under one network upgrade must not answer another"
        );
    }

    #[test]
    fn eviction_is_fifo_and_bounded() {
        let cache = VerifiedScripts::new(2);

        cache.insert(key(1, NetworkUpgrade::Nu5));
        cache.insert(key(2, NetworkUpgrade::Nu5));
        cache.insert(key(3, NetworkUpgrade::Nu5));

        assert!(
            !cache.contains(&key(1, NetworkUpgrade::Nu5)),
            "the oldest entry must be evicted first"
        );
        assert!(cache.contains(&key(2, NetworkUpgrade::Nu5)));
        assert!(cache.contains(&key(3, NetworkUpgrade::Nu5)));

        let inner = cache.inner.lock().expect("not poisoned");
        assert_eq!(inner.lookup.len(), 2, "the lookup set must stay bounded");
        assert_eq!(
            inner.eviction_order.len(),
            2,
            "the eviction queue must stay bounded"
        );
    }

    #[test]
    fn duplicate_insert_does_not_grow_the_eviction_queue() {
        let cache = VerifiedScripts::new(2);

        cache.insert(key(1, NetworkUpgrade::Nu5));
        cache.insert(key(1, NetworkUpgrade::Nu5));
        cache.insert(key(2, NetworkUpgrade::Nu5));

        // If the duplicate had entered the queue, key 1 would occupy two slots
        // and this insert would evict it while `lookup` still answered hits.
        cache.insert(key(3, NetworkUpgrade::Nu5));

        assert!(!cache.contains(&key(1, NetworkUpgrade::Nu5)));
        assert!(cache.contains(&key(2, NetworkUpgrade::Nu5)));
        assert!(cache.contains(&key(3, NetworkUpgrade::Nu5)));

        let inner = cache.inner.lock().expect("not poisoned");
        assert_eq!(
            inner.lookup.len(),
            inner.eviction_order.len(),
            "the two collections must never drift apart"
        );
    }

    #[test]
    fn remove_forgets_one_key_and_keeps_the_collections_in_lockstep() {
        let cache = VerifiedScripts::new(4);

        cache.insert(key(1, NetworkUpgrade::Nu5));
        cache.insert(key(2, NetworkUpgrade::Nu5));
        cache.remove(&key(1, NetworkUpgrade::Nu5));

        assert!(!cache.contains(&key(1, NetworkUpgrade::Nu5)));
        assert!(cache.contains(&key(2, NetworkUpgrade::Nu5)));

        let inner = cache.inner.lock().expect("not poisoned");
        assert_eq!(
            inner.lookup.len(),
            inner.eviction_order.len(),
            "the two collections must never drift apart"
        );
        assert_eq!(inner.eviction_order.len(), 1);
    }
}
