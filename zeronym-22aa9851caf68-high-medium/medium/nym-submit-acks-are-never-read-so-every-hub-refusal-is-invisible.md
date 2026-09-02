# Every hub refusal ack is decoded and then discarded, so on the deployed mixnet transport nothing in the shim can distinguish a hub that refuses 100% of migrations from a healthy one

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/shim/src/nym.rs:595-690` (`NymHandle::submit`), `:652` (the dropped receiver), `:835-911` (`correlate`), `:877`, `:903`, `:1026-1062` (`deliver`), `:1031-1033`, `:1038`; payload definition at `audit-target/zeronym/shim/src/wire.rs:203-267`; sole consumer at `audit-target/zeronym/shim/src/hub.rs:228-250`; status surface at `audit-target/zeronym/shim/src/nym.rs:133-177`, `:277-296`; hub side at `audit-target/zeronym/hub/src/nym.rs:313-334` and `audit-target/zeronym/hub/src/server.rs:248-277`
**Found by agent:** Local (file audit of `shim/src/nym.rs`)
**In scope of audit?** Yes — priority area #4 (mixnet transport), #5 (fail-closed discipline), #6 (log/telemetry discipline)

## Description

The shim→hub mixnet protocol carries a typed acknowledgement frame, `AckV1`, whose
whole purpose is to tell the shim what the hub did with a diverted migration. It
can say `Accepted`, or `Refused` with one of five reasons — `ExpiryTooTight`,
`TooLarge`, `QueueFull`, `TipStale`, `BadFrame` (`shim/src/wire.rs:217-229`). The
type's own doc comment states the contract (`shim/src/wire.rs:209`):

> `/// The hub declined the submission. Every refusal fails closed at the shim.`

**No refusal fails closed at the shim, because no refusal is ever observed by
anything.** `NymHandle::submit` creates the ack waiter and drops its receiving
end in the same statement (`nym.rs:652`), so the verdict is discarded whichever
arm of `deliver` fires — `let _ = waiter.send(kind)` into a dead receiver
(`nym.rs:1031-1033`) if the waiter is still in the map, or `None => {}`
(`nym.rs:1038`) once the correlator's sweep has removed it (`nym.rs:903`). Both
arms are silent. `AckKind` and `AckRefusal` are referenced nowhere in
`shim/src/` outside `wire.rs` and the `Waiter::Ack` variant itself.

Not blocking the wallet on the round trip is a documented and defensible design
(`nym.rs:563-594`), and this issue does **not** dispute it. The defect is
narrower and is not a consequence of that design: **recording the verdict does
not require blocking the wallet**, and nothing records it. A hub whose queue is
full, whose tip has gone stale, or which is answering `bad_frame` to every frame
it receives produces exactly the same observable behaviour at every deployed shim
as a hub that is accepting and publishing everything:

* no `tracing` line (both discard arms are silent),
* no counter — `MixnetStatusInner` (`nym.rs:133-177`) has `deaths`,
  `consecutive_failures`, `replies_received`, `empty_inbound`,
  `sends_dispatched`, `last_reply_unix` and no refusal counter of any kind,
* `/healthz` still answers 200 (`proxy.rs:649` reads only
  `MixnetStatus::is_healthy`, which is client connectivity — `nym.rs:307-309`),
* `/nym-status` still reports `mixnet_connected: true`,
* `/nym-diag`, when opened, reports `replies_received` climbing — it is
  incremented for *any* non-empty inbound message (`nym.rs:242`), so it rises
  identically whether every ack says `Accepted` or every ack says `QueueFull`.

The system pays the full cost of the feedback channel and then throws the signal
away: 13 reply SURBs are minted and emitted per submit **per configured hub
address** (`nym.rs:96`, `:656`), and the hub spends one of its strictly
serialised outbound send slots building and emitting each ack
(`hub/src/nym.rs:313-334`).

## Attack Scenario and Steps

This is a detectability defect. It creates no new attacker capability by itself;
its severity comes from the fact that it is the reason the destruction attacks
this audit has already confirmed can be sustained indefinitely without anyone
being in a position to notice.

