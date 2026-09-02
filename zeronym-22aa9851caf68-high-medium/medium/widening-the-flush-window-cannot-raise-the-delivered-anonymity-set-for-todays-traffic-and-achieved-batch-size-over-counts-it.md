# Widening the flush window cannot raise the delivered anonymity set for the traffic that exists today, because the batch and the dominant on-chain selector grow at the same rate; the two remedies the project names are null and unreachable, and `achieved_batch_size` is the metric that would have shown it

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/REVIEW.md:52-77` (design change #3's superseding note, "it doubles the batch for free" / "the cheapest available improvement to the k=1 problem"), `:195` ("Decisions for humans", first bullet: "The single highest-value lever is widening the migration expiry"), `:175` (the stated k=1 residual, quoted only for context), `audit-target/zeronym/README.md:34` ("The lever is adoption, not code"); the metric at `audit-target/zeronym/hub/src/batcher.rs:335-343`, `:361-378`, `:396-403` and `:412-420`; the constant at `hub/src/batcher.rs:39-41`
**Found by agent:** Global (focus area G3, anonymity-set arithmetic)
**In scope of audit?** Yes — `hub/REVIEW.md` and `README.md` are in scope as security claims, and `audit-context/AUDIT-INSTRUCTIONS.md` directs auditors to report "where the code or the public README claims more than the residual allows".

## Description

This issue is **not** a re-report of the acknowledged k=1 residual
(`hub/REVIEW.md:175`, `README.md:34`). That residual is stipulated. This issue is
about the two remedies the project names for it, both of which are stated as
quantities the code and the documents can act on, and neither of which delivers
what it is said to deliver:

1. `hub/REVIEW.md:69-77` — raising `N` from 10 to 20 *"doubles the batch for
   free"* and is *"the cheapest available improvement to the k=1 problem"*.
2. `hub/REVIEW.md:195` — *"The single highest-value lever is widening the
   migration expiry … Widening it would let N grow past 10 and **make batch size a
   real function of adoption instead of a fixed loss**."*

Both statements are about **batch size `k`**. The quantity that matters is not
`k` but the number of batch members that share the target's on-chain
distinguisher — which for today's non-ZIP-318 Orchard traffic is dominated by
`nExpiryHeight`.

A wallet built on librustzcash sets `expiry_height = target_height +
DEFAULT_TX_EXPIRY_DELTA` with the delta at 40 (`hub/src/batcher.rs:49-52` cites
this default itself), and a light wallet builds a transaction immediately before
broadcasting it. `nExpiryHeight` is therefore **a published, one-block-resolution
timestamp of when the wallet built the transaction**, committed under ZIP 244 into
the txid and readable by anyone off the chain forever.

The consequence is arithmetic and does not depend on any attacker capability:

* a `W`-block flush window admits `k ≈ λW` entries, where `λ` is the per-block
  arrival rate of diverted transactions across the whole fleet;
* those entries were built over `W` consecutive block heights, so the batch
  contains **about `W` distinct expiry values**;
* the expected number of batch members sharing the target's expiry is therefore
  `1 + (k−1)/W = 1 + λ − 1/W ≈ 1 + λ`.

**`W` cancels.** Against an adversary who reads `nExpiryHeight`, a 1152-block
flush window delivers exactly the same anonymity as a 1-block window. The
batching mechanism is not weakened by an attacker; it is neutralised by an
identity.

The same cancellation makes the second remedy unreachable rather than null.
Adoption raises `λ`, and `1 + λ` does grow with `λ` — but to deliver an anonymity
set of 8 requires `λ ≈ 7` diverted Orchard-touching transactions **per block**.
Blockchair's mainnet stats on 2026-08-18 report `transactions_24h = 5238` over
`blocks_24h = 1135`, i.e. **4.61 transactions per block of every kind on the whole
Zcash network**. The required rate is **1.5× the entire network's throughput**, and
**9.1×** the 0.77 Orchard-touching tx/block the design is sized against. Even the
absolute ceiling — every Zcash transaction being Orchard-touching and diverted
through the hub — yields a delivered set of **5.6**.

`README.md:34`'s *"The lever is adoption, not code"* is therefore not merely
wrong in the way the already-filed
`core-linkage-survives-in-the-attested-deployment-…` finding establishes
(selection is untouched by `k`); it is wrong in a way that has a hard numeric
ceiling no adoption level can pass, for as long as wallets stamp a per-block
expiry.

`REVIEW.md:195` reaches the right ask by the wrong mechanism, and the wrong
mechanism corrupts the ask. The mechanism it states is *"let N grow"*, which the
cancellation above shows is null. The mechanism that actually works is the other
half of the same sentence — *"an EPOCH-CANONICAL change adopted by all wallets in
the epoch, in the ZIP 318 sense"* — which works because ZIP 318 makes the expiry
value **identical for every wallet in a 30-day period**, deleting the timestamp
rather than lengthening it. Those are different asks with different outcomes: a
consortium acting on the literal words *"widening the migration expiry"* could
raise every wallet's delta from 40 to 1000 blocks, satisfy `MIN_WALLET_EXPIRY`
with enormous margin, permit any `N` — and change the delivered anonymity set by
exactly zero, because `tip + 1000` is still a one-block-resolution timestamp of
`tip`.

Finally, the one number the design uses to check itself measures `k`:
`batcher::flush` returns `achieved`, logs it as `achieved_batch_size`, and warns
only when `achieved <= 1` (`hub/src/batcher.rs:361-378`, `:396-403`, `:412-420`).
Its doc comment calls it *"the honest measure of the privacy the flush actually
delivered"* (`:335-336`). It is not: it counts published entries, not distinct
`(length, anchor, expiry)` classes. At `λ = 0.77` and `W = 20` it would report
`achieved_batch_size = 15` on a batch whose delivered anonymity set is about
**1.7**, and it would report a healthy number every time the window was widened.
The instrument the project would use to decide whether the anonymity claim holds —
and which `REVIEW.md:109-113` (design change #9) makes the launch gate — cannot
observe the failure.

## Attack Scenario and Steps

No attacker capability is required beyond the one already in the threat model, and
the point of this issue is that the *defence* does not scale, not that a new
attack exists. Concretely, for the operator of the indexer the shim fronts:

1. Client `C` sends one `SendTransaction` that the shim does not forward. The
   operator sees the absence (a conceded residual, `hub/REVIEW.md:181`) and
   timestamps it.
2. At the next height `≡ 0 (mod 20)` the hub publishes a batch of `k`
   transactions simultaneously. The batch is publicly enumerable (conceded,
   `hub/REVIEW.md:177`).
3. The operator reads `nExpiryHeight` off each member. For today's wallets that is
   `build_height + 40`, so it names the block in which each member was built.
4. The operator keeps only the members whose implied build height matches the
   block in which `C`'s request arrived. **On average `1 + λ` members survive**,
   and at any adoption level the network can physically sustain, that is under 6.
5. Steps 3–4 are unaffected if the project ships `FLUSH_INTERVAL_BLOCKS = 144` or
   `1152`: the surviving count is `λ`, not `λW`.

**Attack Requirements and Assumptions:**
- The operator's capabilities are exactly those already stipulated: they see which
  client's `SendTransaction` was not forwarded, and they can read the public chain.
- The arithmetic assumes wallets set a per-transaction expiry as a fixed delta
  from the chain tip at build time and broadcast promptly. That is the shipped
  behaviour of librustzcash-derived wallets, and `hub/src/batcher.rs:49-52` and
  `hub/REVIEW.md:52-57` both rest on the same assumption when they choose 40.
- **The estimate is sensitive to one unmeasured quantity.** `λ` — the per-block
  arrival rate of Orchard-touching transactions actually reaching hubs — has not
  been re-measured in this audit; `audit-context/EXTERNAL-CONTEXT.md` §7 flags the
  0.77/block figure as a 144-block sample taken four days after activation and
  never revisited. Every `λ`-dependent number here is **arithmetic, not
  observation, and is labelled as such**. The `W`-cancellation itself does not
  depend on `λ`.
- Offline signing or any delay between build and broadcast **widens** the spread
  of expiry values in a batch, so `1 + λ` is an upper bound on the delivered set,
  not a lower one.
- `anchorOrchard` for a latest-anchor wallet has the same one-block resolution and
  is derived from the same tip, so it is nearly perfectly correlated with expiry.
  It does not multiply the reduction; it is a second, independent confirmation of
  the same estimate. Transaction length (a function of action count) *is*
  independent and cuts further.

## Impact on Users

- Users of a hub that widens its window get a longer wait and no more privacy, for
  the traffic that exists on mainnet today.
- The project, its reviewers and any wallet consortium acting on `REVIEW.md:195`
  can spend the one expensive, cross-organisational ask they have — a coordinated
  wallet change — on the version of it that does nothing (a longer delta) instead
  of the version that works (a shared constant).
- The telemetry that is supposed to say "the anonymity claim does not hold at this
  adoption level" will say the opposite as soon as either remedy is applied.
- **The adoption lever also has a deadline, which nothing in the documents
  acknowledges.** The public tracker at <https://ironwood.live/v1/status>, fetched
  2026-08-18 at height 3,451,561, reports `orchard_at_activation` = 3,660,833 ZEC,
  `crossed_zec` = 2,913,084 and `pct_crossed` = 79.57 — i.e. **79.6 % of the
  migrating value crossed in the 23,418 blocks (20.3 days) since NU6.3**, most of
  it before zeronym's first attested enclave (2026-08-01) and all of it before the
  Nym transport went live (2026-08-14). Two honest caveats, both of which must
  travel with this number: value is not transaction count and not user count, so a
  small number of large holders could account for most of it; and the Sprout
  precedent the same tracker cites ("~25,409 ZEC still sits in Sprout, never having
  crossed" after eight years) says the *user* tail is long even when the value tail
  is short. Still, the population whose members hide each other is measurably
  draining, and "the lever is adoption" is a remedy that has to arrive before the
  thing it operates on is gone.

## Technical Details / Code Analysis

**The claim, verbatim** — `hub/REVIEW.md:69-77`:

> This is not merely more comfortable, it is the cheapest available improvement to
> the k=1 problem below, and it costs no wallet any change. A 20-block window
> accumulates twice what a 10-block window does: roughly 15 Orchard-touching
> transactions network-wide per window instead of 8, at the measured 0.77 per
> block. It does not solve k=1 at low adoption, because the limiting factor is the
> participating fraction rather than the window, but it doubles the batch for
> free.

and `hub/REVIEW.md:195`:

> The single highest-value lever is widening the migration expiry, and it is not a
> hub decision. 20 blocks is Brave's wallet default, not a consensus constant
> (librustzcash uses 40, Zingo 100). Widening it would let N grow past 10 and make
> batch size a real function of adoption instead of a fixed loss.

**The cancellation.** With `FLUSH_INTERVAL_BLOCKS = W` (`hub/src/batcher.rs:39-41`):

| quantity | scales as |
|---|---|
| batch size `k` | `λW` |
| distinct `nExpiryHeight` values in the batch | `W` |
| members sharing the target's expiry | `λW / W = λ` |

Worked out, with `λ` a free parameter because it is unmeasured:

| `λ` (diverted tx/block, fleet-wide) | `W = 20` | `W = 24` | `W = 144` | `W = 1152` |
|---|---|---|---|---|
| 0.05 (≈ a modal batch of 1 at `W=20`) | 1.00 | 1.01 | 1.04 | 1.05 |
| 0.25 | 1.20 | 1.21 | 1.24 | 1.25 |
| 0.77 (the design's assumed rate, at 100 % participation) | 1.72 | 1.73 | 1.76 | 1.77 |
| 4.61 (**every** Zcash transaction, measured 2026-08-18) | 5.56 | 5.57 | 5.60 | 5.61 |

Reading down a column shows adoption is a lever; reading across a row shows the
window is not one. The rightmost column is a 24-hour flush interval.

**The `nExpiryHeight` identity.** `hub/src/batcher.rs:49-56` is where the project
records the wallet behaviour this rests on:

```rust
/// The tightest wallet expiry the design commits to supporting, in blocks.
///
/// This is librustzcash's default (40), NOT Brave's 20. Brave is out of scope
/// for v1 and the ask to them is to raise their default to 40. If any wallet
/// with an expiry below 40 comes into scope, `FLUSH_INTERVAL_BLOCKS` must come
/// back down and the batch shrinks with it.
pub const MIN_WALLET_EXPIRY: u32 = 40;
```

A wallet honouring that ask still stamps `build_height + 40`. The number 40 is
irrelevant to the leak; the *variability across wallets within one window* is the
leak, and it is exactly `W`.

**The metric.** `hub/src/batcher.rs:335-343`:

```rust
/// Returns the achieved batch size, which is the honest measure of the privacy
/// the flush actually delivered.
pub async fn flush(queue: &Arc<Queue>, chain: &Arc<ChainClient>) -> usize {
    let batch = queue.drain_shuffled();
    let size = batch.len();
```

and `:361-367`, `:412-420`:

```rust
    let mut achieved = 0usize;
    ...
            Some(Publish::Accepted { .. }) | Some(Publish::AlreadyKnown) => achieved += 1,
    ...
    if achieved <= 1 {
        // Honest telemetry, not an error. At batch size 1 the anonymity set is
        // the transaction itself and the shuffle, the simultaneous publish, Nym
        // and the enclave are all irrelevant to it.
        tracing::warn!(
            achieved_batch_size = achieved,
            "batch provides no batching anonymity at this size"
        );
    }
```

`achieved` is a count of publications. The comment's own reasoning — "at batch
size 1 the anonymity set is the transaction itself" — is the right test applied to
the wrong variable: a batch of 15 transactions with 15 distinct expiry heights is
15 anonymity sets of size 1, and this branch stays silent for all of them.

**Why the fix direction is not "widen" but "make identical".** `SPEC-NOTES.md` §3,
quoting ZIP 318 directly:

> The bucketed rule makes the committed value **identical for every migration
> transaction — from any wallet** — whose scheduled broadcast falls within the same
> 30-day period, so an expiry height reveals only the coarse period in which its
> broadcast was scheduled.

Under that rule the batch contains **one** expiry value regardless of `W`, the
denominator collapses to 1, and the window becomes a real lever for the first time
(see the companion issue on what still partitions a conforming batch). The
distinction matters because `REVIEW.md:195` states the ask as a magnitude
("widening") when it is a *coordination* property, and only the parenthetical
"in the ZIP 318 sense" carries the part that works.

**Relationship to other filed issues — do not double-count.**
- `core-linkage-survives-in-the-attested-deployment-because-the-wallet-leg-is-unpadded-and-the-published-transaction-is-self-timestamping.md` (High) establishes *that* selection on `(length, anchor, expiry)` defeats batching. This issue supplies the quantity and shows that both stated remedies fail for different reasons. The user harm is the same harm; this is not a second instance of it.
- `hub-flush-interval-pinned-by-a-ceiling-that-does-not-bind-zip318.md` (Low) recommends widening the window. That recommendation is **correct only after wallets adopt ZIP 318's bucketed expiry**, and is null before it. An addendum has been appended to that issue pointing here so the report does not ship the ordering backwards.
- `review9-launch-gate-on-measured-batch-size-has-no-egress-path-so-the-accepted-k1-residual-can-never-be-measured.md` establishes that the launch-gate metric cannot leave the enclave. This issue establishes that, if it could, it would measure the wrong thing.

## Recommendations

**Ordering rule, before any of the numbered items: do NOT widen `FLUSH_INTERVAL_BLOCKS` first.** Other findings in this audit recommend widening it. Applied before wallets emit a network-wide canonical expiry, widening changes the delivered anonymity set by zero — and `hub/src/batcher.rs`'s startup assertion and `queue.rs`'s admission rule both refuse the wider window anyway while wallets stamp a 40-block delta (see Validation Information §3). Sequence the wallet-side change first.

1. Correct `hub/REVIEW.md:69-77`: raising `N` multiplies `k` but not the delivered
   anonymity set, because the number of distinct `nExpiryHeight` values in a batch
   is proportional to `N`. Say that the window is a lever only once expiry is
   epoch-canonical.
2. Restate `hub/REVIEW.md:195`'s ask as what it needs to be: **not "widen the
   migration expiry" but "adopt one shared expiry value per epoch"**, i.e. ZIP
   318's `floor(h / EXPIRY_MODULUS) * EXPIRY_MODULUS + 2 * EXPIRY_MODULUS`. A
   longer per-wallet delta satisfies the sentence as written and buys nothing.
   `zcash_protocol::zip318::expiry_height()` is already compiled into both attested
   binaries (PROGRESS.md open item 6l), so both sides can compute the same value
   from a crate they already link.
3. Change the metric at `hub/src/batcher.rs:361-421` from a count of published
   entries to a count of **distinct `(tx_len, anchorOrchard, nExpiryHeight)`
   classes in the batch, and the size of the smallest class**. The hub already
   holds every batch member's bytes and already re-parses them as telemetry
   (`REVIEW.md` design change #5), so this needs no new information. Keep
   `achieved_batch_size` for publication accounting; do not keep calling it the
   measure of delivered privacy.
4. Widen the `achieved <= 1` warning to fire on `min_class_size <= 1`, which is the
   condition the surrounding comment actually describes.
5. State in `README.md:34` the ceiling as well as the direction: at the measured
   whole-network transaction rate, no adoption level delivers an anonymity set
   above ~6 while wallets stamp a per-block expiry.
6. Re-measure `λ` before the report ships, and re-derive the table above against
   the measurement rather than against 0.77.

## Validation Information

**VERDICT: CONFIRMED. Severity: Medium (unchanged).** Validated 2026-08-18. The
central result — **the flush window `W` cancels out of the delivered anonymity
set** — was re-derived from scratch rather than inherited, every cited line in the
target was re-read, the wallet behaviour the derivation rests on was checked
against librustzcash's actual source rather than against the project's summary of
it, and the one external measurement was re-taken. **The derivation holds, the
arithmetic in every table cell is correct, and two independent code facts make the
result stronger than filed.** Four corrections and additions are recorded below;
none of them touches the conclusion.

**READ THIS FIRST IF YOU ARE FIXING SOMETHING ELSE IN THIS AUDIT.** Several other
findings recommend *widening the flush window* (`FLUSH_INTERVAL_BLOCKS`) as their
remedy — most directly
`hub-flush-interval-pinned-by-a-ceiling-that-does-not-bind-zip318.md`, which
carries a marked addendum pointing here. **Do not do that first.** Against the
adversary this system is built to stop, and for the wallet population that exists
on mainnet today, widening the window multiplies the batch and multiplies the
number of on-chain selector values by the same factor, so the delivered anonymity
set does not move. Widening buys latency and nothing else until wallets emit a
network-wide canonical expiry. The correct order is: (1) wallet-side ZIP 318
bucketed expiry (and request padding), then (2) widen the window. Reversed, the
engineering effort in (2) is spent for nothing and — because of the metric defect
below — the telemetry will report that it worked.

### 1. The `W`-cancellation: re-derived independently, and correct

Two derivations, both giving the same answer.

*Conditional-on-`k` (the issue's own form).* A batch published at a flush height
holds the entries admitted during the preceding `W` blocks. Under the wallet model
below, each entry's `nExpiryHeight` is a deterministic function of the block in
which it was built, and a light wallet builds immediately before broadcasting, so
a batch drawn from `W` consecutive heights carries about `W` distinct expiry
values, roughly uniformly. Conditioned on a batch of `k`, the expected number of
members sharing the target's value is `1 + (k-1)/W`. Substituting `k = λW` gives
`1 + λ - 1/W`.

*Unconditional (Poisson).* If diverted arrivals are Poisson at `λ` per block, the
number of *other* entries built in the target's own block is `Poisson(λ)` and is
independent of `W` outright. Expected delivered set `= 1 + λ`, exactly, for every
`W`.

Both forms agree, and neither has `W` in the answer. Spot-checked table cells
against `1 + λ - 1/W`: (0.05, W=20) = 1.00; (0.05, W=1152) = 1.049; (0.77, W=20)
= 1.72; (4.61, W=20) = 5.56; (4.61, W=1152) = 5.609. **Every published cell is
right to two decimals.** `7/0.77 = 9.09` ("9.1x") and `7/4.61 = 1.52` ("1.5x")
also check out, as does `0.77 x 20 = 15.4` behind "`achieved_batch_size = 15` on a
batch whose delivered set is about 1.7".

### 2. The wallet behaviour it rests on, checked at the source rather than quoted

The derivation is only as good as the claim that `nExpiryHeight` is a
one-block-resolution timestamp of build time. Verified directly against the
vendored upstream in `audit-context/zero/librustzcash`, not against the project's
paraphrase:

- `zcash_primitives/src/transaction/builder.rs:54` — `pub const DEFAULT_TX_EXPIRY_DELTA: u32 = 40;`
- `zcash_primitives/src/transaction/builder.rs:636-640` — for every non-coinbase
  transaction, `expiry_height = target_height + DEFAULT_TX_EXPIRY_DELTA`, where
  `target_height` is the builder's view of the chain tip.

So the committed value is `tip_at_build + 41`-ish, it is serialized in the clear
on chain, and it is committed under ZIP 244 into the txid. The delta being 40
rather than 20 or 1000 is irrelevant to the leak, exactly as the issue says: what
leaks is the *variance across wallets inside one window*, and that is `W`.

The operator's independent measurement of the target's value is better than the
issue claims, not worse: `GetLatestBlock` and the whole sync surface are
`Route::PassThrough`, so the operator served the victim the very tip the victim
then stamped. The `±1 block` in the issue is conservative.

### 3. TWO CODE FACTS THAT STRENGTHEN THE FINDING AND ARE NOT IN THE FILING

**(a) The window cannot be widened today at all — the code refuses to boot.**
`BatchParams::validate` (`hub/src/batcher.rs:96-115`), called unconditionally at
`hub/src/main.rs:45`, enforces
`flush_interval + mining_margin + delivery_lag <= min_wallet_expiry`. With the
shipped constants that is `W + 4 + 6 <= 40`, i.e. **`W <= 30`**. The `W = 144` and
`W = 1152` columns of the issue's table are therefore not merely useless, they are
unshippable while wallets stamp a 40-block delta.

That is exactly why `hub/REVIEW.md:195` asks for a *wider wallet expiry* — it is
the precondition that would let `N` grow. The finding's force is therefore
sharper than filed: **the project has correctly identified the gate, and the thing
behind the gate is worth zero.** A consortium that raised every wallet's delta from
40 to 1000 would unblock `validate()`, permit `W = 990`, produce batches ~50x
larger, and deliver `1 + λ` — the same number as today.

**(b) Admission control refuses the wide-window batch independently.**
`survives_next_flush` (`hub/src/queue.rs:380-392`) admits an entry only if
`expiry >= next_flush_height(tip, W) + mining_margin`. At `W = 144` an entry
carrying `tip + 40` fails that test for all but the last few blocks of each
window, so even with the startup assertion relaxed the queue would refuse most of
the traffic rather than accumulate it. Two independent mechanisms, both pointing
at the same wallet-side precondition.

### 4. The λ measurement: re-taken, correctly characterised, and slightly harsher than filed

`https://api.blockchair.com/zcash/stats`, re-fetched during this validation:
`transactions_24h = 4,846` over `blocks_24h = 1,146` at height 3,452,485 →
**4.23 transactions per block of every kind, network-wide**. The filed figure
(5,238 / 1,135 = 4.61) is a different sample of the same rolling 24-hour counter
on the same day; both are the same quantity and the drift is ordinary.

The characterisation in the issue is **correct and correctly bounded**: this is
every Zcash transaction of every kind, used *only* as a hard ceiling on `λ`, and
the issue already labels every λ-dependent number as arithmetic rather than
observation. Re-measured, the 100 %-diversion ceiling on the delivered set is
`1 + 4.23 = 5.23` rather than 5.6 — the finding gets marginally stronger, and the
"~6" in recommendation 5 remains the right thing to write. The
**Orchard-touching** rate was not re-measured here either (no reachable endpoint
exposes per-block pool composition); `EXTERNAL-CONTEXT.md` §7 stays open, and that
is stated in the issue rather than papered over.

One honest bound on the ceiling argument, which the report should carry: 4.23
tx/block is *today's network throughput*, not a consensus limit. The ceiling
statement is therefore "no adoption level reachable at the network's current
throughput", not "no adoption level ever". That qualification is already in
recommendation 5's wording ("at the measured whole-network transaction rate") and
should not be dropped.

### 5. The metric defect is real, and the launch-gate coupling is verified

Re-read at the source. `hub/src/batcher.rs:335-336` — *"Returns the achieved batch
size, which is the honest measure of the privacy the flush actually delivered."*
`:361-378` — `achieved` is incremented once per `Publish::Accepted | AlreadyKnown`
verdict, i.e. it is a **count of publications**. `:396-403` logs it as
`achieved_batch_size`; `:412-420` warns only on `achieved <= 1`. And
`hub/REVIEW.md:109` is the launch gate in as many words: *"Compute
`achieved_batch_size` per flush, export the distribution to the hub operator, and
gate LAUNCH on a measured distribution rather than gating each batch."*

The issue's characterisation is exact: a batch of 15 transactions with 15 distinct
expiry heights is fifteen anonymity sets of size 1, and this branch stays silent
for every one of them. The comment beside the warning (*"at batch size 1 the
anonymity set is the transaction itself"*) states the correct test and applies it
to the wrong variable. Recommendation 3 (count distinct
`(tx_len, anchorOrchard, nExpiryHeight)` classes and the smallest class) needs no
new information — the hub already holds every member's bytes and already re-parses
them as telemetry per `REVIEW.md` #5.

### 6. Not double-counted, and not a re-report of an accepted residual

- The *privacy failure itself* (selection on `(length, anchor, expiry)` defeats
  batching) belongs to the confirmed High
  `core-linkage-survives-in-the-attested-deployment-...`. **This issue does not
  re-report it** and says so.
- `hub/REVIEW.md:175` and `README.md:34` concede `k = 1` at today's adoption.
  `AUDIT-INSTRUCTIONS.md` forbids re-reporting a stated residual but explicitly
  invites reporting "where the code or the public README claims more than the
  residual allows". The claims tested here are the *remedies* offered alongside the
  residual — `REVIEW.md:69-77` ("the cheapest available improvement to the k=1
  problem", "doubles the batch for free"), `REVIEW.md:195` ("make batch size a real
  function of adoption instead of a fixed loss") and `README.md:34` ("The lever is
  adoption, not code") — none of which is a residual, and all three of which are
  wrong in the specific way documented. That is squarely inside the instruction.

### 7. Why Medium and not higher or lower

**Not High.** No new leak and no new attacker capability: the harm this issue
adds, on top of the already-confirmed High, is misdirected remediation plus an
instrument that will read green when the property fails.

**Not Low.** It is not theoretical. `REVIEW.md` #9 makes `achieved_batch_size` the
launch decision, so the defective metric is on the path to a real
ship/don't-ship judgement about a real privacy property; `README.md:34` is a
public, user-facing statement about what would fix the one limitation the project
concedes, and it names a lever that cannot reach its target; and the widening
remedy is recommended by at least one other finding in this same audit, so a
developer working the issue list without this one in front of them would spend the
effort and get nothing. Each of those lands on real users through a decision
someone is going to make.

**Two things a report author must not overstate.** (i) The `1 + λ` figure is
arithmetic on an unmeasured `λ`; the `W`-cancellation is what does not depend on
it, and that is the load-bearing half. (ii) The selector requires an adversary who
knows *when the target's diverted request arrived* — that is the shim's operator
(a conceded, stipulated capability), not an arbitrary chain observer. For a pure
chain observer with no target to align against, a bigger `k` is still a bigger
haystack. The finding is about the adversary the product exists to stop, and
should be written that way.


DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
