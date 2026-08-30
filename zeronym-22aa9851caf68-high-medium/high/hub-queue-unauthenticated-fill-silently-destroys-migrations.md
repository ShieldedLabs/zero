# An unauthenticated queue fill refuses every genuine migration fleet-wide, and the deployed submit path never tells the wallet

**Severity**: High
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/queue.rs:170-240` (`admit`), `:63-65` (`MAX_QUEUE_BYTES`), `:181-197` (unparseable payloads get `expiry = None`), `:29-33` (bytes-are-the-budget and never-evict rules), `:35-39` (the no-submitter-identity rule that forbids a rate limit); `audit-target/zeronym/hub/src/server.rs:248-277` (`Hub::admit`); `audit-target/zeronym/shim/src/nym.rs:563-690` (dispatch-only submit); `audit-target/zeronym/shim/tests/divert_nym.rs:235-268` (the project's own test that the refusal is not surfaced); `audit-target/zeronym/hub/deploy/caution/caution.hcl.tmpl:51-55` (`ingress 0.0.0.0/0`)
**Found by agent:** Local (`hub/src/queue.rs`) — this is AUDIT-INSTRUCTIONS "unverified lead #6", now traced end to end; validated 2026-08-18
**In scope of audit?** Yes

## Description

`Queue::admit` refuses a submission with `Refusal::Full` once the queue's byte
total reaches `MAX_QUEUE_BYTES` (64 MiB). Three properties combine to make that
refusal reachable on demand by an anonymous attacker, and to make the
consequence silent:

1. **Admission is unauthenticated and cannot be rate-limited within the stated
   design.** The hub's Nym address is published at `GET /nym-address` with no
   ACL, and `queue.rs:35-39` states that an entry must never carry any submitter
   identifier — which is also the identifier a per-submitter quota would need.
   There is no rate limit, no cost, and no ACL on any submit path.
2. **Junk is a first-class queue citizen with no expiry.** REVIEW #5's
   re-parse-is-telemetry rule is implemented at `queue.rs:189-197`: a payload
   that does not deserialize is admitted with `txid = None` and `expiry = None`.
   `survives_next_flush` returns `true` unconditionally for `None`
   (`queue.rs:386-388`), so **arbitrary bytes are admissible at any tip, forever**.
   Filling the queue therefore costs nothing but bandwidth: no fee, no valid
   transaction, no key material.
3. **The never-evict rule makes the fill stick.** `queue.rs:31-33` and
   `queue.rs:216-225` implement "refuse at the door, never evict an admitted
   entry". That is the right rule against an attacker choosing *which* entry to
   remove, but it means whoever arrives first owns the budget for the rest of the
   window, and the attacker can always arrive first.

The consequence is not a fail-closed error the wallet sees. In the deployed
(mixnet) configuration the shim's submit is **dispatch-only**: it answers the
wallet `error_code 0` as soon as the frame is handed to the mixnet and never
awaits the hub's ack. The project's own test pins this:

```rust
// shim/tests/divert_nym.rs:235-260
    async fn a_hub_refusal_is_not_surfaced_under_best_effort() {
        // ... A hub that
        // would refuse (queue full) therefore does not surface that refusal to the
        // wallet ...
        let (shim, seen) = spawn_nym_shim(backend, OnSubmit::Refuse(AckRefusal::QueueFull), ...).await;
        ...
        assert_eq!(
            resp.error_code, 0,
            "best-effort: the wallet is answered success on dispatch, not the refusal"
        );
```

So a `Refusal::Full` means: the wallet was told the migration was sent, the shim
holds no copy (`shim/src/nym.rs:576-590` — the ack receiver is dropped and never
awaited), and the hub discarded the bytes. **Nothing anywhere retains the
transaction.**

## Attack Scenario and Steps

1. Attacker fetches the hub's mixnet address from `GET /nym-address`
   (`hub/src/server.rs:446-448, 469-479`), which is unauthenticated on an
   `ingress 0.0.0.0/0` enclave and is published deliberately.
2. Attacker submits `SubmitV1` frames carrying arbitrary distinct bytes.
   `hub/src/nym.rs:313-335` decodes them and calls `Hub::admit`, which calls
   `Queue::admit`. Each payload fails `Transaction::zcash_deserialize`, so it is
   admitted with `expiry = None` and never touches the expiry gate.
   Distinctness is required only because dedup keys on `sha256(tx_bytes)`
   (`queue.rs:206`), so varying one byte per frame suffices.
3. **Volume needed:** `MAX_QUEUE_BYTES = 67,108,864`; the mixnet submit path
   carries up to `MAX_NYM_TX_BYTES = FRAME_BYTES - 33 = 65,503` bytes per frame
   (`hub/src/wire.rs:76, 114, 127`). `ceil(67,108,864 / 65,503) = 1025` frames
   fill the budget. Each frame is a fixed 65,536 bytes on the wire, so ~64 MiB of
   frame payload.
4. **Rate needed (corrected during validation).** The queue is emptied at every
   flush (`drain_shuffled` sets `bytes = 0`, `queue.rs:250-262`), every
   `FLUSH_INTERVAL_BLOCKS = 20` blocks (~25 min at 75 s), so the attacker is not
   filling a bucket once — they are racing the drain. With `W = 1500 s`,
   `N = 1025` frames and an aggregate delivery rate `R` frames/s, the fill takes
   `T = N/R` and the queue sits at the cap for `W − T`, so the fraction of each
   window during which genuine migrations are refused is `f = 1 − N/(R·W)`.
   Using the shim's own model of gateway throttling (`shim/src/nym.rs:1087-1115`:
   `PACKET_BYTES = 2048`, `THROTTLED_PACKETS_PER_SEC = 1000/120 ≈ 8.33`), a
   64 KiB frame is 32 packets and one stock-rate client delivers ~0.26 frames/s:

   | Clients | Aggregate | `f` (window at the cap) |
   |---|---|---|
   | 2.6 | 0.68 frames/s ≈ 45 KB/s | **0 %** — merely matches the drain |
   | 5 | 1.3 frames/s ≈ 86 KB/s (0.7 Mbit/s) | ~48 % |
   | 26 | 6.8 frames/s ≈ 447 KB/s (3.6 Mbit/s) | ~90 % |
   | 263 | 68 frames/s ≈ 4.5 MB/s | ~99 % |

   So this is a **bandwidth-proportional flood, not a one-shot fill**: denying
   half of all migrations costs sub-megabit sustained, denying nine in ten costs
   a few megabits. Nym clients are free to create and the hub cannot distinguish
   or count them (`queue.rs:35-39` forbids it), so the only scarce input is
   bandwidth. (An earlier draft of this issue claimed four clients saturate the
   queue continuously; that is the rate that merely keeps pace with the drain
   and denies nothing. Corrected at validation.)
5. While `inner.bytes` is at the cap, every genuine migration arriving from any
   shim, for any operator, is refused `queue_full` (`queue.rs:223-225`).
6. The refusal is encoded as `AckRefusal::QueueFull` (`hub/src/wire.rs:280`) and
   sent back to the shim, which is not listening (step above). The wallet was
   told success minutes earlier. The migration is gone.
7. At each flush the hub publishes the attacker's ~1025 junk payloads to the
   operator's indexer. `chain::broadcast_batch` issues the whole
   (transaction x endpoint) product concurrently with no concurrency cap
   (`hub/src/chain.rs:208-210`) and `unary_inner` dials a fresh TCP+TLS+h2
   connection per call (`chain.rs:310-334`), so this is ~1025 simultaneous
   connections to the indexer every 25 minutes, each carrying a 64 KiB body of
   garbage. That plausibly gets the hub rate-limited or banned by the only
   endpoint it is allowed to reach (the enclave egress allowlist is a per-indexer
   `/32`, `caution.hcl.tmpl`), which then converts every publish into
   `Publish::Retryable` and feeds the unbounded-growth issue
   (`hub-queue-requeue-ignores-byte-budget-unbounded-growth.md`).

**Attack Requirements and Assumptions:**

- **Access:** internet only. No credential, no enclave compromise, no privileged
  mixnet position, no valid Zcash transaction, no fee.
- **What makes it realistic:** the hub's address is published by design; there is
  no ACL, no authentication and no rate limit on any submit path (the shim→hub
  channel has no authentication at all today — `OPEN-QUESTIONS.md` records STEVE
  as designed-not-built); unparseable payloads are *required* to be admitted by
  REVIEW #5; and the design rule at `queue.rs:35-39` structurally forbids the
  per-submitter identity any quota would need. The attack is also
  indistinguishable from honest load in the hub's own telemetry, which logs only
  `reason = "queue_full"` counts (`server.rs:273`).
- **What limits it:** the fixed 64 KiB mixnet frame makes each byte of budget
  cost ~1 byte of wire plus sphinx overhead, and gateway throttling caps a single
  client, so the attacker needs several concurrent clients sustained
  indefinitely rather than one burst. The clearnet `POST /` path, which would be
  far cheaper, is gated behind `ZIH_HTTP_SUBMIT` and is off by default
  (`hub/src/config.rs:96-97`, `hub/src/server.rs:438`), and
  `hub/deploy/caution/OPERATORS.md:268,285` tells operators to leave it off.
- **Prior art to weigh:** `hub/REVIEW.md` acknowledges the *class* under
  "Decisions for humans" — *"Fail-closed is a product decision ... it hands any
  DoS-capable attacker a total availability kill against every participating
  operator"* — and under inherent limits notes that *"Any party who can degrade
  the shim-to-hub path ... chooses the moment that migration is published"*.
  Neither states this vector (fill the hub's own byte budget with free junk),
  its cost, or the silent-loss consequence that dispatch-only submit creates.
  The review's own line 111 argues *against* a batch-size floor partly because
  *"refusing ... hands any DoS-capable attacker a total availability kill"* —
  yet `Refusal::Full` is exactly such a refusal.

## Impact on Users

- **A migration a wallet was told had been sent is destroyed with no error to
  anyone.** The wallet's notes are marked spent-pending against a transaction
  that will never reach a mempool. The user discovers this only by the
  transaction never confirming, and only if their wallet surfaces that.
- **Fleet-wide.** One hub serves every participating operator's shims — that
  shared queue *is* the anonymity set — so one attacker denies migration to all
  of them at once. Orchard is closed to new value by NU6.3/ZIP 258 and the
  migration is the mandatory way out, so "cannot migrate" is a substantive harm,
  not a cosmetic outage.
- **It is a privacy attack, not only an availability one.** The migrations that
  do slip through arrive into a batch whose only other members are the
  attacker's junk — and junk is rejected by the node at publish
  (`batcher.rs:368`, `Rejected` entries are dropped), so the *achieved* batch is
  just the handful of genuine transactions, or one. `batcher.rs:412-419` will
  duly warn `"batch provides no batching anonymity at this size"`. The attacker
  chooses, per window, how many genuine migrations get in.
- **Recovery makes it worse.** A wallet that notices and retries hits the same
  full queue. If the flood outlasts the transaction's expiry the wallet must
  rebuild it; for a non-ZIP-318 Orchard spend carrying ZIP 203's 40-block default
  that is under an hour.

## Technical Details / Code Analysis

The admission path, in order (`hub/src/queue.rs:170-240`):

```rust
    pub fn admit(&self, tx_bytes: &[u8], tip: u32, flush_interval: u32, mining_margin: u32) -> Admission {
        if tx_bytes.len() > self.max_bytes.min(MAX_TX_BYTES) {
            return Admission::Refused(Refusal::TooLarge);
        }

        // Telemetry parse. A failure is never a refusal (REVIEW #5).
        let (txid, expiry) = match Transaction::zcash_deserialize(&mut Cursor::new(tx_bytes)) {
            Ok(tx) => (Some(tx.hash().to_string()),
                       tx.expiry_height().map(|h| h.0).filter(|height| *height != 0)),
            Err(_) => (None, None),          // <-- arbitrary bytes: no txid, NO EXPIRY
        };

        if !survives_next_flush(expiry, tip, flush_interval, mining_margin) {
            return Admission::Refused(Refusal::ExpiryTooTight);
        }                                     // <-- unreachable for expiry = None

        let key: [u8; 32] = Sha256::digest(tx_bytes).into();
        let mut inner = ...;

        if inner.entries.contains_key(&key) {
            return Admission::Duplicate { txid };   // <-- vary one byte to avoid
        }

        if inner.bytes.saturating_add(tx_bytes.len()) > self.max_bytes {
            return Admission::Refused(Refusal::Full);   // <-- the target
        }
        inner.bytes += tx_bytes.len();
        inner.entries.insert(key, Entry { ... });
        Admission::Admitted { txid }
    }
```

and the expiry rule that junk sails past (`queue.rs:380-393`):

```rust
pub fn survives_next_flush(expiry: Option<u32>, tip: u32, flush_interval: u32, mining_margin: u32) -> bool {
    match expiry {
        None => true,
        Some(expiry) => {
            let deadline = next_flush_height(tip, flush_interval).saturating_add(mining_margin);
            expiry >= deadline
        }
    }
}
```

Note that the arithmetic itself is correct and was verified during this audit:
`next_flush_height` is strictly-after and saturating, the `h % N == 0` and
`h % N == N-1` boundaries behave as REVIEW #2 requires, and ZIP 203's
`nExpiryHeight == 0` is correctly folded to `None` rather than to height 0. The
weakness is not in the arithmetic; it is that the *rule has no purchase on
payloads that carry no expiry at all*, and REVIEW #5 requires those to be
admitted.

The refusal reaches the shim as a typed ack (`hub/src/nym.rs:313-326`):

```rust
            let kind = match hub.admit(&tx) {
                Ok(_txid) => AckKind::Accepted,
                Err(refusal) => AckKind::Refused(refusal.into()),
            };
            Some(wire::encode_ack(&nonce, kind).to_vec())
```

and the shim discards it, having already answered the wallet
(`shim/src/nym.rs:576-590`):

```rust
    /// A waiter is registered so the frame carries a nonce the hub CAN ack against
    /// (the frame still carries reply SURBs, M6), but its receiver is dropped and
    /// the reply is never awaited; the correlator sweeps the unclaimed waiter, and
    /// an unmatched ack is discarded.
```

`Refusal::as_str()` is designed for a shim that reacts to it —
*"Typed rather than a string because the shim reacts differently to each: a tight
expiry means hold and retry, an unavailable hub means try another hub"*
(`queue.rs:67-71`) — but in the deployed mixnet configuration no shim reads it.
The typed-refusal design and the dispatch-only submit design are individually
defensible and jointly produce a silent loss.

## Recommendations

1. **Make the hub's byte budget robust to junk, since junk is a required
   admission class.** Options that do not need a submitter identity:
   - Reserve a fraction of `MAX_QUEUE_BYTES` for payloads that *parse* as
     transactions, so an unparseable flood can never consume the whole budget.
     This keeps REVIEW #5 (unparseable is admitted and published) while denying a
     free flood the ability to displace real migrations. It costs the hub only
     the ability to admit an unlimited number of unreadable payloads, which is
     not a property any honest user needs.
   - Refuse a *new* payload rather than an admitted one, but choose which class
     to refuse by parseability rather than by arrival order.
2. **Surface `queue_full` to the wallet.** The dispatch-only trade is documented
   as "the wallet learns the true outcome by confirmation", which is true for a
   *queued* transaction and false for a *refused* one. Either await the ack for
   the refusal-bearing cases, or have the shim retry-on-ack-refusal in the
   background, or (cheapest) have the hub not refuse at all for the parseable
   class per (1). A user must not be told `error_code 0` for bytes nobody holds.
3. **Bound `broadcast_batch` concurrency** (`chain.rs:208-210`) so a large batch
   cannot open a thousand simultaneous connections to the one endpoint the
   enclave is permitted to reach. Today the flood's second-order effect (getting
   the hub banned) may be worse than its first-order effect.
4. **Raise the per-entry floor.** `hub/src/server.rs:534` rejects only *empty*
   bodies and the mixnet path accepts any `declared <= MAX_NYM_TX_BYTES`
   including 0; a minimum plausible transaction size would cost the attacker
   nothing here (frames are fixed size) but would matter if the clearnet submit
   path were ever enabled.
5. **State the residual honestly if it is accepted.** `README.md`'s "Protected"
   list and `REVIEW.md`'s inherent-limits section should say that any
   internet-connected party can, at low cost, prevent every participating
   operator's users from migrating, and that under dispatch-only submit those
   users are told the migration succeeded.

## Validation Information

**Verdict: CONFIRMED. Severity: High (as filed) — but the cost arithmetic in
step 4 of the attack scenario was wrong by roughly an order of magnitude and has
been corrected in place.** The mechanism, the reachability and the decisive
silent-loss leg all hold; what did not hold was "four clients saturate the queue
continuously", which described the rate that merely *keeps pace with* the drain
and therefore denies nothing.

### Every mechanical claim re-verified against the target

| Claim | Verified at |
|---|---|
| `MAX_QUEUE_BYTES = 64 MiB`, bytes are the budget | `hub/src/queue.rs:65`, module docs `:29-30` |
| A payload that does not deserialize is admitted with `txid = None, expiry = None` | `hub/src/queue.rs:190-197` (`Err(_) => (None, None)` at `:196`) |
| `survives_next_flush(None, ..) == true` unconditionally, so junk never meets the expiry gate at any tip | `hub/src/queue.rs:380-393` |
| `Refusal::Full` once `bytes + len > max_bytes` | `hub/src/queue.rs:222-225` |
| Never evict; first arrival owns the budget | module docs `hub/src/queue.rs:31-33`, implemented at `:216-231` |
| Dedup on `sha256(tx_bytes)`, so distinct payloads are required | `hub/src/queue.rs:206-214` |
| No submitter identity exists, and the design forbids one | `hub/src/queue.rs:35-39` — *"There is deliberately no contributor, channel or session identifier on an entry, and there must never be one."* |
| Mixnet admission is inline, unbounded, with no ACL and no rate limit | `hub/src/nym.rs:148-215`; `MAX_CONCURRENT_LOOKUPS = 64` (`:54`) bounds the *lookup* arm only, and `:41-46` says so explicitly ("Admission never waits on this") |
| The hub's Nym address is published to anyone | `hub/src/server.rs:446-449` (`NYM_ADDRESS_PATH` → `GET`, no auth), `:462-468` ("this endpoint is reachable by everyone"), on `ingress { cidr_ipv4 = "0.0.0.0/0" }` (`hub/deploy/caution/caution.hcl.tmpl`) |
| A frame carries at most `MAX_NYM_TX_BYTES = 65,536 − 33 = 65,503` bytes | `hub/src/wire.rs:114`, `:127`, enforced in `decode_submit` at `:320-322` |
| The clearnet `POST /` path really is closed | `hub/src/config.rs:82-98` (`default_value_t = false` at `:98`); grepped the whole tree — `ZIH_HTTP_SUBMIT` appears only in `OPERATORS.md:268,285` ("leave off") and in tests. The deploy never sets it, so the attack must pay mixnet bandwidth |
| The queue drains fully at each flush and the junk does not persist | `hub/src/queue.rs:246-262` (`drain_shuffled` sets `bytes = 0` at `:259`); `hub/src/batcher.rs:364-378` — a `Rejected` verdict is counted at `:368` and never requeued, so junk the node refuses does not come back |
| A flushed batch is published as one fresh TCP+TLS+h2 connection per (transaction × endpoint), uncapped | `hub/src/chain.rs:208-210`, `:300-334` |

**The decisive leg — the refusal never reaches the wallet — is confirmed three
ways.** `shim/src/hub.rs:231-240` has no `Refused` arm and returns
`Submit::Accepted` on hand-off; `shim/src/nym.rs:652` builds the ack waiter
and immediately drops its receiver (`let (ack_tx, _drop_receiver) = oneshot::channel();`);
and the project's own test `shim/tests/divert_nym.rs:235-267`
(`a_hub_refusal_is_not_surfaced_under_best_effort`) drives the mock hub with
`OnSubmit::Refuse(AckRefusal::QueueFull)` and asserts `resp.error_code == 0`
with the comment *"best-effort: the wallet is answered success on dispatch, not
the refusal"*. A `Refusal::Full` is therefore not an error the user sees; it is
a transaction the user was told had been sent, held by nobody.

Worth recording for the report: `hub/src/config.rs:82-88` justifies leaving the
clearnet submit path off on the grounds that *"the mixnet address IS the
credential"* — while `hub/src/server.rs:462-468` publishes that credential to
every unauthenticated caller by design. The two statements cannot both be load
bearing, and this issue is what falls out of the gap.

### The corrected cost, and why the original figure was wrong

The queue is emptied at every flush, so the attacker is not filling a bucket
once; they are racing the drain. Let `W = 1500 s` (20 blocks × 75 s),
`N = 1025` frames, and `R` the attacker's aggregate delivery rate in frames/s.
Filling takes `T = N/R`, so the queue sits at the cap for `W − T` of each
window, and the fraction of the window during which genuine migrations are
refused is `f = 1 − N/(R·W)`.

At the crate's own throttled-client model (`shim/src/nym.rs:1087-1115`: a
64 KiB frame is 32 packets, 8.33 packets/s), one stock client delivers ~0.26
frames/s:

| Clients | Aggregate | `f` (window at the cap) |
|---|---|---|
| 2.6 | 0.68 frames/s ≈ 45 KB/s | **0 %** — this is the rate the filing called "saturating"; it only matches the drain |
| 5 | 1.3 frames/s ≈ 86 KB/s (0.7 Mbit/s) | ~48 % |
| 26 | 6.8 frames/s ≈ 447 KB/s (3.6 Mbit/s) | ~90 % |
| 263 | 68 frames/s ≈ 4.5 MB/s | ~99 % |

So the attack is real but is a *bandwidth-proportional* flood, not a one-shot
fill: denying half of all migrations costs sub-megabit, denying nine in ten
costs a few megabits, sustained. The filing's "roughly four clients saturate the
queue continuously" has been replaced with this curve.

### `AVOIDING-FALSE-POSITIVES.md` §5 applied rigorously

§5 asks: *what resources would the attacker need, and what would stop them?*

*Resources:* 0.7–3.6 Mbit/s of sustained mixnet traffic from Nym clients that
cost nothing to create and are unattributable by construction. No credential,
no valid transaction, no fee, no on-chain activity, no privileged network
position. This is a single cheap VPS.

*What would stop them:* nothing in the target. There is no ACL, no rate limit,
no proof of work, no cost, and no per-submitter accounting — and by
`queue.rs:35-39` there must never be a submitter identifier, so the usual quota
cannot be built. The two infrastructure limiters §5 normally credits (proxy
timeouts, upstream connection caps) do not exist on this path: admission is
inline in the mixnet listener with no bound at all. The only real limiter is
mixnet throughput to the hub's single pinned gateway, which is what the curve
above prices.

*Why this is not the §5 false-positive shape.* This is not an amplifier — the
attacker pays about one byte per byte of budget — and §5's canonical false
positives (requesting a 100 GB database, unlimited connections) are dismissed
because the *outcome* is downtime that infrastructure absorbs. Here the outcome
is not downtime. A refused submission is a transaction the wallet was told had
succeeded (`shim/src/hub.rs:231-240`), that the shim keeps no copy of
(`shim/src/lib.rs:32-34`, stateless by design), and that the hub discarded. It
therefore lands on the *"violates integrity guarantees"* row of the severity
table, not the *"causes DoS"* row. On this target the guide's usual test
inverts: the attacker's resource is bandwidth, and the damage is silent
destruction of transactions the user believes are spent.

### `AVOIDING-FALSE-POSITIVES.md` §6 (intentional design) — stated and answered

Three of the ingredients are deliberate and correct, and the report must not
ask for them to be reversed:

- **Unparseable payloads must be admitted** (REVIEW #5). Refusing them would
  invert the shim's fail-safe into a leak. Correct as designed.
- **Never evict an admitted entry** (`queue.rs:31-33`). Eviction would hand an
  attacker the *selection* lever, which is worse. Correct as designed.
- **No submitter identifier, ever** (`queue.rs:35-39`). An operator-to-migration
  map inside the enclave is precisely what the system exists to destroy.
  Correct as designed — and this is why "add a rate limit" is not an available
  fix and must not be the headline recommendation.

What is *not* a design decision is the composition of those three with a single
undifferentiated byte budget: nothing in `REVIEW.md` states, or appears to have
considered, that an unparseable flood may consume 100 % of `MAX_QUEUE_BYTES`
and displace the parseable class. Recommendation 1 (reserve a fraction of the
budget for payloads that parse) closes exactly that and needs no identity, no
rate limit and no change to any of the three rules above. `hub/REVIEW.md`
acknowledges the *class* ("Fail-closed … hands any DoS-capable attacker a total
availability kill") but neither this vector, nor its cost, nor the silent-loss
consequence dispatch-only submit gives it.

### Impact claims checked

- *Fleet-wide:* yes. One hub serves every participating operator's shims; that
  shared queue is the anonymity set.
- *"The migrations that slip through arrive into a batch whose only other
  members are the attacker's junk":* correct, and in fact stronger than filed —
  the node rejects the junk at publish and `flush` **drops** `Rejected` entries
  (`batcher.rs:364-378`), so the junk never reaches the chain and a chain
  observer sees only the genuine members. The attacker chooses, per window, how
  many of those there are.
- *Recovery:* a wallet that retries re-enters the same full queue. Note the one
  softening fact, recorded honestly: a wallet that resends identical bytes is
  deduplicated at the hub (`queue.rs:206-214`) and will succeed if it happens to
  retry during a gap in the flood, so the loss is not unconditionally
  permanent — it is permanent for any migration whose expiry runs out first.

### Severity justification — High

*Impact:* a mandatory migration the wallet reported as sent reaches no mempool,
with no error surfaced anywhere and no component retaining the bytes; the user's
notes stay pending-spent until expiry, in a pool NU6.3/ZIP 258 has closed to new
value. It hits every operator's users at once.

*Likelihood:* unauthenticated, internet-reachable, free, unattributable
(the mixnet is the attacker's cover too), indistinguishable from honest load in
the hub's own telemetry (`server.rs:272-274` logs only `reason = "queue_full"`),
and priced at sub-megabit to low-megabit sustained.

*Why not Critical:* no funds are stolen and no key is compromised; the attack
must be sustained rather than executed once; a retry into a later window can
still succeed; and at today's adoption the *privacy* half of the harm is largely
redundant with the already-accepted "modal batch is 0 or 1" residual.

*Why not Medium:* it needs no special configuration, no privileged position and
no credential — only bandwidth — and its primary consequence is the silent
destruction of a transaction the user was told had succeeded, which the severity
guidance places above a mere availability outage.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
