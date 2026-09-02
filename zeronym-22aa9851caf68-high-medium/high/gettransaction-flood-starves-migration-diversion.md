# An unauthenticated `GetTransaction` flood converts ~100-byte gRPC requests into 61-packet mixnet emissions, saturating the shim's single egress and taking migration diversion down — loudly on a shared 32-slot channel, silently in the SDK's unbounded buffer

**Severity**: High
**Validation Status**: Confirmed
**Location**:
`audit-target/zeronym/shim/src/intercept.rs:229-247` (routing: with a hub configured, EVERY `GetTransaction` goes to the hub), `:237-295` (`get_transaction`, the whole of its admission control), `:315-324` (fail closed), `:137-215` (`divert`), `:204-214` (the `UNAVAILABLE` arm);
`audit-target/zeronym/shim/src/nym.rs:71` (`REQUEST_TIMEOUT` = 90 s), `:80` (`SUBMIT_DISPATCH_TIMEOUT` = 5 s), `:96` / `:104` (SURB counts), `:307-309` (`is_healthy`), `:595-690` (`NymHandle::submit`, `:660` the 5 s send, `:685-689` `TransportGone`), `:695-790` (`get_transaction` / `each_target`, `:758-759` the 90 s send), `:835-905` (`correlate`), `:1085-1130` (`throughput_budget`, the crate's own emission model);
`audit-target/zeronym/shim/src/main.rs:335-336` (channel capacities 32 and 8);
`audit-target/zeronym/shim/src/nym_driver.rs:362-375` (one send in flight), `:416-436` (the crate's own statement that the SDK holds *"an unbounded transmission buffer drained at the throttled rate"* which *"may include SUBMITS ALREADY ANSWERED SUCCESS to a wallet"*), `:608-623` (`send_frame`);
`audit-target/zeronym/shim/src/hub.rs:228-249` (`Ok(()) => Submit::Accepted` at hand-off; there is no `Refused` arm);
`audit-target/zeronym/shim/src/proxy.rs:470-541` (accept loop: no connection cap), `:596-610` (h2 server: window sizes only, no `max_concurrent_streams`), `:743-748` (`route_for`);
`audit-target/zeronym/shim/src/wire.rs:455-458` and `:72` (`FRAME_BYTES` = 64 KiB, every reply padded);
`audit-target/zeronym/shim/deploy/caution/caution.hcl.tmpl:34-35` (`memory_mb = 2048`), `:141` (`ZIS_LISTEN = 0.0.0.0:8083`), and the `ingress 0.0.0.0/0` block.
The drain rate that decides the arithmetic is in the pinned SDK (`451c2aa3692fc4dc00041b74a352d4158176d9c0`): `common/client-core/src/client/base_client/mod.rs:1013` (capacity-1 input channel), `real_messages_control/mod.rs:150` (8-slot batch channel), `real_messages_control/real_traffic_stream.rs:427-490` (`poll_poisson`: one batch stored, one packet emitted, per tick), `client/transmission_buffer.rs:39-49` (no cap), `real_messages_control/message_handler.rs:500-557` (fragments prepared eagerly and pending acks armed **before** emission), `config-types/src/lib.rs:25`, `:443` (20 ms default).

**Found by agent:** Local (file audit of `shim/src/intercept.rs`); validated 2026-08-18
**In scope of audit?** Yes

## Description

With a hub configured, `intercept::get_transaction` turns **every** well-formed
`GetTransaction` request from an unauthenticated, wallet-facing, internet-reachable
listener into a full mixnet round trip to the hub (`intercept.rs:229-247`,
`:295`). The entire admission control on that path is: body ≤ 1 KiB, a decodable
`TxFilter`, and a 32-byte hash (`:249-293`). Thirty-two random bytes pass all
three. There is no authentication, no rate limit, no per-peer accounting and no
concurrency cap anywhere between the socket and the mixnet.

A lookup costs the shim **61 sphinx packets** — 60 attached reply SURBs plus the
64-byte request (`nym.rs:104`, `:1120-1130`) — against a single serialised mixnet
client whose floor rate the crate itself fixes at **8.33 packets/s**
(`THROTTLED_PACKETS_PER_SEC`, `nym.rs:1090-1094`: `MAX_DELAY_MULTIPLIER` 6 × the
SDK's 20 ms default). So **one ~100-byte HTTP request buys ~7.3 seconds of the
shim's entire outbound mixnet capacity**, and one request every few seconds holds
that capacity permanently consumed. The shim also pays the same 61 packets for a
lookup as it pays 45 for a migration, so lookups and migrations compete for one
non-substitutable resource.

Three separate harms follow, in increasing order of how much attacker
concurrency they need and decreasing order of how loud they are.

**(1) Every wallet's `GetTransaction` through that shim stops working. Certain,
and essentially free.** Once the emission backlog exceeds `REQUEST_TIMEOUT`
(90 s, `nym.rs:71`) every lookup times out, sweeps its address list and fails
closed with `UNAVAILABLE` (`intercept.rs:315-324`). One request every ~7 s is
enough to keep the backlog growing.

**(2) A genuine migration is accepted, answered `error_code 0`, and then rots.**
`shim/src/hub.rs:228-249` answers the wallet `Submit::Accepted` with a locally
computed txid the moment the frame is accepted by an in-process channel; it has
no `Refused` arm at all. Behind that hand-off is the SDK's **unbounded**
transmission buffer, which the shim's own code describes as *"an unbounded
transmission buffer drained at the throttled rate … Frames in there may include
SUBMITS ALREADY ANSWERED SUCCESS to a wallet"* (`nym_driver.rs:416-436`). Under
this flood that buffer grows without bound, so an acknowledged migration either
arrives at the hub long after its admission window (`Refusal::ExpiryTooTight`,
into an ack the shim constructs a receiver for and immediately drops,
`nym.rs:652`) or is destroyed outright by any client rebuild, rotation, redeploy
or SIGTERM. The wallet was told it succeeded and the shim keeps no record.

**(3) With more concurrency, migrations fail closed at the wallet.** Lookups and
submits share **one** `mpsc::Sender<Request>` of capacity 32 (`main.rs:335`), but
a lookup is willing to wait 90 s to be accepted into it (`nym.rs:758-759`) while a
submit is willing to wait 5 s (`nym.rs:639`, `:660`). tokio's bounded `mpsc` is a
fair FIFO semaphore, so a submit joins the queue behind every lookup already
parked and is served in their order. When its 5 s elapses, `NymHandle::submit`
breaks with `dispatched == 0` and returns `NymError::TransportGone`
(`nym.rs:685-689`), and `divert` answers the wallet `UNAVAILABLE: hub unreachable`
(`intercept.rs:204-214`).

Throughout all three, `/healthz` answers 200: `MixnetStatus::is_healthy` is
`configured && connected` (`nym.rs:307-309`) and the client stays connected — it
is merely backlogged.

## Attack Scenario and Steps

1. The attacker picks any operator's shim. The deployment listens on
   `0.0.0.0:8083` behind a public TLS domain with `ingress 0.0.0.0/0`
   (`caution.hcl.tmpl:141` and its network block), and the listener is
   wallet-facing and unauthenticated by design.
2. The attacker opens HTTP/2 connections and issues concurrent
   `POST /cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTransaction` calls, each
   carrying a gRPC-framed `TxFilter { hash: <32 random bytes> }` — about 45 bytes
   of protobuf plus the 5-byte prefix. Random hashes pass every check the shim
   makes (`intercept.rs:274-293`).
3. Each call reaches `diversion.hub.get_transaction(&filter.hash)`
   (`intercept.rs:295`), becomes a `LookupV1` with 60 attached reply SURBs, and
   consumes ~7.3 s of the shim's whole mixnet egress.
4. **For harms (1) and (2), a trickle is enough**: one request every few seconds
   keeps the emission backlog growing without bound, since the SDK absorbs whole
   messages far faster than it emits packets (see Technical Details). Cost: a few
   hundred bytes per minute.
5. **For harm (3), the attacker raises concurrency** until the 32-slot channel's
   waiter list is deeper than the 5 s a submit will wait. The threshold is
   derived below: of order **750 concurrent live requests at the throttled rate**
   (~4,500 at the SDK's unthrottled default). Each is ~100 bytes and lives ≤90 s,
   so sustaining it costs roughly 5 KB/s. Nothing caps it: the accept loop has no
   connection limit (`proxy.rs:470-541`) and the h2 server sets only window sizes,
   never `max_concurrent_streams` (`proxy.rs:596-610`).
6. A real user's wallet then sends its migration. `intercept::send_transaction`
   classifies it `Class::Migration` and calls `divert` → `HubTransport::submit` →
   `NymHandle::submit`, whose `timeout_at(now + 5 s, self.requests.send(request))`
   sits behind the attacker's parked lookups, expires, breaks with
   `dispatched == 0`, and returns `TransportGone`. The wallet receives
   `grpc-status: 14 UNAVAILABLE`.
7. The wallet, correctly reading UNAVAILABLE as "retry", retries — and fails
   again, for as long as the attacker keeps going.

**Attack Requirements and Assumptions:**
- Any host on the internet can reach the shim; no credential, no wallet, no
  funds, no valid transaction, **and no Nym client of any kind**.
- Cost: a few hundred bytes per minute for harms (1) and (2); a few KB/s and
  several hundred concurrent h2 streams for harm (3).
- Nothing in the shim distinguishes the flood from ordinary wallet traffic: the
  requests are byte-identical to what a wallet sends.
- **The operator can run this against their own shim**, from inside their own
  network, deniably — so it is available to adversary #1 in the threat model, not
  only to an outsider.
- What bounds it: it must be sustained, and harm (2)'s *destruction* (as opposed
  to delay) additionally needs either a wallet expiry near the librustzcash
  40-block default or a driver teardown to coincide. For ZIP 318 migrations, whose
  expiry is 30-60 days, the expiry route does not apply and the teardown route
  does — and there the wallet's recovery clock is 30-60 days.

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


## Impact on Users

`intercept.rs` fails **closed** on the lookup path and on a failed dispatch, which
is the right choice and means this is not directly a leak. What it is, is a cheap,
remote, unauthenticated switch that turns the product off, or worse, turns it into
a liar:

- **A user trying to migrate legacy Orchard funds cannot** — either they are told
  `UNAVAILABLE` (harm 3) or, worse, they are told **success with a txid** for a
  transaction that will reach no mempool (harm 2). The shim keeps no per-migration
  state by design (`lib.rs:30-35`), the ack carrying the hub's refusal is
  discarded at construction (`nym.rs:652`), and confirmation tracking does not
  exist. The user's wallet holds the notes as pending-spent until expiry.
- **Every `GetTransaction` for every wallet behind that shim fails closed**
  (`intercept.rs:315-324`), so wallets cannot fetch full transaction data or
  confirm anything. For a light wallet this presents as sync failure, not as one
  missing screen.
- **The realistic user response is the privacy loss.** A user who switches to a
  non-zeronym indexer broadcasts their Orchard-touching transaction directly,
  joining their IP to it on the permanent public chain — the exact outcome the
  product exists to prevent, and the attacker chooses when.
- **`/healthz` answers 200 throughout**, so the operator's monitoring does not
  fire. This is precisely the "dead-client case stayed invisible" failure mode
  `MixnetStatus` was introduced to eliminate, and it is not covered: the client is
  connected, it is drowning.
- **The shim's memory grows while this runs.** Because the SDK stores a whole
  prepared message per tick and emits one packet per tick
  (`real_traffic_stream.rs:443-478`), and fragments are fully-built sphinx packets
  retained together with a clone for retransmission
  (`message_handler.rs:526-556`), a sustained flood adds roughly 60 packets
  (~120 KiB) of retained buffer per absorbed lookup against a `memory_mb = 2048`
  enclave. **[CORRECTED 2026-08-18 by the G15/G16 global auditor — see the marked
  CORRECTION block at the end of the Technical Details section. The retained-memory
  statement above is upheld. The "self-amplification" statement that stood here —
  that `insert_pending_acks` arms retransmission timers before `forward_messages`,
  so timers expire on packets still queued — is WRONG and has been struck: the SDK
  starts the timer from `SentNotificationListener`, after emission, for exactly this
  reason.]** The duplicate-fragment problem the project measured on the hub
  (15-25 duplicate fragments per message, `hub/src/nym_driver.rs:187-201`) is real
  and the shim has **no** `ack_wait_addition` equivalent, but its cause is the
  enclave's ack round trip exceeding the SDK's default timer, not queue-driven
  early firing. A shim OOM remains a realistic endpoint of a sustained flood
  through the retained-fragment path alone, which destroys everything in the buffer
  including acknowledged submits.
- Because the shim's `SendTransaction` path fails *open* (success) while its
  `GetTransaction` path fails *closed*, the user's experience during an attack is
  inverted from the truth: sends appear to work and lookups appear broken.

## Technical Details / Code Analysis

**The whole of the shim's admission control on this path** (`intercept.rs:249-295`)
is a 1 KiB body cap, a decodable `TxFilter`, and `filter.hash.len() == 32`. The
comment at `:272-273` — *"Validate the filter locally … so a bad filter never
becomes a hub round trip"* — is true and is the only throttle present; a *good*
filter always becomes a hub round trip, and 32 random bytes are always a good
filter.

**The shared channel and the asymmetric budgets.** `main.rs:335-336`:

```rust
335    let (req_tx, req_rx) = mpsc::channel(32);
336    let (out_tx, out_rx) = mpsc::channel(8);
```

`req_tx` becomes `NymHandle.requests` and is used by **both** operations. Submit
(`nym.rs:639`, `:660`, `:685-689`):

```rust
639        let deadline = tokio::time::Instant::now() + self.dispatch_timeout;   // 5 s
660            match tokio::time::timeout_at(deadline, self.requests.send(request)).await {
661                Ok(Ok(())) => dispatched += 1,
675                Ok(Err(_)) | Err(_) => break,
...
685        if dispatched > 0 { Ok(()) } else { Err(NymError::TransportGone) }
```

Lookup (`nym.rs:758-759`, inside `each_target`):

```rust
758            let deadline = tokio::time::Instant::now() + self.timeout;   // 90 s
759            match tokio::time::timeout_at(deadline, self.requests.send(request)).await {
```

tokio's bounded `mpsc` acquires a permit from an internal fair, intrusive-FIFO
semaphore, so a submit's `send` future is queued behind every lookup already
waiting and cannot overtake them. Eighteen-to-one patience on a FIFO is the whole
of harm (3).

**What the drain rate actually is — and it is not the emission rate.** This is the
correction that fixes the filed arithmetic. `correlate` reserves an outbound
permit before accepting each request (`nym.rs:851-880`), the driver holds one send
in flight (`nym_driver.rs:362-375`), and `send_frame` awaits
`sender.send_message(...)` (`:608-623`). But that await completes when the **SDK
accepts** the message, not when it is emitted:

- `send_message` → `ClientInput::send` → `mpsc::channel::<InputMessage>(1)`
  (`base_client/mod.rs:1013`);
- the input listener prepares fragments and awaits
  `real_message_sender.send(batch)` on a channel of capacity **8**
  (`real_messages_control/mod.rs:150`);
- `OutQueueControl::poll_poisson` polls that receiver **once per Poisson tick**
  and stores the whole batch, emitting **one packet** per tick
  (`real_traffic_stream.rs:443-478`), into a `TransmissionBuffer` with no size
  limit and no byte budget (`transmission_buffer.rs:39-49`).

So the shim absorbs of order **one whole request per packet-tick** — 8.33/s at the
crate's throttled floor, 50/s at the SDK's 20 ms default — while emitting **one
lookup per 61 ticks** (0.14-0.8/s). The 32-slot channel therefore does *not*
represent tens of seconds of backlog; it drains quickly into an unbounded internal
queue. That is what makes harm (2) automatic and harm (3) a concurrency question:

- to be behind more than 5 s of drain, a submit needs `> 5R` waiters ahead of it:
  ~42 at 8.33/s, ~250 at 50/s;
- to *sustain* a waiter list at all, arrivals must at least match `R`. Each
  attacker request lives ≤ 90 s (`REQUEST_TIMEOUT` covers acceptance and reply
  together, `nym.rs:758-770`), so `W` concurrent requests deliver `W/90` arrivals
  per second and the condition is `W > 90R`: **~750 concurrent requests at the
  throttled rate, ~4,500 at the unthrottled default.**

Both are trivially affordable — ~100 bytes each, ≤90 s each, no connection cap
(`proxy.rs:470-541`) and no advertised `max_concurrent_streams`
(`proxy.rs:596-610`) — but they are two orders of magnitude more than the "few
tens of concurrent requests" originally filed, and the corrected figure is what a
reviewer should check the fix against.

**The wallet is answered before anything leaves the process.** `hub.rs:228-249`:

```rust
            HubTransport::Nym(handle) => match handle.submit(tx_bytes).await {
                // ... There is no Refused arm: the hub's verdict is a full round
                // trip away and is deliberately not waited for ...
                Ok(()) => Ok(Submit::Accepted { txid: crate::nym::local_txid(tx_bytes) }),
```

and what sits behind that hand-off, in the crate's own words
(`nym_driver.rs:418-432`):

> *"be clear about what disconnect() DOES discard: everything the SDK still holds
> internally — its one-slot input, an 8-deep batch channel, and an **unbounded
> transmission buffer drained at the throttled rate**. There is no
> drain-then-disconnect in the SDK. Frames in there may include SUBMITS ALREADY
> ANSWERED SUCCESS to a wallet, and nothing upstream can protect them: a submit's
> waiter is swept the moment it is dispatched, so the supervisor's inflight count
> never sees it."*

**The emission cost, from the crate's own constants** (`nym.rs:1090-1130`):

```rust
    const PACKET_BYTES: usize = 2 * 1024;
    /// The client's own floor on sending, `MAX_DELAY_MULTIPLIER` (6) times the
    /// 20 ms default `message_sending_average_delay`.
    const THROTTLED_PACKETS_PER_SEC: f64 = 1000.0 / 120.0;
```

`packets(LOOKUP_BYTES) + LOOKUP_REPLY_SURBS = 1 + 60 = 61` packets ⇒ **7.3 s per
lookup** at the floor (1.2 s unthrottled) — for one ~100-byte request.

**The failure arms.** `intercept.rs:204-214` (submit) and `:315-324` (lookup) both
answer `GRPC_UNAVAILABLE` and never fall back to the operator's indexer, which is
correct and is why this is a denial/integrity issue rather than a leak. And the
health endpoint that does not notice (`nym.rs:307-309`):

```rust
307    pub fn is_healthy(&self) -> bool {
308        !self.0.configured.load(Ordering::Relaxed) || self.0.connected.load(Ordering::Relaxed)
309    }
```

## Recommendations

1. **Give submits reserved capacity that lookups can never consume.** They
   contend for one transport but have opposite value: a lost submit is a user who
   cannot migrate and may be told they did, a lost lookup is a retry. A separate
   small channel for submits, or a two-priority queue, means no lookup backlog can
   take the diversion path down. This is the single fix that closes harm (3) here
   **and** the same harm in
   `junk-sendtransaction-flood-consumes-the-shims-whole-mixnet-egress-and-converts-acknowledged-migrations-into-silent-loss.md`,
   which that issue's own remediation does not address.
2. **Cap concurrent outstanding lookups in the shim**, the way `hub/src/nym.rs`
   caps them with a semaphore, and answer over-cap lookups with
   `RESOURCE_EXHAUSTED` immediately rather than queueing them. A shim serving a
   normal wallet population needs single-digit concurrency here. Refusing quickly
   is fail-closed and costs the transport nothing.
3. **Bound the accepted backlog by what the transport can actually deliver, and
   stop answering `error_code 0` for work that is only queued.** The design's own
   `MAX_DELIVERY_LAG` (6 blocks, `hub/src/batcher.rs:46-48`) is asserted at
   startup as if it were a constant; this issue makes it attacker-controlled. Once
   the depth of `requests` + `out_frames` + the SDK's lane queue exceeds that
   budget, fail closed so the wallet retries rather than accepting a migration
   that will arrive too late. A visible `UNAVAILABLE` is strictly better for the
   user than a silent success.
4. **Rate-limit `GetTransaction` per source address / per connection** before
   `diversion.hub.get_transaction` is called. The peer address is already
   available at `proxy.rs:488`, and the shim terminates TLS itself. Also set
   `max_concurrent_streams` on the h2 server and cap concurrent connections in
   the accept loop; both are one line each and neither refuses anything a wallet
   does.
5. **Make `/healthz` (and `/nym-status`) reflect the condition that actually
   matters** — whether a submit can currently be dispatched, and how deep the
   emission backlog is — rather than only whether the client is connected. As
   written, the one failure mode `MixnetStatus` was added for is reachable with
   the client up.
6. **Make the submit budget no smaller than the lookup budget**, or drop the
   asymmetry entirely, so an adversary's lookups cannot out-wait a user's
   migration on a shared FIFO. This is a stopgap: item 1 is the real fix.
7. **Give the shim the `ack_wait_addition` knob the hub received**
   (`hub/src/nym_driver.rs:198-217`), and cap the shim's retransmissions. The
   rationale originally given here (*"because `insert_pending_acks` runs before
   `forward_messages`, a deep backlog retransmits packets that were never emitted"*)
   is **withdrawn as incorrect** — see the CORRECTION block below. The
   recommendation itself stands on the correct mechanism, which is that the
   enclave's measured ack round trip exceeds the SDK's default
   `1.5 x expected_delay + 1500 ms` and that the shim's sends carry
   `max_retransmissions: None`. Both are now filed separately, with full evidence,
   as
   `plausible/shim-mixnet-client-has-neither-retransmission-bound-the-hub-has-so-an-unacked-frame-retransmits-forever.md`.

**CORRECTION (added 2026-08-18, G15/G16 global auditor; derived from the pinned SDK
tree at `451c2aa`, which is available locally).** Three passages in this file assert
that `insert_pending_acks` running before `forward_messages`
(`message_handler.rs:554-555`) arms retransmission timers on packets that have not
been emitted, and that this self-amplifies. The SDK does not behave that way:

- `ActionController::handle_insert` inserts each `PendingAcknowledgement` with
  `queue_key = None` and starts **no** timer
  (`common/client-core/src/client/real_messages_control/acknowledgement_control/action_controller.rs:122-137`).
- A timer is created only by `Action::StartTimer` → `handle_start_timer`
  (`action_controller.rs:139-162`), whose enum doc says *"Initiated by
  `SentNotificationListener`"* (`:38-42`).
- `SentNotificationListener`'s own module doc states the purpose verbatim: *"Module
  responsible for starting up retransmission timers. It is required because when we
  send our packet to the `real traffic stream` controlled by a poisson timer,
  there's no guarantee the message will be sent immediately, so we might
  accidentally fire retransmission way quicker than we should have"*
  (`sent_notification_listener.rs:10-13`), and it fires on the sent notification
  (`:30-38`).

So the ordering of `insert_pending_acks` and `forward_messages` is **harmless**, and
the flood does not self-amplify through that path. What survives unchanged is the
*retention* consequence: `try_split_and_send_non_reply_message` clones every fragment
before preparing it — *"we need to clone it because we need to keep it in memory in
case we had to retransmit it"* (`message_handler.rs:530-532`) — and holds the clone
in the pending-ack map until the ack arrives (`:546-547`), which is the ~120 KiB per
absorbed lookup this issue relies on. **The severity and the confirmed verdict are
not affected**; only the mechanism attributed to one contributing leg is.

**Coordinator open item 7b is answered by this correction.** Its premise ("arms
retransmission timers before `forward_messages`, self-amplifying") does not hold. The
shim-OOM leg should **stay inside this issue** — it shares this issue's trigger,
attacker and remediation family — while the missing `ack_wait_addition` knob and the
absent retransmission cap are filed as their own issue (linked above) because they
degrade every deployed shim with no attacker present and have a distinct one-line fix.

## Validation Information

**Verdict: CONFIRMED. Severity: High (as filed), for reasons partly different
from those given in the filing.** The mechanism was traced end to end through the
shim and through the **pinned SDK tree at
`451c2aa3692fc4dc00041b74a352d4158176d9c0`**, which is present locally. Three
corrections were applied; one of them changes which harm carries the severity.

### What was verified

| Claim | Verified at |
|---|---|
| The listener is internet-reachable and unauthenticated; routing is a pure function of the path | `caution.hcl.tmpl` (`ZIS_LISTEN = 0.0.0.0:8083`, `ingress 0.0.0.0/0`), `proxy.rs:743-748` |
| No connection cap, no `max_concurrent_streams`, only window sizes | `proxy.rs:470-541`, `:596-610` |
| With a hub configured, every `GetTransaction` goes to the hub and none to the operator | `intercept.rs:229-247` |
| 32 random bytes pass every check | `intercept.rs:274-293` |
| Lookups and submits share one 32-slot channel | `main.rs:335`, used by both `nym.rs:660` and `:759` |
| 90 s vs 5 s asymmetry, and tokio `mpsc` is a fair FIFO semaphore | `nym.rs:71`, `:80`, `:639`, `:758` |
| `dispatched == 0` → `TransportGone` → `UNAVAILABLE` at the wallet | `nym.rs:685-689`, `intercept.rs:204-214` |
| The wallet is told success at hand-off, with no `Refused` arm | `hub.rs:228-249` |
| The SDK's buffer behind that hand-off is unbounded, and the crate knows it | `nym_driver.rs:416-436`; `transmission_buffer.rs:39-49` |
| 61 packets per lookup at 8.33 packets/s = 7.3 s of the shim's whole egress | `nym.rs:104`, `:1090-1130`, cross-checked against `config-types/src/lib.rs:25`, `:443` |
| `/healthz` is `configured && connected` and stays green | `nym.rs:307-309` |

### Correction 1 — the drain-rate model was wrong, and the concurrency figure with it

The filing said the 32-slot channel "drains at mixnet speed, not at request
speed", so "a handful of concurrent lookups is enough to keep it saturated
indefinitely" and "a few tens of concurrent requests suffice". **That is not how
the SDK behaves.** `send_message` returns when the SDK *accepts* the message:
capacity-1 `InputMessage` channel (`base_client/mod.rs:1013`) → 8-slot batch
channel (`real_messages_control/mod.rs:150`) → `poll_poisson`, which stores one
whole batch and emits one packet per tick (`real_traffic_stream.rs:443-478`) into
an uncapped `TransmissionBuffer`. The shim therefore absorbs ~8-50 requests per
second while emitting 0.14-0.8 lookups per second.

The corrected thresholds are derived in Technical Details: `> 5R` waiters to
outlast a submit's 5 s budget (~42 at the throttled rate, ~250 unthrottled), and
`W > 90R` concurrent live requests to sustain that queue (**~750 to ~4,500**).
This is exactly the check that must be done before a claimed rate is believed —
a rate that merely matches the drain rate denies nothing. Here the corrected
figure is still trivially affordable (~100 bytes per request, ≤90 s each, no cap
on connections or streams anywhere), so the finding stands; but the filed "a few
tens" was off by two orders of magnitude and would have let a reviewer
under-specify the fix.

### Correction 2 — the severity is carried by the silent harm, not the loud one

The filing's headline harm is the loud one: a submit fails closed and the wallet
sees `UNAVAILABLE`. That harm is real, but it is the **most expensive to produce
and the least damaging**, because a wallet that is told UNAVAILABLE retries and
nothing is lost.

The harm that carries the severity needs almost no concurrency at all. At any
sustained rate above roughly one request per 7.3 s the shim's emission backlog
grows without bound inside the SDK, and then:

- every wallet's `GetTransaction` through that shim times out at 90 s and fails
  closed — certain, and free; and
- a migration that *is* dispatched is answered `error_code 0` with a txid
  (`hub.rs:228-249`) and then sits in an unbounded buffer the shim cannot see,
  to be refused `ExpiryTooTight` on arrival (into a discarded ack) or destroyed
  by the next rebuild, rotation, redeploy or SIGTERM — every one of which
  discards the buffer with no drain, as `nym_driver.rs:416-436` states.

So the attacker chooses between a loud denial and a silent destruction of a
migration the user believes is spent, and the silent one is cheaper. The body has
been restructured accordingly.

### Correction 3 — added consequences that were not in the filing

- **Shim memory growth toward OOM.** Fragments are fully-built sphinx packets
  retained with a clone for retransmission (`message_handler.rs:526-556`), so
  ~120 KiB of buffer accrues per absorbed lookup against `memory_mb = 2048`.
- ~~**Self-amplification.** `insert_pending_acks` is called *before*
  `forward_messages` (`message_handler.rs:553-555`), so retransmission timers
  expire on packets still queued.~~ **STRUCK 2026-08-18 — the premise is false; see
  the CORRECTION block at the end of Technical Details. Timers start from
  `SentNotificationListener`, after emission.** What remains true and is retained:
  the project measured 15-25 duplicate fragments per message on the hub
  (`hub/src/nym_driver.rs:187-201`) and mitigated it with
  `ZIH_ACK_WAIT_ADDITION_MS`; the shim has no equivalent and sets no `DebugConfig`
  on a production path. The cause is the ack round trip exceeding the SDK's default
  timer, not early firing.
- Recommendations 3, 4 (h2/connection caps) and 7 are new and follow from these.

The filing's amplification framing ("~60-byte request buys ~64 KiB … well over
1000×") is arithmetically right but is the least useful way to state it; the
operative figure is **7.3 seconds of a single, serialised, non-substitutable
resource per ~100-byte request**, and the body now leads with that.

### `docs/AVOIDING-FALSE-POSITIVES.md` §5 applied

§5's own statement of the real-vulnerability shape is *"amplification attacks
where small input causes disproportionate resource use"*, and its contrasting real
issues are *"1 KB request causing 1 GB memory allocation"* and *"single connection
consuming unbounded resources"*. This is both: ~100 bytes buys 61 sphinx packets
and ~120 KiB of retained enclave buffer, and the resource consumed is not
elastic — it is one throttled mixnet client that also carries every migration.

*What resources would the attacker need?* A few hundred bytes per minute for the
lookup outage and the silent-loss harm; a few KB/s and several hundred concurrent
h2 streams for the loud one. No credential, no wallet, no funds, no Nym client.

*What would stop them?* Nothing in the target and nothing in the deployment: no
authentication, no rate limit, no per-source accounting, no connection cap, no
`max_concurrent_streams`, no lookup concurrency bound, and a `/healthz` that stays
green throughout.

*Why §5 does not cap this at Medium.* §5 would normally cap a throughput attack,
and this issue is graded above that cap for one reason that was checked rather
than assumed: the shim answers `error_code 0` at an in-process channel send
(`hub.rs:228-249`), so the denial is not visible **as** a denial. Note the
discipline this requires — that multiplier is not allowed to carry the grade on
its own. Reachability stands independently: an unauthenticated internet request
to a public DNS name becomes a metered 61-packet mixnet emission at a fixed cost
that does not depend on the request's size, and the corrected concurrency
arithmetic above shows the queue really is deniable rather than merely matched.

### Severity: High, and why it is not a duplicate

*Impact:* for an operator the attacker chooses, the privacy-critical divert path
and the whole `GetTransaction` path are disabled, and the `SendTransaction`
failure mode is a **false success** for a transaction that may reach no mempool,
in a pool NU6.3 has closed to new value. *Likelihood:* an unauthenticated request
to a public DNS name, at a few hundred bytes per minute, from anywhere, with no
detection surface. *Why not Critical:* no funds are stolen and no key or
plaintext is disclosed; the destruction (as opposed to delay) of an acknowledged
migration needs either a near-default wallet expiry or a teardown to coincide;
and a ZIP 318 migration's submission is destroyed rather than its funds lost (see
the CORRECTION above: the wallet keeps resubmitting for the whole expiry window).

*Not a duplicate of
`junk-sendtransaction-flood-…-silent-loss.md` (High).* That issue reaches the same
shared resource through `SendTransaction`, and its remediation — refusing
zero-length/`Unparseable` bodies — does **nothing** here, because these requests
are perfectly well-formed lookups that a wallet also sends. Conversely this
issue's fix (reserved submit capacity) is the one that closes both. The two are
siblings on one root cause — a single 32-slot channel and one serialised emitter
shared by both operations — and both must be fixed.

*Relationship to `hub-nym-lookup-flood-starves-gettransaction-fleet-wide.md`
(Medium).* The two were graded against each other, not in isolation, and this one
is the more severe **despite the smaller blast radius**. That issue is fleet-wide
but needs sustained free Nym clients, denies only the lookup path, fails loudly at
the wallet, destroys nothing, and heals when the backlog drains. This one needs no
mixnet capability at all, lands on the *submit* path where the shim's
success-at-hand-off converts denial into silent destruction of an acknowledged
migration, additionally consumes the hub's fleet-wide emitter through the shim
(the composition recorded as G5 §3.1, reachable by an attacker with no Nym
experience), and grows the shim's memory while it runs. This ordering upholds G5
§2, which ranked the `GetTransaction` flood at a shim (#1) above the direct hub
lookup floods (#4, #5).

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
