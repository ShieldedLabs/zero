# A single indexer can drive the hub's flush clock arbitrarily fast, collapsing every batch to size 0 or 1

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/batcher.rs:160-197` (`TipTracker::observe`), `:299-312` (the epoch-crossing flush trigger in `run_with_poll_interval`), consuming `audit-target/zeronym/hub/src/chain.rs:148-173` (`ChainClient::tip_height`). Deployed configuration: `audit-target/zeronym/deploy.env.example:22-23` (`INDEXERS=66.241.124.200:443`, a single endpoint).
**Found by agent:** Local (file audit of `hub/src/batcher.rs`); validated 2026-08-18
**In scope of audit?** Yes

## Description

The hub publishes a batch when, and only when, the chain height crosses a multiple
of `FLUSH_INTERVAL_BLOCKS`. `batcher.rs` is explicit that this clock must not be
influenceable by anyone (`batcher.rs:8-22`):

> Every conditional trigger is a lever someone else can pull. […] The height is
> taken as the MAX over all nodes that answer […] because a single lagging or
> hostile node would otherwise be a second independent lever on the flush clock:
> a stalled tip freezes flushes, **an advanced tip drains the queue**.

`TipTracker::observe` implements a careful guard in the *backwards* direction —
`REORG_ALLOWANCE = 10`, beyond which a regression is refused — but there is **no
guard whatsoever in the forwards direction**. Any height greater than the current
one is accepted verbatim, with no bound on how far, and no reference to how much
wall-clock time has passed since the last advance, even though `last_advance` is
right there in the same struct.

The max-over-endpoints defence in `chain.rs` only removes the lever when there is
more than one *independent* endpoint. The shipped configuration
(`deploy.env.example:22`) has exactly one, and `chain.rs:17-21` acknowledges this
("an indexer is a single funnel in front of a single node"). With one endpoint,
`max()` over one value is that value: the endpoint is the clock.

The endpoint in the shipped configuration is `na.zec.rocks` — a light-wallet
indexer operator, i.e. a member of the exact adversary class this product exists
to defend users against (`AUDIT-INSTRUCTIONS.md`, attacker #1), and one the audit's
own threat model already names as able to "lie about the tip"
(`AUDIT-INSTRUCTIONS.md`, "Trust boundaries": *hub → indexer … can lie about the
tip and about publish verdicts*).

## Attack Scenario and Steps

The attacker is whoever controls the `GetLightdInfo` answers the hub receives: the
indexer operator, or anyone who compromises that indexer. (A third case — anyone
on the network path, if `--indexer-tls` is unset, which `main.rs:33-38` only
*warns* about — is noted for completeness but is **not** part of the graded
finding: the deployed configuration sets `ZIH_INDEXER_TLS`, so per
`docs/AVOIDING-FALSE-POSITIVES.md` §7 it does not carry severity. See the
validation section.)

1. The hub polls `GetLightdInfo` every `POLL_INTERVAL = 30 s` (`batcher.rs:70-71,
   290-297`).
2. On each poll the attacker returns `block_height = previous + 20` instead of the
   true height.
3. `observe()` accepts it unconditionally (`batcher.rs:171-175`), because
   `height > state.height`.
4. Immediately below, in the same loop iteration,
   `epoch = tip.cadence_height() / 20` has advanced by one, so
   `Some(previous) if epoch > previous` matches and `flush(&queue, &chain)` runs
   (`batcher.rs:300-309`).
5. A flush therefore happens **every 30 seconds** instead of every ~25 minutes,
   publishing whatever arrived in the last 30 s. At the project's own stated
   arrival rate (README: "the modal batch is 0 or 1" even at 25 minutes), every
   published batch is 0 or 1 transactions, i.e. every migration is published alone.
6. Nothing logs the observed height, so the hub's telemetry shows only
   `flush_size` / `achieved_batch_size` falling — which at current adoption is
   indistinguishable from normal operation, and the `achieved <= 1` warning at
   `batcher.rs:412-420` is *expected* to fire today.

**Admission control does not throttle this attack for the traffic that matters.**
The natural objection is that racing the tip ahead makes
`queue.rs::survives_next_flush` refuse submissions (`expiry >= next_flush_height(tip)
+ 4`), which would be self-limiting. That is true only for transactions with a
tight expiry. Per `audit-state/SPEC-NOTES.md` §3 (verified against the ZIP 318
source), a conforming ZIP 318 migration carries a *bucketed absolute* expiry
34,561–69,120 blocks ahead of its broadcast height. At today's tip that is roughly
**39,000 blocks of runway** for the attacker before ZIP 318 traffic starts being
refused. At +20 blocks per 30 s poll that is ~16 hours of continuous maximum-rate
attack; paced at +20 blocks every five minutes it is over a week, and still cuts
the effective batching window from 25 minutes to 5. For the acute use case the
product exists to serve, admission control provides essentially no back-pressure
against an inflated tip.

**Attack Requirements and Assumptions:**
- Requires control of, or a bug in, the single indexer the hub queries — a party
  the threat model already designates as untrusted and able to lie about the tip.
  No mixnet position, no shim, and no submission of the attacker's own
  transactions are needed.
- Costs nothing: it is one integer in one protobuf field per 30 seconds.
- Also reachable *accidentally*: `chain.rs:158` casts the protobuf `u64`
  `block_height` to `u32` with `as`, so any nonsense or sentinel value from a buggy
  or misconfigured indexer becomes an arbitrary accepted height, and a
  misconfiguration pointing at a different network's indexer produces a wrong
  height too.
- The attack is invisible: the observed height is never logged, and there is no
  plausibility check to alarm on.

## Impact on Users

The batch **is** the anonymity set — it is the entire privacy mechanism the hub
provides. Reducing every batch to a single transaction removes it completely, for
every user of every shim pointed at that hub, while the system continues to report
itself healthy. The transactions are still published, so nothing fails visibly;
the users simply do not get the property they were promised, and the loss is
permanent and retrospective because it is recorded on a public chain.

Concretely, the indexer operator running this attack learns, for each published
transaction, the exact 30-second window in which it arrived at the hub. Combined
with a colluding or identical shim-side operator — who already knows "client IP C
submitted an Orchard-touching transaction at time T", the residual `REVIEW.md`
accepts because the batch was supposed to break the rest of the link — this
completes IP → txid → balance, which is the exact linkage the product exists to
destroy.

## Technical Details / Code Analysis

`hub/src/batcher.rs:160-197` — the whole of `observe`. Note the asymmetry: the
`height < state.height` arm has a bound and a loud log, the `height > state.height`
arm has neither.

```rust
    /// Record a height observed from the network (already the max over nodes).
    pub fn observe(&self, height: u32) {
        let mut state = self.write();

        if !state.observed {
            state.height = height;
            state.last_advance = Instant::now();
            state.observed = true;
            return;
        }

        if height > state.height {
            state.height = height;                    // <-- no bound of any kind
            state.last_advance = Instant::now();
            return;
        }

        if height < state.height {
            let drop = state.height - height;
            if drop <= REORG_ALLOWANCE {              // <-- bounded, logged
                tracing::warn!(drop, "chain tip moved backwards within the reorg allowance; following it");
                state.height = height;
                state.last_advance = Instant::now();
            } else {
                tracing::warn!(drop, "ignoring a tip regression larger than the reorg allowance");
            }
        }
    }
