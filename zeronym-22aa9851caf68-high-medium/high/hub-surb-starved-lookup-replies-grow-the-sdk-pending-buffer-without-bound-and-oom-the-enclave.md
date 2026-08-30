# A 64-byte anonymous lookup carrying one reply SURB makes the hub buffer 64 KiB it can never send, forever: unbounded enclave memory growth that ends in an OOM which destroys the queue and permanently strands every shim

**Severity**: High
**Validation Status**: Confirmed
**Location**:
`audit-target/zeronym/hub/src/nym.rs:148-216` (`run_listener`, the lookup arm), `:232-234` (`is_lookup`), `:249-303` (`build_lookup_reply`, whose `hash.is_empty()` arm at `:269-272` answers a full frame with no I/O at all), `:38-54` (`MAX_CONCURRENT_LOOKUPS`), `:56-75` (`REPLY_DEADLINE`);
`audit-target/zeronym/hub/src/nym_driver.rs:452-479` (the reply arm), `:632-643` (`reply_send` → `sender.send_reply(tag, frame)`), `:262-276` (the in-RAM `Ephemeral` store that makes any process restart change the address);
`audit-target/zeronym/hub/src/wire.rs:476-500` (`encode_lookup_reply` pads **every** disposition to `FRAME_BYTES` = 64 KiB);
`audit-target/zeronym/hub/src/server.rs:62-70` and `:462-479` (`GET /nym-address` publishes the target to anyone);
`audit-target/zeronym/hub/src/main.rs:184-185` (both mixnet channels sized 64);
`audit-target/zeronym/hub/deploy/caution/caution.hcl.tmpl:39-41` (`cpu = 2`, `memory_mb = 2048`), `:50-55` (`ingress 0.0.0.0/0`).
The unbounded store is in the pinned SDK, `nym-sdk 1.21.5-rc.1` at git rev `451c2aa3692fc4dc00041b74a352d4158176d9c0` (`hub/Cargo.lock:4773-4775`): `common/client-core/src/client/replies/reply_controller/receiver_controller.rs:218-320` (`handle_send_reply`), `:685-758` (`inspect_stale_pending_data`), `:499-529` (`handle_received_surbs`), `:811-926` (`inspect_and_clear_stale_data`); `common/client-core/src/client/transmission_buffer.rs:39-49`, `:225-236`; `common/client-core/surb-storage/src/surb_storage.rs:190-192`, `:460-478`; `common/client-core/config-types/src/lib.rs:48-62`.

**Found by agent:** Global (focus area G5 — unauthenticated ingress on both enclaves; with G21 resource exhaustion as a privacy attack and G31 the unauthenticated internet attacker)
**In scope of audit?** Yes. This closes `BRAINSTORM.md` §R10-J, which recorded the question as *"the highest-value item in R10 that I could not close"* because it needed `nym-client-core` internals. Those internals were read at the exact pinned revision during validation and the answer is: **the store is not bounded, and the SDK's own two escape hatches are both defeated by an attacker who keeps talking.**

## Description

The hub answers a mixnet `LookupV1` with a `LookupReplyV1` that is **always
padded to 64 KiB** (`wire.rs:476-500`), so that a reply's size cannot reveal
found-versus-not-found. The driver hands that frame to the Nym SDK with
`sender.send_reply(tag, frame)` (`nym_driver.rs:638`), which is **fire and
forget**: it returns as soon as the SDK's one-slot input channel accepts the
`InputMessage`, not when anything is emitted.

A reply travels on **reply SURBs that the requester attached**. The hub attaches
none of its own and has no way to obtain any. So the requester decides whether
the reply can be sent at all — and the hub builds and hands over the full 64 KiB
either way.

What the SDK does with a reply it cannot send is the whole finding. In
`ReceiverReplyController::handle_send_reply`
(`receiver_controller.rs:218-320`):

```rust
        if !self.surbs_storage.contains_surbs_for(&recipient_tag) {
            // ... warn once ...
            return;                                   // dropped: SAFE
        }
        let mut fragments = self.message_handler.split_reply_message(data);
        let available_surbs = self.surbs_storage.available_surbs(&recipient_tag);
        let min_surbs_threshold = self.surbs_storage.min_surb_threshold();   // 10
        let max_to_send = if available_surbs > min_surbs_threshold {
            min(fragments.len(), available_surbs - min_surbs_threshold)
        } else {
            0
        };
        ...
        // if there's leftover data we didn't send because we didn't have enough
        // (or any) surbs - buffer it
        if !fragments.is_empty() {
            ...
            self.insert_pending_replies(&recipient_tag, fragments, lane);   // UNBOUNDED
        }
```

