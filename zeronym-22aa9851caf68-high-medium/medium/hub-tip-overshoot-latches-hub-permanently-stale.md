# One implausibly high tip reading latches the hub into "stale forever": admission stops for the whole fleet and nothing short of a process restart clears it

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/batcher.rs:160-197` (`TipTracker::observe`), `:204-208` (`is_stale`), `:217-230` (`cadence_height`); consumed at `audit-target/zeronym/hub/src/server.rs:248-255` (`Hub::admit`). Trigger reachable via `audit-target/zeronym/hub/src/chain.rs:155-173` and the `info.block_height as u32` cast at `chain.rs:158`.
**Found by agent:** Local (file audit of `hub/src/batcher.rs`); validated 2026-08-18
**In scope of audit?** Yes

## Description

`TipTracker::observe` accepts any forward height without bound, and refuses any
regression larger than `REORG_ALLOWANCE = 10`. The two rules compose into a latch:
once the tracker has recorded a height meaningfully above the real chain, **every
subsequent truthful observation is discarded as an implausible regression**, so
`last_advance` never updates again, `is_stale()` becomes permanently true 15
minutes later, and `Hub::admit` refuses every submission with `Refusal::TipStale`
from then on. The hub keeps running, keeps flushing an empty queue on the
free-running clock, and never recovers until either the real chain climbs to within
10 blocks of the bogus height or the enclave is restarted.

There is no operator override and no runtime control surface of any kind:
`TipTracker` has no reset, `observe` is the only writer of its state and is called
from exactly two places (`main.rs:63` at boot and `batcher.rs:291` in the poll
loop), and the indexer list is baked into an immutable attested enclave. Whether or
not the console is open (it is not, under the canonical `debug { enabled = false }`
runbook) changes nothing: the console carries output, not commands.

**Framing.** This is primarily a *robustness* defect, and it needs no adversary. Any
single implausibly high reading — from a lying indexer, a buggy one, a proxy serving
someone else's cached answer, an endpoint pointed at a different network, or a `u64`
that does not fit the unchecked `as u32` at `chain.rs:158` — is permanent. The
adversarial version is the same mechanism aimed deliberately, and is bounded exactly
as its two confirmed siblings are: it requires control of a configured
`ZIH_INDEXERS` endpoint, so it is a hub-trust defect, not something a stranger on
the internet can reach.

## Attack Scenario and Steps

1. The hub's single indexer (`deploy.env.example:22`) answers one `GetLightdInfo`
   with `block_height = H + K` for any `K > 10` — say `K = 5000` (~4 days of
   blocks). No other lie is ever needed.
2. `observe(H+K)` takes it (`batcher.rs:171-175`): `state.height = H+K`,
   `last_advance = now`.
3. The indexer reverts to telling the truth. Every later observation is
   `real_height < H+K` by more than 10, so it hits the else arm at
   `batcher.rs:188-195`, logs "ignoring a tip regression larger than the reorg
   allowance", and **leaves `last_advance` untouched**.
4. Fifteen minutes later `is_stale()` is true (`batcher.rs:205-208`) and stays true.
   `Hub::admit` (`server.rs:252-254`) now returns `Refusal::TipStale` for every
   submission, from every shim, indefinitely.
5. Meanwhile `cadence_height()` free-runs upward from `H+K`
   (`batcher.rs:224-229`), so `run` keeps crossing epochs and calling `flush` on an
   empty queue every ~25 minutes. From the outside — and from the hub's own logs,
   which print `flush_size = 0, "flush: nothing held"` (`batcher.rs:345-351`) — this
   is indistinguishable from "no one is migrating today".
6. Recovery requires `real_height >= H + K - 10`, i.e. `K - 10` blocks ≈ 4 days for
   `K = 5000`. With a large `K` (for example the `u32::MAX` that
   `chain.rs:158`'s unchecked `as u32` cast produces from a nonsense `u64`), it never
   happens.
7. And "recovery" is not the end of it. During the latch `cadence_height()` has been
   free-running from `H+K` for the whole `K - 10` blocks of real time, so
   `last_flush_epoch` has climbed to roughly `(H + 2K)/20` while the real epoch is
   `(H + K)/20`. `last_flush_epoch` is a monotone high-water mark
   (`batcher.rs:272`, `:300-309`), so when admission reopens **no flush happens for
   a further ~`K` blocks**, while admission is wide open. That second-order effect is
   the separately filed `hub-free-run-overshoot-suppresses-flushes-after-recovery.md`
   and is not counted again here; it is noted because it means the natural recovery
   path does not restore service either.

**Why the refusal is silent rather than a visible failure.** On the deployed mixnet
path the shim's submit is dispatch-only: `shim/src/hub.rs:231-240` returns
`Submit::Accepted` to the wallet the moment the frame reaches the mixnet, and its own
comment says *"There is no Refused arm: the hub's verdict is a full round trip away
and is deliberately not waited for … so a refusal is never surfaced here."* The
`TipStale` refusal therefore never reaches the wallet. The wallet has been told
`error_code 0`, keeps no retry state for it, and the transaction is simply gone.

**Attack Requirements and Assumptions:**
- **No adversary is required.** One wrong high reading from the hub's own indexer,
  whatever its cause, is sufficient and permanent. This is the primary path.
- The deliberate version needs **control of a configured `ZIH_INDEXERS` endpoint**
  — a party the audit's threat model already designates as able to lie about the
  tip, and the party `deploy.env.example:22` points at. It is not reachable by an
  anonymous party. This is the same bound applied to the two confirmed siblings
  (`hub-tip-advance-unbounded-flush-clock.md`, `a-constant-tip-offset-…`) and it is
  what caps the severity at Medium.
- The plaintext-hop variant ("reachable by a network attacker if `--indexer-tls` is
  unset, since `main.rs:33-38` only warns") is **not part of the graded finding**:
  the deployed configuration sets it (`deploy.env.example:23`), and
  `docs/AVOIDING-FALSE-POSITIVES.md` §7 correctly discounts a vulnerability that
  exists only under an explicitly insecure configuration nobody ships.
- With more than one endpoint configured the precondition gets *weaker*, not
  stronger: `tip_height` returns the `max()` (`chain.rs:161-173`), so **any one of
  `n` endpoints** can latch the hub with one packet. See
  `tip-and-verdict-aggregation-scale-in-opposite-directions-…`.

## Impact on Users

Every wallet that submits a migration while the hub is latched is told its
transaction was sent, and it never is. There is no error, no retry, and no record
anywhere: the shim keeps none, the hub refuses admission so the queue keeps none,
and the frame dies at the hub.

**Stated precisely, so it is not overstated.** The user's *funds* are not lost: the
transaction was never broadcast, so the note is not spent and the wallet's own
expiry handling eventually returns it to a spendable state. What is destroyed is the
*submission*, silently, together with the user's belief that the migration happened.
For ordinary (non-ZIP-318) traffic that discovery comes at ~50 minutes; for the ZIP
318 migration the product exists for, the canonical expiry is **30 to 60 days**
away, and nothing in the system produces a non-confirmation signal before then. That
delay is owned by
`zip318-canonical-expiry-is-the-only-recovery-clock-and-a-lost-migration-freezes-the-users-notes-for-30-to-60-days.md`
and is cited here, not re-counted.

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


Secondarily, this is a complete availability kill on the hub, which
`REVIEW.md`'s "Decisions for humans" section already flags as the cost of
fail-closed design ("it hands any DoS-capable attacker a total availability kill").
The new part here is that the kill is (a) permanent rather than lasting as long as
the attack, (b) triggerable by a single packet or a single bug, (c) *silent* to the
user because of the dispatch-only submit, and (d) invisible to the operator, because
`is_stale()` is read at exactly one place in the whole crate — `Hub::admit`
(`server.rs:252`) — and by nothing on the health surface (that gap is owned by
`hub-health-surface-blind-to-the-states-that-destroy-migrations.md`).

**And the only remedy is itself costly.** Clearing the latch requires terminating and
relaunching the hub process. The hub's Nym identity lives in an `Ephemeral` store in
RAM, and the module says so itself (`nym_driver.rs:33-36`): *"What still changes the
address: a real process restart (the store is in RAM, and a diskless enclave has
nowhere to persist it)."* So the restart hands every shim in the fleet a dead
`ZIS_HUB_NYM`, and a shim is an immutable managed app — each one needs a re-assemble
and redeploy, spending one of its five weekly certificate issuances
(`restarts-ledger-budget-model-omits-hub-forced-redeploys-…`, confirmed). The
restart also destroys whatever the RAM queue still holds. A one-packet fault
therefore costs a fleet-wide reconfiguration.

## Technical Details / Code Analysis

The latch is the interaction of three pieces that are individually reasonable.

`hub/src/batcher.rs:177-196` — regressions beyond the allowance are ignored *and do
not refresh `last_advance`*:

```rust
        if height < state.height {
            let drop = state.height - height;
            if drop <= REORG_ALLOWANCE {
                /* follow it, refresh last_advance */
            } else {
                tracing::warn!(drop, "ignoring a tip regression larger than the reorg allowance");
            }
        }