```

`hub/src/batcher.rs:290-312` — observation and flush decision in one loop
iteration, so an accepted advance fires a flush on the same poll:

```rust
        match chain.tip_height().await {
            Ok(height) => tip.observe(height),
            Err(err) => { tracing::debug!(%err, "tip query failed on every node"); }
        }

        if tip.is_ready() {
            let epoch = tip.cadence_height() / params.flush_interval.max(1);
            match last_flush_epoch {
                None => last_flush_epoch = Some(epoch),
                Some(previous) if epoch > previous => {
                    flush(&queue, &chain).await;
                    last_flush_epoch = Some(epoch);
                }
                _ => {}
            }
        }
```

`hub/src/chain.rs:148-173` — `tip_height` takes `.max()` over the endpoints that
answer. This is exactly what `REVIEW.md` #8 asked for, and it is the right
mitigation against a *lagging* endpoint; it is a no-op against an *advancing* one,
and with `endpoints.len() == 1` it is a no-op against both.

`hub/src/config.rs:36-46` requires at least one `--indexer` and allows a list, but
`deploy.env.example:22` ships one: `INDEXERS=66.241.124.200:443`.

`hub/src/queue.rs:380-392` (`survives_next_flush`) is the only feedback path from
an inflated tip, and per `audit-state/SPEC-NOTES.md` §4(a) it is ~1,150× too loose
to constrain the attacker for ZIP 318 traffic.

Why this is not "the tip is trusted infrastructure, out of scope": `REVIEW.md` #8
identifies precisely this attacker and this lever, and specifies a mitigation for
it. The mitigation as built (max-over-nodes plus a backwards-only monotonicity
bound) is incomplete: it removes the lagging-node lever and leaves the advancing-node
lever untouched, and the deployed single-endpoint configuration removes even the
lagging-node half.

## Recommendations

Bound the forward advance the same way the backward one is bounded, using the
`last_advance` timestamp the struct already keeps:

- In `observe`, reject (or clamp, and log loudly) any advance larger than
  wall-clock permits — e.g. `last_advance.elapsed().as_secs() / NOMINAL_BLOCK_SECS`
  plus a generous burst allowance for catch-up after a partition. A chain running
  at 75 s/block cannot legitimately deliver 20 blocks in a 30 s poll.
- Independently, enforce a minimum wall-clock interval between flushes in
  `run_with_poll_interval`, so that no sequence of tip observations can produce
  flushes faster than the cadence is designed to run.
- Log the observed height (an aggregate, not per-entry information) so that tip
  manipulation is visible in the hub operator's telemetry at all.
- Fix the `info.block_height as u32` truncation at `chain.rs:158` to a checked
  conversion, and treat an out-of-range height as a failed tip query.
- Deploy more than one *independent* indexer, or the max-over-nodes rule from
  `REVIEW.md` #8 has nothing to work with.

---

**MARKED ADDENDUM (LocalAuditor, `hub/tests/live_chain.rs` audit, 2026-08-18) — an
in-tree precedent for the missing plausibility check, and a ready-made constant.**
This issue notes that "there is no plausibility check to alarm on". The project has
in fact written one down, in the one place it cannot act:
`hub/tests/live_chain.rs:34-37` asserts `height > 3_000_000` and explains why —
*"mainnet is far past this and it will not regress, so a plausible height proves we
parsed a real answer rather than a default."* That is exactly the floor
`TipTracker::observe`'s first-observation branch lacks, and the reasoning behind it
is the same hazard: `unframe` + prost decode an empty body to `LightdInfo::default()`,
so `tip_height` returns `Ok(0)`, which the tracker adopts unconditionally at startup.
Two consequences for this issue's recommendations, neither changing its substance:
(a) recommendation "reject any advance larger than wall-clock permits" can be paired
with a cheap absolute floor whose value the codebase has already chosen and justified;
and (b) the floor lives in a `#[ignore]`d, environment-gated test that no CI runs, so
it is a comment about production behaviour rather than a check on it. Note also that
`> 3_000_000` does **not** discriminate mainnet from testnet, so it is a defence
against a *default* answer, not against this issue's wrong-network aside on the
`info.block_height as u32` cast at `chain.rs:158`.