`insert_pending_replies` appends into `SenderData::pending_replies`, a
`TransmissionBuffer<FragmentWithMaxRetransmissions>`
(`receiver_controller.rs:29-45`, `:126-138`), which is a bare
`HashMap<TransmissionLane, LaneBufferEntry<T>>` with **no size limit and no
byte budget** (`transmission_buffer.rs:39-49`). Its own
`prune_stale_connections` is never called on this buffer — the only call site in
the whole SDK is the real-traffic stream's own buffer
(`real_traffic_stream.rs:326`).

So one attacker frame of 64 bytes causes ~41 fragments (~64 KiB of prepared
plaintext) to be retained indefinitely, **provided the sender tag has an entry in
the SURB store** so the early `contains_surbs_for` return is not taken.

Two facts about that guard decide the whole economics of the attack, and both
were checked directly against the pinned tree:

- **Zero attached SURBs is safe, one is enough.** A message with no attached
  SURBs never reaches `send_additional_surbs`
  (`received_buffer.rs:322-330`: `if !reply_surbs.is_empty()`), so no store entry
  is created, `contains_surbs_for` is false, and the reply is dropped cleanly.
  One attached SURB creates the entry.
- **`contains_surbs_for` is `contains_key`, not "has a SURB left"**
  (`surb_storage.rs:190-192`). Once the entry exists it stays, so **every later
  message from the same sender tag is buffered whether it carries a SURB or
  not.** The sender tag is stable per (client, recipient) pair
  (`message_handler.rs:239-250`, "using {new_tag} for all anonymous messages sent
  to {recipient}"), so one client keeps one tag for the whole attack.

The SDK has exactly two mechanisms that would eventually free the buffer, and
**both are keyed on "we have not heard from this sender", so both are defeated by
sending one SURB every few seconds** (`receiver_controller.rs:685-758`, run every
5 s from `reply_controller/mod.rs:145-147`):

```rust
            let Some(last_received_time) =
                self.surbs_storage.surbs_last_received_at(pending_reply_target) else { ... };
            let diff = now - last_received_time;
            ...
            if vals.current_clear_rerequest_counter > max_rerequests {   // 5
                to_remove.push(*pending_reply_target); continue;
            }
            if diff > max_rerequest_wait {                               // 10 s
                if diff > max_drop_wait { to_remove.push(...) }          // 5 min
                else { vals.increment_current_clear_rerequest_counter(); ... }
            }
```

- `surbs_last_received_at` is refreshed by **every** SURB that arrives
  (`surb_storage.rs:460-478`), so `diff` never crosses the 10 s re-request wait,
  let alone the 5 min drop wait.
- `handle_received_surbs` calls `reset_rerequest_counter(&from)` on **every**
  arrival (`receiver_controller.rs:499-529`), so the 5-re-request give-up counter
  is reset before it can reach its threshold.
- `surb_senders.remove(...)` at `receiver_controller.rs:756` is the **only**
  removal site in the file; nothing else ever drops a `SenderData`.
- The other periodic sweep, `inspect_and_clear_stale_data`
  (`receiver_controller.rs:811-926`), retains/purges only `surbs_storage`, and its
  own eviction predicate additionally requires `possibly_abandoned` (5 min of
  silence) and `pending_reception() == 0`, neither of which holds here.

And the buffer never drains, because draining requires *more* SURBs than the
minimum threshold: `try_clear_pending_queue` returns immediately unless
`available_surbs > min_surb_threshold` (10) (`receiver_controller.rs:444-455`).
At one attached SURB per message the steady state is: the SDK spends the
attacker's SURBs on futile "send me more SURBs" requests until
`pending_reception` reaches `maximum_reply_surb_storage_threshold` (200) and it
stops asking, after which stored SURBs hover at 10-11 and each arriving SURB
clears **one** buffered fragment while that same message adds **41**. Net growth
is ~40 fragments — about 64 KiB — per attacker message, forever.

## Attack Scenario and Steps

Attacker: anyone with an internet connection. No credential, no Zcash knowledge,
no valid transaction, no privileged network position, no enclave access.

1. `curl https://<hub-domain>/nym-address`. This endpoint exists to publish the
   value and answers everyone by design (`server.rs:62-70`, `:462-479`); the
   enclave declares `ingress { cidr_ipv4 = "0.0.0.0/0" }`
   (`hub/deploy/caution/caution.hcl.tmpl:50-55`).
2. Start a stock `nym-sdk` client. It is free, needs no registration, and
   attaches to a public gateway.
3. Repeatedly `send_message(hub, frame, IncludedSurbs::new(1))` where `frame` is
   64 bytes: `b"ZNL1"` ‖ 16 random bytes ‖ `0x00` (hash length zero) ‖ 43 zero
   bytes. `IncludedSurbs::Amount(1)` produces an **anonymous** message carrying
   the client's stable sender tag and exactly one reply SURB
   (`nym-sdk/src/mixnet/traits.rs:72-95`).
4. On the hub, `is_lookup` accepts it on size-plus-magic
   (`nym.rs:232-234`), a concurrency slot is taken, and `build_lookup_reply`
   reaches the empty-key arm at `nym.rs:269-272`:

   ```rust
       if hash.is_empty() {
           tracing::warn!(reason = "empty lookup key", "lookup refused");
           return Some(error_reply(nonce));
       }
   ```

   `error_reply` is `encode_lookup_reply(&nonce, &LookupReply::Error)` — a full
   `FRAME_BYTES` = 64 KiB buffer (`wire.rs:480`). **No indexer dial, no queue
   scan, no I/O of any kind**, so the hub answers these as fast as it can read
   them and neither `MAX_CONCURRENT_LOOKUPS` nor `REPLY_DEADLINE` slows it: the
   slot is released the moment the reply enters the 64-deep `outgoing` channel,
   and the reply is minutes fresher than the deadline.
5. The driver takes it and calls `send_reply` (`nym_driver.rs:474`, `:638`),
   which returns as soon as the SDK's `mpsc::channel::<InputMessage>(1)` accepts
   it (`base_client/mod.rs:1013`); the input listener does nothing with a
   `Reply` but `reply_controller_sender.send_reply(...)`
   (`input_message_listener.rs:60-70`), which is an **unbounded** channel
   (`reply_controller/requests.rs:12-15`, `:63-77`). There is no backpressure
   anywhere between the hub's driver and the buffer that grows.
6. The reply controller buffers all ~41 fragments of the 64 KiB reply, as above,
   and never sends or frees them.
7. Repeat. Every packet the attacker sends adds ~64 KiB of permanently held
   enclave memory. Sending at least one SURB-bearing packet every ten seconds
   keeps the SDK's two cleanup paths disarmed indefinitely; everything above that
   rate is pure growth, and messages sent *without* SURBs are buffered just the
   same (see `contains_surbs_for` above), so the marginal cost is **one sphinx
   packet per 64 KiB permanently held**.

**Arithmetic.** The enclave has `memory_mb = 2048`, most of which the project's
own manifest attributes to EnclaveOS plus the 64 MiB queue budget
(`hub/deploy/caution/caution.hcl.tmpl:33-41`). Taking ~1 GB as headroom, the
attacker needs ~16,000 messages. A stock client's Poisson stream defaults to
`message_sending_average_delay = 20 ms` (`client-core/config-types/src/lib.rs:25`,
`:443`), i.e. ~50 packets/s, and the project's own nymnet measurement puts one
attached reply SURB at about one sphinx packet (`shim/src/nym.rs:98-104`,
`hub/src/nym.rs:60-64`: 60 SURBs ≈ 60 packets). So **one free client reaches an
OOM in roughly five to ten minutes**; even at the ~8 packets/s floor the project
measures under real gateway backpressure it is about half an hour to an hour, and
clients cost nothing to run in parallel. The attacker's own bandwidth cost is a
few kilobytes per second.

**The attacker has a free progress indicator.** `GET /nym-status` is
unauthenticated and reports `mixnet_connected`, `client_deaths` and
`consecutive_rebuild_failures` (`server.rs:454-457`), and `GET /nym-address`
returns the current address. Polling either tells the attacker the moment the hub
died and came back with a new identity.

**Attack Requirements and Assumptions:**
- Network access, and the hub's Nym address, which the hub publishes on purpose.
- **At least one reply SURB, once.** Zero SURBs on *every* message is not the
  attack: with no store entry for the tag, `handle_send_reply` returns at
  `receiver_controller.rs:225-241` and the reply is dropped cleanly. This
  corrects the guess recorded in `BRAINSTORM.md` §R10-J that zero SURBs would be
  the cheapest variant. One SURB creates the entry, and a SURB every ten seconds
  keeps it and the buffer alive.
- No rate limit, ACL, authentication or submitter accounting exists anywhere on
  this path, by design (`hub/src/nym.rs:12-15`; `hub/src/queue.rs:35-39`).
- The pinned SDK revision is exactly the tree analysed
  (`hub/Cargo.lock:4775`, `shim/Cargo.lock:5065`, both
  `#451c2aa3692fc4dc00041b74a352d4158176d9c0`). **Neither binary sets any
  `DebugConfig` reply-SURB parameter**: the hub's only `debug_config` call sets
  `acknowledgements.ack_wait_addition` (`hub/src/nym_driver.rs:202-216`) and the
  shim's is gated behind the `mixnet-localnet` feature, so every value in
  `config-types/src/lib.rs:48-62` is the shipped default.
- What limits it: the flood is noisy on the mixnet, and a future
  encrypt-to-hub-key or STEVE layer would change the picture — but both are
  listed in `README.md` as *"Designed, no code yet"*.

## Impact on Users

The hub is a single, shared, fleet-wide component, and this is an
unauthenticated remote kill switch for it.

- **Migrations users were told had succeeded are destroyed.** On the deployed
  transport submit is dispatch-only: the wallet is answered `error_code 0` when
  the frame enters an in-process channel (`shim/src/hub.rs:226-240`,
  `shim/src/nym.rs:595-690`). Everything the hub has admitted but not yet
  published lives only in enclave RAM, up to `MAX_QUEUE_BYTES` = 64 MiB
  (`hub/src/queue.rs:65`). An OOM is a `SIGKILL`, so **not even the shutdown
  flush and not even the `unpublished … they are lost` log line
  (`batcher.rs:317-330`) run** — that path is reached only from the SIGTERM /
  ctrl-c handler (`main.rs:220-247`). The loss is completely silent.
- **The attacker chooses when.** Flushes happen on a 20-block cadence and are
  visible on the public chain, so an attacker who can drive an OOM in ten to
  sixty minutes can start early enough to land the kill just before a cadence
  height, when the queue holds a whole epoch of migrations.
- **The whole shim fleet is stranded, and recovery is manual.** The hub's
  identity store is `Ephemeral::default()` built inside `run_driver`
  (`hub/src/nym_driver.rs:269`), so it survives client rebuilds but **not a
  process restart**; the module header says so at `hub/src/nym_driver.rs:34-40`.
  This holds whichever way the platform behaves: if the unit is restarted
  automatically the hub comes back with a **different Nym address**, and if it is
  not, the operator's redeploy produces one just the same. Every shim has the old
  address baked into an immutable Caution enclave config
  (`ZIS_HUB_NYM`, `shim/deploy/caution/assemble-caution.sh:345-356`;
  `shim/src/config.rs:69-78`, read once at startup with no discovery mechanism
  anywhere in `shim/src`). Their submits go to a recipient that no longer exists,
  the SDK reports success, and every migration is silently destroyed until every
  operator re-assembles and redeploys. That is the outage already filed as
  `hub-nym-driver-automatic-fresh-identity-permanently-invalidates-every-shim.md`,
  whose stated trigger was bad luck (~11 minutes of gateway failure) — **this
  issue hands an anonymous outsider an on-demand trigger for it.**
- **Every wallet loses `GetTransaction`, not just migrating ones.** With a hub
  configured the shim routes *every* lookup to the hub and fails closed on error
  (`shim/src/intercept.rs:229-236`, `:315-324`), so a dead hub means no wallet
  behind any zeronym shim can fetch any transaction's full data — including users
  who have never touched Orchard.
- **The realistic user response is the leak.** A wallet that cannot send or
  confirm is pointed at a different, unprotected indexer, where the
  Orchard-touching transaction is broadcast in the clear. The attacker chooses
  the moment.
- **Nothing alerts.** `GET /healthz` is unconditionally `200 ok`
  (`server.rs:449-452`) right up to the OOM, and after the restart it is 200
  again — on a hub no shim can reach.
- **Repeating it burns the TLS budget.** Every enclave restart is a fresh ACME
  order and the duplicate-certificate limit is 5 per week
  (`hub/deploy/caution/RESTARTS.md:1-21`), so an attacker who re-triggers the OOM
  a few times leaves the hub unable to obtain a certificate at all — including for
  the `/nym-address` endpoint operators would need to read the new address from.
  See `acme-nocache-issuance-budget-is-a-restart-triggerable-tls-outage-with-no-health-signal.md`.
- **Submissions are destroyed for as long as the outage lasts, and funds are not
  frozen.** The note is not spent on chain and the wallet keeps retrying (see the
  correction below); what an OOM'd hub destroys is every submission made while it
  is down, plus everything its RAM queue held.

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

**1. Any 64-byte frame with the right magic is a lookup, and every lookup answer
is 64 KiB.**

```rust
// hub/src/nym.rs:232-234
fn is_lookup(frame: &[u8]) -> bool {
    frame.len() == wire::LOOKUP_BYTES && wire::peek_lookup_nonce(frame).is_some()
}
```

`peek_lookup_nonce` checks only length ≥ 21 and the four magic bytes
(`hub/src/wire.rs:463-470`). The doc comment above `is_lookup` records that
dispatching on magic alone *"was an amplifier: a 21-byte message got a
65 536-byte answer"* — the size check fixed the ratio from 3000× to 1000×, but
the amplifier itself is intact, and the finding here is not about the ratio: it
is about the reply being **retained** rather than merely built.

```rust
// hub/src/nym.rs:254-272
    let error_reply = |nonce| {
        wire::encode_lookup_reply(&nonce, &LookupReply::Error)
            .expect("an error reply carries no transaction and always fits")
    };
    let (nonce, hash) = match wire::decode_lookup(frame) { ... };
    if hash.is_empty() {
        tracing::warn!(reason = "empty lookup key", "lookup refused");
        return Some(error_reply(nonce));
    }
```

```rust
// hub/src/wire.rs:476-484
pub fn encode_lookup_reply(nonce: &Nonce, reply: &LookupReply)
    -> Result<Zeroizing<Vec<u8>>, WireError> {
    let mut frame = Zeroizing::new(vec![0u8; FRAME_BYTES]);   // 64 KiB, every time
    ...
        LookupReply::Error => frame[20] = 2,
```

`decode_lookup` accepts `hash_len = 0` without error (`wire.rs:439-456`: it only
rejects `declared > MAX_LOOKUP_HASH_BYTES`), so the empty-key arm is reached with
a well-formed frame and no error path at all.

**2. The bounds that exist do not bound this.**

```rust
// hub/src/nym.rs:171-197
            let permit = match lookups.clone().try_acquire_owned() { ... };
            tokio::spawn(async move {
                if let Some(frame) = build_lookup_reply(&hub, &received.frame, received_at).await {
                    let _ = outgoing.send(Reply { ... }).await;
                }
                drop(permit);
            });
```

`MAX_CONCURRENT_LOOKUPS` (64) and the 64-deep `outgoing` channel together bound
what the **hub** holds to about 8 MiB. They say nothing about what the hub has
already pushed into the SDK, and they do not even throttle the rate: the driver
takes one reply at a time, but `send_reply` completes in microseconds (item 3),
so `outgoing` drains at memory speed. `REPLY_DEADLINE` (60 s) drops *aged*
replies (`nym.rs:285-292`, `nym_driver.rs:465-471`) but these replies are
answered in microseconds and are never aged when handed over.

**3. The hand-off is fire and forget, into an unbounded queue.**

```rust
// hub/src/nym_driver.rs:632-643
fn reply_send(sender: ..., tag: AnonymousSenderTag, frame: Vec<u8>) -> InFlight {
    Box::pin(async move {
        if let Err(err) = sender.send_reply(tag, frame).await {
            tracing::warn!(error = %err, "mixnet reply send failed");
        }
        Sent::Reply
    })
}
```

`send_reply` builds `InputMessage::new_reply(...)` and awaits
`input_sender.send(...)` on a channel of capacity 1
(`nym-sdk/src/mixnet/traits.rs:122-133`, `client-core/base_client/mod.rs:1013`).
The listener's `handle_reply` does nothing but
`reply_controller_sender.send_reply(...)`
(`acknowledgement_control/input_message_listener.rs:60-70`), which is an
`UnboundedSender` (`reply_controller/requests.rs:12-15`, `:63-77`). This is the
root cause named in G5 §4.7: **the memory an enclave can be made to hold on
someone else's behalf must be bounded by something the enclave owns.**

**4. Both SDK cleanup paths are keyed on silence from the sender.**

`inspect_stale_pending_data` runs every 5 s (`reply_controller/mod.rs:145-147`,
`:161-163`) and is quoted in full in the Description. `handle_received_surbs`
resets the give-up counter on every arrival:

```rust
// client-core/.../receiver_controller.rs:499-529
    pub(crate) async fn handle_received_surbs(&mut self, from: ..., reply_surbs: ..., from_surb_request: bool) {
        ...
        self.surbs_storage.insert_fresh_surbs(&from, reply_surbs);   // refreshes surbs_last_received_at
        self.reset_rerequest_counter(&from);                          // disarms the 5-rerequest give-up
        self.try_clear_pending_retransmission(from).await;
        self.try_clear_pending_queue(from).await;                     // no-op below the threshold
        if self.should_request_more_surbs(&from) {
            self.request_reply_surbs_for_queue_clearing(from).await;  // spends the SURB just received
        }
    }
```

```rust
// client-core/surb-storage/src/surb_storage.rs:460-478
    pub(crate) fn insert_fresh_reply_surbs<I: ...>(&mut self, surbs: I) {
        let received_at = OffsetDateTime::now_utc();
        ...
        self.surbs_last_received_at = received_at;
```

**5. The buffer itself has no cap.**

```rust
// client-core/src/client/transmission_buffer.rs:39-49
pub(crate) struct TransmissionBuffer<T> {
    buffer: HashMap<TransmissionLane, LaneBufferEntry<T>>,
}
```

`prune_stale_connections` (`:225-236`) would evict a lane idle for ten minutes,
but its only caller is the real-traffic stream's own buffer
(`real_traffic_stream.rs:326`), never `SenderData::pending_replies`. All replies
use `TransmissionLane::General` (`nym-sdk/src/mixnet/traits.rs:122-128`), so they
all accumulate in one `VecDeque`.

**6. The shipped configuration is the SDK default.** The values that govern all
of this — `minimum_reply_surb_storage_threshold = 10`,
`maximum_reply_surb_storage_threshold = 200`,
`maximum_reply_surb_rerequest_waiting_period = 10 s`,
`maximum_reply_surb_drop_waiting_period = 5 min`,
`maximum_reply_surbs_rerequests = 5` — are
`client-core/config-types/src/lib.rs:48-62`, and neither enclave overrides any of
them. These defaults are sized for a chat client that will eventually stop
talking to an unresponsive peer, not for an enclave holding other people's
funds-in-flight against an adversary who has every reason to keep talking.

**Why the sibling HTTP path is not affected:** `POST /transaction` answers
synchronously into a hyper response (`server.rs:480-517`); nothing is retained
after the connection closes. **Why the shim is not affected:** the shim never
calls `send_reply` — it only sends and awaits (`shim/src/nym_driver.rs:608-624`)
— so `handle_send_reply` is never reached in its client.

## Recommendations

In rough order of value, and all of them compatible with the design rule at
`hub/src/queue.rs:35-39` that forbids a *submitter-to-migration* mapping (none of
these associates a sender with a queue entry):

1. **Do not build a reply the sender cannot receive.** Answer a `LookupV1` only
   when the request arrived with enough attached reply SURBs to carry a full
   frame — the honest shim already attaches `LOOKUP_REPLY_SURBS` = 60
   (`shim/src/nym.rs:98-104`), chosen precisely to clear the SDK's threshold, so
   a conforming client is unaffected and every under-provisioned request is
   refused before 64 KiB is allocated. If `nym-sdk` does not expose the attached
   count on `ReconstructedMessage`, the equivalent is to bound outstanding
   replies per `SenderTag` (next item), which needs nothing from the SDK.
2. **Bound in-flight replies per sender tag.** `Received` already carries the
   tag (`hub/src/nym.rs:83-93`) and the hub already keeps it for the lifetime of
   the request in order to reply at all. A small token bucket keyed on the tag —
   holding a counter and a timestamp, referencing no queue entry, and forgotten
   after a minute — bounds this attack to `K × 64 KiB` per distinct tag and
   costs a Sybil attacker one gateway registration per bucket. **This does not
   violate `queue.rs:35-39`**, which forbids an identifier *on a queue entry*;
   lookups never become queue entries.
3. **Stop answering undecodable and empty-key lookups at all.** The 64 KiB
   padding exists to hide `Found` from `NotFound` (`hub/src/wire.rs:476-479`) — a
   genuine privacy axis. A frame that failed `decode_lookup`, or that declares a
   zero-length hash, is not on that axis: a conforming shim never sends one, and
   the sender already knows what it sent. Dropping those silently (as the submit
   arm already does for a frame with no recoverable nonce,
   `hub/src/nym.rs:327-333`) removes the I/O-free variant of this attack with no
   loss of indistinguishability.
4. **Set a `DebugConfig` for the reply-SURB parameters in both binaries,** but do
   not mistake it for the fix. The hub already builds one for
   `ZIH_ACK_WAIT_ADDITION_MS` (`hub/src/nym_driver.rs:202-216`); extend it.
   Lowering `maximum_reply_surb_rerequest_waiting_period` and
   `maximum_reply_surbs_rerequests` shortens the window but does **not** close
   the hole on its own, because both counters are reset by every arriving SURB —
   treat this as defence in depth and say so in the comment.
5. **Report the condition.** Add the reply controller's pending-queue size, or at
   minimum a resident-memory figure, to `GET /nym-status`. Today the only symptom
   before the OOM is invisible, and the only symptom after it is a
   `/nym-address` that quietly changed.
6. **Fix the underlying fragility, not only this instance:** an enclave must
   never hand a fire-and-forget buffer to a library without a matching admission
   bound of its own. `outgoing` (64) bounds what the hub holds; nothing bounds
   what the hub has already given away.
7. **Make recovery survivable.** Independently of this bug, an address change
   should not require every operator to redeploy an immutable enclave. Any
   mechanism that lets a shim learn the hub's current address (a signed pointer
   record, a second stable identity, an address list refreshed at runtime) turns
   this class of outage from fleet-fatal into an interruption.