1. An attacker fills the hub's queue to `MAX_QUEUE_BYTES` (64 MiB) over the
   unauthenticated, no-ACL Nym ingress — the confirmed High
   `issues/confirmed/hub-queue-unauthenticated-fill-silently-destroys-migrations.md`.
   From that moment `Hub::admit` returns `Refusal::Full` for every genuine
   migration (`hub/src/server.rs:256-276`).
2. A wallet behind any shim sends an Orchard-touching `SendTransaction`. The shim
   classifies it, diverts it, dispatches the `SubmitV1` frame and immediately
   answers the wallet `SendResponse { error_code: 0, error_message: <locally
   computed txid> }` (`intercept.rs:180-203`).
3. The hub refuses it, logs it correctly on its own side, and sends back
   `AckV1(Refused(QueueFull))` over one of the 13 SURBs the shim attached.
4. The shim reassembles it, `wire::decode_ack` parses it correctly, and `deliver`
   discards the decoded verdict without a log or a counter.
5. The migration is gone. The wallet believes it was sent. **Every readable
   surface on the shim is green.**
6. The attacker can sustain this for as long as they like. Nothing raises an
   alarm, because the mechanism that would raise one is unwired.

The same invisibility applies with no attacker at all: `TipStale` (the hub's
indexer is down or lagging), `ExpiryTooTight`, and `BadFrame` (a wire-format skew
between a shim and a hub built from different commits).

**A second-order effect worth stating: the adversary has better visibility into
the hub's admission state than the honest operator does.** Anyone can run a Nym
client, submit a frame with their own reply SURBs, and *read* the returned
refusal code — that is filed separately as
`hub-ackv1-refusal-codes-are-an-anonymous-real-time-admission-state-oracle.md`.
The one party structurally unable to read it is the shim whose users' migrations
are being destroyed.

**Attack Requirements and Assumptions:**

- Causing the *invisibility* requires no access at all; it is unconditional and
  affects the shipped transport (`deploy.env.example` selects `HUB_NYM`).
- Causing the *refusals* requires only the ability to send Nym frames to the
  hub's published address, which is public by design and has no submitter ACL.
- This issue does not claim the wallet could be told. Under dispatch-only the
  wallet has already been answered before the ack exists. The claim is that the
  **operator and the project** have no signal, and that a signal is available for
  free on a channel already paid for.

## Impact on Users

A user's mandatory Orchard→Ironwood migration is destroyed after their wallet was
told it succeeded, and no party — the user, the shim operator, or Shielded Labs —
is in a position to learn that a class of migrations is being destroyed at all.
The user's own recovery path is to notice, unaided, that a transaction they were
told succeeded never confirmed, and to wait out the expiry — which for a ZIP 318
migration is 30 to 60 days (owned by
`zip318-canonical-expiry-is-the-only-recovery-clock-…`).

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


The absence of the signal is what converts a bounded outage into an unbounded
one. Every other loss path in the system has the same property, which is why
`globals/G7-loss-of-a-wallet-acknowledged-migration.md` §5(b) concludes that
**reading the ack is the single change that converts almost every row of the loss
taxonomy from invisible to observable**, at no cost in wallet latency and no new
information for the parent host (the five reason strings are fixed, carry no
per-entry data, and `AckRefusal::as_str` was written and documented as
*"safe to log"*, `wire.rs:257-266`).

## Technical Details / Code Analysis

**1. The receiver is dropped at construction.** `shim/src/nym.rs:642-661`:

```rust
        for target in 0..targets {
            // A FRESH nonce per address: two hubs answering the same nonce would be
            // indistinguishable to the correlator, and the ack is unread anyway.
            let nonce = fresh_nonce();
            ...
            let frame = wire::encode_submit(&nonce, tx_bytes).map_err(NymError::Encode)?;
            let (ack_tx, _drop_receiver) = oneshot::channel();
            let request = Request {
                nonce,
                frame,
                reply_surbs: SUBMIT_REPLY_SURBS,
                waiter: Waiter::Ack(ack_tx),
                target,
            };
```

`_drop_receiver` is a real binding (not `_`), so it lives to the end of the loop
iteration and is dropped as soon as `send()` returns — before `submit` returns to
its caller, and long before the frame reaches the mixnet.

**2. The verdict is discarded on both arms, so timing is immaterial.**
`shim/src/nym.rs:1026-1044`:

