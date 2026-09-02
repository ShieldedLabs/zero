# A constant tip offset turns admission control into an attacker-tunable filter on transaction expiry, silently destroying a chosen fraction of migrations — and no rate-based tip defence can see it

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/queue.rs:199-204` (the expiry gate in `Queue::admit`) and `:357-393` (`next_flush_height`, `survives_next_flush`); `audit-target/zeronym/hub/src/batcher.rs:160-197` (`TipTracker::observe`, especially the unconditional first-observation branch at `:163-169`), `:200-232` (`is_stale`, `observed_height`, `cadence_height`), `:40-56` (`FLUSH_INTERVAL_BLOCKS`, `MINING_MARGIN`, `MIN_WALLET_EXPIRY`); `audit-target/zeronym/hub/src/main.rs:60-68` (the startup seed); `audit-target/zeronym/hub/src/server.rs:248-277` (`Hub::admit` and its refusal log); `audit-target/zeronym/hub/src/chain.rs:155-173` (`tip_height`, `max()` over endpoints, `as u32`); `audit-target/zeronym/shim/src/nym.rs:595-690` (dispatch-only submit, so the refusal is never surfaced); `audit-target/zeronym/deploy.env.example:16-17`, `:22-23`. Claims contradicted: `audit-target/zeronym/hub/REVIEW.md:40` (#2), `audit-target/zeronym/hub/src/queue.rs:11-19`.
**Found by agent:** Global (focus area G2 "the flush clock as an attack surface" / G14 "admission→flush→publish as one system"); see `audit-state/globals/G2-G14-G26-flush-clock-pipeline-and-isolation.md` and coordinator item 6u(d)
**In scope of audit?** Yes — priority area 2, "the anonymity mechanism"

## Description

`REVIEW.md` #2 replaced the early-expiry flush trigger with admission control, and
`queue.rs:11-19` states the property this is supposed to buy:

> **Admission control instead of an early-expiry flush (#2).** […] an entry is
> admitted only if it provably survives the next scheduled flush, which makes
> urgency unreachable rather than rate-limited.

The rule is `expiry >= next_flush_height(tip, N) + mining_margin`
(`queue.rs:379-392`). `tip` is `TipTracker::observed_height()`, which is whatever
the indexer said (`server.rs:256-261`, `batcher.rs:210-215`, `chain.rs:155-173`).

So the admission rule is a **threshold on the transaction's expiry height, and the
threshold's position is set by a number the adversary writes.** Raising the
reported tip by a constant Δ raises the threshold by Δ, refusing every transaction
whose expiry falls below it. That is not a side effect of an attack on the flush
clock; it is a second, independent lever with a different effect, a different
victim set, and — critically — a different detection signature.

**Why it is invisible to every defence proposed for the flush-clock lever.** The
already-confirmed `hub-tip-advance-unbounded-flush-clock.md` is about *racing* the
tip, and its remedies are all rate-based: bound the advance by
`last_advance.elapsed() / NOMINAL_BLOCK_SECS`, enforce a minimum wall-clock
interval between flushes, add an absolute plausibility floor. **A constant offset
violates none of them.**

- The offset can be established at the hub's very first tip query, because the
  `!state.observed` branch of `observe` adopts any height unconditionally
  (`batcher.rs:163-169`, seeded from `main.rs:60-68`). No jump ever occurs, so
  there is nothing for a rate bound to catch.
- Thereafter the reported height advances by exactly one per real block, so
  `last_advance` refreshes every block, `is_stale()` never fires, `cadence_height()`
  never free-runs, and the flush *rate* is exactly the designed one.
- The flush *phase* shifts by `Δ mod 20` real blocks, which is one of twenty
  equally ordinary phases and is not observable as anomalous.
- A "height must be plausible" floor (`> 3_000_000`, the constant
  `hub/tests/live_chain.rs:33-37` already uses) passes trivially.

The only feedback the system produces is a per-refusal
`tracing::info!(reason = "expiry_too_tight", …)` at `server.rs:273`, which reads as
*wallets are submitting transactions with too-tight expiries* — a wallet-side
problem — and which, on the deployed mixnet transport, never reaches the shim or
the wallet at all because submit is dispatch-only and the ack receiver is dropped
at construction (`shim/src/nym.rs:652`; filed as
`nym-submit-acks-are-never-read-so-every-hub-refusal-is-invisible.md`).

## Attack Scenario and Steps

Attacker: whoever answers the hub's `GetLightdInfo`. With
`INDEXERS=66.241.124.200:443` (`deploy.env.example:22`) that is one party, and
`tip_height`'s `max()` over a one-element list is the identity function.

1. From the hub's first tip query onward, the indexer answers `block_height =
   real_height + Δ` for a fixed Δ of its choosing. Everything else about its
   behaviour is honest.
2. `Hub::admit` computes `survives_next_flush(expiry, real + Δ, 20, 4)`, i.e.
   admits iff `expiry >= next_flush_height(real + Δ, 20) + 4`.
3. A transaction built by an ordinary librustzcash-based wallet carries
   `expiry = build_height + 40` — the default `batcher.rs:49-55` names as
   `MIN_WALLET_EXPIRY` and sizes the whole design against. Writing
   `r = (real + Δ) mod 20`, such a transaction built at the current height is
   admitted iff `r >= Δ - 16`:

   | Δ | fraction of ZIP 203-default traffic admitted |
   |---|---|
   | 0 | 20/20 = 100 % |
   | 20 | 16/20 = 80 % |
   | 26 | 10/20 = 50 % |
   | 30 | 6/20 = 30 % |
   | **36 or more** | **0/20 = 0 %** |

   (A wallet whose own chain view lags by `d` blocks builds `expiry = real - d + 40`
   and is refused at a correspondingly smaller Δ, so these are upper bounds on the
   Δ the attacker needs.)
4. A conforming ZIP 318 migration is untouched: its expiry is a bucketed absolute
   height 34,561–69,120 blocks ahead of broadcast (`audit-state/SPEC-NOTES.md` §3),
   so refusing it needs Δ ≈ 34,500 — at which point *everything* is refused. An
   unparseable payload is untouched at any Δ, because `expiry = None` always
   survives (`queue.rs:189-197`, `:385-387`).
5. Every refused submission is a migration the wallet was already told had been
   sent (`shim/src/hub.rs:231-240`), and the hub keeps no copy of it: the shim is
   stateless and the ack is not read, so nothing retries and nothing reports.

The attacker therefore holds a **continuous dial from "everything through" to
"nothing through"**, moved at zero cost, from a party the threat model already
designates as untrusted, with no jump, no rate anomaly, no staleness, no change in
flush cadence, and a log line that blames the wallets.

**Attack Requirements and Assumptions:**
- **Control of, or a bug in, a configured `ZIH_INDEXERS` endpoint.** Nothing else:
  no submissions, no fees, no mixnet position, no shim, no chain observation. This
  is **not** reachable by an anonymous party on the internet — see the severity
  bound in the validation section.
- Also reachable **accidentally**: an indexer serving a different network, a
  misconfigured height offset, an indexer that is permanently behind the chain at
  the moment the hub boots, or the `info.block_height as u32` truncation at
  `chain.rs:158` all produce a wrong tip that is adopted unconditionally at
  startup and never questioned afterwards.
- **What makes it less severe than it first looks:** the primary harm is
  destruction and denial, not deanonymisation. Refused transactions never enter a
  batch, so they are not published in a batch of one — they are not published at
  all. It is an anonymity attack only in the weaker sense that the surviving
  population is filtered along an axis (expiry, hence construction height) the
  attacker chooses.

## Impact on Users

Per `audit-state/SPEC-NOTES.md` §5, **no shipped wallet has been shown to implement
ZIP 318**, and the shim's interception predicate is `is_orchard_touching`, which
diverts every Orchard-touching transaction regardless of shape. So the diverted
population today is essentially all ZIP 203-default traffic, and Δ = 36 refuses
**all of it**, fleet-wide, silently, for as long as the attacker chooses — while
every wallet is told `error_code 0`, every hub log line attributes the refusals to
the wallets, and the hub's own health and status endpoints stay green.

A user in that state has an Orchard note they believe they have spent. They cannot
spend it again until the transaction they think is in flight expires — about 50
minutes at the 40-block default — and their retry then meets the same filter. The
funds are not lost (the note was never spent on-chain, and a syncing wallet
eventually sees non-confirmation), but the migration cannot complete for as long as
the attacker holds the dial, and nothing tells the user, the shim operator or the
hub operator why. Orchard is closed to new value by NU6.3/ZIP 258 and the migration
is the mandatory way out, so a sustained "cannot migrate" is a substantive harm.

At intermediate Δ the harm is worse in one respect and better in another: some
migrations succeed, so the failure looks like flaky infrastructure rather than an
outage, and nobody investigates.

## Technical Details / Code Analysis

The whole of admission control (`hub/src/queue.rs:379-392`):

```rust
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
```

with

```rust
// hub/src/queue.rs:362-368
pub fn next_flush_height(h: u32, n: u32) -> u32 {
    if n == 0 {
        return h;
    }
    ((h / n).saturating_add(1)).saturating_mul(n)
}
```

`tip` reaches it from the tracker, unfiltered (`hub/src/server.rs:252-261`):

```rust
        if self.tip.is_stale() {
            return Err(Refusal::TipStale);
        }

        match self.queue.admit(
            tx_bytes,
            self.tip.observed_height(),
            self.params.flush_interval,
            self.params.mining_margin,
        ) {
```

and `observed_height` is the raw last-observed value (`hub/src/batcher.rs:210-215`),
whose own doc comment states the property this attack breaks: *"This is what
admission checks expiry against, because admission must never be more optimistic
than the chain actually is."*

The branch that lets the offset exist from the first query, with no comparison
against anything (`hub/src/batcher.rs:161-175`):

```rust
    pub fn observe(&self, height: u32) {
        let mut state = self.write();

        if !state.observed {
            state.height = height;
            state.last_advance = Instant::now();
            state.observed = true;
            return;
        }

        if height > state.height {
            state.height = height;
            state.last_advance = Instant::now();
            return;
        }
```

Note that the *backwards* direction is bounded by `REORG_ALLOWANCE` and logged at
`warn!` (`:177-196`), while the forward direction and the initial adoption are
neither bounded nor logged. `observed_height` is never emitted in any log line
anywhere in the crate, so an operator cannot compare the hub's belief about the
chain with the chain.

The refusal's only trace (`hub/src/server.rs:272-275`):

```rust
            Admission::Refused(refusal) => {
                tracing::info!(reason = refusal.as_str(), "submission refused at admission");
                Err(refusal)
            }
```

`Refusal::ExpiryTooTight` renders as the fixed string `"expiry_too_tight"`
(`queue.rs:90-92`), which carries no indication that the tip it was measured
against might be wrong.

Finally, the design statement this defeats (`hub/REVIEW.md:40`, #2):

> Admission control makes the trigger unreachable rather than rate-limited: if
> every admitted entry provably survives the next scheduled flush, no entry can
> ever be urgent.

The conclusion holds — no admitted entry becomes urgent. What the argument does not
establish, and what this issue is about, is that the *admission predicate itself*
is now an adversary-positioned gate, so the attacker's lever moved from "make an
entry urgent" to "decide which entries exist".

### The offset is signed, and the negative direction inverts the failure (found during validation)

`observe`'s unconditional first-observation branch adopts a tip that is too **low**
just as readily as one that is too high, and a permanently-lagging indexer — or an
attacker choosing Δ < 0 — then holds that offset for the process's life, because
every subsequent report still advances at the chain rate.

With reported tip `T = real - L`, the deadline is computed `L` blocks too early
while the flush itself still happens within 20 real blocks of admission (the
cadence runs on the same shifted clock, so only its phase moves). The check
therefore **passes entries it should have refused**:

- honest tip: a transaction with `expiry = real + 5` is measured against
  `next_flush_height(real, 20) + 4` and is **refused** — correctly, and the wallet
  can immediately rebuild with a fresh expiry;
- lagging tip with `L = 1000`: the same transaction is measured against
  `next_flush_height(real - 1000, 20) + 4`, a number ~1000 blocks in its past, is
  **admitted**, is held until the next cadence boundary, is published after it has
  expired, and is refused by the node — which `flush` classes as a verdict and
  **drops permanently** (`batcher.rs:366-368`, `chain.rs:459-474`,
  `chain.rs:513-533`).

So in this direction admission control fails **open** rather than closed, and
REVIEW #2's "every admitted entry provably survives the next scheduled flush" is
false for the entries it matters most for. The affected population is narrower than
the positive-Δ case — it is submissions whose expiry is already tight (a wallet
whose chain view is >16 blocks stale, a resend near expiry, or a wallet with a
shorter expiry default such as the 20 blocks `batcher.rs:51-53` explicitly puts out
of scope) — which is exactly the population admission control exists to protect
from silent destruction. It is recorded here rather than filed separately because
the root cause, the code site and the fix are identical.

## Recommendations

- **Bound the tip against something the reporting party does not control.** A
  wall-clock rate bound (the fix proposed for `hub-tip-advance-unbounded-flush-clock.md`)
  is necessary but not sufficient, because it cannot see a constant offset. The
  offset is only detectable by comparison: against a second, independent endpoint
  (see `tip-and-verdict-aggregation-scale-in-opposite-directions-so-adding-indexers-fixes-one-lever-and-aggravates-three.md`
  for why the current `max()` fold makes that comparison useless), or against the
  hub's own wall clock anchored at a startup height an operator supplies out of
  band. A sanity floor and a sanity *ceiling* on the very first observation —
  `main.rs:60-68` is the one place a human-supplied expectation could be checked —
  would close the accidental half at near-zero cost.
- **Log the observed height and the admission threshold.** Both are aggregates
  carrying no per-entry information, so the counts-only rule (#157) permits them,
  and either one would have made this visible. Today neither is emitted anywhere in
  the crate. This is the cheapest instrumentation fix in the hub and it serves both
  tip findings at once.
- **Alarm on the refusal *rate*, not just the refusal.** A sustained non-zero
  `expiry_too_tight` rate has exactly two explanations — wallets with genuinely
  tight expiries, or a tip that is ahead of the chain — and the hub is currently
  unable to distinguish them or to report either.
- **Surface the refusal to the wallet.** On the deployed transport the wallet is
  told success and the refusal is discarded, which is what turns a refusal into a
  destruction. This is the same root cause as
  `nym-submit-acks-are-never-read-so-every-hub-refusal-is-invisible.md` and fixing
  it there fixes the silent half of this issue.
- **Correct `queue.rs:11-19` and `REVIEW.md` #2** to state that admission control
  makes urgency unreachable *given a trustworthy tip*, and that the tip is supplied
  by a party the same document treats as adversarial elsewhere.

## Validation Information

**Verdict: CONFIRMED. Severity: Medium (as filed), for consistency with the bound
already applied to the sibling tip issue.**

### The arithmetic was re-derived independently and it is correct

With `tip = real + Δ`, `flush_interval = 20`, `mining_margin = 4`, and a wallet
transaction built at the true tip with `expiry = real + 40`:

`next_flush_height(tip, 20) = 20·(⌊tip/20⌋+1)`. Writing `tip = 20q + r` with
`0 ≤ r < 20`, admission holds iff `real + 40 ≥ 20q + 24`, and `20q = tip − r =
real + Δ − r`, giving **`r ≥ Δ − 16`**. The fraction of block phases satisfying
that is `(20 − max(0, Δ−16))/20`, which reproduces every row of the table:
Δ=0→100 %, Δ=20→80 %, Δ=26→50 %, Δ=30→30 %, Δ≥36→0 %. Confirmed.

### Every mechanical claim re-verified against the target at HEAD

| Claim | Verified at |
|---|---|
| `!state.observed` adopts any height with no bound, no floor, no log | `hub/src/batcher.rs:163-169` |
| Forward advance is likewise unbounded; only the backward direction is guarded and logged | `hub/src/batcher.rs:171-196` (`REORG_ALLOWANCE = 10` at `:59`) |
| The startup seed feeds that branch directly from one indexer call | `hub/src/main.rs:60-68` |
| `observed_height()` is the raw value and is what admission uses | `hub/src/batcher.rs:210-215`, `hub/src/server.rs:252-261` |
| `survives_next_flush` is the whole of admission control, and `expiry = None` always passes | `hub/src/queue.rs:379-392`, applied at `:199-204` |
| `next_flush_height` is the next strict multiple of the interval | `hub/src/queue.rs:362-368` |
| Constants: interval 20, margin 4, `MIN_WALLET_EXPIRY = 40` (librustzcash's default) | `hub/src/batcher.rs:40-56` |
| Cadence runs on `cadence_height()/interval`, so a constant offset shifts phase only | `hub/src/batcher.rs:300-311`, `:219-232` |
| `is_stale()` never fires while the reported tip advances | `hub/src/batcher.rs:205-211` (`TIP_STALE_AFTER = 15 min` at `:63`) |
| `tip_height` is `max()` over endpoints, and the shipped list has one entry | `hub/src/chain.rs:155-173`; `deploy.env.example:22` |
| `u64 → u32` truncation is unchecked | `hub/src/chain.rs:158` (`info.block_height as u32`) |
| Refusal renders as a fixed string that blames the wallet | `hub/src/queue.rs:90-92`, logged at `hub/src/server.rs:273` |
| The ack is never read, so no refusal reaches the wallet | `shim/src/nym.rs:652` — `let (ack_tx, _drop_receiver) = oneshot::channel();` |
| `HubTransport::submit` has no `Refused` arm on the Nym path | `shim/src/hub.rs:230-249` |
| Nothing logs the observed height anywhere in the crate | grep: `observed_height` appears only at its definition and two call sites, never in a `tracing!` |
| `BACKEND` and `INDEXERS` are the same host/port in the shipped example | `deploy.env.example:16-17` vs `:22-23` — so the composition needs no collusion |

### The severity bound (coordinator item 6p), applied consistently

The G7 pass's bound is explicit and governs here: **all tip manipulation requires
control of a configured `ZIH_INDEXERS` endpoint; it is a hub-trust / robustness
defect, not an internet-reachable weapon.** The prior validator applied exactly
that bound to the sibling `hub-tip-advance-unbounded-flush-clock.md`, correcting it
High → Medium. The same bound applies unchanged here — nothing in this attack is
reachable by an anonymous party — so **Medium**, not High.

What holds it *at* Medium rather than lower:

1. **Against the party who can reach it, the attack is certain, free and
   invisible.** One integer per 30 s poll. At the shipped `n = 1` there is no
   aggregation to defeat, no rate check, no plausibility check, and no log line to
   alarm on.
2. **That party is adversary #1 by name.** `AUDIT-INSTRUCTIONS.md`'s trust
   boundaries state *hub → indexer … can lie about the tip and about publish
   verdicts*, and `deploy.env.example` points `BACKEND` and `INDEXERS` at the same
   host, so in the shipped example the hub's admission gate is held by a party who
   is simultaneously a shim's backing indexer.
3. **It is reachable with no attacker at all**, by a lagging, misconfigured or
   wrong-network indexer — including the negative-offset direction recorded above.
4. **The harm is fleet-wide and silent**: at Δ ≥ 36 every migration in the system
   is destroyed while every wallet is told `error_code 0` and every health surface
   stays green.

### Anti-double-counting

- **Against `hub-tip-advance-unbounded-flush-clock.md` (confirmed Medium).** That
  issue owns the *race*: an advancing tip that fires flushes early and collapses
  the batch to size 0 or 1 — an anonymity harm. Its validation section explicitly
  delegates the constant-offset form to this file (*"The constant-offset form of
  this … is filed separately as …; this issue's race form reaches the same state as
  a side effect"*), so the allocation is already agreed and is preserved here. The
  two are not the same finding: the remedies proposed for the race (rate bound,
  minimum flush interval, plausibility floor) **all pass** a constant offset, which
  is this issue's entire point, and the harm here is destruction rather than batch
  collapse.
- **Against `indexer-chooses-which-batch-members-reach-the-chain-…` (confirmed
  High).** That is the *publish* gate; this is the *admission* gate. Different
  code, different verdict path, different fix.
- **The isolation/batch-of-one framing is deliberately NOT claimed here.** A
  temporal admission window (open the gate for one epoch, close it for the rest)
  would isolate a target, but it requires either a jump or a stall, both of which
  belong to the sibling race issue and to `is_stale`'s existing fail-closed
  behaviour. Per coordinator item 6u(b) the isolation harm must not be stacked;
  this issue claims destruction and denial only.
- **The silent-loss half** is owned by
  `nym-submit-acks-are-never-read-so-every-hub-refusal-is-invisible.md`; it is
  cited here as the reason a refusal becomes a destruction, not re-counted.

### Corrections applied against the filed text

- *"The transaction is gone"* and *"no copy of it exists anywhere"* were softened.
  The user's note is not spent on-chain, and per coordinator item 7n a syncing
  wallet does eventually observe non-confirmation and can rebuild after expiry
  (~50 minutes at the 40-block default). What is genuinely destroyed is the
  submission, silently, with a false success already delivered — and the retry
  meets the same filter. The Impact section now says this precisely.
- The line references to `TipTracker::observe`, `survives_next_flush`,
  `Refusal::as_str` and the startup seed were re-derived and corrected
  (`batcher.rs:163-169`, `queue.rs:379-392`, `queue.rs:90-92`, `main.rs:60-68`).
- A **second direction** was added: the offset is signed, and a negative offset
  makes the same predicate fail *open*, admitting entries that are then published
  after expiry and dropped as a verdict. This was verified by working the cadence
  arithmetic through in both units; an earlier form of this claim (that *any*
  lagging tip delays publication past expiry) is **false** and is not stated —
  publication always occurs within 20 real blocks of admission because the cadence
  runs on the same shifted clock. The correct statement is the narrower one now in
  the text.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
