//! The batching queue: what is held between flushes, and what is refused entry.
//!
//! This module and [`crate::batcher`] are where the anonymity property lives. A
//! migration is not broadcast when it arrives; it waits for the next scheduled
//! flush and is published simultaneously with everything else admitted in that
//! window, in an order nobody can predict. The batch IS the anonymity set.
//!
//! Every rule here comes from the adversarial review in `REVIEW.md`, and each
//! one closes an attack rather than expressing a preference:
//!
//! * **Admission control instead of an early-expiry flush (#2).** There is no
//!   "flush early because something is about to expire" trigger, because that is
//!   an attacker-operated flush clock: one cheap junk transaction per block with
//!   a tight expiry would collapse every window network-wide, permanently.
//!   Instead an entry is admitted only if it provably survives the next
//!   scheduled flush, which makes urgency unreachable rather than rate-limited.
//! * **Keyed on the payload hash, not the txid (#4).** Under ZIP 244 a v5/v6
//!   txid is a digest over transaction effects and excludes authorizing data, so
//!   two different byte strings can legitimately share a txid. A txid is
//!   therefore the wrong identity for a byte-level dedup, and a submitter-chosen
//!   key would let an attacker suppress someone else's entry by colliding with
//!   it. A payload hash is unforgeable by the submitter.
//! * **A parse failure is never a refusal (#5, amended 2026-10-28).** A
//!   transaction the hub cannot parse is precisely one the shim deliberately
//!   diverted because it could not read it either. Unparseable payloads are
//!   queued with `expiry = None` and published, in the small unverified lane.
//!   The amendment: a submission that touches Orchard must carry proofs that
//!   verify ([`crate::admission`]), so junk costs its sender a proof. The node
//!   remains the only authority on everything that needs chain state.
//! * **Three lanes, one cap.** The entry cap is split by what a submission
//!   costs to forge: proven, transparent-only, and unverified. Free junk fills
//!   only the lane it can reach, and the proven lane costs a proof per entry.
//! * **One entry per proof, and one publish per proof.** Entries are keyed on
//!   their payload hash AND on their bundle keys, so re-signing one proof
//!   collapses into the held entry. A bundle key this hub has published to a verdict (rejected, or
//!   already known) is burned and refused, so one proof buys one window.
//! * **Bytes are the budget, not entries.** Real Orchard migrations are 2 to
//!   16 KB, so fixed-size slots would waste most of the budget and hand an
//!   attacker a cheaper occupancy attack than the memory attack.
//! * **Never evict an admitted entry.** An admission is a promise. Refuse at the
//!   door instead; oldest-first eviction would evict exactly the entries closest
//!   to expiry, which is the attacker's selection lever.
//!
//! **There is deliberately no contributor, channel or session identifier on an
//! entry, and there must never be one.** An operator-to-migration mapping inside
//! the enclave is precisely the linkage this system exists to destroy, and it
//! would become exposure to enclave compromise, side channels and legal
//! compulsion.

// A panic in a diskless enclave destroys the queue, and every shim then walks
// its retry ladder simultaneously: an enclave panic is a fleet-wide privacy
// event, not a crash.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use rand::seq::SliceRandom;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::admission::{self, Inspected, Lane};

/// Per-entry ceiling, matching the fixed frame the batching design pads to.
///
/// A deliberate, tight bound, and NOT the shim's 4 MiB HTTP-body limit, which
/// bounds a wallet's request into a shim and is unrelated. 64 KiB accommodates
/// roughly 75 Orchard actions; a transaction larger than the frame cannot be
/// privately batched at all, which is the price of leaking zero bits of length.
pub const MAX_TX_BYTES: usize = 64 * 1024;

/// Total resident bytes the queue will hold. Bounds enclave memory against a
/// submitter who simply keeps submitting.
pub const MAX_QUEUE_BYTES: usize = 64 * 1024 * 1024;

/// Total ENTRIES the queue will hold, independently of their size.
///
/// The byte budget alone does not bound the batch: a submitter sending
/// minimum-size transactions fills 64 MB with tens of thousands of entries, and
/// a flush publishes every entry to every endpoint at once, so the entry count
/// -- not the byte count -- is what sizes the flush's concurrent dials. Left
/// unbounded that is a file-descriptor exhaustion path reachable by anyone who
/// can submit.
///
/// 1024 is where the byte budget lands for maximum-size transactions
/// (`MAX_QUEUE_BYTES / MAX_TX_BYTES`), so this tightens nothing for the traffic
/// the queue was sized for; it only removes the small-transaction blow-up.
/// Refusing past it is strictly better than the alternative: a submitter who can
/// flood the queue has already destroyed the anonymity set for that window, so
/// the batch is worth nothing anyway, and refusal at least keeps the hub alive
/// to serve the next one.
pub const MAX_QUEUE_ENTRIES: usize = MAX_QUEUE_BYTES / MAX_TX_BYTES;

/// The entry cap's share for transactions whose proofs verified. Junk here
/// costs its sender about 2.6 core-seconds per entry, so this lane gets most of
/// the cap. Honest migrations run about 15 per window network-wide.
pub const PROVEN_LANE_ENTRIES: usize = 896;

/// The share for transparent-only spends that no held parent vouches for:
/// Ironwood-funded TEX payments and ordinary transparent sends, which the shim
/// diverts and which carry nothing to verify. Free to fill, so a flood here
/// shuts this lane alone.
pub const TRANSPARENT_LANE_ENTRIES: usize = 112;

/// The share for what the hub cannot price: unparseable bytes (REVIEW #5), a
/// circuit era this build has no key for, a class no honest shim sends. Honest
/// traffic here is close to zero while hub and shim share one parser.
pub const UNVERIFIED_LANE_ENTRIES: usize = 16;

/// How many bundle keys the hub remembers having published to a verdict.
///
/// Oldest first out. An attacker who wants a burned proof back must push a
/// million newer keys through first, about a million proofs, roughly 30
/// core-days. Honest traffic, two keys per migration, takes years to cycle it.
/// About 100 MB at the cap. Lost on restart, like everything in the enclave.
pub const MAX_BURNED_KEYS: usize = 1 << 20;

/// A lane's share of [`MAX_QUEUE_ENTRIES`].
pub fn lane_cap(lane: Lane) -> usize {
    match lane {
        Lane::Proven => PROVEN_LANE_ENTRIES,
        Lane::Transparent => TRANSPARENT_LANE_ENTRIES,
        Lane::Unverified => UNVERIFIED_LANE_ENTRIES,
    }
}

fn lane_index(lane: Lane) -> usize {
    match lane {
        Lane::Proven => 0,
        Lane::Transparent => 1,
        Lane::Unverified => 2,
    }
}

/// How many flushes may fail to place one entry before it is given up on.
///
/// For most entries this, not expiry, is the bound that ends the retries: a
/// ZIP 318 migration expires a month or more out, and a payload that does not
/// deserialize has no expiry at all, correctly treated here as "never expires".
/// Without a count either would be re-offered to the network for the life of
/// the process. Only an entry the node never judged gets here: a node that
/// cannot parse a payload has rejected it, and it leaves at that flush.
///
/// Eight flushes is a few hours at the production cadence -- long enough to ride
/// out an indexer outage, far short of forever.
pub const MAX_REQUEUE_ATTEMPTS: u32 = 8;