## Validation Information

**Status: CONFIRMED. Severity confirmed at High.**

The decisive claim was verified line by line against the **pinned SDK tree at
`451c2aa3692fc4dc00041b74a352d4158176d9c0`**, which is present locally, not
against the report. Everything checked:

1. **The reply is built with no I/O.** `decode_lookup` (`hub/src/wire.rs:439-456`)
   accepts `hash_len = 0`; `build_lookup_reply` then takes the `hash.is_empty()`
   arm (`hub/src/nym.rs:269-272`) and returns a full `FRAME_BYTES` frame without
   dialling an indexer or touching the queue. Confirmed.
2. **One attached SURB is necessary and sufficient.** `received_buffer.rs:322-330`
   only forwards SURBs to the reply controller `if !reply_surbs.is_empty()`, so
   zero SURBs never creates a store entry and the reply is dropped at
   `handle_send_reply`'s `contains_surbs_for` guard. Confirmed — and this is the
   opposite of `BRAINSTORM.md` §R10-J's guess.
3. **The buffer is unbounded and never pruned.** `TransmissionBuffer` has no cap
   (`transmission_buffer.rs:39-49`); `prune_stale_connections` has exactly one
   caller in the whole SDK and it is the real-traffic stream's buffer, not
   `pending_replies`. `surb_senders.remove` appears exactly once
   (`receiver_controller.rs:756`). Confirmed.
