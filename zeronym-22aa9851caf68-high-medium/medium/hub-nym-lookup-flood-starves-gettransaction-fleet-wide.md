# An anonymous lookup flood consumes the hub's single fleet-wide mixnet emitter, so `GetTransaction` stops working for every wallet on every shim

**Severity**: Medium
**Validation Status**: Confirmed
**Location**:
`audit-target/zeronym/hub/src/nym.rs:38-54` (`MAX_CONCURRENT_LOOKUPS`), `:56-75` (`REPLY_DEADLINE`), `:148-216` (`run_listener`, the lookup arm), `:218-234` (`is_lookup`), `:249-303` (`build_lookup_reply`);
`audit-target/zeronym/hub/src/nym_driver.rs:384-403` (the one-reply-in-flight guard), `:452-479` (the reply arm), `:632-643` (`reply_send` → `send_reply`);
`audit-target/zeronym/hub/src/wire.rs:476-501` (`encode_lookup_reply` pads every disposition to 64 KiB), `:439-470` (`decode_lookup`, `peek_lookup_nonce`);
`audit-target/zeronym/hub/src/main.rs:184-185` (both mixnet channels sized 64);
`audit-target/zeronym/hub/src/server.rs:62-70`, `:446-479` (`GET /nym-address` publishes the target to anyone);
`audit-target/zeronym/hub/deploy/caution/caution.hcl.tmpl:50-55` (`ingress 0.0.0.0/0`);
`audit-target/zeronym/shim/src/intercept.rs:229-247` (every `GetTransaction` goes to the hub), `:315-324` (fail closed with `UNAVAILABLE`);
`audit-target/zeronym/shim/src/nym.rs:45-71` (`REQUEST_TIMEOUT` = 90 s, and that it multiplies over the hub-address list), `:98-104` (`LOOKUP_REPLY_SURBS` = 60 and the measured 41-packet reply), `:1085-1130` (`throughput_budget`, the crate's own emission model).
The emission FIFO that actually starves honest lookups is in the pinned SDK (`nym-sdk` at git rev `451c2aa3692fc4dc00041b74a352d4158176d9c0`, `hub/Cargo.lock:4773-4775`): `common/client-core/src/client/real_messages_control/real_traffic_stream.rs:427-490` (`poll_poisson`), `:156-157` and `real_messages_control/mod.rs:150` (the 8-slot batch channel), `client/transmission_buffer.rs:39-49`, `:190-250` (`pop_next_message_at_random`, FIFO within a lane); `client/replies/reply_controller/requests.rs:12-15` (the unbounded reply-controller channel); `client/replies/reply_controller/receiver_controller.rs:218-270` (`handle_send_reply`'s SURB arithmetic); `common/client-core/config-types/src/lib.rs:25`, `:48-62`, `:443` (all shipped defaults).

**Found by agent:** Local (`hub/src/nym.rs`); validated 2026-08-18
**In scope of audit?** Yes — priority area 4 ("the mixnet transport") and `AUDIT-INSTRUCTIONS.md` unverified lead #7.

## Description

The hub answers every shim's `GetTransaction` over the Nym mixnet, and every
answer is a `LookupReplyV1` padded to a fixed 64 KiB (`wire.rs:476-501`) so its
size cannot reveal found-versus-not-found. The project's own nymnet measurement
puts that frame at **41 sphinx packets** (`shim/src/nym.rs:98-104`), and the
project's own emission model puts one mixnet client's floor rate at
**8.33 packets/s** — `MAX_DELAY_MULTIPLIER` (6) against the SDK's 20 ms default
`message_sending_average_delay` (`shim/src/nym.rs:1090-1094`, confirmed against
`client-core/config-types/src/lib.rs:25`, `:443`).

The hub has exactly **one** mixnet client, pinned to one gateway and one
identity by design (D10, address stability). So the hub's whole reply capacity —
for every shim, every operator, every wallet — is:

| regime | packets/s | seconds per 64 KiB reply | replies per minute, fleet-wide |
|---|---|---|---|
| the project's throttled floor (`THROTTLED_PACKETS_PER_SEC`) | 8.33 | ~4.9 | **~12** |
| the SDK's unthrottled default (20 ms) | 50 | ~0.8 | ~73 |

A stranger can consume all of it. The hub's Nym address is published on purpose
and answers everyone (`GET /nym-address`, `server.rs:62-70`; the enclave declares
`ingress 0.0.0.0/0`), a lookup is a 64-byte frame that only has to start with
`b"ZNL1"` (`is_lookup`, `nym.rs:232-234`), and **every** failure arm of
`build_lookup_reply` — an undecodable frame, an empty hash, an unavailable
indexer — still returns a full 64 KiB frame (`nym.rs:254-302`). There is no
authentication, no ACL, no rate limit, and no submitter identity to key one on:
`nym.rs:12-15` and `queue.rs:35-39` forbid holding one.

Because the shim routes **every** `GetTransaction` to the hub — it is stateless
and cannot recognise its own migrations (`intercept.rs:229-247`) — and fails
**closed** with `UNAVAILABLE` when the hub does not answer in 90 s
(`intercept.rs:315-324`, `nym.rs:45-71`), starving this one emitter breaks
transaction lookup for every wallet behind every shim pointing at that hub, not
only for migrating ones.

### The mechanism is not the one the hub's own comments describe

This was the central correction of validation, and it matters because it changes
which mitigation is load-bearing and which is inert.

The hub's design intends `outgoing` (64 deep) plus the driver's one-reply-in-flight
guard to be the queue, and `REPLY_DEADLINE` (60 s) to drop replies that have
outlived the shim's budget *before* they cost an emission
(`nym.rs:56-75`, `nym_driver.rs:384-403`, `:452-471`). **On the deployed SDK none
of that binds**, because handing a reply over is non-blocking:

`nym_driver.rs:638` calls `sender.send_reply(tag, frame)`, which is
`ClientInput::send` into an `mpsc::channel::<InputMessage>(1)`
(`base_client/mod.rs:1013`); `InputMessageListener::on_input_message` does
nothing at all for a `Reply` variant except
`reply_controller_sender.send_reply(...)`
(`acknowledgement_control/input_message_listener.rs:60-70`), and that channel is
`mpsc::unbounded` (`reply_controller/requests.rs:12-15`). So `send_reply`
returns in microseconds, `in_flight` is never held, `outgoing` drains at memory
speed, and a reply is essentially never in `outgoing` long enough for
`is_dead()` to fire.

The FIFO that really forms is one layer down, inside the SDK, and it is
strictly worse than the one the hub designed:

- it is `OutQueueControl::transmission_buffer`, a bare
  `HashMap<TransmissionLane, VecDeque<..>>` with **no size limit and no byte
  budget** (`transmission_buffer.rs:39-49`);
- it is drained one packet per Poisson tick and *filled* one whole message per
  tick (`real_traffic_stream.rs:443-478` polls `real_receiver` once per tick and
  stores the entire batch), so under overload it absorbs ~41× faster than it
  emits;
- **all** replies travel on `TransmissionLane::General`
  (`nym-sdk/src/mixnet/traits.rs:122-133`), and within a lane
  `pop_next_message_at_random` is `pop_front` on a `VecDeque`
  (`transmission_buffer.rs:190-208`) — strict FIFO;
- nothing in it ages anything out. `prune_stale_connections` only evicts a lane
  that has been idle for ten minutes, and under a flood the General lane is never
  idle.

So an honest shim's reply is enqueued behind the attacker's entire backlog and
emitted whenever that backlog clears — which under a sustained flood is never
inside the shim's 90 s budget. Two consequences follow that the filed text did
not have: the hub's only anti-starvation mitigation (`REPLY_DEADLINE`) is
**inoperative**, and the outage **outlives the attacker** by however long the
accumulated backlog takes to drain.

`MAX_CONCURRENT_LOOKUPS = 64` is also not the constraint here. It bounds memory
and concurrent indexer dials, which it does correctly, but the cheapest attack
frame (`hash_len = 0`) does no I/O at all (`nym.rs:269-272`), so its slot is held
for microseconds and the semaphore is never near full.

## Attack Scenario and Steps

Attacker: anyone with an internet connection. No credential, no enclave
compromise, no privileged network position, no Zcash knowledge, no funds.

1. `curl https://<hub>/nym-address`. The endpoint exists to publish the value and
   answers everyone (`server.rs:62-70`, `:462-479`).
2. Start two or three stock `nym-sdk` clients. They are free and need no
   registration: neither enclave sets `enabled_credentials_mode`, and the SDK
   default is `false` (`nym-sdk/src/mixnet/client.rs:281-288`), so an attacker's
   client runs on the same terms the target's own does.
3. From each, send a stream of 64-byte messages — `b"ZNL1"` ‖ 16 random bytes ‖
   `0x00` ‖ 43 zero bytes — as **anonymous** messages carrying **≥51 reply
   SURBs**. Fifty-one is the threshold: `handle_send_reply` sends
   `min(fragments, available_surbs − min_surb_threshold)` fragments with
   `min_surb_threshold = 10` (`receiver_controller.rs:248-256`,
   `config-types/src/lib.rs:48`), so 41 fragments need 51 SURBs to go out in one
   pass. The honest shim already attaches 60 (`shim/src/nym.rs:98-104`).
4. Each frame reaches `is_lookup` on size-plus-magic, takes the empty-key arm
   (`nym.rs:269-272`) with **no indexer dial and no queue scan**, and produces a
   full 64 KiB `error` reply that the hub hands to the SDK immediately.
5. The SDK emits those replies FIFO on lane General at 8-50 packets/s. Every
   honest shim reply enqueued afterwards waits behind them.
6. Each starved wallet lookup costs the wallet the shim's `REQUEST_TIMEOUT`
   (90 s), multiplied by the number of configured hub addresses
   (`each_target`, `shim/src/nym.rs:695-790`), and then returns `UNAVAILABLE`.

**Attack Requirements and Assumptions:**

- **The cost is symmetric per packet, and that is the point.** To have a reply
  emitted at all the attacker must attach ~51 reply SURBs, which the project
  measures at roughly one sphinx packet each (60 SURBs ≈ 60 packets,
  `shim/src/nym.rs:98-104`, `:1120-1130`). So ~51 attacker packets buy ~41
  packets of hub egress. **There is no bandwidth amplification.** The asymmetry
  is structural: the hub is one throttled client serving the entire fleet, and
  the attacker may run as many clients as they like. At equal rates ~1.3
  attacker clients match the hub's whole capacity; three swamp it; ten leave an
  honest lookup ~8 % of the emitter.
- Nothing stops them. There is no ACL, no rate limit and no per-submitter
  accounting anywhere on this path, and the anonymity requirement
  (`nym.rs:12-15`, `queue.rs:35-39`) is why — see the Recommendations for the
  identity-free controls that are nevertheless available.
- The attacker is unattributable: there is no source address, and the sender tag
  is never interpreted by design.
- Nothing alerts. `/healthz` is unconditionally 200 (`server.rs:449-452`),
  `/nym-status` reports `mixnet_connected: true, client_deaths: 0`, and the only
  in-process signal is a `warn!` that under `debug { enabled = false }` reaches
  no console. The SDK's own `log_status` does warn once the transmission buffer
  passes 1,000 packets (`real_traffic_stream.rs:570-578`) — into the same absent
  console.
- **What bounds it:** the flood is noisy on the mixnet, it must be sustained,
  and it costs the attacker real (if free) mixnet egress. It destroys nothing and
  discloses nothing.

## Impact on Users

- **Every wallet behind every shim pointing at this hub loses `GetTransaction`,
  fleet-wide, for as long as the attack runs plus the drain time afterwards.**
  Not only migrations: the shim routes *all* lookups to the hub. Each attempt
  hangs 90 s per configured hub address and then fails `UNAVAILABLE`.
- **This is more than cosmetic for a light wallet.** `GetTransaction` is the
  call a wallet makes to fetch a full transaction after finding it in a compact
  block ("enhancement"), so a fleet-wide outage presents to users as sync
  failures, not as one missing detail screen.
- **The realistic user response is the privacy loss.** A wallet that cannot
  fetch its transactions gets pointed at a different, unprotected indexer — which
  is the deanonymisation this product exists to prevent, and the attacker chooses
  the moment.
- **Migrations themselves keep working during this attack**, which bounds the
  severity and is worth stating plainly: admission is inline and never waits on
  the lookup bound (`nym.rs:120-144`), the shim's submit is dispatch-only and
  never awaits the ack, and the batcher publishes over HTTP, not the mixnet.
  Nothing is lost or corrupted by this issue on its own.
- **Sustained, it degenerates into the confirmed OOM.** Because the SDK absorbs
  ~41× faster than it emits, a sustained flood also grows the hub's transmission
  buffer without bound — the same terminal outcome as
  `hub-surb-starved-lookup-replies-grow-the-sdk-pending-buffer-without-bound-and-oom-the-enclave.md`,
  reached ~40× more expensively. That outcome is graded there, not here.

## Technical Details / Code Analysis

**1. Any 64-byte frame with the lookup magic buys a full 64 KiB frame, with no
I/O.**

`hub/src/nym.rs:232-234`:

```rust
fn is_lookup(frame: &[u8]) -> bool {
    frame.len() == wire::LOOKUP_BYTES && wire::peek_lookup_nonce(frame).is_some()
}
```

`peek_lookup_nonce` (`hub/src/wire.rs:459-470`) checks only length ≥ 21 and the
4-byte magic, so the added condition is `len == 64`. `decode_lookup`
(`wire.rs:438-457`) accepts `hash_len = 0` without error, so a well-formed frame
reaches:

```rust
// hub/src/nym.rs:269-272
    if hash.is_empty() {
        tracing::warn!(reason = "empty lookup key", "lookup refused");
        return Some(error_reply(nonce));
    }
```

and `error_reply` is `encode_lookup_reply(&nonce, &LookupReply::Error)`, which
allocates and pads `FRAME_BYTES` = 65,536 every time (`wire.rs:480-499`).

The doc comment above `is_lookup` (`nym.rs:218-231`) presents the size check as
having closed an amplifier — *"a 21-byte message got a 65 536-byte answer, 41
sphinx packets of the hub's own metered egress"*. On the mixnet it closes
nothing: the smallest message a sender can put on the mixnet is one sphinx
packet, so a 21-byte payload and a 64-byte payload cost the sender exactly the
same, and the answer is the same 65,536 bytes. The real cost driver — the
attached reply SURBs — is untouched by a length check. `hub/tests/nym.rs`'s
`a_runt_lookup_shaped_message_buys_no_reply_at_all` pins a property that does not
hold on the transport that ships.

**2. The hand-off is non-blocking, so the hub's own queue and deadline never
engage.**

```rust
// hub/src/nym_driver.rs:632-643
fn reply_send(sender: ..., tag: AnonymousSenderTag, frame: Vec<u8>) -> InFlight {
    Box::pin(async move {
        if let Err(err) = sender.send_reply(tag, frame).await { ... }
        Sent::Reply
    })
}
```

`send_reply` → `ClientInput::send` → `mpsc::channel::<InputMessage>(1)`
(`base_client/mod.rs:1013`) → `InputMessageListener::handle_reply`, whose entire
body is:

```rust
// client-core/.../input_message_listener.rs:60-70
    async fn handle_reply(&mut self, recipient_tag, data, lane, max_retransmissions) {
        // offload reply handling to the dedicated task
        let _ = self.reply_controller_sender
            .send_reply(recipient_tag, data, lane, max_retransmissions);
    }
```

and `ReplyControllerSender` wraps `futures::channel::mpsc::unbounded`
(`reply_controller/requests.rs:12-15`, `:38-45`). Nothing between
`nym_driver.rs:474` and an unbounded queue applies backpressure, so the guard at
`nym_driver.rs:455` (`if in_flight.is_none()`) and the drop at `:465-471`
(`reply.is_dead()`) are both inert in practice.

**3. The FIFO that does form is unbounded, un-aged and shared.**

```rust
// client-core/.../real_traffic_stream.rs:443-478 (poll_poisson, one tick)
            match Pin::new(&mut self.real_receiver).poll_recv(cx) {
                Poll::Ready(Some((real_messages, conn_id))) => {
                    self.transmission_buffer.store(&conn_id, real_messages);
                    let real_next = self.pop_next_message().expect("Just stored one");
                    Poll::Ready(Some(StreamMessage::Real(Box::new(real_next))))
                }
                Poll::Pending => { /* pop one, else Cover */ }
            }
```

One tick stores a whole message (41 fragments) and emits **one packet**.
`pop_next_message_at_random` picks a lane and then `pop_front`s
(`transmission_buffer.rs:190-208`); all replies are
`TransmissionLane::General` (`nym-sdk/src/mixnet/traits.rs:122-133`), so honest
and attacker replies are one FIFO. `TransmissionBuffer` has no cap
(`transmission_buffer.rs:39-49`).

**4. The rate, from the project's own constants.**

```rust
// shim/src/nym.rs:1090-1094
    const PACKET_BYTES: usize = 2 * 1024;
    /// The client's own floor on sending, `MAX_DELAY_MULTIPLIER` (6) times the
    /// 20 ms default `message_sending_average_delay`.
    const THROTTLED_PACKETS_PER_SEC: f64 = 1000.0 / 120.0;
```

41 packets ÷ 8.33 packets/s ≈ **4.9 s per reply ⇒ ~12 replies/minute fleet-wide**;
at the unthrottled 20 ms default (`config-types/src/lib.rs:25`, `:443`) it is
~0.8 s ⇒ ~73/minute. `hub/src/nym.rs:62-70` and `nym_driver.rs:456-464` state the
same arithmetic in prose. Both figures are ceilings for the whole fleet, not per
shim.

**5. What the attacker must pay, exactly.**

```rust
// client-core/.../receiver_controller.rs:248-256
        let available_surbs = self.surbs_storage.available_surbs(&recipient_tag);
        let min_surbs_threshold = self.surbs_storage.min_surb_threshold();   // 10
        let max_to_send = if available_surbs > min_surbs_threshold {
            min(fragments.len(), available_surbs - min_surbs_threshold)
        } else { 0 };
```

so ≥51 attached SURBs to get all 41 fragments emitted. Fewer than 11 is a
different attack entirely (the reply is buffered forever — that is the confirmed
OOM issue). Zero is safe: `contains_surbs_for` fails and the reply is dropped
(`receiver_controller.rs:225-241`).

**6. The shim's fail-closed arm turns this into a wallet-visible outage.**

```rust
// shim/src/intercept.rs:315-324
        Err(err) => {
            tracing::warn!(target: "zis::classify", %err, "hub lookup failed; failing closed");
            Ok(grpc_error(GRPC_UNAVAILABLE, "zero-indexer-shim: hub unreachable"))
        }
```

with `REQUEST_TIMEOUT = 90 s` (`shim/src/nym.rs:71`), and `each_target`
multiplying it by the number of configured hub addresses
(`shim/src/nym.rs:695-790`).

## Recommendations

In order of value. Items 1 and 2 are the same two controls G5 §4.1/§4.2
identified, and both are compatible with the strictest reading of
`hub/src/queue.rs:35-39`, which forbids an identifier *on a queue entry*; a
lookup never becomes a queue entry.

1. **Answer a lookup only when the request carried enough reply SURBs to carry a
   full frame.** The conforming shim already attaches 60 precisely so the hub
   never has to re-request (`shim/src/nym.rs:98-104`), so no honest client is
   affected, and every under-provisioned request is refused before 64 KiB is
   allocated. This raises this attack's cost floor to what an honest client
   already pays and simultaneously removes the confirmed OOM.
2. **Bound in-flight replies per sender tag.** `Received` already carries the
   tag (`nym.rs:83-93`) and the hub already holds it for the life of the request
   in order to reply at all. A token bucket keyed on the tag — a counter and a
   timestamp, referencing no queue entry, forgotten after a minute — caps any one
   tag at `K` replies and costs a Sybil attacker one gateway registration per
   bucket.
3. **Do not spend a full frame on requests a conforming shim never sends.** The
   `decode_lookup` failure arm and the `hash.is_empty()` arm
   (`nym.rs:258-272`) each cost 41 packets today. The 64 KiB padding exists to
   hide `Found` from `NotFound`, an axis these two are not on. Drop them
   silently, exactly as the submit arm already drops a frame with no recoverable
   nonce (`nym.rs:327-333`).
4. **Fair-share the emitter across sender tags.** The tag is already on `Reply`
   (`nym.rs:98-105`). Round-robin across distinct tags in `outgoing` gives an
   honest shim `1/(T+1)` of the emitter against `T` attacker tags instead of a
   vanishing share, turning a total outage into graceful degradation. This is
   again a counter, not an identity.
5. **Make `REPLY_DEADLINE` actually bind, or delete it and say why.** As shipped
   it can almost never fire, because `send_reply` returns before anything is
   emitted. If the hub is to keep this mitigation it must stop handing the SDK
   more than the SDK can emit — e.g. gate `outgoing.recv()` on the SDK's own
   lane-queue length (`ClientState::lane_queue_lengths` is exposed by the SDK)
   rather than on a boolean `in_flight`. Leaving an inert mitigation in place with
   a 20-line comment explaining the starvation it prevents is worse than having
   none, because it stops the next reader looking further.
6. **Give the reply path more than one emitter.** One pinned client is deliberate
   for address stability (D10), but nothing stops the hub from holding one stable
   *inbound* identity and a small pool of additional clients used only for
   outbound replies, multiplying reply capacity by the pool size.
7. **Surface the condition.** Export the SDK's pending lane length, or at minimum
   a delayed aggregate count of lookups answered and lookups dropped, on
   `/nym-status`, so an operator can tell "the hub is being flooded" from "the
   mixnet is slow". Today both look identical and both look healthy.
8. **Correct the `is_lookup` doc comment and
   `a_runt_lookup_shaped_message_buys_no_reply_at_all`** so neither is read as
   having closed an amplification gap; on the mixnet the size check does not
   change the sender's cost.
9. Revisit `REQUEST_TIMEOUT` × `each_target` on the shim side, so a saturated hub
   does not cost a wallet 90 s per configured address per lookup.

Related, do not duplicate:

- `hub-surb-starved-lookup-replies-grow-the-sdk-pending-buffer-without-bound-and-oom-the-enclave.md`
  (High) is the **under**-provisioned-SURB case: one SURB, the reply is buffered
  forever, the effect is cumulative and permanent. This issue is the
  **over**-provisioned case: ≥51 SURBs, the reply is emitted, the effect is
  transient starvation of everyone else. Different cost, different consequence,
  different fix — though recommendation 1 above closes both.
- `hub-unauthenticated-pre-publication-transaction-disclosure.md` covers the
  *disclosure* properties of the same lookup path.
- `hub-http-lookup-path-has-no-concurrency-bound.md` is the complementary finding
  on the clearnet leg (the bound is missing there). This issue is the opposite
  observation on the mixnet leg: the bound is present and correct for what it was
  written for, and still does not prevent starvation, because the scarce resource
  is the single emitter.
- `gettransaction-flood-starves-migration-diversion.md` (High) is the shim-side
  analogue and the cheaper route to a subset of this effect (G5 §3.1): a plain
  HTTP request at any shim becomes a mixnet lookup at the shim's expense which
  then lands on this hub, so an attacker with no mixnet capability at all reaches
  both enclaves with one request.

## Validation Information

**Verdict: CONFIRMED. Severity corrected from High to Medium.**

Everything decisive was checked against the code and against the **pinned SDK
tree at `451c2aa3692fc4dc00041b74a352d4158176d9c0`**, which is available locally,
rather than against the filing.

### What was verified

1. **Reachability.** `GET /nym-address` is unauthenticated and answers everyone
   (`server.rs:446-479`); the enclave declares `ingress 0.0.0.0/0`
   (`caution.hcl.tmpl:50-55`). `is_lookup` accepts on size-plus-magic
   (`nym.rs:232-234`), `decode_lookup` accepts `hash_len = 0` (`wire.rs:438-457`),
   and the empty-key arm returns a full `FRAME_BYTES` frame with **no indexer
   dial and no queue scan** (`nym.rs:269-272`, `wire.rs:480-499`). Confirmed.
2. **Attacker clients are free.** Neither binary enables
   `enabled_credentials_mode`, and the SDK default is `false`
   (`nym-sdk/src/mixnet/client.rs:281-288`), so an attacker's stock client runs on
   exactly the terms the target's own does. Confirmed.
3. **The packet arithmetic, recomputed independently.** 41 packets per 64 KiB
   reply is the project's measured figure (`shim/src/nym.rs:98-104`); 8.33
   packets/s is the project's own `THROTTLED_PACKETS_PER_SEC`
   (`shim/src/nym.rs:1090-1094`), correctly derived as
   `MAX_DELAY_MULTIPLIER (6) × DEFAULT_MESSAGE_STREAM_AVERAGE_DELAY (20 ms)`,
   which matches `config-types/src/lib.rs:25`, `:443`. So **~12 replies/min at the
   floor and ~73/min unthrottled, fleet-wide**, from a single client the hub pins
   by design. The filed "≈12 answered lookups per minute" is right at the floor;
   the range is now stated so the finding does not rest on the worst case alone.
   Confirmed.
4. **The attacker must pay ≥51 SURBs**, i.e. roughly 51 packets of their own
   egress for 41 of the hub's (`receiver_controller.rs:248-256` with
   `min_surb_threshold = 10`). The filing already said this and it is correct:
   **there is no bandwidth amplification here.** The finding rests entirely on
   the structural asymmetry of one fleet-wide emitter against N attacker clients,
   and that asymmetry is real. Confirmed.
5. **The starvation is total, not proportional.** All replies use
   `TransmissionLane::General` (`nym-sdk/src/mixnet/traits.rs:122-133`) and a lane
   is a FIFO `VecDeque` (`transmission_buffer.rs:190-208`), so an honest reply is
   emitted only after the attacker's entire backlog. Confirmed.

### Corrections made to the filing

- **The stated mechanism was wrong and has been replaced.** The filing modelled a
  128-deep pipeline (`outgoing` 64 + 64 permits) that "accepts ~2.1 lookups/s and
  emits ~0.2/s, so ~90 % of everything accepted is discarded as dead". That
  equilibrium does not exist: `send_reply` returns as soon as an **unbounded**
  channel accepts the reply (`nym_driver.rs:638` → capacity-1 `InputMessage`
  channel → `input_message_listener.rs:60-70` → `reply_controller/requests.rs:12-15`),
  so `outgoing` drains at memory speed, the one-in-flight guard is never held, and
  `REPLY_DEADLINE` essentially never fires. The real queue is the SDK's unbounded,
  un-aged, FIFO `transmission_buffer`. **This is a strengthening correction**: the
  hub's only designed defence against exactly this failure is inoperative, and the
  backlog survives the attacker instead of being discarded at 60 s.
- **`MAX_CONCURRENT_LOOKUPS` is not the contested resource and the "first-come
  drop hits honest lookups" claim has been removed.** The cheapest attack frame
  holds a slot for microseconds, so the semaphore is nowhere near full; it is the
  emitter that is exhausted. (The filing already said this in its headline; the
  supporting paragraph contradicted it.)
- **Recommendation 5 is new** and follows directly from the mechanism correction:
  an inert `REPLY_DEADLINE` with a twenty-line comment describing the starvation
  it prevents is actively misleading to the next reader.
- Recommendation 1 was promoted to first place (it also closes the confirmed OOM),
  and the per-tag bucket and fair-share round-robin were added from G5 §4.2/§4.6,
  since the filing's original recommendation set led with measures that either
  trade against the padding property or need new infrastructure.

### `docs/AVOIDING-FALSE-POSITIVES.md` §5 applied

*What resources would the attacker need?* Two to three free, unregistered
`nym-sdk` clients emitting a few tens of KB/s. *What would stop them?* Nothing in
the target and nothing in the deployment: no ACL, no rate limit, no per-submitter
accounting (currently read as forbidden by `queue.rs:35-39`), `ingress 0.0.0.0/0`,
and no alerting that distinguishes a flood from a slow mixnet.

§5's caution applies only in part, and the issue is graded accordingly. There is
**no amplification** — the attacker pays ~51 packets for ~41 of the hub's — so
this is not the guide's "1 KB request causing 1 GB allocation" shape. What makes
it a real finding rather than "the attacker must out-resource the target" is that
the target's capacity is *structurally* one throttled client for the entire fleet,
so "out-resourcing it" costs about 1.3 free clients. The correct grade for that is
neither dismissal nor High.

### Severity: Medium, downgraded from the filed High

*Why not High.* This issue destroys nothing, discloses nothing, and corrupts
nothing. Migrations continue to be diverted, admitted, batched and published
throughout: admission is inline and unbounded (`nym.rs:120-144`), the shim's
submit is dispatch-only and never awaits the ack, and the batcher publishes over
HTTP. The failure is loud at the wallet (`UNAVAILABLE` after 90 s) rather than a
false success. It requires the attacker to keep paying continuously, and it heals
once the backlog drains. That places it below the three confirmed High findings on
this same surface — `hub-queue-unauthenticated-fill-silently-destroys-migrations`,
`hub-surb-starved-…-oom-the-enclave` and `junk-sendtransaction-flood-…` — each of
which destroys migrations a wallet was told had succeeded. It is graded level with
`hub-unauthenticated-pre-publication-transaction-disclosure` (Medium), which is
the calibration point for "serious, cheap, unauthenticated, but not
loss-of-funds-in-flight".

*Why not Low.* The blast radius is every wallet on every shim in the fleet, the
attacker is anonymous and free, `GetTransaction` failure presents to a light
wallet as sync failure rather than a missing detail view, and the realistic user
response — switching to an unprotected indexer — is precisely the deanonymisation
the product exists to prevent. Nothing in the current design can rate-limit it,
and nothing reports it.

*Relationship to `gettransaction-flood-starves-migration-diversion.md` (High).*
The two were graded against each other, not in isolation. That issue is **worse
despite the smaller blast radius**: it needs no mixnet capability at all
(~100-byte HTTP requests), it lands on the *submit* path where
`shim/src/hub.rs:231-240` answers the wallet `error_code 0` at hand-off — so
denial there becomes silent destruction of an acknowledged migration — and it
additionally reaches this hub through the shim (G5 §3.1). This issue is broader
but confined to the lookup path, fails loudly, loses nothing, and heals. G5's cost
ranking (S3 above H9/H10) is therefore upheld, and the severities follow it.

---

### ADDENDUM (Global Auditor, focus area G21 dedicated re-run, 2026-08-18) — a scope qualification, no leg withdrawn, no verdict or severity changed

This issue's mechanism was re-derived independently from the same pinned SDK tree
and is confirmed in every particular. Three notes.

1. **The stated severity bound holds only for the SUSTAINED attack shape.** The
   Impact section says *"migrations themselves keep working during this attack,
   which bounds the severity and is worth stating plainly."* That is correct for a
   sustained flood — and is correct for the reason given, that admission is inline
   and never waits on the lookup bound. It does **not** hold for a **pulsed**
   variant (burst, then be silent for ~130 s), which is a different attack with a
   permanent outcome: the hub's liveness probe cannot leave while the backlog this
   issue describes is draining, so the hub concludes its gateway has stopped
   delivering to it, tears its client down and charges a `short_life`; five such
   pulses mint a fresh Nym identity and permanently strand the fleet. Filed
   separately as
   `hub-liveness-probe-reads-its-own-send-backlog-as-gateway-silence-so-any-stranger-can-drive-the-fresh-identity-fleet-kill.md`,
   because the code defect is a different one (a missing conjunct in
   `hub/src/nym_driver.rs:421` that `shim/src/nym_driver.rs:312` has) and the fix
   is different. Please keep the sentence, and qualify it with "during a sustained
   flood".

2. **A third mitigation belongs on the inoperative list.** The issue already shows
   `REPLY_DEADLINE` and the `in_flight` guard do not bind. `MAX_CONCURRENT_LOOKUPS`
   does not bind *emission* either, and for the same unit mismatch: the permit is
   released once `outgoing` accepts the reply (`hub/src/nym.rs:184-197`), which
   happens within seconds, so the semaphore recycles at ~8.33/s and never limits
   the total emission an attacker has committed the hub to. It bounds concurrent
   indexer dials and resident reply frames correctly, which is what it was written
   for; it is worth saying explicitly that it is the third bound denominated in the
   wrong unit.

3. **Recommendation 1 must not ship alone.** Requiring a lookup to arrive with
   enough reply SURBs to carry its own padded reply closes the SURB-starved OOM by
   making well-provisioned lookups the only answerable ones — and a
   well-provisioned lookup is exactly the input the pulsed variant in note 1 needs.
   Recommendations 1, 2 and 5 are one change, not three options.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