/// Why a submission was not admitted.
///
/// Typed rather than a string because the shim reacts differently to each: a
/// tight expiry means hold and retry, an unavailable hub means try another hub.
/// Neither may ever be answered by broadcasting through the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The transaction would expire on or before the flush that would publish
    /// it. Publishing it late is worse than not accepting it, because the wallet
    /// can still hold and retry into a later window.
    ExpiryTooTight,
    /// Larger than the fixed frame. Cannot be batched without leaking its size.
    TooLarge,
    /// The queue is at its byte budget.
    Full,
    /// The chain tip is stale, so neither the flush schedule nor the expiry
    /// check can be trusted. Refusing is the fail-closed answer.
    TipStale,
    /// The hub is shutting down and has stopped admitting, so that the final
    /// flush is genuinely final. Distinct from [`Refusal::Full`] in this hub's
    /// own logs, but it deliberately shares `Full`'s code ON THE WIRE: a shim
    /// does the same thing with both (this hub cannot take it, try another or
    /// tell the wallet), and minting a new code would make an older shim answer
    /// `UnknownRefusal` for the length of any rolling deploy.
    Draining,
    /// A proof or signature did not verify, or this hub already published these
    /// proofs to a verdict. Over HTTP the reason is `rejected`; on the wire it
    /// shares `Full`'s code for the reason [`Refusal::Draining`] gives.
    Rejected,
}

impl Refusal {
    /// A stable machine-readable reason, safe to log and to return. Carries no
    /// per-entry information.
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::ExpiryTooTight => "expiry_too_tight",
            Refusal::TooLarge => "too_large",
            Refusal::Full => "queue_full",
            Refusal::TipStale => "tip_stale",
            Refusal::Draining => "draining",
            Refusal::Rejected => "rejected",
        }
    }
}

/// The outcome of offering a transaction to the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Held for the next flush. The txid is computed from the bytes, not
    /// supplied by the submitter, and is `None` when the payload did not parse.
    Admitted {
        txid: Option<String>,
    },
    /// These exact bytes, or another signing of the same proofs, are already
    /// queued. Idempotent by construction: honest resends and cross-hub
    /// duplicates collapse, because identical bytes hash identically, and a
    /// re-signing collapses on its bundle keys.
    Duplicate {
        txid: Option<String>,
    },
    Refused(Refusal),
}

/// One queued migration.
///
/// Note what is absent: any identifier of who submitted it. See the module docs.
pub struct Entry {
    /// `sha256(tx_bytes)`. The dedup identity, unforgeable by the submitter.
    pub key: [u8; 32],
    /// Computed by the hub from the bytes. `None` when the payload did not
    /// parse, which is queued and published like any other.
    pub txid: Option<String>,
    /// `None` means NO expiry, i.e. infinitely far away. Never fold this to 0:
    /// under an `expiry >= bound` test that would reject every legal no-expiry
    /// transaction, and under an `expiry <= bound` test it would pin the hub
    /// into permanent early flush.
    pub expiry: Option<u32>,
    /// Wiped on drop: the hub holds migrations in plaintext and a freed copy
    /// lingering in enclave memory is exactly what attestation cannot excuse.
    pub tx_bytes: Zeroizing<Vec<u8>>,
    /// How many flushes have offered this entry to the network without getting
    /// a verdict. Zero on admission, incremented on every requeue, and the
    /// backstop bound for a payload that has no expiry.
    pub attempts: u32,
    /// The tip when this was admitted. Drives the confirmation deadline.
    pub received_height: u32,
    /// Which share of the entry cap this holds.
    pub lane: Lane,
    /// One per verified bundle (see [`crate::admission`]). Indexed while the
    /// entry is held, burned once the network has judged it.
    pub bundle_keys: Vec<[u8; 32]>,
    /// How many transparent outputs this entry has for a second leg to spend.
    pub transparent_outputs: usize,
    /// Whether a second leg already rides on this entry's proofs.
    pub has_child: bool,
}

struct Inner {
    entries: HashMap<[u8; 32], Entry>,
    bytes: usize,
    /// Held entries per lane, indexed by [`lane_index`].
    lanes: [usize; 3],
    /// Bundle key to the payload hash of the entry that holds it.
    bundle_keys: HashMap<[u8; 32], [u8; 32]>,
    burned: Burned,
}

impl Inner {
    fn insert(&mut self, entry: Entry) {
        self.bytes = self.bytes.saturating_add(entry.tx_bytes.len());
        self.lanes[lane_index(entry.lane)] += 1;
        for bundle_key in &entry.bundle_keys {
            self.bundle_keys.insert(*bundle_key, entry.key);
        }
        self.entries.insert(entry.key, entry);
    }

    /// The bundle keys already held, or burned: either way these proofs buy no
    /// new entry.
    fn holds_any(&self, bundle_keys: &[[u8; 32]]) -> bool {
        bundle_keys
            .iter()
            .any(|key| self.bundle_keys.contains_key(key))
    }

    /// The held parent a transparent-only spend can ride on, if any.
    ///
    /// The TEX shape exactly: one input, spending an existing transparent output
    /// of a held entry whose own proofs verified, which carries no second leg
    /// yet. One proof then pays for at most two entries. A child is never a
    /// parent, because it carries no proof of its own.
    fn parent_of(&self, inspected: &Inspected) -> Option<[u8; 32]> {
        let [spend] = inspected.spends.as_slice() else {
            return None;
        };
        let parent_txid = spend.hash.to_string();
        let index = usize::try_from(spend.index).ok()?;
        self.entries
            .values()
            .find(|entry| {
                !entry.bundle_keys.is_empty()
                    && !entry.has_child
                    && index < entry.transparent_outputs
                    && entry.txid.as_deref() == Some(parent_txid.as_str())
            })
            .map(|entry| entry.key)
    }

    /// The cheap half of admission, run once before the proof check and again
    /// after it: everything that collapses or refuses without verifying.
    /// `Ok` carries the lane the entry would hold.
    fn screen(
        &self,
        key: &[u8; 32],
        inspected: &Inspected,
        len: usize,
        max_bytes: usize,
    ) -> Result<(Lane, Option<[u8; 32]>), Admission> {
        let duplicate = || Admission::Duplicate {
            txid: inspected.txid.clone(),
        };
        if self.entries.contains_key(key) {
            return Err(duplicate());
        }
        // Another signing of proofs already held. Collapsed like identical
        // bytes: only one of them can be mined, and the held one will be
        // published.
        if self.holds_any(&inspected.bundle_keys) {
            return Err(duplicate());
        }
        if inspected
            .bundle_keys
            .iter()
            .any(|bundle_key| self.burned.contains(bundle_key))
        {
            return Err(Admission::Refused(Refusal::Rejected));
        }

        let parent = match inspected.lane {
            Lane::Transparent => self.parent_of(inspected),
            Lane::Proven | Lane::Unverified => None,
        };
        let lane = if parent.is_some() {
            Lane::Proven
        } else {
            inspected.lane
        };

        // Reserve the budget and insert BEFORE the caller acknowledges. An
        // acknowledgement is a promise; never emit one for an entry that is not
        // resident. Full by bytes, by the lane's count, and by the total count
        // (a requeue may overrun a lane); the same refusal for each, because the
        // shim reacts to "this hub cannot take it" identically.
        if self.bytes.saturating_add(len) > max_bytes
            || self.lanes[lane_index(lane)] >= lane_cap(lane)
            || self.entries.len() >= MAX_QUEUE_ENTRIES
        {
            return Err(Admission::Refused(Refusal::Full));
        }
        Ok((lane, parent))
    }
}