```rust
fn deliver(pending: &mut HashMap<Nonce, Waiter>, bytes: &[u8]) {
    match bytes.len() {
        wire::ACK_BYTES => match wire::decode_ack(bytes) {
            Ok((nonce, kind)) => match pending.remove(&nonce) {
                Some(Waiter::Ack(waiter)) => {
                    let _ = waiter.send(kind);
                }
                Some(other) => {
                    pending.insert(nonce, other);
                    tracing::warn!("an ack arrived for a lookup's nonce; ignoring it");
                }
                None => {}
            },
            Err(err) => {
                tracing::warn!(reason = %err, "inbound message could not be decoded as an ack")
            }
        },
```

`kind` is the fully decoded `AckKind::Refused(AckRefusal::QueueFull)`. If the
waiter is still in `pending`, `let _ = waiter.send(kind)` sends it into a receiver
that no longer exists and the `Err` is discarded by the `let _`. If the sweep has
already removed it, `None => {}` fires. **Neither logs and neither counts.**

`deliver` has exactly three non-logging arms — the ack-delivery arm above and the
two `None => {}` misses (`:1038`, `:1055`). Every arm that *does* log is a decode
failure or a kind mismatch. Stated as a property: **`deliver` speaks only when it
cannot understand a reply, and is silent exactly when it understood one and threw
it away.**

**3. In practice it is the `None` arm that fires, because the sweep runs first.**
`shim/src/nym.rs:869-905`:

```rust
            request = requests.recv(), if permit.is_some() && requests_open => match request {
                Some(Request { nonce, frame, reply_surbs, waiter, target }) => {
                    permit
                        .take()
                        .expect("the arm is guarded on holding a permit")
                        .send(OutFrame { frame, reply_surbs, target });
                    pending.insert(nonce, waiter);
                }
                None => requests_open = false,
            },
            ...
        }
        // Callers that timed out (or were cancelled) have dropped their
        // receivers; ...
        pending.retain(|_, waiter| !waiter.is_abandoned());
```

`pending.retain` runs unconditionally after **every** turn of the select, and
`Waiter::is_abandoned` is `tx.is_closed()` (`nym.rs:320-329`), which is already
true. So the waiter is inserted at `:877` and removed at `:903` on the same turn,
or on the next one at the latest (`SWEEP_INTERVAL = 1 s`, `nym.rs:501`). The ack
cannot arrive inside that window: by the crate's own throughput arithmetic
(`nym.rs:1104-1115`) a single `SubmitV1` is `packets(FRAME_BYTES) +
SUBMIT_REPLY_SURBS` = 32 + 13 = 45 Sphinx packets at
`THROTTLED_PACKETS_PER_SEC ≈ 8.33`, i.e. **≈5.4 s of pure outbound emission
before any mix delay**, and that is only the outbound leg.

**4. Nothing else consumes an `AckKind`.** A grep over `shim/src/` finds
`AckKind`/`AckRefusal` only in `wire.rs` (codec + unit tests), `nym.rs:316` (the
`Waiter::Ack` variant), and `nym.rs:1169-1290` (tests). `hub.rs:228-250` — the
only caller of `NymHandle::submit` — says so itself:

```rust
            HubTransport::Nym(handle) => match handle.submit(tx_bytes).await {
                // ... There is no Refused arm: the hub's verdict is a full round trip
                // away and is deliberately not waited for (see `NymHandle::submit`), so
                // a refusal is never surfaced here.
                Ok(()) => Ok(Submit::Accepted {
                    txid: crate::nym::local_txid(tx_bytes),
                }),
```

**5. The hub does its half correctly, and writes it where nobody can read it.**
`hub/src/nym.rs:313-334` builds the ack from the real admission verdict, and
`hub/src/server.rs:273` logs `info!(reason = refusal.as_str(), "submission
refused at admission")`. So it is not true that *nothing* in the system records a
refusal — the hub does. What is true, and is the point, is that (a) **nothing on
the shim side records anything**, and (b) the hub's record goes to `tracing`,
which in a correctly attested enclave (`debug { enabled = false }`) reaches no
console at all — see `globals/G7-…` §5(a) and the separately filed
`hub-health-surface-blind-to-the-states-that-destroy-migrations.md`. The two
readable surfaces the hub exposes, `/healthz` and `/nym-status`, are blind to
`Full` and `TipStale`.

