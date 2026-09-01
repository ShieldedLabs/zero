# The hub mints a fresh Nym identity after ~10.5 minutes of inbound mixnet silence, permanently invalidating every shim's baked-in configuration — and the counter the operator runbook names as the warning sign never moves on that path

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/nym_driver.rs:69` (`REBUILD_BACKOFF`), `:70-99` (`REBUILDS_BEFORE_NEW_IDENTITY`), `:101-112` (`STABLE_LIFE`), `:114-124` (`SHORT_LIVES_BEFORE_NEW_IDENTITY`), `:127-138` (`PROBE_INTERVAL`, `SILENT_ROUNDS_BEFORE_REBUILD`), `:225-245` (`Failures` / `exhausted`), `:279-320` (the fallback block and the connect-failure accounting), `:329-338` (address announcement), `:417-449` (the probe arm), `:525-578` (the short-life accounting), `:647-670` (`probe_send` / `send_probe`); with `audit-target/zeronym/hub/src/main.rs:205-214` (`NymAddress::set` on every published address) and `audit-target/zeronym/hub/src/server.rs:118-190` (`set`, `set_died`, `set_rebuild_failed`, `status_json`); the documentation mismatch at `audit-target/zeronym/hub/deploy/caution/OPERATORS.md:168-186` and `:221-245`; reachability and cost from `audit-target/zeronym/deploy.env.example:43-63`; user impact from `audit-target/zeronym/shim/src/hub.rs:231-241` and `audit-target/zeronym/shim/src/config.rs:252-289` (`--hub-nym` is static startup configuration)
**Found by agent:** Local (file audit of `hub/src/nym_driver.rs`)
**In scope of audit?** Yes — priority area #4 ("the mixnet transport ... identity rotation"), and the `*/src/nym_driver.rs` row of "Code Areas That Should Get Extra Attention"

## Description

The hub's whole reason for pinning one `Ephemeral` store across client rebuilds is
that its Nym address is *configuration for other people's enclaves*. The module
says so itself (`hub/src/nym_driver.rs:24-32`):

> **The address survives a client death.** ... That matters because the address is
> baked into every shim's enclave config and a Caution managed app is immutable,
> so an address change costs every operator a re-assemble and redeploy.

The file then provides an escape hatch that throws that away automatically, on a
timer, with no operator in the loop (`hub/src/nym_driver.rs:279-306`):

```rust
    loop {
        // The stored registration is not coming back, whether the gateway
        // refuses us outright or registers us and then drops or starves every
        // client. Give up on it and mint a new identity, ...
        if failures.exhausted() {
            storage = Ephemeral::default();
            gateway = None;
            tracing::warn!(
                after_failed_connects = failures.rebuilds,
                after_short_lived_clients = failures.short_lives,
                "the hub's gateway registration is unrecoverable; taking a FRESH \
                 identity and dropping the gateway pin. The hub's Nym address WILL \
                 change: read the new one from /nym-address and re-point every shim, \
                 or migrations keep failing closed"
            );
            failures = Failures::default();
        }
```

with (`hub/src/nym_driver.rs:240-245`):

```rust
impl Failures {
    fn exhausted(&self) -> bool {
        self.rebuilds >= REBUILDS_BEFORE_NEW_IDENTITY
            || self.short_lives >= SHORT_LIVES_BEFORE_NEW_IDENTITY
    }
}
```

`SHORT_LIVES_BEFORE_NEW_IDENTITY` is **5** (`:124`). A "short life" is a client
torn down for inbound silence *however long it lived* (`:558`:
`if silent || lived < STABLE_LIFE`), and inbound silence is declared after
`SILENT_ROUNDS_BEFORE_REBUILD = 2` rounds of `PROBE_INTERVAL = 60 s`. Each cycle
is ~120 s of probing plus a 5 s backoff, so **about ten and a half minutes of "no
inbound mixnet message of any kind reached this client" is sufficient to
irreversibly change the hub's Nym address and disconnect every shim in the
fleet.**

Three properties make that expensive:

- **automatic and unattended** — no flag, no confirmation, no operator action;
- **irreversible** — `Ephemeral::default()` mints new identity *and* encryption
  keys (`hub/tests/nym_identity.rs:63-86` pins exactly this), and the old ones are
  in RAM only, so the previous address can never be recovered;
- **fleet-wide** — every shim's `--hub-nym` is validated once at startup
  (`shim/src/config.rs:252-289`) and never re-read; nothing in `shim/src/` fetches
  `/nym-address` (verified by grep: the path string appears nowhere under
  `shim/src/`), and the value is baked into an immutable Caution enclave.

**And the signal the operator runbook tells them to watch does not move on this
path.** `hub/deploy/caution/OPERATORS.md:172` describes
`consecutive_rebuild_failures` as *"growing — it is down and not recovering; **at
60 it takes a NEW identity** and every shim needs re-pointing"*, and `:182-184`
repeats *"After 60 consecutive failed rebuilds the hub also deliberately takes a
fresh identity"*. Neither the field table nor the failure-mode list mentions the
five-short-lives path at all. But on that path every cycle **connects
successfully**, so `main.rs:213` calls `NymAddress::set`, which zeroes
`consecutive_failures` (`server.rs:127-137`). The counter the runbook names as the
predictor therefore reads **0 for the entire ten-and-a-half-minute walk to the
identity change.** The one field that does move, `client_deaths`, is characterised
in the same table as benign: *"climbing: gateway churn"*.

## Attack Scenario and Steps

The dominant path is **not** an attacker; it is an ordinary transient.

**Path A — a transient inbound outage of ~11 minutes.**

1. The hub is connected. Its inbound path stops delivering for eleven minutes.
   Any of these does it: the pinned entry gateway restarts or is upgraded; the
   gateway registers the client and stops delivering to it (the exact fault this
   project *measured in production* — `hub/src/nym_driver.rs:345-353` and
   `shim/src/nym_driver.rs:221-231` record that on 2026-08-14, **two of four**
   deployed clients on identical config never received a single inbound message,
   one broken three minutes after boot and still broken hours later); a mix node
   on the probe's route churns; the free-tier bandwidth allowance runs out, which
   `hub/deploy/caution/OPERATORS.md:197-199` documents as *"reception stops"*.
2. The self-probe (`:663-670`) is a message to the hub's own address, so it
   traverses gateway -> mix hops -> gateway -> client. Nothing on that path
   returning means `inbound_total` does not advance.
3. `silent_rounds` reaches 2 at ~t+120 s -> `Step::Silent` -> `client.disconnect()`
   -> `failures.short_lives += 1` -> 5 s backoff -> rebuild on the same storage.
4. The condition persists, so the cycle repeats. Each cycle is ~125 s.
5. At the fifth, `exhausted()` is true at the top of the outer loop. Fresh
   identity, gateway pin dropped, new address.
6. **The outage clears — or, more likely, the fresh identity's new gateway draw
   simply works, because dropping the pin is what the fallback does.** The hub is
   perfectly healthy, at an address nobody has.

Step 6 is the sting: the fallback is *most* likely to succeed exactly when the
fault was localised to one gateway — i.e. exactly when the old identity would
also have recovered on its own.

**Path B — a transient connect failure.** `REBUILDS_BEFORE_NEW_IDENTITY = 60`
with `REBUILD_BACKOFF = 5 s` per cycle; the constant's own doc (`:83-90`) measures
~370 s against a locally refused port and warns the real figure is longer against
a remote gateway that times out. `deploy.env.example:43-63` makes the timing-out
case the deployed one: the nym-api endpoints are allowlisted **by IP snapshot**,
blocked packets are *dropped* rather than refused so each blocked attempt "waits
out the SDK's full 30 s timeout", and the same comment says those IPs *"are DNS
snapshots, not configuration — re-derive them ... on every redeploy **or they rot
silently**"*. A nym-api IP rotation or outage therefore produces a run of 30-90 s
connect failures; 60 of them is ~35-90 minutes, and then the hub mints a new
identity **which cannot help**, because the blockage is the topology fetch, not
the registration — and it will mint another one every 35-90 minutes until the
blockage clears. The marginal harm is on the transient case: without the
fallback, a nym-api outage that clears leaves the hub reconnecting at the *same*
address and the fleet recovering with zero human action; with it, the hub returns
at a new one.

**Path C — an adversary.** Attacker class #5 in `AUDIT-INSTRUCTIONS.md` ("mixnet
parties: gateways, mix nodes, and anyone able to degrade the shim->hub path"). The
hub deliberately pins **one** entry gateway for the life of its identity
(`:170-177`), and that gateway is a public Nym node with a published IP the
enclave's egress rule names explicitly. Eleven minutes of interference against
that one IP — or *being* that gateway, accepting the registration and dropping
inbound, which is free and undetectable — forces the fleet-kill, repeatably. The
attacker needs no credentials, no ACL bypass and no access to any enclave.
(Separately, `hub-surb-starved-lookup-replies-...-oom.md`, already confirmed,
hands an anonymous outsider an on-demand trigger for the same terminal state via
a process restart. That issue owns that impact; it is cited here only so the
report does not present the two as unrelated.)

**Attack Requirements and Assumptions:**

- Paths A and B require **no attacker** — only a Nym gateway or nym-api endpoint
  unavailable for a window the project's own field notes show is routine.
- Path C requires only the ability to degrade one publicly-addressed Nym node's
  delivery path for ~11 minutes, or to be that node.
- Recovery requires **every third-party operator** to read the hub's new
  `/nym-address`, re-run `assemble-caution.sh`, and redeploy an immutable
  enclave. `hub/deploy/caution/OPERATORS.md:226-245` budgets this at ~25 minutes
  per component and *"well over an hour"* end to end, and step 4 of its own
  runbook is *"Send it to every shim operator. There is no discovery mechanism;
  the handoff is a human message."*

## Impact on Users

While the fleet holds a stale address:

1. **Every diverted migration is silently destroyed, and the wallet is told it
   succeeded.** `shim/src/hub.rs:231-241` returns `Submit::Accepted { txid }` the
   moment the frame is handed to the in-process mixnet transport, with its own
   comment recording that a hub refusal *"is never surfaced here"*. The frame goes
   to a `Recipient` whose identity key no longer exists and is dropped. The wallet
   holds `error_code 0` and a txid for a transaction that exists nowhere, on a
   migration path that is mandatory and whose funds sit in a pool closed to new
   value.
2. **Every wallet's `GetTransaction` fails.** With a hub configured the shim routes
   *every* lookup to the hub (`shim/src/intercept.rs:229-236`) and fails closed on
   timeout, so **no** wallet behind **any** zeronym shim can fetch a transaction's
   full data — including users who have never touched Orchard.
3. **Users are pushed off the protected path.** A wallet that cannot broadcast or
   confirm gets pointed at a different, unprotected indexer, where the
   Orchard-touching transaction is broadcast in the clear — converting an
   availability failure into the permanent, on-chain privacy leak the product
   exists to prevent. In this system availability *is* a privacy property.
4. **The documented alarm does not fire in time.** The runbook's named predictor
   (`consecutive_rebuild_failures` approaching 60) stays at 0; the runbook's other
   instruction, *"poll `/nym-address` and alert on change"* (`:186`, `:221-222`),
   is detection after the fact and is a manual instruction nothing in the
   repository implements. `mixnet_connected` does flap false during each silent
   round, so an operator polling it frequently would see *something* — but the
   runbook's guidance for that state is "the hub is receiving nothing", not "you
   have ten minutes before the fleet's configuration is void".

## Technical Details / Code Analysis

**The accounting, in full.** The counters are cleared only by a client that both
lived out `STABLE_LIFE` *and* was not torn down for silence
(`hub/src/nym_driver.rs:556-569`):

```rust
                let lived = connected_at.elapsed();
                if silent || lived < STABLE_LIFE {
                    failures.short_lives += 1;
                    tracing::warn!(
                        lived_secs = lived.as_secs(),
                        silent,
                        consecutive_short_lives = failures.short_lives,
                        "hub mixnet client did not prove the stored registration; \
                         counting it against it"
                    );
                } else {
                    failures = Failures::default();
                }
```

and by the fallback itself (`:305`). A failed connect only ever increments
(`:313-320`). There is no decay and no half-life — only consecutiveness, which
`:94-98` deliberately widened to include clients that connected successfully.

**The silence timer, step by step** (`:417-449`, with
`probe.set_missed_tick_behavior(Delay)` at `:376` and the first tick immediate):

| t | arm | state |
|---|---|---|
| ~0 s | first tick, `inbound_at_probe == None` -> catch-all arm | `silent_rounds = 0`, probe sent; on completion (`:490`) `inbound_at_probe = Some(inbound_total)` |
| ~60 s | tick, `inbound_total == mark` | `silent_rounds = 1`, warn, probe re-sent |
| ~120 s | tick, `inbound_total == mark` | `silent_rounds = 2 >= SILENT_ROUNDS_BEFORE_REBUILD` -> `Step::Silent` |

`Step::Silent` then runs `client.disconnect().await` (`:537`), `status.set_died()`
(`:550`), `failures.short_lives += 1` (`:559`), and `REBUILD_BACKOFF` (`:574`).
Five iterations is ~625 s.

**Why the runbook's counter cannot warn.** On this path `build_client` *succeeds*
every cycle, so `address_out.send(address)` (`:338`) reaches `main.rs:213`'s
`nym_address.set(...)`, which sets `connected = true` **and stores 0 into
`consecutive_failures`** (`server.rs:127-137`). `set_rebuild_failed`
(`server.rs:160-167`) — the only thing that increments that counter — is reached
only from the connect-failure arm. `status_json` (`server.rs:182-190`) carries
`mixnet_connected`, `address_published`, `client_deaths` and
`consecutive_rebuild_failures`, and **no identity-change counter of any kind**:

```rust
        serde_json::json!({
            "mixnet_connected": self.0.connected.load(Ordering::Relaxed),
            "address_published": self.get().is_some(),
            "client_deaths": self.0.deaths.load(Ordering::Relaxed),
            "consecutive_rebuild_failures": self.0.consecutive_failures.load(Ordering::Relaxed),
        })
```

So `/nym-status` cannot distinguish "reconnected, same address" from "the fleet's
configuration is now void" — the single most important distinction this endpoint
could make, on an enclave whose console does not exist.

**The evidence the decision is taken on is about the whole mixnet, not the
registration.** `inbound_total` is incremented before any filtering (`:494-503`),
so SURB-replenishment artifacts and the self-probe all count — the right choice
for the liveness question, but it makes the silence verdict a statement about the
entire round trip (outbound leg, mix hops, return leg, gateway delivery) while the
action taken on it is scoped to *this client's registration* (`:363-369`). A
mixnet-wide or route-level fault is therefore attributed to the hub's own
registration and paid for with the fleet's configuration.

Worse, an **outbound-only** fault counts as inbound silence: `send_probe`
(`:660-670`) logs and swallows a send error, and `probe_send` (`:647-652`) returns
`Sent::Probe` regardless, so `inbound_at_probe` is stamped at `:490` even for a
probe that never left.

**Contrast with the shim, which has the guard the hub lacks.** The shim's
identical arm refuses to declare silence while its own send queue is non-empty
(`shim/src/nym_driver.rs:312`: `Some(mark) if seen == mark && out_frames.len() == 0`),
because rebuilding on it *"would disconnect a healthy client and discard that
whole queue"*. The hub's arm (`hub/src/nym_driver.rs:421`) has no such condition.
For the shim a false rebuild costs one identity that was meant to rotate anyway;
for the hub it is a strike toward invalidating every operator's enclave. The
asymmetry runs the wrong way.

**Why "a hub restart changes the address anyway" does not excuse this.** The
constant's rationale says (`:79-82`) *"a hub restart would change the address
anyway, so this only automates what an operator would otherwise do by hand"*. A
restart is an operator action taken with knowledge, at a chosen time, with the
operator ready to notify the fleet. This is an unattended action taken by a timer
on evidence that is (a) indirect, (b) unable to distinguish a transient from a
permanent fault, and (c) unable to distinguish a fault in the hub's registration
from a fault anywhere else on the mixnet. In Path B it is additionally futile and
repeating.

**No executable evidence exists for any of this.** `hub/src/nym_driver.rs` has no
`#[cfg(test)] mod tests` (the shim's does, at `shim/src/nym_driver.rs:657`), and
nothing in `hub/tests/` references `run_driver`, `Failures`, `short_lives` or
`probe_send`. The only thing that drives the fallback is `nymnet/probe`, which
needs a human to kill and restart a localnet gateway by hand — and
`nymnet/localnet.sh` exposes only
`up|down|status|smoke|lookup|wire|e2e|e2e-driver|clean|env` (`:248`), with **no
gateway-kill subcommand**, contradicting the citation at
`hub/src/nym_driver.rs:90-91`. Per the coordinator's verified facts no CI runs
`cargo test` at all. Both thresholds, the short-life/connect-failure split, and
the ~370 s measurement are held by prose.

## Recommendations

In rough order of value:

1. **Make `/nym-status` able to say the one thing that matters.** Add
   `identity_changes` (a monotone counter) and `address_generation` to
   `NymAddress::status_json` (`hub/src/server.rs:182-190`) and bump them from the
   fallback block (`nym_driver.rs:286-306`). This is client-lifecycle data of
   exactly the kind the endpoint already carries, the address itself is public, so
   it costs nothing in anonymity terms — and it is the cheapest fix in the file.
2. **Require persistence, not just consecutiveness.** Gate `exhausted()` on
   wall-clock as well as counts — e.g. "the stored registration has produced no
   stable client for at least N hours". Ten minutes is far shorter than the time
   it takes a human to notice, and two orders of magnitude shorter than the
   recovery it triggers.
3. **Make the fresh-identity fallback opt-in** (`ZIH_NYM_ALLOW_NEW_IDENTITY`,
   default off) or operator-triggered. "The hub is down and will come back at the
   same address" is strictly recoverable; "the hub is up at a new address" is not.
4. **Correct `hub/deploy/caution/OPERATORS.md:168-186`.** It documents only the
   60-connect-failure path and names `consecutive_rebuild_failures` as the
   predictor, which stays at 0 on the faster path. Document the five-short-lives
   path, and change the `client_deaths` row from "climbing: gateway churn" to
   state that five consecutive silence teardowns mint a new identity.
5. **Do not attribute a whole-mixnet fault to the local registration.** At minimum
   do not count a round in which `send_probe` returned an error, and add the
   shim's `out_frames.len() == 0` backlog guard.
6. **Give the fleet a way to follow an address change.** `--hub-nym` already
   accepts a list (`shim/src/config.rs:252-289`); a pre-published successor
   address, or an address record shims could pin by hub public key rather than by
   gateway-bound `Recipient`, would turn a fleet-kill into a reconnection. Until
   something like that exists the address must be treated as immutable state, not
   as something a timer may replace.

## Validation Information

**Verdict: CONFIRMED as a real defect. Severity: DEFLATED High -> Medium.**

**Every mechanical claim re-verified against `audit-target/zeronym/`:**

- `hub/src/nym_driver.rs:69` `REBUILD_BACKOFF = 5s`; `:99`
  `REBUILDS_BEFORE_NEW_IDENTITY = 60`; `:112` `STABLE_LIFE = 3 min`; `:124`
  `SHORT_LIVES_BEFORE_NEW_IDENTITY = 5`; `:133` `PROBE_INTERVAL = 60s`; `:138`
  `SILENT_ROUNDS_BEFORE_REBUILD = 2`. All confirmed verbatim. The code's own
  comment at `:121-123` says *"Five short lives is ~10 min"* — the project agrees
  with the timeline.
- `:240-245` `exhausted()` is an **OR** over the two counters, so the
  five-short-lives path is independent of the sixty-connect-failures path.
  Confirmed.
- `:279-306` the fallback replaces `storage` with `Ephemeral::default()` and drops
  the gateway pin; `hub/tests/nym_identity.rs:67-86` asserts two independent
  stores have different identity keys. **Irreversibility confirmed.**
- `:417-449` and `:525-578` — the probe/silence timeline and the short-life
  accounting are exactly as described; ~125 s per cycle, five cycles.
- `:647-670` — `send_probe` swallows the send error and `probe_send` returns
  `Sent::Probe` unconditionally, so `:490` stamps the mark even for a probe that
  never left. **An outbound-only fault does count as inbound silence.** Confirmed.
- `shim/src/nym_driver.rs:312` has the `out_frames.len() == 0` guard; the hub's
  `:421` does not. Confirmed.
- `shim/src/config.rs:252-289` validates `--hub-nym` once at startup;
  `grep -rn "nym-address" shim/src/` returns **no hits**, so no shim ever refetches
  it. Confirmed.
- `shim/src/hub.rs:231-241` returns `Submit::Accepted` on hand-off to the
  transport, with the comment *"a refusal is never surfaced here"*. Confirmed.
- `hub/src/nym_driver.rs:345-353` and `shim/src/nym_driver.rs:221-231` — the
  2026-08-14 field note ("two of four ... never answered") is in the source, so
  the base rate of the triggering condition is the project's own measurement, not
  the auditor's speculation. Confirmed.
- `nymnet/localnet.sh:248` — the subcommand list is
  `up|down|status|smoke|lookup|wire|e2e|e2e-driver|clean|env`, with no
  gateway-kill arm; `hub/src/nym_driver.rs` has no `#[cfg(test)]`. Confirmed.

**One claim in the filed draft was too strong and has been replaced with a
sharper, verified one.** The draft said the identity change is *"invisible to the
health surface"*. That is not accurate: `GET /nym-address` serves the **new**
address, `client_deaths` increments once per cycle, `mixnet_connected` flaps
false during each silent round, and `hub/deploy/caution/OPERATORS.md:186` and
`:221-222` explicitly tell operators to *"poll `/nym-address` and alert on
change"*. The correct and more damaging statement, verified during validation, is
the **documentation/instrumentation mismatch**:

- `OPERATORS.md:172` names `consecutive_rebuild_failures` and says *"at 60 it
  takes a NEW identity"*; `:182-184` repeats the 60-failure story. **Neither
  mentions the five-short-lives path**, which is roughly five times faster.
- On that path every cycle connects, so `main.rs:213` -> `NymAddress::set` ->
  `server.rs:132-137` **stores 0 into `consecutive_failures`**. The runbook's
  named predictor reads 0 for the entire walk.
- `status_json` (`server.rs:182-190`) carries no identity or generation counter,
  so even after the fact `/nym-status` cannot distinguish a benign reconnect from
  a fleet-invalidating identity change.

The issue body and recommendations have been rewritten around this; recommendation
4 (correcting the runbook) is new and follows from it.

**Why the severity is Medium and not High.** Four reasons, applied in the order
the audit's own precedents require:

1. **The terminal harm is already owned at High by two confirmed issues, and
   stacking it a third time would triple-count one outage.**
   `hub-surb-starved-lookup-replies-...-oom.md` (Confirmed, High) reaches exactly
   this end state — a process restart, a new address, every shim stranded,
   `GetTransaction` dead fleet-wide, acknowledged migrations silently destroyed —
   and its own Impact section names this file as *"the outage already filed as ...
   whose stated trigger was bad luck"*. `hub-queue-unauthenticated-fill-silently-destroys-migrations.md`
   (Confirmed, High) owns the silent-destruction-after-ack harm. This issue's
   **marginal** contribution is a *further* trigger for the same terminal state,
   plus the instrumentation gap.
2. **The marginal harm is confined to the transient case.** If the inbound silence
   is genuinely persistent, the fallback is defensible and the code's rationale
   holds: keeping the old identity leaves the hub off the mixnet and the fleet
   equally dead, and the eventual hub redeploy changes the address anyway. The
   defect is real but bounded — it is the case where the fault would have cleared
   between ~10.5 minutes and whenever a human would have acted, which the fallback
   converts from a self-healing outage into a multi-hour, multi-party
   reconfiguration.
3. **The mechanism's existence is a documented design decision**
   (`nym_driver.rs:70-99`, `OPERATORS.md:182-184`), so `AVOIDING-FALSE-POSITIVES`
   §6 covers the *escape hatch*. What is **not** covered, and is the actual
   finding, is the threshold (5 short lives / ~10.5 min, with no wall-clock
   persistence and no decay), the attribution error (a whole-mixnet round-trip
   measurement driving a local-registration decision, including probes that never
   left), and the runbook naming a counter that cannot move.
4. **No confidentiality is breached directly.** The privacy harm (users pushed to
   an unprotected indexer) is second-order and depends on user behaviour.

**Why Medium and not Low.** The trigger threshold is ~10.5 minutes against a
condition the project's own production notes show hitting **two of four** deployed
clients; the blast radius is every user of every zeronym endpoint at once,
including non-Orchard users who lose `GetTransaction`; the change is irreversible
and recovery is documented by the project itself at *"well over an hour"* with a
human message as the only distribution channel; and the operator's documented
early-warning signal provably never fires. This sits at the top of Medium, and the
two highest-value fixes (a counter in `status_json`, a wall-clock gate on
`exhausted()`) are each a handful of lines.

**One composition worth recording without re-counting it.** Coordinator item 6z
established that the hub's Nym address is an **unanchored** value: it is the
recipient's key material, published over plain WebPKI on operator-controlled DNS
at `GET /nym-address`, with the alternative distribution channel being verbatim
"a human message", and `hub/deploy/caution/OPERATORS.md:244-245` telling operators
that `caution verify` *"does NOT belong on the critical path — restore service
first, verify after"*. So a party who can force this fleet-kill also manufactures
the exact circumstance in which an unauthenticated address handoff happens under
time pressure across many organisations. That composition belongs in the report's
narrative; the substitution harm itself is already owned by
`shim-config-hub-identity-is-unattested-unobservable-operator-configuration.md`
and must not be counted again here.

**False-positive checks applied.**

- *§6 Intentional design?* Partly — see reason 3 above. The escape hatch is by
  design; the threshold, the attribution and the missing signal are defects.
- *§5 Impractical resource exhaustion?* Not applicable to paths A and B, which
  need no attacker at all. Path C is cheap rather than expensive.
- *§9 Obviously broken functionality?* No — the fallback only fires on a
  condition that is uncommon per deployment even if common across a fleet, and the
  project has observed the *related* address-instability problem in production and
  written a failover runbook for it, which is consistent with the code as written.
- *§1 Assumption an attacker cannot violate?* The assumption "eleven minutes of
  inbound silence means this registration is unrecoverable" is violated by an
  ordinary gateway restart.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
