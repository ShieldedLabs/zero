# Following a reorg refreshes `last_advance`, so an oscillating tip masks staleness and freezes the flush cadence forever

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/batcher.rs:177-196` (the reorg arm of `TipTracker::observe`), interacting with `:204-208` (`is_stale`), `:217-230` (`cadence_height`) and `:299-312` (the epoch flush trigger); admission gate at `audit-target/zeronym/hub/src/server.rs:248-255`.
**Found by agent:** Local (file audit of `hub/src/batcher.rs`); validated 2026-08-18
**In scope of audit?** Yes

## Description

`TipTracker` decides the tip is stale from one timestamp, `last_advance`, which its
own field comment and `REVIEW.md` #8 define as *the last time a node advanced*:

> Declare the tip stale only when **no node has advanced** for 15 minutes.
> (`hub/REVIEW.md` #8)

The implementation refreshes `last_advance` not only on an advance but also when it
*follows a regression* inside `REORG_ALLOWANCE` (`batcher.rs:184-186`). A backwards
move is not an advance. Consequently a tip source whose reported height oscillates
inside the reorg allowance — `h, h-1, h, h-1, …` — refreshes `last_advance` on
**every single observation**, in both directions, while the effective height never
moves.

The result is that both halves of the stale-tip response specified in `REVIEW.md` #8
are disabled at once:

* `is_stale()` is never true, so `Hub::admit` keeps admitting migrations
  (`server.rs:252`) instead of failing closed with `Refusal::TipStale`; and
* `cadence_height()` returns the frozen `state.height` rather than free-running
  (`batcher.rs:224-227`), so `epoch` stops increasing and **`flush` stops being
  called** (`batcher.rs:300-309`) — permanently, because `last_flush_epoch` is a
  high-water mark that the non-matching arm leaves untouched.

The two effects share one root: `is_stale()` and `cadence_height()` are both pure
functions of `last_advance` (`batcher.rs:205-208`, `:222-230`), and `last_advance`
is the field the reorg arm wrongly stamps. Note that `is_stale()` has exactly one
consumer in the whole crate — `Hub::admit` (`server.rs:252`) — so nothing else in
the hub can notice.

The hub therefore accepts migrations it will never publish, for as long as the
oscillation continues, and the wall-clock fallback that exists precisely to keep
publishing through a stalled tip never engages because the tip does not look
stalled.

## Attack Scenario and Steps

1. The hub's tip source returns heights that alternate between `h` and `h-1` on
   successive 30-second polls. (Both values are inside `REORG_ALLOWANCE = 10`, so
   both are followed.)
2. `observe(h-1)` takes the reorg arm: `state.height = h-1`, `last_advance = now`.
   `observe(h)` takes the advance arm: `state.height = h`, `last_advance = now`.
   `last_advance` is thus refreshed roughly every 30 seconds, forever.
3. `is_stale()` never fires. `Hub::admit` continues to admit: `observed_height()` is
   `h` or `h-1`, and `survives_next_flush(expiry, h, 20, 4)` passes easily for any
   real wallet transaction, so migrations flow into the queue normally and shims are
   acknowledged.
4. `cadence_height()` returns `h` (or `h-1`). `epoch` therefore takes at most two
   adjacent values, and `last_flush_epoch` is only ever *raised* — the non-matching
   arm is `_ => {}` (`batcher.rs:308`), which leaves it alone. So after **at most
   one** further flush (the one that fires if the loop happens to see the higher of
   the two epochs first), `epoch > previous` never matches again. **Publication
   stops.**
5. Migrations accumulate until `MAX_QUEUE_BYTES` (64 MiB) is reached, after which
   further submissions are refused `Full`. Everything held expires unpublished. On
   the deployed mixnet path none of this reaches the wallet: `shim/src/hub.rs:231-240`
   answers `Submit::Accepted` at mixnet hand-off and its comment states that a hub
   refusal "is never surfaced here", so the wallet was told `error_code 0` for every
   one of them.
6. The only signal to the hub operator is the *absence* of `"flush published"` lines
   and the presence of `"chain tip moved backwards within the reorg allowance"`
   warnings — a message whose own comment says it is expected to be rare, and which
   is not wired to any alarm.

**Attack Requirements and Assumptions:**
- Deliberate: the hub's single indexer (`deploy.env.example:22`) simply alternates
  two heights. Costs one integer per poll, needs no transactions and no mixnet
  position, and — unlike inflating the tip — produces no implausible values that a
  future sanity check would catch, because a 1-block regression is exactly what a
  real reorg looks like.
- Accidental, and this is the path that makes the issue worth fixing regardless of
  any adversary. What is required is that the **observed** height oscillate within
  the allowance while making no net progress. Two realistic shapes produce that:
  - **One endpoint, two backends.** An indexer address behind a load balancer over
    two nodes a block apart answers from whichever backend it picks, so the observed
    height alternates. While both backends advance this is harmless jitter; once
    they stop advancing (their node halts, or their sync stalls) the alternation
    continues at a fixed pair of heights and the hub silently stops publishing.
  - **Several endpoints, one of them flaky — and note this gets *worse* with the
    multi-endpoint configuration the project recommends.** `tip_height` returns the
    `max()` over the endpoints that answered (`chain.rs:161-173`), so if the
    highest endpoint intermittently times out the max itself alternates between
    `h` and `h-1`. Combine that with a stalled set and the same freeze follows. See
    `tip-and-verdict-aggregation-scale-in-opposite-directions-…`.
- **Which stall is destructive matters, and the filed text did not separate them.**
  If the *chain* has halted, nothing expires (expiry is a height), the queue simply
  waits, and publication resumes when the chain does — the harm is bounded to
  delay and to holding plaintext longer than designed. If instead the *indexers'
  view* has stalled while the chain keeps advancing — a routine indexer failure, and
  precisely the case `REVIEW.md` #8's free-running clock exists for — then admission
  keeps accepting transactions that are aging toward their expiry inside a queue
  that will not be flushed, and they are destroyed. This second case is the graded
  one.
- No shim, no submitter and no chain observer capability is required.

## Impact on Users

Every migration admitted during a freeze that outlives its expiry is destroyed: the
wallet was told it was sent, the transaction is never broadcast, and it ages out
inside the hub's RAM. When the tip source eventually recovers, the first flush
offers the expired entries to the node, which refuses them; `flush` classes those
`Rejected` and **drops** them (`batcher.rs:368`) rather than requeueing. The user
has no error to react to and no copy to retry.

**Stated precisely, so it is not overstated.** The user's *funds* are not lost — the
transaction was never broadcast, so the note is not spent — but the submission is
gone and the user believes it succeeded. The delay before they can discover that is
~50 minutes for ordinary traffic and **30 to 60 days** for the ZIP 318 migration the
product exists for; that delay is owned by
`zip318-canonical-expiry-is-the-only-recovery-clock-…` and is cited, not re-counted.

> **CORRECTION 2026-08-18 (validation of the cited file — SUPERSEDES the sentence above).**
> The wallet does **not** wait for expiry. Both official Zcash light-wallet SDKs
> automatically resubmit a sent-but-unmined transaction for as long as it remains
> unexpired — the Android SDK at the head of every ~20 s sync loop and after every
> processed block batch (`CompactBlockProcessor.kt:573,615,723`; selection
> `mined_height IS NULL AND expiry_height > ?`), the iOS SDK at most once per 300 s
> (`TxResubmitter.swift:8-15`, `TransactionDao.swift:218-228`) — and the hub's
> payload-hash dedup makes the resend free. The wallet's non-confirmation signal comes
> from compact-block scanning, which the shim does not intercept (`proxy.rs:1068-1074`).
> Expiry is therefore the **retry horizon**, not the wait: ~50 minutes for the ZIP 203
> default traffic the shim also diverts, 30–60 days for a ZIP 318 migration. A
> *transient* loss self-heals within minutes; only a loss condition that **outlives the
> horizon** destroys the submission permanently — which is exactly what this issue's
> condition does, so this issue's severity is unaffected. Do not write "the user waits
> 30 to 60 days" in the report. Full refutation and the replacement paragraph:
> `issues/invalid/zip318-canonical-expiry-is-the-only-recovery-clock-and-a-lost-migration-freezes-the-users-notes-for-30-to-60-days.md`.


Secondarily, the enclave holds a growing set of plaintext migrations (up to 64 MiB)
for an unbounded period rather than the few minutes the design assumes, which
enlarges the blast radius of the stated residual "the hub sees every migration in
plaintext" — compromise or compulsion against the hub during the freeze now yields
hours or days of accumulated traffic rather than one flush window.

## Technical Details / Code Analysis

`hub/src/batcher.rs:161-197`, with the two arms that both refresh the timestamp:

```rust
        if height > state.height {
            state.height = height;
            state.last_advance = Instant::now();       // an advance: correct
            return;
        }

        if height < state.height {
            let drop = state.height - height;
            if drop <= REORG_ALLOWANCE {
                tracing::warn!(drop, "chain tip moved backwards within the reorg allowance; following it");
                state.height = height;
                state.last_advance = Instant::now();   // NOT an advance
            } else { /* ignored */ }
        }