**6. The dead channel inflates the cost of every submit by 40%, on the exact
budget the confirmed flood attack exhausts.** `nym.rs:88-96` already concedes the
waste:

```rust
/// NOTE (dispatch-only submit): the shim no longer awaits the ack, so most of
/// these SURBs are now unused send-path overhead — the hub spends them replying
/// into a dropped receiver. ... Trimming it toward the
/// anonymity minimum is a throughput follow-up, ... and low priority, since submits
/// are rare (a migration is ~0.77 per block) next to the continuous cover traffic.
```

13 of the 45 packets a submit costs are SURBs for a reply nobody reads. The
stated reason for deprioritising the trim — *"submits are rare"* — is exactly the
premise that the confirmed High
`junk-sendtransaction-flood-consumes-the-shims-whole-mixnet-egress-…` destroys:
an unauthenticated stranger makes submits arbitrarily frequent for about a byte a
second. Trimming the SURB count would reduce that attack's per-frame amplification
by ~29% (45 → 33 packets); it does **not** close it, and it must not be done by
setting the count to zero (see Recommendations).

## Recommendations

Keep dispatch-only. Stop discarding the verdict.

1. **Do not drop the ack receiver in `NymHandle::submit` (`nym.rs:652`).** Either
   hold it in a detached task that records the outcome, or replace the
   `Waiter::Ack` oneshot with a sink owned by the transport so `deliver` can
   record every ack it decodes.
2. **This is the load-bearing fix: add refusal counters to `MixnetStatusInner`,
   keyed by `AckRefusal::as_str()`** (five fixed strings, no per-entry data), and
   publish them on `/nym-status`. A non-zero `queue_full` or `tip_stale` count is
   precisely the "this shim's migrations are being destroyed" signal that has no
   representation anywhere today. Do this even if nothing else on this list is
   done.
3. A rate-limited `tracing::warn!(reason = refusal.as_str(), …)` is worth adding
   for the debug/non-attested case, but **on its own it fixes nothing in the
   deployment that matters**: under `debug { enabled = false }` the enclave has no
   console, which is why item 2 and not this one is the fix.
4. At minimum, count the `None` arm of `deliver` (`:1038`) so "acks are arriving
   and matching nothing" is distinguishable from "no acks are arriving".
5. **Do NOT delete the ack from the `SubmitV1` flow, and do NOT set
   `SUBMIT_REPLY_SURBS` to zero.** An earlier draft of this issue recommended
   exactly that; it is wrong and would cause harm on two counts. (a) The ack's
   inbound packet increments `replies_received` (`nym.rs:242`), which feeds
   `inbound_total` (`nym.rs:251-253`), which is the **sole** input to the driver's
   liveness probe (`nym_driver.rs:285`, `:312`); `globals/G21-…` records that this
   backflow is exactly why the shim is not exposed to the hub's confirmed liveness
   fleet-kill. (b) `nym.rs:93-95` states that a zero SURB count would push the
   driver off the anonymous-send path (M6/D3), which is an anonymity regression.
   The correct change is to *trim* the count toward the measured minimum while
   keeping it non-zero, and to *read* what comes back.
6. Fix `shim/src/wire.rs:209`, which currently asserts *"Every refusal fails
   closed at the shim"* — false on the deployed transport. (Owned by
   `wire-ack-refusal-documented-as-fail-closed-is-discarded-on-the-mixnet-path.md`;
   listed here only so the recommendations are complete.)

**Cross-references (distinct findings; do not merge):**
`hub-queue-unauthenticated-fill-silently-destroys-migrations.md` (confirmed High —
a way to make the hub refuse),
`junk-sendtransaction-flood-…md` (confirmed High — the shim-side denial this
blindness compounds), `hub-tip-overshoot-latches-hub-permanently-stale.md`,
`hub-ackv1-refusal-codes-are-an-anonymous-real-time-admission-state-oracle.md`
(the same codes, readable by an attacker),
`readme-says-a-failed-migration-can-fail-silently-…md` (confirmed Low — the
documentation side), `divert-nym-hub-refusal-test-is-vacuous-and-the-only-wallet-facing-refusal-arm-is-untested.md`,
`shim-hub-submit-verdict-type-means-different-things-on-the-two-transports.md`,
`hub-health-surface-blind-to-the-states-that-destroy-migrations.md`, and
`globals/G7-loss-of-a-wallet-acknowledged-migration.md` (the taxonomy).