4. **Both escape hatches are disarmed by an arriving SURB.**
   `insert_fresh_reply_surbs` sets `surbs_last_received_at = now`
   (`surb_storage.rs:460-478`) and `handle_received_surbs` calls
   `reset_rerequest_counter` unconditionally (`receiver_controller.rs:517`).
   `inspect_and_clear_stale_data`'s eviction additionally requires
   `pending_reception() == 0` and 5 minutes of silence, neither of which holds.
   Confirmed.
5. **The hand-off is genuinely unbackpressured.** `send_reply` → capacity-1
   `InputMessage` channel → input listener → `unbounded_send` to the reply
   controller. The input listener does no work at all for a `Reply` variant, so
   `send_reply` returns in microseconds and `MAX_CONCURRENT_LOOKUPS` /
   `outgoing` / `REPLY_DEADLINE` throttle nothing. Confirmed.

**Corrections made during validation** (the filed text has been updated):

- The original text said the attack needs one SURB *per message*. It does not:
  `contains_surbs_for` is `contains_key` (`surb_storage.rs:190-192`), so once the
  first SURB-bearing message has created the entry, **messages with zero attached
  SURBs are buffered too**. The marginal cost is therefore **one sphinx packet
  per 64 KiB permanently held**, with one keep-alive SURB every ten seconds.