```

`hub/src/batcher.rs:204-208` — staleness is a function of `last_advance` only:

```rust
    pub fn is_stale(&self) -> bool {
        let state = self.read();
        !state.observed || state.last_advance.elapsed() > TIP_STALE_AFTER
    }
```

`hub/src/server.rs:248-255` — a stale tip stops admission entirely:

```rust
    pub fn admit(&self, tx_bytes: &[u8]) -> Result<Option<String>, Refusal> {
        if self.tip.is_stale() {
            return Err(Refusal::TipStale);
        }
```

`hub/src/chain.rs:155-159` — the cast that lets an out-of-range `u64` become an
arbitrary `u32` height:

```rust
        let queries = self.endpoints.iter().map(|addr| async move {
            let info: LightdInfo = self.unary(*addr, GET_LIGHTD_INFO, Empty {}).await?;
            Ok::<u32, BoxError>(info.block_height as u32)
        });
```

**The root error, stated in one line.** `observe` conflates *"I received an
observation"* with *"the chain advanced"*. The large-regression arm treats an
observation as evidence to discard and thereby latches; the small-regression arm
(`batcher.rs:179-187`, the separately filed
`hub-reorg-branch-resets-last-advance-masks-stale-tip.md`) treats an observation as
evidence of liveness and thereby masks. The two are exact opposites of each other:
this issue pins `is_stale()` permanently **true** and the free-running cadence
permanently **on**; that one pins them permanently **false** and **off**.

Note that this is not the same defect as the unbounded-forward-advance issue filed
separately (`hub-tip-advance-unbounded-flush-clock.md`, confirmed Medium), although
it shares the same missing guard: that one is about *repeated* advances driving the
flush clock, this one is about a *single* advance making the tracker permanently
unable to accept reality. Fixing the forward bound fixes both; fixing only the
flush-rate limit fixes neither.

The free-running clock is not a mitigation here. `REVIEW.md` #8 specifies it so that
"the cadence keeps running off a free-running wall clock" during a stall — and it
does, but it free-runs from the *bogus* height, so it publishes nothing (admission
is closed) and drifts further from reality with every hour.

## Recommendations

- Bound forward advances in `observe` against `last_advance.elapsed()` (see the
  companion issue); an advance that wall-clock cannot justify should be rejected and
  logged, exactly as an implausible regression already is.
- Make the "implausible regression" arm self-healing: if the same lower height (or a
  monotone sequence of them) persists for longer than `TIP_STALE_AFTER`, the
  tracker's own recorded height is the outlier, not the network's — adopt it and log
  loudly. A tracker that can never be corrected by the only source of truth it has
  is a latch by construction.
- Replace `info.block_height as u32` with a checked conversion, treating an
  out-of-range height as a failed tip query.
- Surfacing hub refusals to the wallet on the mixnet path would turn this from
  silent destruction into a visible failure. That fix is **owned by
  `nym-submit-acks-are-never-read-so-every-hub-refusal-is-invisible.md`** (confirmed
  Medium) and should be taken there, not here. *Correction applied during
  validation:* the filed parenthetical "have the shim retry against the other hub"
  does not work — `nym.rs:642` already submits every migration to **every**
  configured hub address (`shim-submits-every-migration-to-every-configured-hub-…`,
  confirmed High), so there is no unused hub to fail over to.
- Add the aggregate telemetry that would make this state visible at all: the
  observed height and the fact of a latched tracker are logged nowhere, and
  `is_stale()` has exactly one consumer in the crate. Both are aggregates, so the
  counts-only rule (#157) permits them. (Owner for the health-surface half:
  `hub-health-surface-blind-to-the-states-that-destroy-migrations.md`.)

## Validation Information

**Verdict: CONFIRMED. Severity: Medium (as filed).** The latch is real, permanent,
and re-derivable from three short functions; it needs no attacker at all; and the
adversarial form of it carries the same precondition as its two confirmed siblings,
which is what holds it at Medium rather than High.

### Every mechanical claim re-verified against the target at HEAD

| Claim | Verified at |
|---|---|
| A forward advance of any size is accepted verbatim and stamps `last_advance` | `hub/src/batcher.rs:171-175` |
| A regression larger than `REORG_ALLOWANCE` is ignored **and does not stamp `last_advance`** | `hub/src/batcher.rs:177-196`; `REORG_ALLOWANCE = 10` at `:59` |
| `is_stale()` reads nothing but `observed` and `last_advance` | `hub/src/batcher.rs:204-208` |
| A stale tip refuses **every** admission, on both ingress paths | `hub/src/server.rs:248-254`; `Hub::admit` is the single funnel for HTTP (`server.rs:541`) and mixnet (`nym.rs:321`) |
| `cadence_height()` free-runs from the *stored* height, which is the bogus one | `hub/src/batcher.rs:217-231` — the estimate is never written back into `state.height` |
| The `u64 → u32` truncation that can manufacture an arbitrary high reading | `hub/src/chain.rs:158` — `info.block_height as u32`, unchecked |
| `blockHeight` is `uint64` field 7 of `LightdInfo`, so an out-of-range value is representable on the wire | `zaino/packages/zaino-proto/proto/service.proto:105` |
| The shipped configuration is a single endpoint | `deploy.env.example:22` |
| The refusal is never surfaced to the wallet on the deployed transport | `shim/src/hub.rs:231-240` — no `Refused` arm, `Submit::Accepted` on mixnet hand-off |

### The decisive question: is recovery genuinely impossible short of a restart? YES

This was checked exhaustively rather than assumed.

- **`observe` is the only writer of `TipState`.** A repository-wide grep over
  `hub/src` and `hub/tests` returns exactly two production call sites:
  `main.rs:63` (the boot seed) and `batcher.rs:291` (the 30 s poll). `TipTracker`
  exposes no reset, no setter, and no constructor path reachable after startup.
- **No configuration change helps.** `--indexers` is startup-only
  (`hub/src/config.rs:36-46`) and the enclave image is immutable, so the operator
  cannot even repoint the hub at a different indexer without a redeploy.
- **No console command exists.** Under the canonical runbook
  (`debug { enabled = false }`) the parent has no console at all; under
  `deploy.sh`'s `DEBUG=1` default it has a read-only one. Neither is a control
  channel. There is no admin endpoint: `server.rs` serves `/healthz`,
  `/nym-status`, `/nym-address`, lookup and (optionally) submit, and nothing else.
- **The three ways out, all verified:**
  1. the real chain climbs to within `REORG_ALLOWANCE` of the bogus height — for
     `K = 5000` that is ~4.3 days, and for a `K` produced by the `as u32`
     truncation it is effectively never (`u32::MAX` ≈ 4.29e9 blocks ≈ 10,000 years
     at 75 s/block);
  2. the *same* source reports a still-higher value, which is an advance and does
     stamp `last_advance` — i.e. only the party who broke it can fix it, and doing
     so gives them an on/off switch over fleet-wide admission rather than a repair;
  3. a process restart, whose cost is the confirmed fleet-wide consequence recorded
     in Impact above.

So the filed claim survives, and it is stronger than filed: even route 1 does not
restore *publication* promptly, because `last_flush_epoch` has free-run ahead in the
meantime (step 7, added during validation).

### Reachable by a buggy endpoint, not only a hostile one — and that is the primary framing

The coordinator's question was whether this is an adversary-only finding. It is not.
`observe` has no plausibility floor, no ceiling, and no clock reference on the
forward path, so a single erroneous high reading from *any* cause is terminal. The
project has already written the plausibility check it needs, in the one place it
cannot act: `hub/tests/live_chain.rs:34-37` asserts `height > 3_000_000` with the
comment *"a plausible height proves we parsed a real answer rather than a default"*.
That test is `#[ignore]`d and environment-gated, and in any case a floor only catches
the *low* direction; nothing anywhere catches the high one.

Two accidental triggers were checked and are real: the unchecked `as u32` at
`chain.rs:158` (any `u64` ≥ 2^32 becomes an arbitrary `u32`), and an endpoint
serving a height for a chain other than the one the hub publishes to. A third — a
degenerate/empty body decoding to `LightdInfo::default()` — was checked and yields
`block_height = 0`, i.e. the **low** direction, which is the sibling's concern and
not this one. Stating that explicitly so it is not mis-cited later.

### What was checked and is NOT claimed

- **"A stale tip causes an early flush" is REFUTED and is not asserted here.**
  `cadence_height()` extrapolates from `last_advance.elapsed()` at the nominal block
  rate, so the free-running estimate lands back in phase with where the chain would
  have been; the observable direction is *late* (the cadence is frozen for the first
  `TIP_STALE_AFTER` and then steps forward by ~12 blocks at once), never early.
  `REVIEW.md` #8 also says "Never flush early because the tip is stale". The filed
  step 5 says only that empty flushes continue at ~25-minute intervals, which is
  correct.
- **The batch-collapse / isolation harm is not claimed here.** The one immediate
  off-cadence flush that the overshoot itself triggers belongs to
  `hub-tip-advance-unbounded-flush-clock.md`; this issue's harm is the state the hub
  is left in *afterwards*.
- **Loss of funds is not claimed.** Corrected in Impact above, consistently with the
  same softening applied to `a-constant-tip-offset-…` during its validation.
- **The detection gap is cited, not re-counted** (owner:
  `hub-health-surface-blind-to-the-states-that-destroy-migrations.md`), and so is the
  30-to-60-day discovery delay (owner: the ZIP 318 expiry issue).

### Boundary against the three siblings in `TipTracker::observe`

`observe` is 37 lines and carries four distinct filed defects. The split is
deliberate and each has a different fix:

| Issue | Input pattern | Effect | Fix |
|---|---|---|---|
| `hub-tip-advance-unbounded-flush-clock.md` (confirmed, Medium) | *repeated* advances | flush clock runs fast, batches collapse to 0 or 1 | bound the forward advance against wall clock |
| `a-constant-tip-offset-…` (confirmed, Medium) | a *constant* offset adopted at first observation | admission threshold moves; no jump, no staleness | absolute/cross-endpoint plausibility; rate-based fixes miss it |
| **this issue** | *one* advance beyond `REORG_ALLOWANCE` of reality | tracker can never re-converge; admission closed forever | the same forward bound, **plus** a self-healing regression arm |
| `hub-reorg-branch-resets-last-advance-…` | oscillation *within* `REORG_ALLOWANCE` | staleness masked; cadence frozen; nothing ever published | stop stamping `last_advance` on a regression |

The forward bound recommended by the first issue also fixes this one; it does **not**
fix the third or fourth. That is why the four are filed separately rather than merged.

### Severity justification — Medium

*Impact:* severe and total. Every migration submitted to a latched hub is silently
discarded, for every shim pointed at it, for as long as the latch holds — which is
"forever" in the accidental `as u32` case. The repair is a fleet-wide redeploy.

*Likelihood:* bounded by the same precondition as both confirmed siblings — control
of, or a fault in, a configured `ZIH_INDEXERS` endpoint. Not reachable by an
anonymous party (item 6p). Raised somewhat above the siblings by the fact that no
adversary is needed at all, and lowered by the fact that a single wrong high reading
is not an everyday event.

*Why not High:* the audit applies one consistent bound to every tip-manipulation
finding — it is a hub-trust / robustness defect, not an internet-reachable weapon —
and `hub-tip-advance-unbounded-flush-clock.md` was corrected High → Medium on
exactly this basis. Grading this one higher would be inconsistent with its siblings.

*Why not Low:* the state is permanent, has no runtime remedy, is invisible on every
health surface the hub exposes, and destroys wallet-acknowledged submissions the
whole time it holds.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