## Validation Information

**Validated 2026-08-18. Verdict: CONFIRMED. Severity held at Medium.**

### Mechanics — every claim re-derived from the target, not inherited

| claim | verified |
|---|---|
| the ack receiver is dropped at construction | yes — `shim/src/nym.rs:652`, `let (ack_tx, _drop_receiver) = oneshot::channel();`. `_drop_receiver` is a binding, not `_`, so it is dropped at the end of the loop iteration, immediately after `send()` returns |
| `is_abandoned` is `tx.is_closed()` and is already true when the waiter is inserted | yes — `nym.rs:320-329` |
| `pending.retain` runs after **every** select turn, not only on the sweep tick | yes — `nym.rs:903`, outside the `select!`, unconditional |
| the sweep therefore removes the waiter on the same turn (worst case one turn / 1 s later) | yes — `SWEEP_INTERVAL = 1 s`, `nym.rs:501` |
| a submit is 45 Sphinx packets ≈ 5.4 s of emission at the throttled rate | yes — `FRAME_BYTES = 65536` (`wire.rs:72`), `PACKET_BYTES = 2048`, `SUBMIT_REPLY_SURBS = 13` (`nym.rs:96`), `THROTTLED_PACKETS_PER_SEC = 1000/120` (`nym.rs:1093`). 32 + 13 = 45; 45 / 8.33 = 5.4 s outbound alone |
| so 100% of acks arrive after their waiter is gone | yes, and **the conclusion does not depend on it** — see the correction below |
| `AckKind`/`AckRefusal` appear nowhere in `shim/src/` outside `wire.rs` and the `Waiter::Ack` variant | yes — full grep of `shim/`; the only other hits are in `shim/tests/nym.rs` and `shim/tests/divert_nym.rs` |
| `HubTransport::submit` has no `Refused` arm on the Nym path | yes — `shim/src/hub.rs:228-250`, with the comment quoted |
| `MixnetStatusInner` has no refusal counter | yes — `nym.rs:133-177` enumerated in full |
| `replies_received` counts any non-empty inbound message | yes — `nym.rs:239-242` |
| `/healthz` reads only client connectivity | yes — `proxy.rs:649` → `nym.rs:307-309` (`!configured || connected`) |
| `wire.rs:209` claims every refusal fails closed at the shim | yes, verbatim |
| the hub emits a correct ack from the real verdict | yes — `hub/src/nym.rs:313-334` |

### Four corrections applied to the filed text

1. **"Nothing anywhere in the system records that migrations are being refused"
   was too strong and has been struck.** The hub *does* record it —
   `hub/src/server.rs:273`, `info!(reason = refusal.as_str(), "submission refused
   at admission")`. The accurate claim, now in the file, is that nothing on the
   **shim** side records anything, and that the hub's record goes to a `tracing`
   console that does not exist under `debug { enabled = false }`. This matters:
   a report sentence saying the refusal is recorded nowhere would be refutable in
   one grep.
2. **"`nym.rs:1031-1033` is the only arm of `deliver` that does not log" is not
   literally true and has been restated.** `deliver` has three silent arms:
   the ack-delivery arm and the two `None => {}` misses (`:1038`, `:1055`). The
   true and stronger statement, which the report should use instead, is that
   *`deliver` logs exactly when it cannot understand a reply and is silent exactly
   when it understood one and discarded it.* **Note for the report generator: the
   confirmed sibling `readme-says-a-failed-migration-can-fail-silently-…md` says
   "Every other arm of this function logs." Use the corrected form above; the
   point it was making survives intact and in a stronger form.**
3. **The filed emphasis on the sweep race was misplaced and has been
   de-emphasised.** Whether the sweep wins or the ack does is irrelevant: if the
   waiter is still present, `let _ = waiter.send(kind)` drops the verdict into a
   dead receiver and swallows the `Err`. The verdict is discarded on *both* arms.
   This makes the finding independent of any timing argument, which is a
   strengthening, not a weakening.