- Fragment count corrected from "~33" to ~41, which is the project's own measured
  figure for a 64 KiB frame (`shim/src/nym.rs:98-104`).
- The steady state was refined: `pending_reception` saturates at 200 and the SDK
  stops asking for more SURBs, after which each arriving SURB clears one buffered
  fragment while its own message adds 41. Growth continues at ~40 fragments per
  message; the attack is not defeated, only slowed by 2.5%.
- The OOM is a `SIGKILL`, so the `unpublished … they are lost` line in
  `batcher.rs:317-330` does **not** run — that path is reachable only from the
  SIGTERM/ctrl-c shutdown handler. The loss is quieter than the original text
  claimed.
- The fleet-strand composition was checked and **holds independently of whether
  the platform auto-restarts the unit**: `Ephemeral::default()` is constructed
  inside `run_driver` (`nym_driver.rs:269`) in a diskless enclave, so *any*
  recovery — automatic restart or operator redeploy — produces a new Nym address,
  and `ZIS_HUB_NYM` is static startup configuration in an immutable enclave with
  no discovery path anywhere in `shim/src`. This is what makes the severity.
- Added: repeated OOMs burn the ACME duplicate-certificate budget (5/week,
  `hub/deploy/caution/RESTARTS.md`), which can leave the hub without a
  certificate for the very `/nym-address` endpoint operators need to recover.