/// Bundle keys this hub has published to a verdict, oldest first out.
#[derive(Default)]
struct Burned {
    keys: HashSet<[u8; 32]>,
    order: VecDeque<[u8; 32]>,
}

impl Burned {
    fn contains(&self, key: &[u8; 32]) -> bool {
        self.keys.contains(key)
    }

    fn insert(&mut self, key: [u8; 32]) {
        if self.keys.insert(key) {
            self.order.push_back(key);
        }
        if self.order.len() > MAX_BURNED_KEYS {
            if let Some(oldest) = self.order.pop_front() {
                self.keys.remove(&oldest);
            }
        }
    }
}

/// What one requeue did with the entries a flush could not place.
///
/// Counts only. An operator needs to see that entries are being given up on and
/// why; a per-entry line here would be a timing oracle over exactly the
/// migrations in flight.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Requeued {
    /// Put back, and offered again at the next flush.
    pub held: usize,
    /// Dropped: cannot survive the flush that would publish it.
    pub dropped_expired: usize,
    /// Dropped: out of attempts, before the entry's expiry arrived.
    pub dropped_exhausted: usize,
}

/// The in-memory batching queue.
pub struct Queue {
    inner: Mutex<Inner>,
    max_bytes: usize,
    /// Set once, at shutdown, before the final flush runs. See
    /// [`Queue::begin_draining`].
    draining: std::sync::atomic::AtomicBool,
}

impl Queue {
    pub fn new() -> Self {
        Self::with_capacity(MAX_QUEUE_BYTES)
    }