## Validation Information

**Verdict: CONFIRMED. Severity corrected: High → Medium.** The mechanism is
real, deterministic, and free for the attacker who can reach it; the filed
severity was too high because that attacker is not a stranger on the internet.

### Every mechanical claim re-verified against the target

| Claim | Verified at |
|---|---|
| `observe` accepts any forward advance with no bound and no clock reference | `hub/src/batcher.rs:161-193` — the `height > state.height` arm is three lines: assign, stamp `last_advance`, return |
| The backward direction *is* bounded and logged | `hub/src/batcher.rs:178-192` (`REORG_ALLOWANCE = 10` at `:59`) — the asymmetry is exactly as filed |
| One accepted advance fires a flush in the same loop iteration | `hub/src/batcher.rs:287-311`: `tip.observe(height)` then `epoch = tip.cadence_height() / flush_interval`, then `Some(previous) if epoch > previous => flush(...)` |
| Maximum forced flush rate is one per poll | `hub/src/batcher.rs:71` (`POLL_INTERVAL = 30 s`) and the single `flush` call per iteration — a 400-block jump still yields one flush, so 30 s is the floor |
| `tip_height` is `max()` over endpoints | `hub/src/chain.rs:155-173` |
| Shipped configuration has exactly one endpoint | `deploy.env.example:22` — `INDEXERS=66.241.124.200:443`. `max()` over one value is the identity function, so that endpoint **is** the flush clock |
| The `u64 → u32` truncation | `hub/src/chain.rs:158` — `info.block_height as u32`, unchecked |
| A degenerate answer decodes to height 0 and is adopted unconditionally | `hub/src/chain.rs:415-430` (`unframe`: a 5-byte all-zero body gives `declared = 0`, and `LightdInfo::decode(&[])` is `Ok(default)`) feeding `batcher.rs:163-168`, the `!state.observed` branch |
| The `> 3_000_000` plausibility floor exists only in an ignored test | `hub/tests/live_chain.rs:34-37` — the addendum is accurate |
| Nothing logs the observed height | Confirmed by grep: `observe` has no `tracing` call on the accept path, and no other site logs a height |
| The threat model designates this party untrusted | `AUDIT-INSTRUCTIONS.md`, "Trust boundaries": *hub → indexer … can lie about the tip and about publish verdicts*. So this is untrusted input reaching a security-critical decision, not a trusted-infrastructure assumption |
| `REVIEW.md` #8 named this exact lever | `hub/src/batcher.rs:19-22`: *"a stalled tip freezes flushes, **an advanced tip drains the queue**"* — the code documents the attack it does not defend against |
| ZIP 318 expiry gives ~34,561–69,120 blocks of runway, so admission control does not throttle the attack for the traffic that matters | `audit-state/SPEC-NOTES.md:46-47`, `:235` (confirmed against the ZIP source) |