**Applying `docs/AVOIDING-FALSE-POSITIVES.md` §5 (impractical resource
exhaustion) explicitly.** The guide's test is "what resources would the attacker
need, and what would stop them?" Here the answer is one free, unregistered
`nym-sdk` client emitting a few kilobytes per second, and **nothing stops them**:
there is no ACL, no rate limit, no per-submitter accounting (forbidden by
`queue.rs:35-39` as it is currently read), no platform-level ingress restriction
(`ingress 0.0.0.0/0`), and no alerting. One 2 KB sphinx packet causes 64 KiB of
**permanently retained** enclave memory — a 32× byte amplification into a
cumulative resource rather than a transient one. That is the guide's stated
*real* vulnerability ("1 KB request causing 1 GB memory allocation"), i.e. the
exact inverse of the pattern §5 warns about, not an instance of it.

**Not a duplicate of `hub-nym-lookup-flood-starves-gettransaction-fleet-wide.md`.**
That issue is about the *emitter* being a single serialised fleet-wide resource
and needs the attacker to attach ~51+ SURBs so the reply is actually sent; it
costs the attacker roughly as many packets as it costs the hub, and its effect
stops when the attacker stops. This issue needs *one* SURB, costs ~40× less, and
its effect is cumulative and survives the attacker. Different mechanism,
different fix; both are confirmed and cross-referenced.

**Severity justification (High, not Critical).** It is remotely triggerable by
the weakest adversary in the threat model, at negligible cost, with no detection;
it destroys migrations the wallet was told had succeeded, and its recovery
necessarily strands every shim in the fleet until every operator redeploys. What
holds it below Critical: no funds are stolen and no key or plaintext is
disclosed to the attacker — the destroyed migrations are recoverable by the
wallet once the outage outlives its retry horizon (loss of the submission, not of
funds; see the CORRECTION above), and
the fleet-strand is repairable by a coordinated redeploy. It is nevertheless the
single worst outcome reachable from the unauthenticated ingress surface and
should be fixed before any further operator onboarding.

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


DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