    pub fn with_capacity(max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: HashMap::new(),
                bytes: 0,
                lanes: [0; 3],
                bundle_keys: HashMap::new(),
                burned: Burned::default(),
            }),
            max_bytes,
            draining: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Stop admitting, permanently. Called on the shutdown signal and BEFORE the
    /// cadence runs its final flush.
    ///
    /// The ordering is the whole point and it is not decoration. Without it the
    /// serving path keeps accepting right up to the instant the process exits:
    /// the cadence publishes what it holds, returns, `main`'s `select!` resolves,
    /// and every migration admitted in the window between that flush and the
    /// exit is dropped -- each one already acked to a wallet that believes it is
    /// on its way to the network, and held nowhere else, because the shim keeps
    /// no copy. That is a silent loss of funds bounded only by how long the
    /// runtime takes to wind down.
    ///
    /// Refusing is the safe half of the trade: a refused submission is one the
    /// wallet can retry, against this hub when it comes back or another hub now.
    pub fn begin_draining(&self) {
        self.draining
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether [`begin_draining`](Self::begin_draining) has been called.
    pub fn is_draining(&self) -> bool {
        self.draining.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Offer a transaction to the queue.
    ///
    /// `tip` is the height admission judges at (the observed tip, or the
    /// boundary of the flush epoch the cadence last recorded if that is higher)
    /// and `flush_interval` the cadence, together deciding whether this entry
    /// provably survives the flush that would publish it.
    ///
    /// Blocking: a submission that touches Orchard has its proofs verified here,
    /// about 9 ms on four cores and 30 ms on one. Every refusal that costs less
    /// runs first, and the queue's lock is never held across the proof check.
    pub fn admit(
        &self,
        tx_bytes: &[u8],
        tip: u32,
        flush_interval: u32,
        mining_margin: u32,
    ) -> Admission {
        // Before every other check: once draining, this hub admits nothing, so
        // the flush that is about to run is the last word on what it holds.
        if self.is_draining() {
            return Admission::Refused(Refusal::Draining);
        }

        if tx_bytes.len() > self.max_bytes.min(MAX_TX_BYTES) {
            return Admission::Refused(Refusal::TooLarge);
        }

        // A parse failure is never a refusal (REVIEW #5): such a payload is
        // admitted with no txid and no expiry, in the unverified lane.
        let inspected = admission::inspect(tx_bytes);

        // Admission control: admit only if this entry provably survives the next
        // scheduled flush, which is what makes the early-flush trigger
        // unreachable rather than merely rate-limited (REVIEW #2).
        if !survives_next_flush(inspected.expiry, tip, flush_interval, mining_margin) {
            return Admission::Refused(Refusal::ExpiryTooTight);
        }

        let key: [u8; 32] = Sha256::digest(tx_bytes).into();

        // First look: a duplicate, a burned proof or a full lane is answered
        // before any proof is verified.
        if let Err(decided) = self
            .lock()
            .screen(&key, &inspected, tx_bytes.len(), self.max_bytes)
        {
            return decided;
        }

        if !inspected.verify() {
            return Admission::Refused(Refusal::Rejected);
        }

        // Second look, under the lock that inserts: the queue moved while the
        // proofs were checked.
        let mut inner = self.lock();
        let (lane, parent) = match inner.screen(&key, &inspected, tx_bytes.len(), self.max_bytes) {
            Ok(placed) => placed,
            Err(decided) => return decided,
        };
        if let Some(parent) = parent.and_then(|parent| inner.entries.get_mut(&parent)) {
            parent.has_child = true;
        }
        let txid = inspected.txid;
        inner.insert(Entry {
            key,
            txid: txid.clone(),
            expiry: inspected.expiry,
            tx_bytes: Zeroizing::new(tx_bytes.to_vec()),
            attempts: 0,
            received_height: tip,
            lane,
            bundle_keys: inspected.bundle_keys,
            transparent_outputs: inspected.transparent_outputs,
            has_child: false,
        });

        Admission::Admitted { txid }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(inner) => inner,
            // A poisoned lock means a thread panicked while holding it. Recover
            // the data rather than propagating the panic: dropping the queue
            // would push every pending migration onto its shim's retry ladder.
            Err(poison) => poison.into_inner(),
        }
    }

    /// Take everything pending, in an order nobody can predict.
    ///
    /// Shuffled with `rand::rng()`, which is a ChaCha-based CSPRNG seeded from
    /// the operating system and periodically reseeded. The requirement is that
    /// the permutation be cryptographically unpredictable and NOT reproducible:
    /// an observer who could predict intra-batch ordering could re-derive
    /// arrival order, which is the very thing simultaneous publication hides.
    /// Never seed this from a height, a counter, a timestamp or anything else an
    /// observer can also see. Iterating a `HashMap` is NOT a shuffle, and
    /// neither is sorting by any key.
    pub fn drain_shuffled(&self) -> Vec<Entry> {
        let mut inner = self.lock();

        let mut batch: Vec<Entry> = inner.entries.drain().map(|(_, entry)| entry).collect();
        inner.bytes = 0;
        inner.lanes = [0; 3];
        inner.bundle_keys.clear();
        drop(inner);

        batch.shuffle(&mut rand::rng());
        batch
    }

    /// Remember the proofs of entries the network has judged, so the same
    /// proofs are refused from now on.
    ///
    /// Called for an entry rejected or already known at a flush, and by
    /// [`Self::requeue`] for one out of attempts. An entry a node accepted stays
    /// unburned, so if it drops out of every mempool, its wallet's resend gets
    /// through. Burning an already-known entry is what stops a mined
    /// transaction, copied off the chain, from filling the queue every window.
    pub fn burn<'a>(&self, entries: impl IntoIterator<Item = &'a Entry>) {
        let mut inner = self.lock();
        for entry in entries {
            for bundle_key in &entry.bundle_keys {
                inner.burned.insert(*bundle_key);
            }
        }
    }

    /// Put back entries a flush drained but could not get a verdict on, so the
    /// next flush offers them again. Returns how many were reinserted.
    ///
    /// The byte budget and the lane counts are charged again, and may be
    /// overrun: the queue refilled while the batch was out, and these entries
    /// were admitted (and their admission acknowledged) BEFORE anything now
    /// resident. An admission is a promise and this honours the older one;
    /// `admit` refuses `Full` until the next flush brings the counts back under
    /// their caps. Resident memory is not made worse by this, because a drained
    /// batch is already held alongside a refilling queue for the whole flush
    /// window.
    ///
    /// Same bytes or same proofs already resident (a shim resent during the
    /// window) win, and the returning copy is dropped, so no key is ever counted
    /// twice.
    pub fn requeue(
        &self,
        entries: Vec<Entry>,
        tip: u32,
        flush_interval: u32,
        mining_margin: u32,
    ) -> Requeued {
        let mut inner = self.lock();

        let mut out = Requeued::default();
        for mut entry in entries {
            if inner.entries.contains_key(&entry.key) || inner.holds_any(&entry.bundle_keys) {
                continue;
            }

            entry.attempts = entry.attempts.saturating_add(1);

            // Bounded, and this bound is what makes biasing every ambiguous
            // publish failure toward "retry" safe rather than reckless. Without
            // it, "retry" means "forever": a transaction the network will never
            // accept would be re-offered at every flush for the life of the
            // process, and each re-offer is another emission on a throttled lane
            // and another timing signal about one transaction.
            //
            // Expiry ends a tight one. The attempt count ends the rest: a ZIP 318
            // migration expires a month or more out, and a payload that does not
            // parse has no expiry.
            if !survives_next_flush(entry.expiry, tip, flush_interval, mining_margin) {
                out.dropped_expired += 1;
                continue;
            }
            if entry.attempts > MAX_REQUEUE_ATTEMPTS {
                // Out of attempts is a verdict too: behind an indexer that
                // relays every node rejection as retryable, a proof the network
                // refuses ends here, and is burned so it stays out.
                for bundle_key in &entry.bundle_keys {
                    inner.burned.insert(*bundle_key);
                }
                out.dropped_exhausted += 1;
                continue;
            }

            inner.insert(entry);
            out.held += 1;
        }
        out
    }

    /// Number of entries currently held. For the hub operator's own metrics
    /// only: this is an anonymity-set-size oracle and must never be returned
    /// down a shim channel.
    pub fn len(&self) -> usize {
        match self.inner.lock() {
            Ok(inner) => inner.entries.len(),
            Err(poison) => poison.into_inner().entries.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The held bytes for a transaction named by a wallet's `TxFilter.hash`, if
    /// this hub is holding it unbroadcast.
    ///
    /// BOTH byte orders are checked: an `Entry.txid` is display-order hex (from
    /// zebra's `Display`), while the wire hash is internal/little-endian order,
    /// its reverse. Depending on one order alone would silently stop answering
    /// for half the callers. Both candidate strings are computed BEFORE the lock
    /// so the critical section is two `==` per entry, and the scan never holds
    /// the lock across an await.
    ///
    /// Linear scan on purpose: the modal queue is 0 to 1 entries, and the two
    /// hex strings are precomputed. If junk-stuffing ever makes the queue large
    /// enough for this to matter, add a `txid -> key` index maintained in
    /// `admit`/`drain_shuffled`/`requeue`; deferred until measured.
    ///
    /// The returned copy is `Zeroizing`, but note the response `hyper` builds
    /// from it downstream is an ordinary buffer that cannot be wiped.
    pub fn find_by_txid(&self, wire_hash: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
        let forward = hex::encode(wire_hash);
        let mut reversed = wire_hash.to_vec();
        reversed.reverse();
        let reversed = hex::encode(reversed);

        let inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poison) => poison.into_inner(),
        };
        inner
            .entries
            .values()
            .find(|entry| {
                entry
                    .txid
                    .as_deref()
                    .is_some_and(|txid| txid == forward || txid == reversed)
            })
            .map(|entry| Zeroizing::new(entry.tx_bytes.to_vec()))
    }
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

/// The next height at which a flush is scheduled, strictly after `h`.
///
/// Bounding admission against this rather than against `h + N` is strictly more
/// permissive and equally safe: the wait until publication is uniform over
/// `0..N` rather than always `N`, which matters because the tightest wallet
/// expiry is the binding constraint on the whole design.
pub fn next_flush_height(h: u32, n: u32) -> u32 {
    if n == 0 {
        return h;
    }
    ((h / n).saturating_add(1)).saturating_mul(n)
}

/// Whether an entry admitted at `tip` provably survives the flush that would
/// publish it.
///
/// This is the whole of admission control, kept as a pure function because it is
/// the rule that makes every early-flush trigger unreachable: if every admitted
/// entry survives its scheduled flush, no entry can ever become urgent, so there
/// is nothing for an attacker to make urgent.
///
/// `expiry == None` means the transaction does not expire, so it always
/// survives. Never fold that to zero.
pub fn survives_next_flush(
    expiry: Option<u32>,
    tip: u32,
    flush_interval: u32,
    mining_margin: u32,
) -> bool {
    match expiry {
        None => true,
        Some(expiry) => {
            let deadline = next_flush_height(tip, flush_interval).saturating_add(mining_margin);
            expiry >= deadline
        }
    }
}