4. **Filed recommendation 5 was inverted because it would have caused harm.**
   It read: *"If the decision is instead to keep the ack unread, delete the ack
   from the `SubmitV1` flow entirely and drop `SUBMIT_REPLY_SURBS` handling for
   submits."* Implementing that would (a) remove the inbound backflow that
   `replies_received` → `inbound_total` → the driver's liveness probe depends on
   (`nym.rs:242`, `:251-253`, `nym_driver.rs:285`, `:312`) — which
   `globals/G21-…` identifies as precisely why the shim is **not** exposed to the
   hub's confirmed liveness fleet-kill — and (b) per `nym.rs:93-95`, push the
   driver off the anonymous-send path if the count reached zero, an anonymity
   regression. The recommendation now says the opposite: keep the channel, keep
   the count non-zero, trim it, and read the reply.

### Two things this issue does **not** claim, stated so the report does not overreach

- It does not claim the wallet could have been told. Under dispatch-only the
  wallet is answered before the ack exists; that trade is documented at
  `nym.rs:563-594` and is not the finding.
- It does not claim a new attacker capability. The destruction is owned by two
  **confirmed High** issues. This issue owns the reason the destruction is
  undetectable and therefore unbounded in duration.

### Consequence found during validation that the filing did not state

The dead channel costs 13 of every submit's 45 Sphinx packets, on the shim's
throttled mixnet egress — the exact budget the confirmed High
`junk-sendtransaction-flood-…` exhausts. The code's own note deprioritises
trimming it *"since submits are rare"* (`nym.rs:88-96`), which is the premise that
issue removes: an unauthenticated stranger makes submits arbitrarily frequent for
about a byte a second. Trimming the count is therefore a ~29% mitigation of a
confirmed High as well as a throughput change — but it is a mitigation, not a fix,
and it must not be done by zeroing the count.

### Why Medium, and not higher or lower

**Not High.** The defect creates no attacker capability on its own; every
migration it fails to report was destroyed by a mechanism that is filed and graded
elsewhere (two confirmed Highs). No user data is exposed and no funds move.

**Not Low or Info.** Three things keep it at Medium:

1. It is the **mechanism carrier** for the entire acknowledgement-boundary family.
   The sibling documentation, test and type-level findings
   (`readme-says-a-failed-migration-…` Low,
   `divert-nym-test-certifies-…`, `wire-ack-refusal-documented-as-fail-closed-…`,
   `shim-hub-submit-verdict-type-means-different-things-…`) all describe the same
   defect from a different surface; this file is the code. The confirmed README
   issue was explicitly deflated Medium → Low **on the stated basis that this file
   owns the harm** — grading this one below Medium would leave the family with no
   carrier and would trigger that file's recorded revisit condition. It is not
   being triggered: this issue is graded Medium, so
   `readme-says-a-failed-migration-…` correctly stays at Low.
2. An entire wire-protocol safety feature is inert while the system pays 100% of
   its cost, and the code documents the opposite contract (`wire.rs:209`). That is
   a real, unconditional defect on the shipped transport, affecting every user of
   every diverting endpoint on every failure.
3. `globals/G7-…` §5(b) establishes that reading the ack is the single highest-
   leverage change available for the whole silent-loss class. A defect whose fix
   is the system's best available detection improvement is not a code-quality note.

### Instruction to the report generator

This is the **code-side owner** of the acknowledgement-boundary chapter. Carry it
as the mechanism; carry `readme-says-a-failed-migration-…` (Low) beside it as the
disclosure defect; do not count them as two loss findings. The three facts worth
quoting are (a) `wire.rs:209` states *"Every refusal fails closed at the shim"*
and none does; (b) `deliver` logs exactly when it cannot understand a reply and is
silent exactly when it understood one and discarded it; and (c) the transport
asymmetry — identical hub state yields `error_code = -1` on clearnet
(`hub.rs:143-151` → `intercept.rs:188`) and `error_code = 0` on the mixnet, and
`intercept.rs:188` is the only line in the system that can report a hub refusal to
a wallet, is unreachable in the deployed configuration, and is executed by no
test.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
