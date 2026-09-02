# Process shutdown destroys migrations the wallet was already told had succeeded: `Step::Stop` disconnects the mixnet client with no drain, no count and no log, `main` never joins the driver, and one exit path never signals it at all

**Severity**: Medium
**Validation Status**: Confirmed
**Location**:
`audit-target/zeronym/shim/src/nym_driver.rs:410-415` (`Step::Stop` — `client.disconnect().await; return;`),
`audit-target/zeronym/shim/src/nym_driver.rs:353-358` (the two ways `Step::Stop` is produced), `:362-369` (the `out_frames` arm that is never drained),
`audit-target/zeronym/shim/src/nym_driver.rs:416-467` (`Step::Rebuild`, and the residual statement at `:421-436`), `:468-491` (`Step::Died`),
`audit-target/zeronym/shim/src/nym_driver.rs:25-29` (the module's own "a clean rotation must run it to completion"),
`audit-target/zeronym/shim/src/nym.rs:595-690` (`NymHandle::submit`; `Ok(())` at `:660-661`, `:684-689`), `:915-1010` (`run_supervisor`, the shutdown arm at `:953-956`), `:835-905` (`correlate`, the reserved `out_frames` permit at `:853`), `:392-398` (why disconnect is a command),
`audit-target/zeronym/shim/src/hub.rs:236-240` (`Submit::Accepted` at mixnet hand-off, *"a refusal is never surfaced here"*),
`audit-target/zeronym/shim/src/main.rs:166-176` (the only thing `main` awaits), `:335-339` (channel capacities 32 / 8 / 32 / 8 / 8), `:341-356` (the three detached, never-joined tasks),
`audit-target/zeronym/shim/src/proxy.rs:180` (`DRAIN_TIMEOUT` = 10 s), `:545-553` (the drain, which covers only the wallet-facing leg), `:499` (the fatal-accept-error return)
**Found by agent:** Local (file audit of `shim/src/nym_driver.rs`); validated 2026-08-18
**In scope of audit?** Yes — priority area #4 (the dispatch-only submit's loss window)

## Description

Submit is dispatch-only. `NymHandle::submit` returns `Ok(())` as soon as a
`Request` is accepted into the shim's in-process request channel
(`nym.rs:660-661`, `:684-689`), and `HubTransport::submit` maps that straight to
`Submit::Accepted { txid }` (`hub.rs:236-240`), whose own comment says *"a
refusal is never surfaced here"*. The wallet is told `error_code 0` and handed a
txid **before the frame has reached the SDK, let alone the gateway, let alone
the hub**.

Everything between that acknowledgement and the mixnet is destroyed on process
shutdown, with no drain, no accounting and no log line:

```rust
// shim/src/nym_driver.rs:410-415
        match step {
            Step::Ferried => {}
            Step::Stop => {
                client.disconnect().await;
                return;
            }
```

That is the whole of it. There is no attempt to pump `out_frames` first, no
`in_flight` accounting, and no count of how many frames were abandoned.
`Step::Stop` is what **every** SIGTERM, enclave stop and redeploy produces
(`nym_driver.rs:353-358`: `ClientCommand::Disconnect`, which `run_supervisor`
sends from its shutdown arm at `nym.rs:953-956`).

Three further facts make it worse than a missing log line:

1. **The 10-second drain the shim does have protects the wrong queue.**
   `serve_with_shutdown` waits up to `DRAIN_TIMEOUT` = 10 s for in-flight
   *wallet connections* (`proxy.rs:180`, `:545-553`). **Zero seconds** protect
   the mixnet queue, which is where the acknowledged submits are. The drain that
   exists guards the leg on which nothing has been promised yet; the leg on
   which success has already been promised has none.
2. **`main` never joins the driver, so the `disconnect()` the module documents as
   load-bearing is not run to completion.** `nym_driver.rs:25-29` and
   `nym.rs:392-398` both state the reason `Disconnect` is a command rather than a
   drop: *"it is not cancel-safe and a dropped LIVE client leaks its background
   tasks (D12), so a clean rotation must run it to completion."* But
   `build_nym_transport` spawns `run_transport`, `run_supervisor` and
   `run_driver` detached (`main.rs:341-356`) and keeps no `JoinHandle`; `main`
   awaits only `serve_with_shutdown` (`main.rs:166-176`). Both the proxy and the
   supervisor are woken by the *same* signal, and with no wallet connections open
   the drain returns essentially immediately — so `main` returns, the
   `#[tokio::main]` runtime is dropped, and the driver task is cancelled wherever
   it is, very often inside `client.disconnect().await` at `:413`.
3. **There is a third exit that never signals the driver at all.** A non-transient
   `accept()` error returns `Err` from `serve_with_shutdown` (`proxy.rs:499`),
   which propagates out of `main`. The `shutdown()` future the supervisor is
   waiting on never resolves, so `ClientCommand::Disconnect` is never sent: the
   driver is cancelled mid-anything, holding a **live** client. This is the exact
   case the module's D12 note says must not happen, and it is reachable — see
   `shim-accept-loop-treats-documented-transient-errors-as-fatal.md`.

## Attack Scenario and Steps

The loss happens on its own during ordinary operation, and both the primary
adversary and an anonymous stranger can widen or aim it.

**A. Ordinary operation, no attacker.** An operator stops or redeploys the
enclave. Everything the pipeline holds at that instant is destroyed:

- up to **32** acknowledged submits in the request channel (`main.rs:335`,
  `mpsc::channel(32)`) — the wallet has already had `error_code 0` for each;
- up to **8** in `out_frames` (`main.rs:336`), plus the one capacity slot
  `correlate` keeps reserved (`nym.rs:853-863`);
- the **1** hand-off in the driver's `in_flight`;
- everything the SDK holds: a one-slot input, an 8-deep batch channel, and an
  unbounded transmission buffer drained at the throttled rate.

That is **41 frames the shim itself is holding**, plus ~9 more inside the SDK.
At the emission rate the crate derives for itself — `MAX_DELAY_MULTIPLIER` = 6
against the 20 ms default average delay, i.e. ~8.33 packets/s
(`nym.rs:1085-1094`) — one submit is 32 frame packets plus 13 reply SURBs = 45
packets ≈ 5.4 s. A full ~50-slot pipeline is therefore **~2,250 packets ≈ 270
seconds of emission**, all of it already answered "success" to somebody.

**B. Any anonymous party can guarantee the pipeline is full.** The single
mitigating factor the original filing named — "at ~0.77 Orchard-touching
transactions per block the pipeline is usually empty, so a randomly-timed
restart usually destroys nothing" — is removable by an unauthenticated stranger
for about **one byte per second**. That is the confirmed High
`junk-sendtransaction-flood-consumes-the-shims-whole-mixnet-egress-and-converts-acknowledged-migrations-into-silent-loss.md`:
a 5-byte `SendTransaction` body classifies `Unparseable`, fails safe toward
diversion, and buys 45 packets ≈ 5.4 s of the shim's *entire* egress. Held full,
the pipeline is a standing ~270-second queue, and a real wallet's migration
entering it is queued behind the junk with a success already returned. **Any
restart during the flood therefore destroys real, acknowledged migrations**, and
the "usually empty" defence does not apply.

**C. The operator can time it.** The indexer operator is adversary #1 and owns
the Nitro parent host and the process lifecycle. The README concedes they learn
*that* a client diverted — a diverted `SendTransaction` is the one request that
produces no corresponding upstream connection to their indexer, on a wallet
connection they can see at the TCP layer. So: watch for a divert, SIGTERM the
shim within the next few seconds, and the frame dies inside `Step::Stop` while
the wallet holds a success and a txid. The action is fully deniable ("we
restarted the enclave").

**D. Same outcome without a restart.** Blackholing the enclave's egress to the
gateway port until the SDK hits its 20-consecutive-failure hard stop
(`nym.rs:377-381`) yields `Step::Died` (`nym_driver.rs:468-491`), which drops
`in_flight` and the client — and with it the buffer — with no guard and no
accounting. Egress rules are operator-controlled (`deploy.env.example`,
`NYM_EGRESS`). Here the frames were arguably undeliverable anyway; what the code
withholds is any record that they existed.

**Attack Requirements and Assumptions:**

- Scenario A needs no attacker at all; it is the normal redeploy path, and it is
  the one that runs on every deployment of every operator.
- Scenario B needs only the ability to send unauthenticated gRPC requests to a
  public endpoint, at ~1 byte/second.
- Scenarios C and D need the operator, who controls process lifecycle,
  configuration and egress by construction.
- No attacker needs to break the mixnet, the enclave, or any cryptography.

## Impact on Users

A wallet is told its migration was broadcast, and is handed a txid it will
display and record. The transaction exists nowhere: not in a mempool, not in the
hub's queue, not on the mixnet. Nothing recovers it:

- the hub never received it, so there is no queue entry, no payload-hash dedup
  entry and no requeue;
- the shim keeps no per-migration state by design (`lib.rs:30-35`);
- confirmation tracking is *"designed, not built"* (`AUDIT-INSTRUCTIONS.md`,
  self-declared limitations);
- the only feedback channel the design names is the wallet noticing
  non-confirmation and resending (`nym_driver.rs:598-607`), which requires the
  wallet to distrust the success it was given;
- **nothing anywhere logs or counts the loss**, so no operator can ever learn
  that a redeploy destroyed N migrations, and no user can be told.

For a migration this is worse than an ordinary lost broadcast: the funds are in a
pool the network has closed to new value, the migration is the user's only route
out, and the user has been told it is done. Under ZIP 318's expiry schedule the
notes can then sit unusable for a long time (see
`zip318-canonical-expiry-is-the-only-recovery-clock-and-a-lost-migration-freezes-the-users-notes-for-30-to-60-days.md`).

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


## Technical Details / Code Analysis

**The wallet is answered before anything leaves the process.**
`shim/src/nym.rs:659-689`:

```rust
659            match tokio::time::timeout_at(deadline, self.requests.send(request)).await {
660                Ok(Ok(())) => dispatched += 1,
...
684        if dispatched > 0 {
685            Ok(())
686        } else {
687            Err(NymError::TransportGone)
688        }
```

`shim/src/hub.rs:236-240`:

```rust
236                // ... so a refusal is never surfaced here.
237                Ok(()) => Ok(Submit::Accepted {
238                    txid: crate::nym::local_txid(tx_bytes),
239                }),
```

`self.requests` is the sender end of `mpsc::channel(32)` (`main.rs:335`). So
"success" means "one of 32 slots in an in-process channel accepted a struct".

**The unguarded shutdown path**, quoted in full in the Description above
(`nym_driver.rs:410-415`). Note also that the driver's `select!` is **not**
`biased`: when the shutdown arrives, the `commands.recv()` arm and the
`out_frames.recv()` arm are both ready and tokio picks pseudo-randomly, so the
driver can take `Disconnect` with all 8 `out_frames` slots occupied.

**Nothing joins the driver.** `shim/src/main.rs:341-356`:

```rust
341    tokio::spawn(nym::run_transport(req_rx, out_tx, in_rx, inflight.clone()));
...
345    tokio::spawn(nym::run_supervisor(rotation, evt_rx, cmd_tx, inflight, shutdown()));
346    tokio::spawn(nym_driver::run_driver(
```

Three detached tasks, no `JoinHandle` retained. `main` (`:166-176`) awaits only
`serve_with_shutdown`, whose shutdown path is `shim/src/proxy.rs:545-553`:

```rust
545    drop(live_tx);
546    tracing::info!("shutdown requested, draining in-flight connections");
547    match tokio::time::timeout(DRAIN_TIMEOUT, live_rx.recv()).await {
548        Ok(_) => tracing::info!("drained, exiting"),
```

With no wallet connections open, every `live_tx` clone is already gone, so
`recv()` resolves immediately and `main` returns in the same poll cycle in which
the signal fired. The supervisor's `commands.send(ClientCommand::Disconnect)`
(`nym.rs:953-956`) succeeds into an empty 8-slot channel without waiting for the
driver, and then the runtime is dropped out from under whatever the driver was
doing.

**The pipeline depth, precisely.** `correlate` reserves capacity on `out_frames`
*before* accepting a request (`nym.rs:853-863`), and hands the frame over
non-blockingly; the driver takes one frame at a time (`nym_driver.rs:362`,
guarded on `in_flight.is_none()`). So under load `out_frames` sits at 8 and
`requests` backs up to 32. 32 + 8 + 1 = **41** frames held by the shim's own
code, all of which have been answered "success", plus the SDK's one-slot input,
its 8-deep batch channel, and its unbounded transmission buffer.

**The residual statement at `nym_driver.rs:421-436` is not accurate.** It says
the exposure is *"real, bounded by the drain rate, and recorded in PRODUCTION.md
rather than papered over here."* Three problems: the drain **rate** is not a
bound on the **quantity** (the SDK's buffer is described as unbounded in the same
sentence, and the driver's `out_frames` arm keeps feeding it on every loop turn,
`nym_driver.rs:362-369`); the paragraph discusses only `Step::Rebuild` and never
mentions `Step::Stop`, which is the path every deployment takes; and
`PRODUCTION.md` **does not exist in this repository**, so the residual is
recorded nowhere an auditor or operator can read it.

**A second inaccurate quantity, in the probe guard's rationale.**
`nym_driver.rs:304-311` argues that *"once we stop feeding it it can only drain,
and two silent rounds is 120 s of drain at the throttled rate — far more than any
residual it could be holding."* Both halves are wrong: the driver never stops
feeding the SDK (the `out_frames` arm at `:362` is armed on every turn of the
loop for the whole window), and a full pipeline is ~2,250 packets ≈ **270 s**,
i.e. **2.3x** the stated 120 s. `out_frames.len() == 0` at a tick instant says
nothing about the SDK's own buffer. See the Validation Information for why this
is a comment/reasoning defect rather than a live loss path.

## Recommendations

1. **Account for the loss. This is the cheapest change and the one with no
   downside.** `Step::Stop`, `Step::Rebuild` and `Step::Died` should each emit a
   count of what was abandoned — `out_frames.len()`, `in_flight.is_some()`, and
   ideally the request channel's `len()` — as **counts only**, which is #157-legal
   and tells an operator that migrations were destroyed. Today all three are
   completely silent.
2. **Drain before disconnecting on the shutdown path.** `Step::Stop` can pump
   `out_frames` into the SDK under a bounded deadline before calling
   `disconnect()`. That does not empty the SDK's own buffer, but it removes the
   41 frames the shim itself is holding, which is the part under this code's
   control.
3. **Join the driver at shutdown.** Return a `JoinHandle` for `run_driver` from
   `build_nym_transport` and have `main` await it (bounded) after
   `serve_with_shutdown` returns, so the documented "run it to completion" is
   actually true and so step 2 has time to run. The mixnet leg deserves at least
   the same courtesy as the 10 s `DRAIN_TIMEOUT` already given to the wallet leg.
4. **Signal the driver on *every* exit.** The fatal-accept-error return
   (`proxy.rs:499`) must also drive the shutdown sequence, or the supervisor must
   observe `main`'s exit some other way; today that path cancels a live client
   with no `disconnect()` at all.
5. **Stop calling channel hand-off "success", or hold the acknowledgement
   longer.** As long as `hub.rs:236-240` answers `Accepted` at hand-off, every
   defect downstream converts into "a transaction the user believes is spent and
   that exists nowhere". The honest options are to hold the acknowledgement until
   the frame has at least been accepted by the SDK, or to surface the uncertainty
   to the wallet.
6. **Correct the two inaccurate comments** — the residual at
   `nym_driver.rs:421-436` (the window is not bounded by the drain rate, and it
   applies to shutdown as much as to rebuild; and `PRODUCTION.md` is not in this
   repository) and the drain figure at `nym_driver.rs:304-311` (a full pipeline
   is ~270 s, not 120 s, and the driver never stops feeding the SDK).

## Validation Information

**Verdict: CONFIRMED, Medium.** Validated 2026-08-18. Every mechanical claim
about the shutdown path holds; one of the filing's three legs is demoted from a
harm to a comment defect, and two facts are added that the filing did not have.

### Mechanical verification

| Claim | Verified at |
|---|---|
| `Step::Stop` is `disconnect(); return;` with no guard, drain, count or log | `shim/src/nym_driver.rs:410-415` |
| `Step::Stop` is what a SIGTERM produces | `shim/src/nym_driver.rs:353-358`; `shim/src/nym.rs:953-956` (`run_supervisor`'s shutdown arm sends `Disconnect` and returns) |
| The driver's `select!` is not `biased`, so `Disconnect` can win over a non-empty `out_frames` | `shim/src/nym_driver.rs:352-369` |
| The wallet is answered at channel hand-off | `shim/src/nym.rs:659-661`, `:684-689`; `shim/src/hub.rs:236-240` |
| Pipeline depth 32 + 8 + 1 = 41 frames, plus the SDK's 1 + 8 | `shim/src/main.rs:335-336`; `shim/src/nym.rs:853-863`; `shim/src/nym_driver.rs:362` |
| 45 packets per submit (32 frame + 13 SURBs) at ~8.33 packets/s | `shim/src/nym.rs:96`, `:1085-1094`, `:1109` |
| `main` retains no `JoinHandle` and awaits only the proxy | `shim/src/main.rs:166-176`, `:341-356` |
| The 10 s drain covers the wallet leg only, and returns immediately when no connections are open | `shim/src/proxy.rs:180`, `:545-553` |
| A fatal accept error returns from `main` with the supervisor never signalled | `shim/src/proxy.rs:499`; `shim/src/nym.rs:929-1010` (the only `Disconnect` sender is the shutdown arm) |
| The module documents `disconnect()` as needing to run to completion | `shim/src/nym_driver.rs:25-29`; `shim/src/nym.rs:392-398` |
| `PRODUCTION.md` does not exist anywhere in the repository | `find audit-target/zeronym -name PRODUCTION.md` returns nothing |

### One leg demoted — this is a correction, not a confirmation

The filing's second numbered claim was that the probe-triggered rebuild is a live
loss path: that the `out_frames.len() == 0` guard (`nym_driver.rs:312`) "cannot
see what it is guarding", so a spurious `Step::Silent` fires `ClientEvent::Died`,
the supervisor answers `Rebuild`, and `:440` disconnects a **live** client
holding acknowledged submits.

**The code observation is correct and the harm does not follow.** The guard's
stated reasoning is genuinely wrong on both halves (the driver never stops
feeding the SDK; 120 s is 2.3x too small), and that is worth fixing. But firing
the false `Silent` additionally requires `seen == mark` — **zero inbound
messages across two 60-second rounds** (`nym_driver.rs:312`, `:574`, `:579`).
Every submit carries 13 reply SURBs and the hub answers each with a one-packet
`AckV1`, and that backflow arrives continuously while the backlog drains,
advancing `inbound_total` (`nym_driver.rs:372-384`,
`shim/src/nym.rs:237-251`, `:1027-1062`). A backlog large enough to strand the probe is
therefore also a backlog generating inbound traffic, and the `_` arm at
`nym_driver.rs:343-350` resets `silent_rounds` to 0. The states in which no
backflow arrives — the hub's mixnet client dead, the addresses all stale, the
gateway genuinely not delivering — are states in which those frames were
undeliverable anyway, so the rebuild is not what destroys them.

This is exactly the asymmetry the dedicated G21 pass established from the pinned
SDK (`globals/G21-…` §6.3): the hub is vulnerable to the equivalent false-Silent
because a SURB reply generates nothing in return, and **the shim is not**. The
harm is claimed only for `Step::Stop` (and, as an accounting defect, for
`Step::Died`); the probe leg is carried as recommendation 6.

The *scheduled-rotation* trigger for `Step::Rebuild` is a separate, still-open
filing (`nym-rotation-deferral-cannot-protect-submits-so-rotating-destroys-acknowledged-migrations.md`)
and is deliberately not re-argued here.

### Two things added that the filing did not have

1. **The loss window is adversary-widenable, and the filing said the opposite.**
   Its "what limits the attack" bullet named "the pipeline is usually empty" as
   the mitigating factor. The confirmed High
   `junk-sendtransaction-flood-…` removes it: ~1 byte/second of unauthenticated
   junk holds the ~50-slot pipeline permanently full, so a real migration
   entering it sits behind ~270 seconds of junk with a success already returned,
   and **any** restart during that window destroys it. The mitigating factor is
   now stated as removable, with its price.
2. **The 10-s-versus-0-s asymmetry, and the third exit.** The shim already
   contains the mechanism this issue asks for — a bounded drain — and applies it
   to the leg where nothing has been promised (`proxy.rs:545-553`) while giving
   zero seconds to the leg where success has already been returned. And a fatal
   `accept()` error (`proxy.rs:499`) exits without the supervisor ever being told
   to disconnect, so that path violates the module's own D12 rule outright.

### Corrections to the filed text

- **`hub.rs:231-233` corrected to `hub.rs:236-240`** (the `Ok(()) => Ok(Submit::Accepted { … })` arm).
- **`nym.rs:376-381` corrected to `nym.rs:377-381`** (the 20-consecutive-failure note).
- **`nym_driver.rs:601-607` corrected to `nym_driver.rs:598-607`** (`send_frame`'s doc comment).
- The severity claim that the truncated `disconnect()` matters in itself was kept
  deliberately small: at process exit the leaked SDK tasks die with the process.
  What matters is that the mechanism the module exists to provide is not
  delivered on the one path that always runs, and that this removes any window in
  which a drain could happen.
- The title and framing were narrowed from "all three teardown paths" to the
  shutdown path plus the accounting defect on the other two, per the demotion
  above.

### Severity: Medium, and why not higher or lower

- **Not Low.** It fires on every operator redeploy without any attacker; the
  quantity lost is adversary-controlled for about one byte per second; the loss
  is of transactions the user was explicitly told had succeeded, in a pool the
  network has closed; and it is invisible to everyone — there is no counter, no
  log line and no metric anywhere in either binary that records it.
- **Not High.** Each event destroys at most a few dozen frames, and at today's
  observed traffic an untimed restart usually destroys nothing unless an attacker
  is actively holding the pipeline full or the operator is deliberately aiming
  it. The two attacker-driven variants each require another party's capability
  (the confirmed High flood, or operator control of the process), and the
  worst-case volume is bounded by a ~50-slot pipeline rather than being
  open-ended.

### Distinct from, and not double-counting, three neighbours

- `nym-submit-acks-are-never-read-so-every-hub-refusal-is-invisible.md`
  (**Confirmed, Medium**) owns *refusals being invisible* — the hub receives the
  frame and says no, and nobody hears it. This issue owns *teardown destroying
  in-flight work* — the hub never receives the frame at all. Different mechanism,
  different fix (read the ack vs. drain and join).
- `junk-sendtransaction-flood-…` (**Confirmed, High**) owns the flood itself and
  is cited here only as the thing that removes this issue's limiting factor.
- `nym-rotation-deferral-cannot-protect-submits-…` (plausible) owns the
  scheduled-rotation trigger for `Step::Rebuild`.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