#[cfg(test)]
// The module-level deny above is about PRODUCTION paths: a panic in a diskless
// enclave is a fleet-wide privacy event. In tests, panicking IS the failure
// report, so the assertion macros are allowed here and nowhere else.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const N: u32 = 20;
    const MARGIN: u32 = 4;

    #[test]
    fn next_flush_height_lands_on_the_following_multiple() {
        // The two boundaries the review calls out by name.
        assert_eq!(
            next_flush_height(100, 20),
            120,
            "h % N == 0 must not return h"
        );
        assert_eq!(next_flush_height(119, 20), 120, "h % N == N-1");
        assert_eq!(next_flush_height(101, 20), 120);
        assert_eq!(next_flush_height(0, 20), 20);
    }

    #[test]
    fn next_flush_height_is_saturating_not_panicking() {
        // A hostile or corrupt tip must not take the enclave down.
        let _ = next_flush_height(u32::MAX, 20);
        assert_eq!(next_flush_height(5, 0), 5);
    }

    #[test]
    fn a_transaction_with_no_expiry_is_always_admissible() {
        // None means infinitely far away, NOT height zero. Folding it to 0 would
        // reject every legal no-expiry transaction forever, which is the mirror
        // of the `unwrap_or(0)` mistake the review warns about. Note that
        // `nExpiryHeight == 0` on the wire means exactly this (ZIP 203) and is
        // folded to None at parse time.
        assert!(survives_next_flush(None, 0, N, MARGIN));
        assert!(survives_next_flush(None, u32::MAX - 1, N, MARGIN));
    }

    #[test]
    fn admission_requires_surviving_the_flush_that_would_publish_it() {
        // Tip 100 with N=20: the next flush is at 120, plus 4 blocks to get
        // mined, so 124 is the first expiry that survives.
        assert_eq!(next_flush_height(100, N) + MARGIN, 124);
        assert!(!survives_next_flush(Some(123), 100, N, MARGIN), "one short");
        assert!(
            survives_next_flush(Some(124), 100, N, MARGIN),
            "exactly enough"
        );
        assert!(survives_next_flush(Some(200), 100, N, MARGIN));
    }

    #[test]
    fn the_admission_bound_is_uniform_over_the_window_not_always_a_full_interval() {
        // Bounding against next_flush_height rather than tip + N is strictly
        // more permissive and equally safe: late in a window the wait until
        // publication is short, so a tighter expiry is still admissible. That
        // matters because the wallet expiry ceiling is the binding constraint on
        // the whole design.
        //
        // At tip 119 the next flush is one block away, so expiry 124 works; at
        // tip 100 it is twenty blocks away and 124 is the minimum. A naive
        // tip + N bound would demand 143 at tip 119 and needlessly refuse.
        assert!(survives_next_flush(Some(124), 119, N, MARGIN));
        assert!(!survives_next_flush(Some(124 - 1), 119, N, MARGIN));
        assert!(
            survives_next_flush(Some(124), 119, N, MARGIN)
                && !survives_next_flush(Some(142), 119 + 1, N, MARGIN),
            "the bound moves with the window boundary, not with the tip"
        );
    }

    #[test]
    fn admission_boundaries_at_both_ends_of_a_window() {
        // The two cases the review names explicitly.
        // h % N == 0: the next flush is a full interval away, never `h` itself.
        assert!(!survives_next_flush(Some(100 + MARGIN), 100, N, MARGIN));
        assert!(survives_next_flush(Some(120 + MARGIN), 100, N, MARGIN));
        // h % N == N - 1: the next flush is one block away.
        assert!(survives_next_flush(Some(120 + MARGIN), 119, N, MARGIN));
    }

    #[test]
    fn admission_arithmetic_saturates_rather_than_overflowing() {
        // A hostile or corrupt tip must not take the enclave down, and must not
        // wrap into accidentally admitting everything.
        let _ = survives_next_flush(Some(u32::MAX), u32::MAX, N, MARGIN);
        let _ = survives_next_flush(Some(0), u32::MAX, N, MARGIN);
    }

    /// A minimal payload that will not deserialize as a transaction.
    fn junk(seed: u8) -> Vec<u8> {
        vec![seed; 64]
    }

    /// A migration whose proofs verify (tests/proven_fixtures.rs). Admitting it
    /// gives an entry with a genuine display-order txid, which is what
    /// `find_by_txid` keys on.
    const V6_ORCHARD_ONLY: &[u8] = include_bytes!("../tests/fixtures/v6_proven_a.bin");
    /// The same proofs as [`V6_ORCHARD_ONLY`] under fresh signatures.
    const PROVEN_A_RESIGNED: &[u8] = include_bytes!("../tests/fixtures/v6_proven_a_resigned.bin");
    /// The same proofs signed over the wrong sighash.
    const PROVEN_A_BAD_SIG: &[u8] = include_bytes!("../tests/fixtures/v6_proven_a_bad_sig.bin");
    /// An independent migration.
    const PROVEN_B: &[u8] = include_bytes!("../tests/fixtures/v6_proven_b.bin");
    /// The shim's migration fixture: it parses, and its proofs are zero-filled.
    const ZERO_PROOF: &[u8] = include_bytes!("../../shim/tests/fixtures/v6_migration.bin");
    /// A transparent-only spend of an output no held entry has.
    const TRANSPARENT_ONLY: &[u8] =
        include_bytes!("../../shim/tests/fixtures/v6_transparent_only.bin");

    /// A transparent-only spend of `(txid(parent), 0)`, the second leg of a TEX
    /// payment. `value` tells two such legs apart.
    fn second_leg(parent: &[u8], value: u64) -> Vec<u8> {
        use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};
        use zebra_chain::{block, parameters::NetworkUpgrade, transaction, transparent};

        let parent = transaction::Transaction::zcash_deserialize(parent).unwrap();
        transaction::Transaction::V5 {
            network_upgrade: NetworkUpgrade::Nu6_3,
            lock_time: transaction::LockTime::unlocked(),
            expiry_height: block::Height(0),
            inputs: vec![transparent::Input::PrevOut {
                outpoint: transparent::OutPoint {
                    hash: parent.hash(),
                    index: 0,
                },
                unlock_script: transparent::Script::new(&[0x51]),
                sequence: u32::MAX,
            }],
            outputs: vec![transparent::Output {
                value: value.try_into().unwrap(),
                lock_script: transparent::Script::new(&[0x51]),
            }],
            sapling_shielded_data: None,
            orchard_shielded_data: None,
        }
        .zcash_serialize_to_vec()
        .unwrap()
    }

    fn admitted(admission: Admission) -> bool {
        matches!(admission, Admission::Admitted { .. })
    }

    #[test]
    fn a_proven_migration_is_admitted_and_a_failed_proof_is_refused() {
        let q = Queue::new();
        assert!(matches!(
            q.admit(V6_ORCHARD_ONLY, 100, N, MARGIN),
            Admission::Admitted { txid: Some(_) }
        ));
        // Zero-filled proofs, and real proofs under bad signatures: both cost
        // their sender zero work, and both are refused.
        let q = Queue::new();
        for refused in [ZERO_PROOF, PROVEN_A_BAD_SIG] {
            assert_eq!(
                q.admit(refused, 100, N, MARGIN),
                Admission::Refused(Refusal::Rejected)
            );
        }
        assert!(q.is_empty());
    }

    #[test]
    fn re_signing_a_held_proof_buys_no_second_entry() {
        // The attack bundle keys close: the queue keys on the payload hash, and
        // fresh signatures make fresh bytes from one proof.
        let q = Queue::new();
        assert!(admitted(q.admit(V6_ORCHARD_ONLY, 100, N, MARGIN)));
        assert!(matches!(
            q.admit(PROVEN_A_RESIGNED, 100, N, MARGIN),
            Admission::Duplicate { txid: Some(_) }
        ));
        assert!(admitted(q.admit(PROVEN_B, 100, N, MARGIN)));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn a_proof_published_to_a_verdict_is_refused_for_good() {
        // One proof buys one window: after the flush that judged it, no signing
        // of it is admitted again.
        let q = Queue::new();
        assert!(admitted(q.admit(V6_ORCHARD_ONLY, 100, N, MARGIN)));
        let judged = q.drain_shuffled();
        q.burn(&judged);
        for burned in [V6_ORCHARD_ONLY, PROVEN_A_RESIGNED] {
            assert_eq!(
                q.admit(burned, 100, N, MARGIN),
                Admission::Refused(Refusal::Rejected)
            );
        }
        assert!(admitted(q.admit(PROVEN_B, 100, N, MARGIN)));
    }

    #[test]
    fn a_proof_out_of_attempts_is_burned_too() {
        // Behind an indexer that relays a node's refusal as retryable, running
        // out of attempts is the only verdict a refused proof ever gets.
        let q = Queue::new();
        assert!(admitted(q.admit(V6_ORCHARD_ONLY, 100, N, MARGIN)));
        let mut last = Requeued::default();
        for _ in 0..=MAX_REQUEUE_ATTEMPTS {
            last = q.requeue(q.drain_shuffled(), 100, N, MARGIN);
        }
        assert_eq!(last.dropped_exhausted, 1);
        assert_eq!(
            q.admit(V6_ORCHARD_ONLY, 100, N, MARGIN),
            Admission::Refused(Refusal::Rejected)
        );
    }

    #[test]
    fn a_requeue_drops_a_copy_whose_proofs_are_held_again() {
        // A shim resent a re-signing while the batch was out. Both carry the same
        // proofs, so one entry is all they may hold.
        let q = Queue::new();
        assert!(admitted(q.admit(V6_ORCHARD_ONLY, 100, N, MARGIN)));
        let batch = q.drain_shuffled();
        assert!(admitted(q.admit(PROVEN_A_RESIGNED, 100, N, MARGIN)));
        assert_eq!(q.requeue(batch, 100, N, MARGIN).held, 0);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn a_second_leg_rides_on_its_held_parent_once() {
        let q = Queue::new();
        assert!(admitted(q.admit(V6_ORCHARD_ONLY, 100, N, MARGIN)));
        assert!(admitted(q.admit(
            &second_leg(V6_ORCHARD_ONLY, 1000),
            100,
            N,
            MARGIN
        )));
        // A second leg of the same parent is a conflicting spend; the parent's
        // proofs pay for one.
        assert!(admitted(q.admit(
            &second_leg(V6_ORCHARD_ONLY, 2000),
            100,
            N,
            MARGIN
        )));
        // So is a leg whose parent the queue does not hold.
        assert!(admitted(q.admit(
            &second_leg(PROVEN_B, 1000),
            100,
            N,
            MARGIN
        )));

        let lanes: Vec<(u64, Lane)> = q
            .drain_shuffled()
            .iter()
            .filter(|entry| entry.bundle_keys.is_empty())
            .map(|entry| (entry.tx_bytes.len() as u64, entry.lane))
            .collect();
        assert_eq!(lanes.len(), 3);
        assert_eq!(
            lanes
                .iter()
                .filter(|(_, lane)| *lane == Lane::Proven)
                .count(),
            1,
            "exactly one leg rides on the held parent"
        );
    }

    /// The wire-order (internal, little-endian) bytes a wallet's `TxFilter.hash`
    /// would carry for a display-order txid hex: decode, then reverse.
    fn wire_hash(display_txid: &str) -> Vec<u8> {
        let mut bytes = hex::decode(display_txid).expect("txid is hex");
        bytes.reverse();
        bytes
    }

    #[test]
    fn find_by_txid_matches_both_byte_orders() {
        let q = Queue::new();
        let txid = match q.admit(V6_ORCHARD_ONLY, 100, N, MARGIN) {
            Admission::Admitted { txid: Some(txid) } => txid,
            other => panic!("expected an admitted tx with a txid, got {other:?}"),
        };

        // What a wallet actually sends (internal order) hits.
        let internal = wire_hash(&txid);
        let found = q
            .find_by_txid(&internal)
            .expect("internal-order lookup must hit");
        assert_eq!(found.as_slice(), V6_ORCHARD_ONLY);

        // Display order also hits, so a wallet that sends the reverse still works.
        let display = hex::decode(&txid).expect("txid is hex");
        assert!(
            q.find_by_txid(&display).is_some(),
            "display-order lookup must hit"
        );
    }

    #[test]
    fn find_by_txid_misses_an_unrelated_hash() {
        let q = Queue::new();
        let _ = q.admit(V6_ORCHARD_ONLY, 100, N, MARGIN);
        assert!(q.find_by_txid(&[0xab; 32]).is_none());
    }

    #[test]
    fn find_by_txid_skips_entries_with_no_txid() {
        // An unparseable payload is queued with txid None (REVIEW #5). It must be
        // present but never match a lookup, so a poll for it falls through to the
        // indexer rather than matching the wrong entry.
        let q = Queue::new();
        let _ = q.admit(&junk(1), 100, N, MARGIN);
        assert_eq!(q.len(), 1, "the None-txid entry is held");
        assert!(q.find_by_txid(&[0x11; 32]).is_none());
    }

    #[test]
    fn find_by_txid_misses_after_the_queue_is_drained() {
        let q = Queue::new();
        let txid = match q.admit(V6_ORCHARD_ONLY, 100, N, MARGIN) {
            Admission::Admitted { txid: Some(txid) } => txid,
            other => panic!("expected an admitted tx, got {other:?}"),
        };
        let internal = wire_hash(&txid);
        assert!(q.find_by_txid(&internal).is_some());
        let _ = q.drain_shuffled();
        assert!(
            q.find_by_txid(&internal).is_none(),
            "a flushed transaction is no longer held; the lookup must fall through to the indexer"
        );
    }

    #[test]
    fn an_unparseable_payload_is_queued_not_refused() {
        // The shim diverts what it cannot read; refusing it here would invert
        // that fail-safe into a leak (REVIEW #5).
        let q = Queue::new();
        match q.admit(&junk(1), 100, N, MARGIN) {
            Admission::Admitted { txid } => {
                assert!(txid.is_none(), "no txid for an unparseable body")
            }
            other => panic!("an unparseable payload must be admitted, got {other:?}"),
        }
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn identical_bytes_collapse_to_one_entry() {
        // Honest resends and cross-hub duplicates must not inflate a batch.
        let q = Queue::new();
        assert!(matches!(
            q.admit(&junk(7), 100, N, MARGIN),
            Admission::Admitted { .. }
        ));
        assert!(matches!(
            q.admit(&junk(7), 100, N, MARGIN),
            Admission::Duplicate { .. }
        ));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn distinct_bytes_are_distinct_entries() {
        let q = Queue::new();
        assert!(matches!(
            q.admit(&junk(1), 100, N, MARGIN),
            Admission::Admitted { .. }
        ));
        assert!(matches!(
            q.admit(&junk(2), 100, N, MARGIN),
            Admission::Admitted { .. }
        ));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn an_oversized_payload_is_refused_rather_than_batched() {
        // Batching it would leak its length; the shim must surface this to the
        // wallet rather than fall back to a direct broadcast.
        let q = Queue::new();
        let huge = vec![0u8; MAX_TX_BYTES + 1];
        assert_eq!(
            q.admit(&huge, 100, N, MARGIN),
            Admission::Refused(Refusal::TooLarge)
        );
    }

    #[test]
    fn the_byte_budget_refuses_at_the_door_and_keeps_what_it_admitted() {
        // Never evict an admitted entry: an admission is a promise, and
        // oldest-first eviction would drop exactly the entries nearest expiry.
        let q = Queue::with_capacity(200);
        assert!(matches!(
            q.admit(&junk(1), 100, N, MARGIN),
            Admission::Admitted { .. }
        ));
        assert!(matches!(
            q.admit(&junk(2), 100, N, MARGIN),
            Admission::Admitted { .. }
        ));
        assert!(matches!(
            q.admit(&junk(3), 100, N, MARGIN),
            Admission::Admitted { .. }
        ));
        // Fourth would exceed 200 bytes.
        assert_eq!(
            q.admit(&junk(4), 100, N, MARGIN),
            Admission::Refused(Refusal::Full)
        );
        assert_eq!(
            q.len(),
            3,
            "admitted entries are never evicted to make room"
        );
    }

    #[test]
    fn draining_empties_the_queue_and_releases_the_budget() {
        let q = Queue::with_capacity(200);
        let _ = q.admit(&junk(1), 100, N, MARGIN);
        let _ = q.admit(&junk(2), 100, N, MARGIN);
        let batch = q.drain_shuffled();
        assert_eq!(batch.len(), 2);
        assert!(q.is_empty());
        // The budget came back with them.
        assert!(matches!(
            q.admit(&junk(3), 100, N, MARGIN),
            Admission::Admitted { .. }
        ));
        assert!(matches!(
            q.admit(&junk(4), 100, N, MARGIN),
            Admission::Admitted { .. }
        ));
        assert!(matches!(
            q.admit(&junk(5), 100, N, MARGIN),
            Admission::Admitted { .. }
        ));
    }

    #[test]
    fn requeued_entries_are_held_again_and_charged_to_the_budget_again() {
        // The batch came back unpublished; it must be resident (a flush will
        // offer it again) and it must occupy the bytes it did before, otherwise
        // the budget silently doubles every time an indexer is down.
        let q = Queue::with_capacity(200);
        let _ = q.admit(&junk(1), 100, N, MARGIN);
        let _ = q.admit(&junk(2), 100, N, MARGIN);
        let _ = q.admit(&junk(3), 100, N, MARGIN);
        let batch = q.drain_shuffled();
        assert!(q.is_empty());

        assert_eq!(q.requeue(batch, 100, N, MARGIN).held, 3);
        assert_eq!(q.len(), 3);
        assert_eq!(
            q.admit(&junk(4), 100, N, MARGIN),
            Admission::Refused(Refusal::Full),
            "requeued bytes count against the budget"
        );

        // And they come out again on the next drain, budget released.
        assert_eq!(q.drain_shuffled().len(), 3);
        assert!(matches!(
            q.admit(&junk(4), 100, N, MARGIN),
            Admission::Admitted { .. }
        ));
    }

    #[test]
    fn a_requeue_never_double_counts_bytes_that_were_resent_meanwhile() {
        // A shim that resent the same bytes during the flush window has an
        // entry resident already; the returning copy is dropped, not stacked.
        let q = Queue::with_capacity(200);
        let _ = q.admit(&junk(1), 100, N, MARGIN);
        let batch = q.drain_shuffled();
        let _ = q.admit(&junk(1), 100, N, MARGIN);
        assert_eq!(q.requeue(batch, 100, N, MARGIN).held, 0);
        assert_eq!(q.len(), 1);
        // 64 held, so two more 64-byte entries fit under 200 and a third does not.
        let _ = q.admit(&junk(2), 100, N, MARGIN);
        let _ = q.admit(&junk(3), 100, N, MARGIN);
        assert_eq!(q.len(), 3, "only one copy of the resent bytes was charged");
    }

    #[test]
    fn a_requeue_may_overrun_the_budget_and_admit_then_refuses_until_a_flush() {
        // The queue refilled while the batch was out. Both sets were admitted,
        // both admissions were promises; the older one is honoured by holding
        // it, the door refuses until the next flush clears the overrun.
        let q = Queue::with_capacity(200);
        let _ = q.admit(&junk(1), 100, N, MARGIN);
        let _ = q.admit(&junk(2), 100, N, MARGIN);
        let batch = q.drain_shuffled();
        let _ = q.admit(&junk(3), 100, N, MARGIN);
        let _ = q.admit(&junk(4), 100, N, MARGIN);
        let _ = q.admit(&junk(5), 100, N, MARGIN);
        assert_eq!(q.requeue(batch, 100, N, MARGIN).held, 2);
        assert_eq!(
            q.len(),
            5,
            "an admitted entry is never evicted, from either side"
        );
        assert_eq!(
            q.admit(&junk(6), 100, N, MARGIN),
            Admission::Refused(Refusal::Full)
        );
        assert_eq!(q.drain_shuffled().len(), 5);
        assert!(matches!(
            q.admit(&junk(6), 100, N, MARGIN),
            Admission::Admitted { .. }
        ));
    }

    #[test]
    fn draining_an_empty_queue_is_an_empty_batch_not_a_panic() {
        let q = Queue::new();
        assert!(q.drain_shuffled().is_empty());
    }

    #[test]
    fn the_order_a_batch_comes_out_in_is_not_the_order_it_went_in() {
        // Statistical, and deliberately generous: the point is that SOME
        // shuffling happens, not that a particular permutation appears. With 16
        // entries the chance of the identity permutation is 1/16!, so a run that
        // never permutes across 8 drains means the shuffle is not wired up.
        let mut ever_differed = false;
        for _ in 0..8 {
            let q = Queue::new();
            let inputs: Vec<Vec<u8>> = (0..UNVERIFIED_LANE_ENTRIES as u8).map(junk).collect();
            for input in &inputs {
                let _ = q.admit(input, 100, N, MARGIN);
            }
            let batch = q.drain_shuffled();
            assert_eq!(batch.len(), UNVERIFIED_LANE_ENTRIES);
            let out: Vec<Vec<u8>> = batch.iter().map(|e| e.tx_bytes.to_vec()).collect();
            if out != inputs {
                ever_differed = true;
                break;
            }
        }
        assert!(
            ever_differed,
            "drain_shuffled must not preserve insertion order"
        );
    }

    /// A distinct 64-byte payload per index.
    fn distinct(seed: u32) -> Vec<u8> {
        let mut v = vec![0u8; 64];
        v[..4].copy_from_slice(&seed.to_le_bytes());
        v
    }

    #[test]
    fn a_flood_of_free_junk_fills_only_its_own_lane() {
        // 64-byte payloads cost nothing to forge, and 1024 of them used to shut
        // the whole queue for a window while holding ~64 KiB of a 64 MiB budget.
        // They now fill the unverified lane, and the other lanes stay open.
        let q = Queue::new();
        for i in 0..UNVERIFIED_LANE_ENTRIES {
            assert!(
                admitted(q.admit(&distinct(i as u32), 100, N, MARGIN)),
                "entry {i} fits the unverified lane"
            );
        }
        assert_eq!(
            q.admit(&distinct(u32::MAX), 100, N, MARGIN),
            Admission::Refused(Refusal::Full),
            "past its lane's share the junk is refused"
        );
        assert!(admitted(q.admit(TRANSPARENT_ONLY, 100, N, MARGIN)));
        assert!(admitted(q.admit(V6_ORCHARD_ONLY, 100, N, MARGIN)));
        assert_eq!(
            q.len(),
            UNVERIFIED_LANE_ENTRIES + 2,
            "a refusal must not evict: an admission is a promise"
        );
    }

    #[test]
    fn the_lanes_partition_the_entry_cap() {
        assert_eq!(
            PROVEN_LANE_ENTRIES + TRANSPARENT_LANE_ENTRIES + UNVERIFIED_LANE_ENTRIES,
            MAX_QUEUE_ENTRIES
        );
    }

    #[test]
    fn the_entry_cap_is_where_the_byte_budget_lands_for_full_size_transactions() {
        // The two budgets are meant to bind at the same point for the traffic
        // the queue was sized for, so the entry cap tightens nothing real. If
        // someone retunes one, this says the other has to move with it.
        assert_eq!(MAX_QUEUE_ENTRIES, MAX_QUEUE_BYTES / MAX_TX_BYTES);
    }

    #[test]
    fn a_draining_queue_refuses_new_work_but_still_yields_what_it_holds() {
        // The shutdown sequence in one test. Everything admitted before the
        // drain must still come out of the final flush; everything offered after
        // it must be refused rather than accepted into a batch that will never
        // be published.
        let q = Queue::new();
        assert!(matches!(
            q.admit(&junk(1), 100, N, MARGIN),
            Admission::Admitted { .. }
        ));
        assert!(matches!(
            q.admit(&junk(2), 100, N, MARGIN),
            Admission::Admitted { .. }
        ));

        q.begin_draining();
        assert!(q.is_draining());

        assert_eq!(
            q.admit(&junk(3), 100, N, MARGIN),
            Admission::Refused(Refusal::Draining),
            "after the drain begins nothing new may be admitted: the flush that \
             is about to run is the last one, and an admission after it is a \
             migration acked to a wallet and then dropped on exit"
        );

        // The two admitted before the drain are still there to be published.
        let batch = q.drain_shuffled();
        assert_eq!(
            batch.len(),
            2,
            "draining must not discard what was promised"
        );
    }

    #[test]
    fn draining_is_checked_before_every_other_refusal() {
        // Ordering inside `admit` matters for the operator reading the log. A
        // shutting-down hub that reports `too_large` or `expiry_too_tight` sends
        // whoever is debugging it after the transaction instead of the hub.
        let q = Queue::new();
        q.begin_draining();

        let huge = vec![0u8; MAX_TX_BYTES + 1];
        assert_eq!(
            q.admit(&huge, 100, N, MARGIN),
            Admission::Refused(Refusal::Draining),
            "a draining hub says so, whatever else is also wrong with the request"
        );
    }

    #[test]
    fn a_requeue_still_works_while_draining() {
        // The final flush requeues anything it could not get a verdict on, and
        // that path runs entirely inside the drain. Gating it would throw away
        // exactly the entries the ordered shutdown exists to protect.
        let q = Queue::new();
        let _ = q.admit(&junk(1), 100, N, MARGIN);
        let batch = q.drain_shuffled();
        assert_eq!(batch.len(), 1);

        q.begin_draining();
        assert_eq!(
            q.requeue(batch, 100, N, MARGIN).held,
            1,
            "a drain closes the door to NEW work, not to work already promised"
        );
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn a_requeue_gives_up_on_an_entry_that_can_no_longer_be_mined() {
        // The bound that makes "retry anything ambiguous" safe rather than
        // reckless. `chain::classify_publish_error` deliberately treats an
        // unrecognised node error as retryable, because dropping a recoverable
        // migration is unrecoverable while retrying a doomed one is cheap. That
        // is only true if the retrying STOPS.
        // Built directly rather than admitted, because every committed fixture
        // has `nExpiryHeight == 0` -- no expiry at all -- and admitting one would
        // make this test pass without exercising the branch it is named for.
        let q = Queue::new();
        let entry = Entry {
            key: [0xA1; 32],
            txid: Some("a1".repeat(32)),
            expiry: Some(150),
            tx_bytes: Zeroizing::new(vec![7u8; 64]),
            attempts: 0,
            received_height: 100,
            lane: Lane::Unverified,
            bundle_keys: Vec::new(),
            transparent_outputs: 0,
            has_child: false,
        };

        // Tip 200 is past the entry's expiry of 150, so no future flush can
        // publish it.
        let outcome = q.requeue(vec![entry], 200, N, MARGIN);
        assert_eq!(
            outcome,
            Requeued {
                held: 0,
                dropped_expired: 1,
                dropped_exhausted: 0
            },
            "an entry that cannot survive its next flush is given up on, not held"
        );
        assert!(q.is_empty());
    }

    #[test]
    fn a_payload_with_no_expiry_is_bounded_by_attempts_instead() {
        // The case expiry cannot bound at all. A payload that does not
        // deserialize has no expiry -- correctly, since "no expiry" means "never
        // expires" -- so nothing about the chain will ever retire it. Without a
        // count it would be re-offered to the network for the life of the
        // process, and every re-offer is another emission on a throttled lane
        // and another timing signal about one transaction.
        let q = Queue::new();
        let _ = q.admit(&junk(1), 100, N, MARGIN);

        // Round-trip it through drain/requeue until the attempts run out. Each
        // pass is one flush that failed to place it.
        let mut last = Requeued::default();
        for _ in 0..MAX_REQUEUE_ATTEMPTS + 1 {
            let batch = q.drain_shuffled();
            assert_eq!(batch.len(), 1);
            last = q.requeue(batch, 100, N, MARGIN);
        }

        assert_eq!(
            last,
            Requeued {
                held: 0,
                dropped_expired: 0,
                dropped_exhausted: 1
            },
            "a no-expiry payload must still stop being retried"
        );
        assert!(q.is_empty(), "and it must actually be gone");
    }

    #[test]
    fn a_requeue_within_its_budget_is_still_held() {
        // The other side of the bound: the common case is an indexer that was
        // briefly unreachable, and that entry must survive to the next flush.
        let q = Queue::new();
        let _ = q.admit(&junk(1), 100, N, MARGIN);
        let batch = q.drain_shuffled();

        let outcome = q.requeue(batch, 100, N, MARGIN);
        assert_eq!(
            outcome,
            Requeued {
                held: 1,
                dropped_expired: 0,
                dropped_exhausted: 0
            }
        );
        assert_eq!(q.len(), 1);
    }
}
