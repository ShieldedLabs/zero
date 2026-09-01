# The hub's inbound-liveness verdict is measured through the SDK send queue an anonymous stranger controls, so a pulsed lookup burst makes a healthy hub declare itself undelivered-to — and `short_lives` never decays, so five pulses reach the fresh-identity fallback

**Severity**: Medium
**Validation Status**: Confirmed
**Location**:
`audit-target/zeronym/hub/src/nym_driver.rs:417-451` (the probe arm), specifically `:421` (the silence predicate, with **no** send-backlog conjunct), `:432` (`Step::Silent`), `:484-493` (`Sent::Probe => inbound_at_probe = Some(inbound_total)`), `:494-503` (`inbound_total += 1`), `:530-570` (the short-life accounting, especially `:558` `if silent || lived < STABLE_LIFE` and `:568` the reset), `:240-245` (`Failures::exhausted`), `:279-306` (the fresh-identity fallback), `:112` (`STABLE_LIFE`), `:124` (`SHORT_LIVES_BEFORE_NEW_IDENTITY`), `:133` (`PROBE_INTERVAL`), `:138` (`SILENT_ROUNDS_BEFORE_REBUILD`), `:632-643` (`reply_send`), `:647-671` (`probe_send` / `send_probe`);
the guard the hub is missing: `audit-target/zeronym/shim/src/nym_driver.rs:284-311` (the rationale) and `:312` (`Some(mark) if seen == mark && out_frames.len() == 0`);
the cheap reply that manufactures the backlog: `audit-target/zeronym/hub/src/nym.rs:167-196` (the lookup arm), `:232-234` (`is_lookup`), `:265-272` (the `hash.is_empty()` arm), `audit-target/zeronym/hub/src/wire.rs:44-49` (the `LookupV1` layout) and `:86` (`LOOKUP_BYTES = 64`);
the emission arithmetic: `audit-target/zeronym/shim/src/nym.rs:96-104` (`LOOKUP_REPLY_SURBS`; a full frame is 41 reply packets) and `:1066-1152` (`throughput_budget`, and the project's own measurement that a deployed enclave pegs at the throttled rate);
the ingress: `audit-target/zeronym/hub/src/server.rs:62-70` (`GET /nym-address` publishes the target), `:76-83` (`GET /nym-status` publishes `client_deaths`), `audit-target/zeronym/hub/deploy/caution/caution.hcl.tmpl:51-55` (`ingress 0.0.0.0/0`);
the pinned SDK (`nym-sdk` git rev `451c2aa3692fc4dc00041b74a352d4158176d9c0`): `sdk/rust/nym-sdk/src/mixnet/traits.rs:80` and `:126` (both `send_message` and `send_reply` use `TransmissionLane::General`), `common/client-core/src/client/real_messages_control/real_traffic_stream.rs:427-500` (one whole message stored per Poisson tick, one packet emitted per tick), `common/client-core/src/client/transmission_buffer.rs:39-49` (unbounded) and `:170-178` (`pop_front_from_lane`, strict FIFO within a lane), `common/client-core/src/client/received_buffer.rs:66-73` (loop cover traffic is filtered out and therefore never advances `inbound_total`), `common/client-libs/gateway-client/src/packet_router.rs:39-73` (acks are routed on a separate channel and never surface as messages), `common/client-core/src/client/replies/reply_controller/receiver_controller.rs:179-216` (`should_request_more_surbs`) and `common/client-core/config-types/src/lib.rs:48-50` (the SURB thresholds that decide whether the hub asks the attacker for more SURBs).
**Found by agent:** Global (focus area G21, resource exhaustion as a privacy attack — dedicated re-run)
**In scope of audit?** Yes — priority area #4 ("the mixnet transport ... identity rotation, the liveness probe"), and the `*/src/nym_driver.rs` row of "Code Areas That Should Get Extra Attention"

## Description

The hub's mixnet driver has exactly one self-repair mechanism: every 60 seconds it
sends an empty message to its **own** Nym address and counts the probe rounds in
which **no inbound message of any kind** arrived. Two such rounds tear the client
down (`Step::Silent`); five teardowns mint a fresh Nym identity, which permanently
invalidates the `ZIS_HUB_NYM` value baked into every shim's immutable enclave
configuration.

That terminal state is already confirmed as
`hub-nym-driver-automatic-fresh-identity-permanently-invalidates-every-shim.md`
(Medium) and is reached by two other confirmed issues. **This issue is not about
the terminal state. It is about a defect in the predicate that decides to go
there: the silence test does not measure whether the gateway is delivering to the
hub. It measures whether the hub's own outbound emission queue has drained far
enough for the probe to have left.** Those are the same thing only while the queue
is short, and any anonymous stranger can make it long for a few thousand sphinx
packets.

The predicate is (`hub/src/nym_driver.rs:419-421`):

```rust
                    match inbound_at_probe {
                        // A probe was outstanding and nothing at all has arrived
                        // since.
                        Some(mark) if inbound_total == mark => {
                            silent_rounds += 1;
```

The sibling binary has the identical mechanism **with a backlog conjunct**
(`shim/src/nym_driver.rs:312`):

```rust
                    Some(mark) if seen == mark && out_frames.len() == 0 => {
```

and the shim's author wrote out exactly why, at `shim/src/nym_driver.rs:289-311`:

> But "since" only means anything if the probe actually LEFT. The mark is
> stamped when the SDK accepts the probe into its one-slot input channel, not
> when it is emitted, and behind that slot sit an 8-deep batch channel and an
> unbounded FIFO drained at the throttled rate. Under a send backlog the probe
> is still queued behind every frame ahead of it, nothing has been asked of the
> gateway yet, and **"silent" is a statement about OUR queue, not about
> delivery.** Rebuilding on it would disconnect a healthy client and discard
> that whole queue.

The hub has no such conjunct, the hazard is cheaper to induce on the hub than on
the shim, and it costs far more when it fires.

### Why the hub's SDK queue can be made to hold minutes of emission

Three facts, all verified against the pinned SDK tree during validation:

1. **Handing a reply to the SDK is ~41x faster than emitting it.**
   `OutQueueControl::poll_poisson` polls its 8-slot `real_receiver` **once per
   Poisson tick** and, on `Ready`, stores the *entire* fragment vector of one
   message into the transmission buffer while emitting exactly **one packet**
   (`real_traffic_stream.rs:461-479`). A 64 KiB `LookupReplyV1` is 41 packets by
   the project's own measurement (`shim/src/nym.rs:99-104`), and the throttled
   emission rate is 8.33 packets/s — `MAX_DELAY_MULTIPLIER = 6` times the 20 ms
   default `message_sending_average_delay`, which the project states its deployed
   enclaves actually sit at (`shim/src/nym.rs:1071-1077`: *"the same code against
   a shared public gateway pegged at multiplier 6"*). So the General lane grows by
   **40 packets per tick** for as long as the hub has replies to hand over.
2. **The buffer is unbounded, un-aged and shared with the probe.**
   `TransmissionBuffer` is a bare `HashMap<TransmissionLane, VecDeque<..>>` with
   no cap (`transmission_buffer.rs:39-49`), `pop_front_from_lane` is strict FIFO
   within a lane (`:170-178`), and **both** `send_reply` and `send_message` use
   `TransmissionLane::General` (`nym-sdk/src/mixnet/traits.rs:80`, `:126`). The
   liveness probe is therefore queued strictly behind every reply fragment already
   stored.
3. **Nothing else advances `inbound_total`.** It is incremented once per
   reconstructed inbound message (`hub/src/nym_driver.rs:501`). Loop cover traffic
   is discarded before reconstruction (`received_buffer.rs:66-73`) and
   acknowledgements are routed on a separate channel that never reaches the
   message buffer (`gateway-client/src/packet_router.rs:39-73`), so neither
   surfaces through `wait_for_messages`. When the attacker stops sending, the
   hub's own probe echo is the **only** thing that can move the counter.

Points 1 and 2 are not new: the confirmed
`hub-nym-lookup-flood-starves-gettransaction-fleet-wide.md` established them and
showed that `REPLY_DEADLINE` and the `in_flight` guard are inoperative for the
same reason. What is new is point 3's consequence: **the mechanism that is
supposed to detect a dead gateway is measured through the very queue the attacker
controls.**

### Why five teardowns need not be five consecutive minutes

`failures` is reset in exactly one place (`hub/src/nym_driver.rs:567-569`): the
`else` of `if silent || lived < STABLE_LIFE`, i.e. only when a client **dies
non-silently** after living at least three minutes. A `Step::Silent` teardown
always increments `short_lives` regardless of how long the client lived (`:558`,
and the comment at `:553-557` says so deliberately), and a client that simply
keeps running never resets anything. So `short_lives` has **no decay**: a hub that
runs healthily for a week between attacker pulses still carries whatever
`short_lives` it had. The counter's own doc comment calls these "CONSECUTIVE
short-lived clients" (`:114-115`); in practice the only thing that clears them is
a rare event the attacker is not required to avoid for long.

## Attack Scenario and Steps

Attacker: anyone on the internet. No credential, no funds, no enclave compromise,
no gateway position, no ability to degrade any Nym node.

1. `curl https://<hub-domain>/nym-address`. The endpoint exists to publish the
   value and answers everybody (`hub/src/server.rs:62-70`).
2. Start a few stock `nym-sdk` clients. They are free: neither enclave sets
   `enabled_credentials_mode` and the SDK default is `false`.
3. **Burst.** Send anonymous 64-byte `LookupV1` frames
   (`b"ZNL1"` ‖ 16 random bytes ‖ `0x00` ‖ 43 zero bytes — `hub/src/wire.rs:44-49`),
   each carrying **>= 51 reply SURBs**, which is the provisioning a conforming shim
   uses (`LOOKUP_REPLY_SURBS = 60`, `shim/src/nym.rs:99-104`). Each takes the
   `hash.is_empty()` arm (`hub/src/nym.rs:269-272`), does **no** indexer dial and
   **no** queue scan, and yields a full 64 KiB `error` reply that the hub hands to
   the SDK within milliseconds. Two properties of the SURB count matter and are
   both required:
   - >= 51 (41 for the reply plus the SDK's `minimum_reply_surb_storage_threshold`
     of 10) means the whole 41-packet reply is prepared and queued at once, which
     is what builds the backlog;
   - it also leaves the hub's SURB pool above threshold afterwards, so
     `should_request_more_surbs` stays false
     (`receiver_controller.rs:179-216`) and the hub never asks the attacker for
     more SURBs. That matters because a SURB top-up would arrive as an inbound
     message and reset `silent_rounds` — see step 4.
4. **Go quiet for ~135 seconds.** Send nothing further, from any client.
5. Once the hub's own `outgoing` channel drains (it drains almost instantly — see
   Technical Details), the deferred probe tick fires. The first quiet tick sees
   `inbound_total != mark` (traffic arrived during the burst), resets
   `silent_rounds` and queues probe P1, stamping the mark at the final count. P1
   is appended to the General lane behind the attacker's backlog. Sixty seconds
   later nothing has arrived — P1 has not been emitted, the attacker is silent,
   cover traffic does not count and acks do not count — so `silent_rounds` reaches
   1; sixty seconds after that it reaches `SILENT_ROUNDS_BEFORE_REBUILD` and the
   driver takes `Step::Silent` (`:423-432`).
6. The hub calls `client.disconnect().await` (`:537`), which discards everything
   the SDK still holds — **including any honest shim's reply queued behind the
   attacker's** — records `status.set_died()`, increments `failures.short_lives`
   (`:559`), backs off 5 s and rebuilds on the same storage at the same address.
7. Repeat steps 3-6 four more times, at any spacing. On the fifth,
   `failures.exhausted()` is true at the top of the outer loop (`:286`), and the
   hub executes `storage = Ephemeral::default(); gateway = None;` — **a fresh Nym
   identity and therefore a new address.**

**Cost, recomputed during validation** (the original filing understated the burst
and overstated the role of `MAX_CONCURRENT_LOOKUPS`; see Technical Details §3):

- The probe must stay unemitted for two probe intervals plus the round trip, so
  the General lane must hold roughly `8.33 x 130 s` ~= **1,100 packets ~= 27
  full-frame replies** at the moment P1 is queued.
- Backlog grows at `41 x lambda - 8.33` packets/s, where `lambda` is the rate at
  which lookups reach the hub. The attacker therefore needs
  **`lambda > 0.21 lookups/s`**, i.e. more than ~11 sphinx packets/s of their own
  emission at ~52 packets per well-SURBed lookup.
- One stock client throttled to 8.33 packets/s cannot do it; **two can (~4
  minutes of bursting), three do it in ~100 s, five in ~45 s.** A client on an
  unthrottled gateway (multiplier 1, ~50 packets/s) does it alone, and an attacker
  writing raw sphinx traffic has no client-side shaping at all.
- Per pulse that is **~45-75 lookups, ~2,400-3,900 sphinx packets (~5-8 MB)**;
  five pulses is **~12,000-20,000 packets (~25-40 MB)**, spread over any interval
  the attacker likes.

**Attack Requirements and Assumptions:**

- **What makes it realistic.** The hub's Nym address is published by design and
  the enclave declares `ingress 0.0.0.0/0`. There is no ACL, no rate limit and no
  submitter identity to key one on. Every step uses stock SDK clients sending
  traffic that is byte-for-byte the shape a conforming shim sends — the SURB
  provisioning in step 3 is *exactly* what an honest lookup carries, so nothing
  distinguishes it.
- **The honest bound: the attacker needs a ~135-second window in which no shim
  traffic reaches the hub, five times in total.** Any inbound message — a real
  submit, a real lookup, or a SURB-replenishment artefact — advances
  `inbound_total` and resets `silent_rounds`. This is why the attack is a *pulsed*
  burst and not a sustained flood; a sustained flood keeps the hub looking alive.
  Two things make the window obtainable today: traffic is sparse (`README.md`
  records that no migration has yet been observed crossing Nym; the first
  third-party operator dates from 2026-08-10), and `short_lives` never decays, so
  the attacker needs five such windows *in total* rather than five in a row. It
  gets harder as the fleet grows, and every wallet `GetTransaction` behind any
  shim is a reset.
- **The other bound: an intervening non-silent client death after `STABLE_LIFE`
  resets the count.** The attacker cannot prevent that, but can race it by pulsing
  every few minutes, and failed attempts cost only packets.
- The attacker gets a free progress counter: `GET /nym-status` publishes
  `client_deaths` unauthenticated (`hub/src/server.rs:182-190`), which increments
  on every `Step::Silent`, and `GET /nym-address` confirms the final rotation.

## Impact on Users

**Scope note, so this is not double-counted.** The terminal state and everything
that follows from it are already owned by
`hub-nym-driver-automatic-fresh-identity-permanently-invalidates-every-shim.md`
(Confirmed, Medium) and are reached independently by
`hub-surb-starved-lookup-replies-grow-the-sdk-pending-buffer-without-bound-and-oom-the-enclave.md`
(Confirmed, High). **What this issue contributes is the trigger**: a deliberate,
cheap, stranger-reachable path to that state that needs no memory exhaustion, no
process restart and no position on the mixnet — and, critically, one that the
confirmed High's recommended fix does not close.

The consequences that belong to *this* file are:

1. **A stranger can tear down the hub's mixnet client on demand, repeatedly.**
   Each cycle costs the whole fleet a `disconnect()` plus a 5 s backoff plus a
   reconnect, during which `mixnet_connected` reads false and lookups fail closed
   at every shim. `client.disconnect()` at `:537` discards the SDK's whole
   transmission buffer, so every honest shim's reply queued behind the attacker's
   burst is destroyed too and those wallets see `UNAVAILABLE`. This is a
   *chosen-moment* outage: an adversary who wants a particular shim's lookups to
   fail, or wants to shape when the hub is carrying traffic, can produce it.
2. **The hub charges itself an irreversible strike for it.** Because
   `short_lives` never decays, each successful pulse is permanent progress toward
   an action — a fresh Nym identity — whose recovery the project's own runbook
   budgets at *"well over an hour"* across every operator, with *"no discovery
   mechanism; the handoff is a human message"*
   (`hub/deploy/caution/OPERATORS.md:230-243`).
3. **The log line asserts a conclusion the predicate cannot support.** `:426-431`
   says *"the hub is registered but not being delivered to"*. What the predicate
   actually established is "our own probe has not come back yet", and the probe
   may never have been emitted. An operator reading that line is told the wrong
   thing about their own system.
4. **The monitoring is misleading rather than absent, and the misleading part is
   what this trigger exploits.** `mixnet_connected` does flap false during each
   silent round and `client_deaths` does climb, so the attack is not invisible —
   but `OPERATORS.md:167-174` characterises climbing `client_deaths` as benign
   *"gateway churn"*, and the counter the same table names as the fresh-identity
   predictor, `consecutive_rebuild_failures`, reads **0** for the entire walk,
   because every cycle connects successfully and `NymAddress::set` zeroes it
   (`hub/src/server.rs:127-137`). The runbook's own early-warning field cannot
   move on this path. (This instrumentation gap is shared with, and primarily
   owned by, the confirmed fresh-identity issue.)

## Technical Details / Code Analysis

**1. The hub's probe arm, in full (`hub/src/nym_driver.rs:417-451`):**

```rust
                _ = probe.tick(), if in_flight.is_none() => {
                    match inbound_at_probe {
                        // A probe was outstanding and nothing at all has arrived
                        // since.
                        Some(mark) if inbound_total == mark => {
                            silent_rounds += 1;
                            if silent_rounds >= SILENT_ROUNDS_BEFORE_REBUILD {
                                tracing::error!(
                                    silent_rounds,
                                    gateway = %own.gateway(),
                                    "no inbound mixnet traffic across consecutive probes; the hub \
                                     is registered but not being delivered to. Rebuilding on the \
                                     same registration; if that stays silent the fresh-identity \
                                     fallback follows."
                                );
                                Step::Silent
                            } else {
                                tracing::warn!(
                                    silent_rounds,
                                    "no inbound mixnet traffic since the last probe; watching"
                                );
                                in_flight = Some(probe_send(sender.clone(), own));
                                Step::Ferried
                            }
                        }
                        // Either the first round, or traffic HAS arrived since the
                        // last probe ...
                        _ => {
                            silent_rounds = 0;
                            in_flight = Some(probe_send(sender.clone(), own));
                            Step::Ferried
                        }
                    }
                },
```

**2. The mark is stamped at SDK acceptance, not at emission
(`hub/src/nym_driver.rs:484-493`):**

```rust
                sent = drive(&mut in_flight), if in_flight.is_some() => {
                    in_flight = None;
                    match sent {
                        Sent::Reply => {}
                        // The mark means "inbound seen as of the probe going out",
                        // so it is read here and not when the probe was queued.
                        Sent::Probe => inbound_at_probe = Some(inbound_total),
                    }
                    Step::Ferried
                },
```

The comment says *"as of the probe going out"*. `Sent::Probe` resolves when
`sender.send_message(...).await` returns, which is when the SDK has taken the
message off its one-slot input channel and pushed its single fragment into the
8-slot `real_sender` — not when the packet is emitted.

**3. Why `MAX_CONCURRENT_LOOKUPS` and `outgoing` are not the limiting factor
(correction to the original filing).** The hub's reply hand-off is *not*
rate-limited. `sender.send_reply(tag, frame)` puts an `InputMessage::Reply` on the
capacity-1 input channel; `InputMessageListener::on_input_message` handles that
variant with a single `reply_controller_sender.send_reply(...)` on an **unbounded**
channel and returns (`acknowledgement_control/input_message_listener.rs:60-71`,
`:150-158`). So the driver empties its 64-deep `outgoing` channel in microseconds,
each lookup task's `MAX_CONCURRENT_LOOKUPS` permit is released almost immediately
(`hub/src/nym.rs:183-196`), and the pipeline recycles far faster than the attacker
can feed it. **The limiting factor is the attacker's own delivery rate**, and the
backlog is built inside the SDK's `ReplyController`/transmission buffer, not
inside zeronym's channels. The corrected arithmetic is in the Attack Scenario; the
original "one burst of 64 lookups puts 2,624 packets into the SDK" was right about
the packet count of 64 replies but wrong to treat 64 as the cap.

**4. The 41x fill/drain mismatch, from the pinned SDK
(`real_traffic_stream.rs:461-479`):**

```rust
            match Pin::new(&mut self.real_receiver).poll_recv(cx) {
                Poll::Ready(Some((real_messages, conn_id))) => {
                    self.transmission_buffer.store(&conn_id, real_messages);
                    let real_next = self.pop_next_message().expect("Just stored one");
                    Poll::Ready(Some(StreamMessage::Real(Box::new(real_next))))
                }
                Poll::Pending => {
                    if let Some(real_next) = self.pop_next_message() { ... }
                }
            }
```

One tick stores a whole message and emits one packet. `pop_next_message` ->
`pop_next_message_at_random` -> `pop_front_from_lane`
(`transmission_buffer.rs:170-178`), strict FIFO within a lane, and there is one
lane: `TransmissionLane::General` for both `send_reply`
(`nym-sdk/src/mixnet/traits.rs:126`) and `send_message` (`:80`).
`TransmissionBuffer` has no size limit (`transmission_buffer.rs:39-49`);
`prune_stale_connections` only evicts a lane idle for ten minutes, which the
General lane is not during the two-minute window that matters.

**5. Nothing but a real inbound message can clear the state
(`hub/src/nym_driver.rs:494-503`):**

```rust
                messages = client.wait_for_messages() => match messages {
                    Some(messages) => {
                        for message in messages {
                            // Counted BEFORE `deliver` filters ...
                            inbound_total += 1;
                            deliver(&incoming, message).await;
                        }
```

Loop cover traffic never reaches this point
(`received_buffer.rs:66-73`, `if nym_sphinx::cover::is_cover(fragment_data) { ...
return None }`), and acknowledgements are routed to a separate channel by the
gateway client (`packet_router.rs:39-73`), so they never become reconstructed
messages either.

**6. Why the attacker must over-provision SURBs, and why that is free.** If the
hub's SURB pool for a tag falls below `min_surb_threshold + buffer` it queues an
`AdditionalReplySurbs` request (`receiver_controller.rs:179-216`,
`:322-345`) on its own transmission lane, and
`pick_random_small_lane` (`transmission_buffer.rs:149-157`, "small" = fewer than
100 items) makes that short lane preempt the General backlog — so it would be
emitted promptly, and a stock client answering it would produce an inbound message
at the hub, resetting `silent_rounds`. With the defaults
(`min = 10`, `max = 200`, `buffer = 0`, `config-types/src/lib.rs:48-50`), a lookup
carrying 51-60 SURBs leaves 10-19 in the pool after its 41-packet reply, which is
at or above the threshold, so no request is made. This is precisely the
provisioning an honest shim uses, so the attack traffic is indistinguishable from
conforming traffic.

**7. A `Step::Silent` teardown always counts, and the counter is monotone in
practice (`hub/src/nym_driver.rs:530-570`):**

```rust
            Step::Died | Step::Silent => {
                let silent = matches!(step, Step::Silent);
                if silent {
                    client.disconnect().await;
                } else { ... }
                status.set_died();
                let lived = connected_at.elapsed();
                if silent || lived < STABLE_LIFE {
                    failures.short_lives += 1;
                    ...
                } else {
                    failures = Failures::default();
                }
```

**8. And five of them is the fleet kill (`hub/src/nym_driver.rs:240-245`,
`:286-306`):**

```rust
    fn exhausted(&self) -> bool {
        self.rebuilds >= REBUILDS_BEFORE_NEW_IDENTITY
            || self.short_lives >= SHORT_LIVES_BEFORE_NEW_IDENTITY   // 5
    }
```

```rust
        if failures.exhausted() {
            storage = Ephemeral::default();
            gateway = None;
```

**9. The reply that manufactures the backlog costs the hub 41 packets and the
attacker ~52 (`hub/src/nym.rs:232-234`, `:265-272`):**

```rust
fn is_lookup(frame: &[u8]) -> bool {
    frame.len() == wire::LOOKUP_BYTES && wire::peek_lookup_nonce(frame).is_some()
}
```

```rust
    if hash.is_empty() {
        tracing::warn!(reason = "empty lookup key", "lookup refused");
        return Some(error_reply(nonce));
    }
```

`error_reply` is `encode_lookup_reply(&nonce, &LookupReply::Error)`, which pads to
`FRAME_BYTES` = 65,536 (`hub/src/wire.rs:476-501`) with **no indexer dial and no
queue scan**, so the concurrency semaphore is never the constraint.

### Relationship to the already-filed issues, and why this is not a duplicate

- `hub-nym-lookup-flood-starves-gettransaction-fleet-wide.md` (confirmed, Medium)
  owns points 1-2 above and the **transient starvation** they cause. Its Impact
  section correctly states, for the attack shape it describes, that *"migrations
  themselves keep working during this attack, which bounds the severity"*. That
  bound holds for a **sustained** flood, which keeps `inbound_total` moving and so
  keeps the hub alive. It does not hold for the **pulsed** shape described here.
- `hub-nym-driver-automatic-fresh-identity-permanently-invalidates-every-shim.md`
  (confirmed, Medium) owns the terminal state and the instrumentation gap. Its
  triggers are environmental (a gateway outage, a nym-api outage, an adversary who
  can degrade one publicly-addressed Nym node). This issue supplies a strictly
  weaker adversary — no mixnet position at all — and a code defect (the predicate
  measured on the wrong queue) that its triggers do not involve.
- `hub-surb-starved-lookup-replies-...-oom-the-enclave.md` (confirmed, High)
  reaches the same terminal state via OOM and process restart. **Its headline
  remediation does not fix this one, and makes this attack's input the only
  answerable one — see the Recommendations.**
- `shim-nym-driver-liveness-selfheal-resets-instead-of-deferring.md` (plausible)
  is the same mechanism on the shim, failing in the **opposite** direction (the
  guard is present but resets instead of deferring, so a flood *prevents* the
  self-heal).

### Checked and does *not* apply to the shim

The shim's equivalent burst — fill its submit pipeline with junk
`SendTransaction` bodies, then go quiet — puts hundreds of packets of emission
into the shim's SDK, past the shim's own stated bound (*"two silent rounds is
120 s of drain at the throttled rate -- far more than any residual it could be
holding"*, `shim/src/nym_driver.rs:305-311`; a full pipeline is closer to 270 s,
so that premise is quantitatively wrong by ~2.3x and the comment should be
corrected). But it does **not** produce a false Silent on the shim, because each
junk submit carries 13 reply SURBs and the hub answers each with a 1-packet
`AckV1`; that backflow arrives throughout the drain window and advances the shim's
`inbound_total` (`shim/src/nym_driver.rs:393`). The hub has no analogous backflow:
a SURB reply generates nothing in return. Separately, a shim that did go falsely
Silent would only rebuild through its supervisor and reroll an identity it rotates
by design, so there is no fleet-invalidating consequence on that side.

## Recommendations

**These must ship together with the SURB requirement recommended by the confirmed
SURB-starvation OOM issue. The two remediations are anti-correlated:** that
issue's headline fix — answer a lookup only if it carried enough reply SURBs to
carry its own padded reply — closes the SURB-starved OOM **by making
well-provisioned lookups the only answerable ones**, and a well-provisioned lookup
is exactly the input this attack sends. Fixing either alone moves the attack
rather than removing it. Present the SURB requirement, a per-tag in-flight reply
bound, and a backlog-aware liveness verdict as **one** change, not three options.

1. **Make the silence verdict about the right queue.** `outgoing.len() == 0` (the
   conjunct the shim has) is necessary but not sufficient here, because the hub's
   own channel drains in microseconds while the SDK holds minutes. The SDK exposes
   `MixnetClient::shared_lane_queue_lengths()`
   (`sdk/rust/nym-sdk/src/mixnet/native_client.rs:259-261`); gate the silence
   verdict on the General lane being empty or below a small threshold, so "silent"
   once again means "the gateway is not delivering to us".
2. **Defer, do not count, and do not reset.** Under a backlog the round should be
   skipped entirely, leaving `silent_rounds` unchanged — the fix
   `shim-nym-driver-liveness-selfheal-resets-instead-of-deferring.md` recommends,
   applied to both binaries.
3. **Stop handing the SDK more than it can emit.** The `in_flight` boolean is not
   backpressure: it clears in microseconds while the emission it authorised takes
   ~5 s. Gate `outgoing.recv()` on the SDK's General lane length instead. This is
   the single change that also makes `REPLY_DEADLINE` and `MAX_CONCURRENT_LOOKUPS`
   operative again and bounds the transmission-buffer growth that the confirmed
   lookup-flood and OOM issues both end in.
4. **Do not spend a full 64 KiB frame on a request no conforming shim sends.** The
   `decode_lookup` failure arm and the `hash.is_empty()` arm
   (`hub/src/nym.rs:258-272`) are the cheapest way to manufacture backlog. The
   padding exists to hide `Found` from `NotFound`; these two are not on that axis.
   Drop them silently, exactly as the submit arm already drops a frame with no
   recoverable nonce (`hub/src/nym.rs:327-333`).
5. **Decay `short_lives`, or bound the fallback in wall-clock time.** A counter
   that only resets on a rare event is a counter that only goes up. Reset or age
   it out after any period longer than `STABLE_LIFE` in which the client stayed
   connected, and require the five strikes to fall inside a bounded window.
6. **Make the identity change require a human, or at least make it visible.** A
   fresh identity is a fleet-wide, irreversible action; gate it behind an explicit
   operator opt-in (`ZIH_ALLOW_FRESH_IDENTITY`), and in either case put an
   `identity_generation` counter on `/nym-status` beside `client_deaths`.
7. **Correct the log line at `:426-431`.** It asserts "registered but not being
   delivered to" from a predicate that cannot distinguish that from "our own queue
   is long".
8. **Test it.** Nothing in `hub/tests/` exercises `run_driver`'s probe arm or the
   short-life accounting (`grep -rn "run_driver\|silent_rounds\|short_lives"
   hub/tests/` returns nothing); `hub/tests/nym_identity.rs` only pins that
   `Ephemeral::default()` yields new keys. A test that feeds `outgoing` and asserts
   `silent_rounds` across ticks would catch both this and the shim's mirror defect.

## Validation Information

**Verdict: CONFIRMED as a real defect. Severity: High -> Medium (top of Medium),
for the double-counting reason set out below. The mechanism is confirmed in full;
the arithmetic and two impact claims were corrected.**

### The defect, verified line by line

- `hub/src/nym_driver.rs:421` is `Some(mark) if inbound_total == mark =>` with no
  second conjunct. `shim/src/nym_driver.rs:312` is
  `Some(mark) if seen == mark && out_frames.len() == 0 =>` with the twelve-line
  rationale quoted above. **The asymmetry is real and verbatim.**
- `:558` `if silent || lived < STABLE_LIFE { failures.short_lives += 1 }` and
  `:568` `else { failures = Failures::default() }`. `failures` is assigned in
  exactly three places in the file: `Failures::default()` at initialisation
  (`:269`), `+= 1` on a failed connect (`:314`) and on a short/silent life
  (`:559`), and the two resets — after the fallback fires (`:305`) and in that
  `else`. **`short_lives` has no decay path.** A hub whose client stays connected
  for a week, or whose clients only ever die silently, never clears it. Confirmed
  as claimed, and it is what turns a transient annoyance into permanent progress.
- `:240-245` `exhausted()` is an OR, so five short lives is independent of the
  sixty-connect-failures path. `:286-306` replaces `storage` with
  `Ephemeral::default()` and drops the gateway pin. Confirmed.

### The SDK mechanism, verified against the pinned tree at `451c2aa`

The tree was read directly, not inferred from zeronym's comments about it.

1. `traits.rs:80` and `:126`: `send_message` and `send_reply` both set
   `let lane = TransmissionLane::General;`. **Probe and replies share one lane.**
2. `transmission_buffer.rs:170-178`: `pop_front_from_lane` is a `VecDeque`
   `pop_front`. **Strict FIFO within the lane**, so a probe appended after 1,100
   reply packets is emitted after them. `:39-49`: no capacity anywhere.
   `:149-157`: `pick_random_small_lane` prefers lanes with fewer than 100 items,
   which is what makes a SURB-request lane preempt the backlog (§6 above) and is
   why the attacker must over-provision.
3. `real_traffic_stream.rs:461-479`: one `real_receiver` poll per Poisson tick,
   `transmission_buffer.store(&conn_id, real_messages)` for the whole fragment
   vector, one packet emitted. `config-types/src/lib.rs:25` gives the 20 ms base
   delay and `sending_delay_controller.rs:23` `MAX_DELAY_MULTIPLIER = 6`, so the
   throttled rate is 8.33 packets/s and the unthrottled one 50/s. The project's
   own comment (`shim/src/nym.rs:1071-1077`) records that deployed enclaves sit at
   multiplier 6, which is the case favourable to the attacker.
4. `received_buffer.rs:66-73`: cover traffic returns `None` before reconstruction.
   `gateway-client/src/packet_router.rs:39-73`: acks go to `ack_sender`, messages
   to `mixnet_message_sender`; they never mix. **Nothing but a real inbound
   message advances `inbound_total`.**
5. `receiver_controller.rs:179-216` plus `config-types/src/lib.rs:48-50`
   (`min = 10`, `max = 200`, `buffer = 0`): with >= 51 SURBs attached, the pool
   after a 41-packet reply is >= 10, `is_below_required_surbs` is false and no
   `AdditionalReplySurbs` request is emitted. `check_surb_refresh`
   (`:760-808`) only fires on a key-rotation change, which is an epoch-scale
   event. **So a well-SURBed attacker genuinely generates no inbound traffic at
   the hub**, which is the precondition the whole attack rests on. This was the
   most likely way for the attack to fail and it does not.
6. `nym-node/src/node/mixnet/handler.rs:281-320`: irrelevant to this issue but
   checked while validating the sibling — acks are forwarded by the destination
   gateway regardless of client state.

### Corrections made to the filing

- **The cost arithmetic was wrong and has been replaced.** The original said
  "~1,400 sphinx packets" per pulse from "two or three clients ... under two
  minutes", and treated `MAX_CONCURRENT_LOOKUPS` + `outgoing` as a 128-reply
  reservoir. In fact the hub's reply hand-off is unbackpressured
  (`input_message_listener.rs:60-71` forwards `InputMessage::Reply` on an
  unbounded channel and returns), so neither bound limits anything; the limiting
  factor is the attacker's own emission. Corrected model:
  `backlog_growth = 41*lambda - 8.33 packets/s`, needing `lambda > 0.21
  lookups/s`; **one stock throttled client is not enough**, two to five are, and
  the per-pulse cost is ~2,400-3,900 packets rather than 1,400. The total (five
  pulses, ~12,000-20,000 packets) is close to the original estimate.
- **"Nothing warns anybody" was too strong and has been replaced.** During each
  pulse `mixnet_connected` flaps to false and `client_deaths` increments, and
  `OPERATORS.md:186` tells operators to alert on `/nym-address` changing. The
  accurate and still-damaging statement, now in the Impact section, is that the
  runbook classifies climbing `client_deaths` as benign gateway churn and names
  `consecutive_rebuild_failures` as the fresh-identity predictor — and that
  counter provably reads 0 for the whole walk because every cycle connects and
  `NymAddress::set` zeroes it (`hub/src/server.rs:127-137`).
- **The claim that the missing conjunct is the fix was softened.** Adding
  `outgoing.is_empty()` to the hub would not stop this attack, because that
  channel drains in microseconds. The filing already said so in its
  recommendations; the Description now says it up front so no reader takes the
  one-line diff as sufficient.
- The "~130 s of quiet" figure was checked against the tick sequence and is right:
  the first quiet tick resets (traffic arrived during the burst) and re-marks, and
  two further ticks are needed, so the window is ~120-180 s depending on tick
  phase.

### Exploitability assessment

A real-world attacker can meet every precondition. The target address is published
by design at an endpoint whose purpose is to publish it; the enclave takes
`ingress 0.0.0.0/0`; there is no ACL, no rate limit and no credential requirement;
the traffic is byte-identical in shape and SURB provisioning to what a conforming
shim sends; and `/nym-status` gives the attacker a free progress counter. The
resource cost is tens of megabytes across a handful of free clients.

The two things the attacker does *not* control are the honest bounds, and they are
real: five ~135-second windows with no shim traffic reaching the hub, and no
intervening non-silent client death after `STABLE_LIFE`. On today's deployment
(one hub, a pilot fleet, no observed migrations over Nym) those are easy; on a
busy fleet where every wallet `GetTransaction` behind every shim is a reset, they
are materially harder and force the attacker to work at quiet hours and retry.
That is what separates this from the confirmed SURB-starvation OOM, which needs
one client, no timing, and works regardless of traffic.

### Severity: why Medium, and why not High or Low

Per the coordinator's instruction, the **trigger** is graded here and the
**consequence** is not re-counted.

- The terminal state — fresh identity, fleet-wide strand, silently destroyed
  acknowledged migrations, >1 h multi-party recovery — is already owned by
  `hub-surb-starved-...-oom` (High) and
  `hub-nym-driver-automatic-fresh-identity-...` (Medium). Grading a third route at
  High would count one outage three times, which is exactly the reasoning the
  validator of the fresh-identity issue applied when deflating it.
- Graded on its own, the trigger is: an unauthenticated stranger can, for tens of
  megabytes, tear down the hub's mixnet client at a moment of their choosing,
  destroy whatever replies were queued behind their burst, and bank irreversible
  progress toward a fleet-invalidating action. That is a repeatable,
  chosen-moment denial of service with collateral loss — Medium under this audit's
  scale ("Causes DoS with significant effort").
- Not Low, because the effort is genuinely small, the progress is permanent, the
  attacker gets a progress oracle, and the code defect is unambiguous.

**Remediation priority is higher than the severity label implies, and the report
must say so.** The confirmed High's recommended fix (require a lookup to carry
enough reply SURBs for its own padded reply) makes well-provisioned lookups the
only answerable ones, which is precisely this attack's input. Shipping that fix
alone removes one route to the fleet kill and leaves this one fully open. The SURB
requirement, a per-tag in-flight reply bound, and a backlog-aware liveness verdict
are one change.

### False-positive checks applied

- *§1 Assumption an attacker cannot violate?* No — the assumption "two probe
  rounds with no inbound means the gateway is not delivering to us" is violated by
  an ordinary send backlog, and the sibling binary's source says so explicitly.
- *§5 Impractical resource exhaustion?* No. The cost is tens of megabytes from
  free clients against a target with no rate limit, and the amplification is
  ~1:41 in packets (52 attacker packets buy 41 packets of a scarce, serialised,
  shaped emitter).
- *§6 Intentional design?* The fresh-identity escape hatch is by design; the
  predicate that walks to it is not, and the shim's copy of the same predicate
  documents the defect as a defect.
- *§9 Obviously broken functionality?* No — the false Silent requires a backlog
  that ordinary sparse traffic never produces, which is why the deployment works.
- *Double counting?* Explicitly avoided; see the severity section and the scope
  note in Impact.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