### The bound that fixes the severity (PROGRESS open item 6p)

The global audit's severity bound applies verbatim: **all tip manipulation
requires control of a configured `ZIH_INDEXERS` endpoint. This is a
hub-trust / robustness defect, not an internet-reachable weapon.** Nothing in
this issue is reachable by an anonymous party. Concretely:

- The hub dials literal IPv4 addresses supplied by its own operator
  (`hub/src/config.rs:36-46`), with no DNS egress at all
  (`hub/deploy/caution/caution.hcl.tmpl`, "no port 53, even though the indexer
  is authenticated by NAME"), so there is no name-resolution path to hijack.
- The network-path variant the issue raises ("if `--indexer-tls` is unset —
  `main.rs:33-38` only *warns*") is **not** part of the graded finding. The
  deployed configuration sets `ZIH_INDEXER_TLS` (`caution.hcl.tmpl`,
  `deploy.env.example:23`), and `AVOIDING-FALSE-POSITIVES.md` §7 correctly
  discounts a vulnerability that exists only under an explicitly insecure
  configuration the shipped one does not use. It remains worth the one-line
  hardening, but it does not raise the severity.

What survives the bound, and why it is still a real finding:

1. **Given a hostile or buggy indexer, the attack is certain, free and
   invisible.** One integer per 30 s. At `n = 1` there is no aggregation to
   defeat, no rate check, no plausibility check, and no log line to alarm on.
2. **The party in question is in the adversary set by name.** The audit
   instructions rank "the indexer operator the shim fronts" as attacker #1, and
   `deploy.env.example` points `BACKEND` (`:16-17`) and `INDEXERS` (`:22-23`) at
   the *same host, port and TLS name* — so in the shipped example the hub's
   flush clock is held by a party who is simultaneously a shim's backing
   indexer. (The full composition is filed separately as
   `single-party-composition-the-operator-who-sees-the-wallet-leg-also-owns-the-flush-clock-and-the-publish-gate-in-the-shipped-configuration.md`.)
3. **It is reachable with no attacker at all.** A buggy indexer, one pointed at
   the wrong network, or one whose `block_height` overflows 32 bits
   (`chain.rs:158`) produces the same effect. That half is a plain robustness
   defect.

### Impact, stated precisely

Two distinct consequences, and the attacker gets both:

- **The batching anonymity set collapses.** A 30 s cadence publishes whatever
  arrived in 30 s, which at any realistic arrival rate is 0 or 1 transactions.
  The batch *is* the anonymity set; there is no other mechanism behind it. The
  loss is silent (`achieved_batch_size` falling is indistinguishable from low
  adoption, and `batcher.rs:412-420`'s warning is expected to fire today
  anyway) and permanent, because it is recorded on a public chain.
- **Beyond ~36 blocks of accumulated offset, ZIP 203-default traffic is
  silently destroyed.** `survives_next_flush` (`queue.rs:380-393`) is evaluated
  against the inflated `observed_height`, so a wallet's `expiry = build + 40`
  stops clearing the bar; the hub answers `Refusal::ExpiryTooTight`, and by
  `shim/src/hub.rs:231-240` that refusal never reaches the wallet, which was
  told `error_code 0` minutes earlier. The *constant-offset* form of this
  (which no rate-based defence proposed here would catch, because no jump ever
  occurs) is filed separately as
  `a-constant-tip-offset-is-a-tunable-expiry-keyed-admission-filter-that-every-proposed-tip-rate-defence-misses.md`;
  this issue's race form reaches the same state as a side effect.

### Severity justification — Medium

*Impact if exploited:* severe. The product's entire privacy mechanism is voided
for every user of every shim pointed at that hub, silently and irreversibly,
and past a threshold the same lever destroys migrations the wallet was told had
succeeded.

*Likelihood:* bounded. It needs control of, or a fault in, an endpoint the hub
operator explicitly configures and could replace. It is not reachable by
"anyone on the internet" (attacker #2), which is what separates this from the
two shim/hub flood findings validated alongside it.

*Why not High:* the audit's own global pass established that tip manipulation
is a hub-trust defect rather than an internet-reachable weapon, and the
severity guidance reserves High for issues that are *likely* to have severe
impact on many users. Additionally, at today's adoption the achieved batch is
already 0 or 1 — an accepted, documented residual — so the marginal privacy
loss *today* is small. The finding's value is that the defence `REVIEW.md` #8
specified is not buildable as written, so the property will not appear when
adoption arrives.

*Why not Low:* the code names this exact attack in its own module docs and
ships a mitigation that is a no-op against it; the deployed `n = 1` removes
even the half that does work; the defect is also reachable by accident; and
there is no telemetry that would let an operator notice either case.

### Note on `AVOIDING-FALSE-POSITIVES.md` §1

§1 warns against flagging code for relying on properties an attacker cannot
violate. It does not apply: the tip is not an externally-enforced invariant here
but a single unauthenticated integer read from a party the project's own threat
model lists as able to lie about it, and `batcher.rs:19-22` states the
consequence of that lie in the code itself.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