```

Note also that `height == state.height` falls through both arms and does *not*
refresh `last_advance` — so a tip source that reports a **constant** height is
correctly detected as stale after 15 minutes. It is only the oscillating case that
defeats detection, which is why this is a live gap rather than a redundant one: the
straightforward failure is handled, the near-miss is not.

`hub/src/batcher.rs:217-230` — the free-running fallback is gated on the same
timestamp, so masking staleness also disables the fallback:

```rust
    fn cadence_height(&self) -> u32 {
        let state = self.read();
        if !state.observed { return 0; }
        let elapsed = state.last_advance.elapsed();
        if elapsed <= TIP_STALE_AFTER {
            return state.height;              // <-- always taken while oscillating
        }
        let estimated_blocks = (elapsed.as_secs() / NOMINAL_BLOCK_SECS) as u32;
        state.height.saturating_add(estimated_blocks)
    }
```

`hub/src/batcher.rs:299-312` — with a constant `cadence_height`, `epoch` is constant
and the flush arm is unreachable.

The module documentation at `batcher.rs:23-25` states the intended invariant that
this breaks: *"Staleness is a wall-clock fact (no node has advanced for
`TIP_STALE_AFTER`) …"* — the code's condition is "no node has been observed at all",
not "no node has advanced".

## Recommendations

- Do not refresh `last_advance` in the reorg arm. Follow the regression (adopting the
  lower height is the right call), but leave the advance timestamp alone: the tip has
  not advanced, and 15 minutes without an advance is exactly the condition the design
  wants to detect. If liveness evidence from a reorg is considered valuable, track it
  in a *separate* timestamp that does not gate `is_stale` or `cadence_height`.
- Add a test that drives `observe` with an oscillating sequence and asserts that the
  tracker becomes stale, and a test that asserts a frozen `cadence_height` eventually
  free-runs. Neither behaviour is covered today (see the companion test-coverage
  issue).
- Consider alarming when a flush has not happened for more than ~2 cadence intervals
  of wall-clock time, which would catch this class of freeze regardless of cause.

## Validation Information

**Verdict: CONFIRMED. Severity: Medium (as filed).** The defect is real, the "both
halves at once" claim is the distinguishing one and it holds, and the reachability
bound is the same one this audit applies to every tip finding.

### Both halves were verified independently, because that claim is what this issue turns on

**Half 1 — the fail-closed admission gate never fires.** `is_stale()` is
`!observed || last_advance.elapsed() > TIP_STALE_AFTER` (`batcher.rs:204-208`) and
nothing else. Under `h, h-1, h, h-1, …` at the 30 s poll interval
(`batcher.rs:71`), every observation lands in one of the two arms that stamp
`last_advance` — the advance arm at `:171-175` and the reorg arm at `:179-187` — so
`elapsed()` is bounded by 30 s forever and `TIP_STALE_AFTER` (15 min, `:62`) is
never reached. `Hub::admit` (`server.rs:248-254`) therefore never returns
`Refusal::TipStale`, and admission proceeds into `queue.admit(...,
observed_height(), ...)` where `survives_next_flush` (`queue.rs:380-393`) compares
the transaction's expiry against `next_flush_height(h, 20) + 4 <= h + 24`. Because
`h` is frozen *below* the advancing real chain, a freshly built wallet transaction
(`expiry = build + 40`) clears that bar by an ever-growing margin — so admission does
not merely stay open, it gets **easier** the longer the freeze runs.

**Half 2 — the free-running cadence never engages, and publication stops for good.**
`cadence_height()` (`batcher.rs:217-231`) returns `state.height` unchanged whenever
`last_advance.elapsed() <= TIP_STALE_AFTER`, which half 1 guarantees. `epoch` is
therefore `h/20` or `(h-1)/20`. In `run_with_poll_interval` the flush arm is
`Some(previous) if epoch > previous` and the fall-through is `_ => {}`
(`batcher.rs:300-309`), which does **not** lower `last_flush_epoch`. So once the
higher of the two epochs has been seen, no later observation can match. Verified by
enumerating both phases:
- `h mod 20 != 0`: both values map to the same epoch; the arm never matches; **zero**
  further flushes.
- `h mod 20 == 0`: the values map to `e` and `e-1`; at most **one** further flush
  (whichever ordering the loop sees first), then never again.

Either way the hub admits indefinitely and publishes nothing. Both halves of
`REVIEW.md` #8's stale-tip response — *"stop admitting"* and *"keep the cadence
running off a free-running wall-clock clock"* — are disabled by the same one-line
mistake. That simultaneity is exactly what distinguishes this issue from its
siblings, and it is confirmed.

### The `height == state.height` case is the proof that this is a bug, not a design choice

An equal reading falls through both `if` blocks and does **not** stamp
`last_advance`, so a tip source reporting a *constant* height is correctly detected
as stale after 15 minutes. The author's model is therefore "advance", not "answered"
— and the reorg arm silently departs from it. `REVIEW.md` #8 (`hub/REVIEW.md:103`)
says *"Declare the tip stale only when no node has advanced for 15 minutes"*, and
`batcher.rs:23-24` restates it. The straightforward failure is handled; the
near-miss is not.

Removing the stamp was checked for regressions and is safe: after a legitimate
reorg the chain rebuilds within `REORG_ALLOWANCE = 10` blocks, i.e. ~12 minutes at
the nominal rate, and the first rebuilt block is an advance that re-stamps
`last_advance` normally. A deeper or slower rebuild *should* register as stale.

### Reachability, and the same bound as every other tip finding

- **Deliberate:** requires control of a configured `ZIH_INDEXERS` endpoint. That is
  item 6p's bound, applied here exactly as it was to
  `hub-tip-advance-unbounded-flush-clock.md` (corrected High → Medium) and to
  `a-constant-tip-offset-…` (confirmed Medium). This is a hub-trust / robustness
  defect, **not** an internet-reachable weapon. Worth noting that it is the *least*
  detectable of the three: a 1-block regression is indistinguishable from a real
  reorg, so no plausibility check on the *value* can catch it.
- **Accidental:** genuinely reachable with no adversary, but the preconditions are
  narrower than the filed text implied and were tightened during validation. The
  observed height must oscillate within the allowance *while making no net
  progress*, which needs a stalled and heterogeneous tip source; and the destructive
  variant additionally needs the real chain to keep advancing while the indexers'
  view does not. Both are ordinary indexer-operations failures, and the second is
  precisely the scenario `REVIEW.md` #8's free-running clock was written for.
- **Not reachable by an anonymous party**, by a shim, or by a submitter.

### Corrections applied against the filed text

1. *"`flush` is never called"* → at most one further flush then never again, with the
   `h mod 20` case analysis. The filed absolute was very nearly right but not
   exactly.
2. *"Everything held expires unpublished"* → separated into the chain-halted variant
   (benign: nothing expires, because expiry is a height) and the indexer-stalled
   variant (destructive). The filed text conflated them, which would have overstated
   the harm in one case and understated the mechanism in the other.
3. *"Every migration admitted during the freeze is lost"* → the submission is
   destroyed; the funds are not. Same softening the validator of
   `a-constant-tip-offset-…` applied, for consistency.
4. Added the multi-endpoint accidental path — a flaky *highest* endpoint makes
   `max()` itself oscillate — which also makes this the fourth item on
   `tip-and-verdict-aggregation-…`'s "adding endpoints makes it worse" list.

### Checked and NOT claimed

- **"A stale tip causes an early flush" is REFUTED and is not asserted here.** This
  issue's whole point is the opposite: the free-run never engages at all. Nothing in
  the text claims an early publication, and nothing should be added that does.
- **The detection gap is cited, not re-counted.** `/healthz` is unconditionally 200
  and no health surface reads `is_stale()`; that belongs to
  `hub-health-surface-blind-to-the-states-that-destroy-migrations.md`.
- **The missing tests are cited, not re-counted** —
  `hub-batcher-staleness-and-free-run-paths-are-untested.md` owns the fact that no
  test drives `observe` with an oscillating sequence. Verified: the five
  `TipTracker` tests (`batcher.rs:473-516`) cover fresh-tracker, advance, small
  regression, large regression and cadence-while-fresh, and **none of them exercises
  the passage of time** — `is_stale()` and `cadence_height()` are only ever asserted
  immediately after an observation, so no test can distinguish a refreshed
  `last_advance` from a stale one. There is no clock seam to write such a test
  through.
- **The plaintext-accumulation residual is a secondary note, not the grade.** The
  64 MiB ceiling is real (`queue.rs:65`) and there is no expiry-based eviction, but
  the queue-growth harm is owned by `hub-queue-requeue-ignores-byte-budget-…`.

### Boundary against the three siblings in `TipTracker::observe`

This is the **mirror image** of `hub-tip-overshoot-latches-hub-permanently-stale.md`
(confirmed Medium): that issue pins `is_stale()` permanently **true** and the
free-running cadence permanently **on** (admission dead, empty flushes forever); this
one pins them permanently **false** and **off** (admission wide open, no flush ever).
Both come from the same root confusion in `observe` — treating *receipt of an
observation* as *evidence the chain advanced* — but they are triggered by opposite
inputs and need different fixes:

| Issue | Trigger | `is_stale()` | Cadence | Fix |
|---|---|---|---|---|
| `hub-tip-advance-unbounded-flush-clock.md` (Medium) | repeated advances | false | runs fast | bound the forward advance |
| `a-constant-tip-offset-…` (Medium) | constant offset from first observation | false | normal | plausibility / cross-endpoint check |
| `hub-tip-overshoot-latches-…` (Medium) | one advance beyond the allowance | **stuck true** | free-runs from a bogus height | forward bound + self-healing regression arm |
| **this issue** (Medium) | oscillation within the allowance | **stuck false** | **frozen** | do not stamp `last_advance` on a regression |

The forward-bound fix recommended by the first issue does **not** fix this one, and
this one's fix does not fix any of the other three. That is why they are four files.

### Severity justification — Medium

*Impact:* the hub silently stops publishing while continuing to accept, so every
migration admitted during an indexer-side stall long enough to outlive its expiry is
destroyed after the wallet was told it succeeded — and the mechanism specifically
defeats the safety net the design built for that exact scenario.

*Likelihood:* the deliberate form needs a configured indexer endpoint (item 6p's
bound). The accidental form needs a stalled, oscillating tip source, which is an
ordinary operations failure but not an everyday one.

*Why not High:* same bound as its three siblings — not reachable by an anonymous
attacker; grading it above `hub-tip-advance-unbounded-flush-clock.md` would be
inconsistent.

*Why not Low:* it disables **both** halves of a specified safety response at once,
in the failure mode that response was written for; it needs no adversary; nothing in
the hub's telemetry or health surface can see it; and the loss it produces is the
wallet-acknowledged silent destruction the whole design works to avoid.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
