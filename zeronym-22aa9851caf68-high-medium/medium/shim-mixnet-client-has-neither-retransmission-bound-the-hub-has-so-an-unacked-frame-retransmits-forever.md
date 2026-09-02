# The shim's mixnet client has neither of the two retransmission bounds the hub has, so every submit and lookup retransmits out of the shim's whole migration-carrying egress budget, and an un-acknowledgeable destination retransmits forever

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/shim/src/nym_driver.rs:104-156` (`build_client`, no `DebugConfig` on any production path) and `:618` (`send_message` with `IncludedSurbs`); `audit-target/zeronym/shim/deploy/caution/assemble-caution.sh` (no ack-wait argument anywhere); `audit-target/zeronym/deploy.sh:110-113` (the shim branch of the only deploy driver). Contrast: `audit-target/zeronym/hub/src/nym_driver.rs:186-216` and `:638` (`send_reply`), `audit-target/zeronym/hub/deploy/caution/assemble-caution.sh:107,353`, `audit-target/zeronym/deploy.sh:121`, `audit-target/zeronym/deploy.env.example:25-41`. Mechanism in the pinned SDK (`nym-binaries-v2026.15-bydgoszcz`, commit `451c2aa`): `common/client-core/src/client/inbound_messages.rs:107-147`, `common/client-core/config-types/src/lib.rs:25,393-395,446`, `common/client-core/src/client/real_messages_control/acknowledgement_control/mod.rs:124-133`, `.../retransmission_request_listener.rs:67-165`, `.../sent_notification_listener.rs:10-38`, `.../message_handler.rs:500-555`, `common/client-core/src/client/transmission_buffer.rs:149-157,201-223`, `common/client-core/src/client/real_messages_control/real_traffic_stream/sending_delay_controller.rs:23`; `nym-node/src/node/mixnet/handler.rs:281-320` (the ack is emitted by the destination gateway).
**Found by agent:** Global, focus area G15 (the Nym transport read as one system, both sides)
**In scope of audit?** Yes

## Description

The hub and the shim run the same `nym-sdk` client, in the same enclave, on the
same mixnet. The project measured that this environment's acknowledgement path is
too slow for the SDK's default retransmission timer, and fixed it **on the hub
only**. Reading the two sides together, the shim is missing *both* of the bounds
that keep the hub's retransmission behaviour finite:

1. **No ack-wait knob.** `hub/src/nym_driver.rs:202-216` reads
   `ZIH_ACK_WAIT_ADDITION_MS` and raises `debug.acknowledgements.ack_wait_addition`;
   `deploy.env.example:41` ships `HUB_ACK_WAIT_MS=15000` with the measurement that
   justifies it. `shim/src/nym_driver.rs:104-156` builds **no** `DebugConfig` on any
   production path — its only `debug_config` call (`:126-140`) is behind
   `#[cfg(feature = "mixnet-localnet")]`, sets `message_sending_average_delay` rather
   than `ack_wait_addition`, and is not compiled into the shipped image
   (`shim/deploy/Containerfile:88`: `ARG CARGO_FEATURES="mixnet-driver"`). There is no
   `--ack-wait-ms` in `shim/deploy/caution/assemble-caution.sh` and no variable for it
   in `deploy.sh`'s shim branch. Every deployed shim therefore runs at the SDK default
   of `1.5 x expected_delay + 1500 ms`.

2. **No retransmission cap.** The hub's replies go out as `InputMessage::new_reply`,
   which sets `max_retransmissions: Some(10)` (`inbound_messages.rs:128-147`). The
   shim's submits and lookups go out as `InputMessage::new_anonymous`, which sets
   `max_retransmissions: None` (`inbound_messages.rs:107-126`), and the SDK's global
   cap defaults to `None` — documented in the field as *"None - no limit"*
   (`config-types/src/lib.rs:393-395`, default at `:446`). Neither binary overrides
   it. `PendingAcknowledgement::reached_max_retransmissions` is therefore
   **permanently false** for every packet the shim sends
   (`acknowledgement_control/mod.rs:124-133`), and the only other removal path is the
   arrival of the acknowledgement itself
   (`retransmission_request_listener.rs:81-90`).

The consequence in normal operation is wasted egress on the one leg that carries
migrations, on a budget the project's own tests describe as barely sufficient. The
consequence in the failure mode the design itself describes — a `ZIS_HUB_NYM` entry
whose gateway is gone — is a permanently retransmitting frame that is never freed
and never gives up.

## Attack Scenario and Steps

**Path A — no attacker (the shipped steady state).**

1. A shim is deployed by `deploy.sh` with `HUB_NYM` set. Nothing sets an ack-wait
   value; the SDK default of 1500 ms applies.
2. The enclave's real acknowledgement round trip exceeds
   `1.5 x expected_delay + 1500 ms`. This is not hypothetical: `deploy.env.example:30-40`
   records that on the hub, *at 6000 ms*, "the same client ... still resent two of four
   replies in their ENTIRETY (31 and 35 duplicates of a 32-packet reply, still arriving
   8-11 s after the first)". The shim runs the same client in the same enclave against
   the same mixnet, with **larger** messages (45 packets for a submit against the hub's
   32-41 for a reply).
3. Every duplicate consumes one emission slot at the shaped ~8.33 packets/s the shim's
   own `throughput_budget` tests pin (`shim/src/nym.rs:1094`; verified against
   `sending_delay_controller.rs:23` `MAX_DELAY_MULTIPLIER = 6` and
   `config-types/src/lib.rs:25` 20 ms). Roughly doubling the packets per migration
   roughly halves the shim's migration throughput, against a budget the project's own
   test says has only ~3x headroom inside `REQUEST_TIMEOUT`.

**Path B — an unacknowledgeable destination (no attacker needed either).**

4. `NymHandle::submit` sends every migration to **every** configured `ZIS_HUB_NYM`
   entry (`shim/src/nym.rs:595-689`), and the module's own comment describes that list
   as *"the current address, and the one it just rotated away from ... nothing is
   listening at the stale one"* (`:620-626`).
5. The hub takes a fresh identity precisely when its gateway registration is
   unrecoverable — 60 consecutive failed connects, or five short-lived clients
   (`hub/src/nym_driver.rs:99,124,240-245,286-306`). In the connect-failure case the
   old address's **gateway** is the thing that is down.
6. A sphinx packet's acknowledgement is emitted by the *destination gateway* after the
   final hop, not by the recipient client — `nym-node/src/node/mixnet/handler.rs:285-317`
   forwards the ack after either pushing to the client or storing for it, and the
   forwarding call sits **outside** that match, so an unknown or offline *client* is
   still acknowledged. Only an unreachable *gateway node* produces the permanent case.
7. Each submit fanned out to such an address produces 45 fragments that will never be
   acknowledged. With `max_retransmissions = None` each one is re-prepared and
   re-queued once per ack-wait period **for the life of the client**, and the
   `PendingAcknowledgement` holding a clone of the fragment
   (`message_handler.rs:529-531,545-546`) is never freed. Two sub-cases, both verified:
   - **the dead gateway is still in the topology**: preparation succeeds, the packet is
     re-queued on `TransmissionLane::Retransmission`, and 45 fragments each demanding a
     re-emission every ~1.65 s cannot be served by an 8.33 packets/s emitter, so that
     lane is never empty. Because `pick_random_small_lane` prefers any lane holding
     fewer than 100 items (`transmission_buffer.rs:149-157`, `:201-223`), a
     permanently-short Retransmission lane **preempts** the General lane that carries
     real submits and lookups: real traffic gets roughly half the budget while the
     General lane is short, and **nothing at all** while it exceeds 100 packets (about
     three full frames). This condition never clears.
   - **the dead gateway has fallen out of the topology**: preparation fails and the
     listener restarts the timer instead of dropping the ack
     (`retransmission_request_listener.rs:116-128`, *"we NEED to start timer here
     otherwise we will have this guy permanently stuck in memory"*). No emission is
     spent, but the fragment clone is retained for the life of the client and the retry
     loop runs forever. Every subsequent migration adds another 45.

**Path C — an attacker makes Path A worse, cheaply.** The confirmed issues
`gettransaction-flood-starves-migration-diversion.md` and
`junk-sendtransaction-flood-consumes-the-shims-whole-mixnet-egress-...md` both work by
buying sphinx packets out of the shim's shaped emission budget from an unauthenticated
wallet-facing endpoint. Doubling the packets each honest migration costs halves the
traffic an attacker must generate to reach the same starvation.

**Attack Requirements and Assumptions:**
- Paths A and B require **no attacker at all** — A is the shipped configuration, B is
  the failover shape the design documents (D10) plus a gateway-node outage.
- Path B additionally requires that the stale `ZIS_HUB_NYM` entry's gateway **node** be
  unreachable rather than merely not hosting that client. A gateway that is up but no
  longer registers the client still forwards the ack, so that variant costs only the
  honest duplicate rate. The permanent case therefore needs a retired or offline
  gateway node — common over months on a public mixnet, and specifically likely in the
  connect-failure branch of the hub's own fresh-identity fallback.
- Path C requires only what those two confirmed issues already require.
- What makes this realistic: the project has already measured the underlying condition
  in production, on the other component, and shipped a mitigation for it there.

## Impact on Users

The shim's mixnet client is the sole carrier of every diverted migration. Its emission
budget is a single, serialised, non-elastic resource, and `NymHandle::submit` answers
the wallet **success at dispatch** (`shim/src/hub.rs:228-240`), before anything has
left the enclave. Consequences, in order of severity:

- **Migrations the wallet was told had succeeded are destroyed.** Anything still inside
  the SDK when the client is torn down is discarded — the driver documents this itself
  (`shim/src/nym_driver.rs:418-436`). Halving the drain rate roughly doubles the time a
  dispatched-and-acknowledged submit spends inside that window, and Path B keeps the
  window permanently occupied.
- **`GetTransaction` over the mixnet fails closed for real wallets** while duplicates
  occupy the emission slots, which is the user-visible symptom of both confirmed
  flood issues. In Path B's first sub-case this becomes permanent rather than
  transient, and no redeploy-free action clears it.
- **The enclave's memory grows and is never reclaimed** in Path B (45 retained fragment
  clones, ~2 KB each, per stuck submit, against `memory_mb = 2048`), and a shim OOM
  destroys the entire in-flight set. This is bounded per submit but unbounded in the
  number of submits, because every new migration fanned out to the dead address adds
  another 45.
- **Nothing reports it.** `/nym-status` (`shim/src/nym.rs:205-215`) exposes only
  `diversion_configured`, `mixnet_connected`, `client_deaths` and
  `consecutive_rebuild_failures`. A client that is connected and spending its whole
  budget on duplicates reports `mixnet_connected: true, client_deaths: 0`.

## Technical Details / Code Analysis

**The hub has the knob (`hub/src/nym_driver.rs:186-216`):**

```rust
    // How long the SDK waits for a packet's ack before it RETRANSMITS. The SDK
    // computes an expected ack round trip from the CONFIGURED mix delays -- not
    // measured -- and resends after `expected * ack_wait_multiplier +
    // ack_wait_addition` (defaults 1.5x + 1.5 s). ... Measured 2026-08-17: a local hub's
    // replies reached a shim with ~1 duplicate fragment per lookup; every
    // DEPLOYED hub's reached the same shim with 15-25 -- and each duplicate is a
    // full send slot at the throttled rate, which is how a 32-packet reply that
    // should take ~5 s took 45-90 s from every enclave and timed out. ...
    let builder = match std::env::var("ZIH_ACK_WAIT_ADDITION_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(ms) => {
            let mut debug = nym_sdk::DebugConfig::default();
            debug.acknowledgements.ack_wait_addition = Duration::from_millis(ms);
            ...
            builder.debug_config(debug)
        }
        None => builder,
    };
```

**The shim's equivalent function has no such branch (`shim/src/nym_driver.rs:104-156`).**
Its whole `build_client` is `MixnetClientBuilder::new_ephemeral()`, an optional
`request_gateway`, an optional localnet-only `debug_config`, an optional localnet-only
topology provider, then `build()`/`connect_to_mixnet()`. The localnet branch is
explicitly and correctly excluded from production:

```rust
    // Gated on `mixnet-localnet` so a PRODUCTION binary cannot read it at all: a
    // non-default send rate would make this client's traffic distinguishable from
    // every other Nym client, which is a fingerprint ...
    #[cfg(feature = "mixnet-localnet")]
    let builder = match std::env::var("ZIS_LOCALNET_SEND_DELAY_MS")
```

and `shim/deploy/Containerfile:88` sets `ARG CARGO_FEATURES="mixnet-driver"`, so
`mixnet-localnet` is off in every shipped image. A repository-wide search for
`DebugConfig|debug_config|ack_wait` over `shim/`, `hub/` and `nymnet/` returns exactly
the hub's branch, the shim's localnet branch, and the hub's deploy plumbing — the shim
has no production `DebugConfig` at all.

**The deploy chain mirrors the asymmetry.** `deploy.sh:114-121` (hub branch) forwards
`HUB_ACK_WAIT_MS` as `--ack-wait-ms`, which
`hub/deploy/caution/assemble-caution.sh:107,353` turns into
`ZIH_ACK_WAIT_ADDITION_MS` in `unit.env`. `deploy.sh:110-113` (shim branch) forwards
only `--backend`, `--backend-tls` and `--hub-nym`; `grep -c ack-wait` over
`shim/deploy/caution/assemble-caution.sh` returns **0**.

**The SDK caps the hub's replies and not the shim's sends**
(`common/client-core/src/client/inbound_messages.rs`):

```rust
    pub fn new_anonymous(                       // :107  <- every shim submit and lookup
        ...
        let message = InputMessage::Anonymous {
            ...
            max_retransmissions: None,          // :119
        };

    pub fn new_reply(                           // :128  <- every hub reply
        ...
        let message = InputMessage::Reply {
            ...
            // \/ set it to SOME sane default so that if we run out of surbs and constantly
            // fail to request more, we wouldn't be stuck in limbo
            max_retransmissions: Some(10),      // :140
        };
```

`shim/src/nym_driver.rs:616-620` calls `sender.send_message(recipient, out.frame.to_vec(),
IncludedSurbs::new(out.reply_surbs))`, and `IncludedSurbs::Amount(_)` routes to
`InputMessage::new_anonymous` (`sdk/rust/nym-sdk/src/mixnet/traits.rs:80-96`).
`hub/src/nym_driver.rs:638` calls `sender.send_reply(tag, frame)`, which routes to
`InputMessage::new_reply` (`traits.rs:122-134`).

The global cap that could have saved the shim is also unset
(`common/client-core/config-types/src/lib.rs:393-395` and `:446`):

```rust
    /// Specify how many times particular packet can be retransmitted
    /// None - no limit
    pub maximum_number_of_retransmissions: Option<u32>,
    ...
            maximum_number_of_retransmissions: None,
```

so `reached_max_retransmissions` is a disjunction of two `is_some_and` tests over two
`None`s (`acknowledgement_control/mod.rs:124-133`) and can never be true for a shim
packet. `retransmission_request_listener.rs:81-90` is the only place that consults it,
and the only other path that removes a pending acknowledgement is the ack itself.

**The retained data.** `try_split_and_send_non_reply_message`
(`message_handler.rs:500-555`) clones every fragment before preparing it — *"we need to
clone it because we need to keep it in memory in case we had to retransmit it"*
(`:529-531`) — and stores the clone in a `PendingAcknowledgement`
(`:545-546`). Those clones are held until the ack arrives; with no cap and no ack, for
the life of the client.

**How fast retransmissions can actually be generated, and why the lane matters.** A
retransmission timer is started **only after the packet has been emitted**, by
`SentNotificationListener`, whose module doc states the reason verbatim: *"It is
required because when we send our packet to the `real traffic stream` controlled by a
poisson timer, there's no guarantee the message will be sent immediately, so we might
accidentally fire retransmission way quicker than we should have"*
(`sent_notification_listener.rs:10-13,30-38`). So retransmission demand is
self-limiting: it cannot exceed the emission rate, and the Retransmission lane does not
grow without bound. What it does instead is **occupy the emitter permanently**: 45
stuck fragments want a slot every ~1.65 s (about 27 packets/s of demand) against an
8.33 packets/s ceiling, so the lane is always non-empty, and
`pop_next_message_at_random` prefers any lane with fewer than 100 items over a longer
one (`transmission_buffer.rs:149-157`, `:201-223`). Real traffic therefore shares the
budget roughly evenly while the General lane is short and is starved outright once it
is long. The hub's `Some(10)` bounds the same behaviour to ~16 s per stuck reply; the
shim has no such bound.

**CORRECTION, carried forward from `PROGRESS.md` item 7b-REFUTED and re-verified here.**
An earlier claim held that *"`insert_pending_acks` arms the retransmission timers before
`forward_messages`, so ack timers expire on packets still sitting in the queue"*, i.e.
that retransmission **self-amplifies**. **That is not what the SDK does and the premise
is withdrawn.** `ActionController::handle_insert` inserts the pending ack with
`queue_key = None` and starts no timer; the timer is started only by `Action::StartTimer`
from `SentNotificationListener`, after emission, as quoted above. Nothing in this issue
depends on that withdrawn premise: the retransmission problem here is caused by the ack
round trip exceeding a timer the shim cannot tune, and by the absence of any cap on how
many times a packet may be retried. The **memory-retention** half of the old claim is
correct and is restated above.

## Recommendations

1. **Give the shim the knob the hub has.** Add a `ZIS_ACK_WAIT_ADDITION_MS` branch to
   `shim/src/nym_driver.rs::build_client` identical to `hub/src/nym_driver.rs:202-216`,
   a `--ack-wait-ms` argument to `shim/deploy/caution/assemble-caution.sh`, and a
   `SHIM_ACK_WAIT_MS` variable to `deploy.sh`'s shim branch and `deploy.env.example`.
   The hub's shipped value (15000) is the measured starting point; the shim's frames
   are larger, so it needs at least as much.
2. **Cap the shim's retransmissions.** Set
   `debug.traffic.maximum_number_of_retransmissions = Some(n)` in the same
   `DebugConfig` (the SDK already applies `Some(10)` to the hub's replies, and
   `InputMessage::with_max_retransmissions` exists for per-message control). Without a
   cap, an unacknowledgeable destination is a permanent, unrecoverable drain on the one
   resource that carries migrations.
3. **Make the drain visible.** `/nym-status` cannot currently distinguish a healthy
   client from one spending its entire budget on duplicates. The SDK's
   `MixnetClient::shared_lane_queue_lengths()` (which exposes the Retransmission lane
   separately) and the pending-ack count are the two numbers that would; failing that,
   publish `out_frames.len()` and a dispatched-versus-acked delta (the ack nonce and
   waiter already exist — see
   `nym-submit-acks-are-never-read-so-every-hub-refusal-is-invisible.md`).
4. **Reconsider the unconditional submit fan-out to stale addresses.** A destination
   that has not acknowledged anything for a long interval should stop being fanned out
   to, or the list should be prunable without a redeploy.

## Validation Information

**Verdict: CONFIRMED. Severity confirmed at Medium.**

### Verified against the pinned SDK tree at `451c2aa` (read directly, not inferred)

- `inbound_messages.rs:107-126` — `new_anonymous` sets `max_retransmissions: None`.
  `:128-147` — `new_reply` sets `Some(10)` with the quoted comment. `new_regular`
  (`:88-105`) is also `None`. **The asymmetry is by construction in the SDK, exactly as
  claimed.**
- `config-types/src/lib.rs:393-395` and `:446` — `maximum_number_of_retransmissions`
  defaults to `None`. Neither zeronym binary sets it (`grep -rn
  "maximum_number_of_retransmissions\|DebugConfig\|debug_config" shim/ hub/` returns
  only the hub's `ack_wait_addition` branch and the shim's localnet-only
  `message_sending_average_delay` branch).
- `acknowledgement_control/mod.rs:124-133` — `reached_max_retransmissions` is
  `local.is_some_and(..) || global.is_some_and(..)`, so it is **permanently false** for a
  shim packet.
- `retransmission_request_listener.rs:81-90` — that predicate is the only path to
  `Action::new_remove`. `:116-128` — when preparation fails the timer is restarted
  rather than the entry dropped, with the comment *"we NEED to start timer here
  otherwise we will have this guy permanently stuck in memory"*, which is what makes
  the removed-from-topology sub-case a permanent retention rather than a permanent
  emission cost.
- `sdk/rust/nym-sdk/src/mixnet/traits.rs:72-96` and `:122-134` — the routing from
  `send_message`/`send_reply` to the two constructors, confirming which zeronym call
  site gets which cap.
- `sent_notification_listener.rs:10-13,30-38` — timers start after emission. This is
  the fact that **refutes** the withdrawn self-amplification premise and also bounds
  the retransmission rate; the issue text has been rewritten around it.
- `transmission_buffer.rs:149-157` (`is_small()` = fewer than 100 items) and
  `:201-223` (`pop_next_message_at_random` prefers a small lane over everything else)
  — this is new to the filing and is what makes a stuck Retransmission lane preempt
  real traffic. Verified in source; added to Path B.
- `nym-node/src/node/mixnet/handler.rs:281-320` — `forward_ack_packet` is called
  **after** the push-or-store match and outside it, so an offline or unknown client is
  still acknowledged. The filing's central caveat ("only an unreachable gateway
  produces the permanent case") is therefore correct, and this was checked before the
  claim rather than after.
- `sending_delay_controller.rs:23` `MAX_DELAY_MULTIPLIER = 6` and
  `config-types/src/lib.rs:25` 20 ms give the 8.33 packets/s ceiling the arithmetic
  uses; `real_traffic_stream.rs:373-377` shows the multiplier rises only when the
  gateway channel is full, which the project reports its deployed enclaves reach
  (`shim/src/nym.rs:1071-1077`).

### Verified against the target

- `shim/src/nym_driver.rs:104-156` — `build_client` in full; the only `debug_config`
  call is `#[cfg(feature = "mixnet-localnet")]` at `:126-140`, and
  `shim/deploy/Containerfile:88` is `ARG CARGO_FEATURES="mixnet-driver"`.
- `hub/src/nym_driver.rs:186-216` — the `ZIH_ACK_WAIT_ADDITION_MS` branch, with the
  2026-08-17 measurement in the comment.
- `deploy.sh:110-113` (shim) versus `:114-121` (hub, including
  `--ack-wait-ms "$HUB_ACK_WAIT_MS"`); `hub/deploy/caution/assemble-caution.sh:107` and
  `:353`; `grep -c ack-wait shim/deploy/caution/assemble-caution.sh` = **0**.
- `deploy.env.example:25-41` — the shipped `HUB_ACK_WAIT_MS=15000` and the measurement
  that at 6000 ms two of four replies were still resent in their entirety. This is the
  project's own production evidence that the condition is real in this exact
  environment.
- `shim/src/nym.rs:595-689` — the fan-out to every configured address, and `:620-626`
  the comment stating that the stale address has nothing listening. So Path B's
  precondition is a *documented, intended* configuration, not a hypothetical
  misconfiguration.
- `shim/src/hub.rs:228-240` — `Submit::Accepted` at hand-off, with *"a refusal is never
  surfaced here"*; `shim/src/nym.rs:205-215` — `/nym-status` carries no queue or
  duplicate signal.

### Corrections made during validation

- **"27 pkt/s of demand against an 8.33 pkt/s ceiling: one stuck submit is enough to
  saturate the shim's entire mixnet egress permanently"** overstated the mechanism.
  Retransmission timers restart only after emission, so demand cannot exceed supply and
  the lane does not grow without bound. The accurate and still-serious statement, now in
  Path B, is that the lane is permanently non-empty and, because the SDK prioritises
  short lanes, permanently preempts real traffic — roughly half the budget when the
  General lane is short and effectively all of it once the General lane exceeds 100
  packets.
- **Path B was split into its two real sub-cases** (dead gateway still in topology =
  permanent emission cost; dead gateway out of topology = permanent memory retention
  and a forever retry loop with no emission), because the SDK behaves differently in
  each and only the first starves the emitter.
- The memory claim was made precise: ~90 KB per stuck submit is bounded, but it is
  **unbounded in the number of submits**, since each new migration fanned out to the
  dead address adds another 45 retained clones that are never freed.
- The withdrawn self-amplification premise (item 7b-REFUTED) is restated as a marked
  correction, and nothing in the issue now depends on it. The words "self-amplif*" do
  not appear except inside that correction.

### Exploitability and real-world impact

Paths A and B need no attacker. Path A is the shipped configuration and its cost is
measured by the project itself on the sibling component; its user-visible effect is
reduced migration throughput and more `UNAVAILABLE` on `GetTransaction`, on a budget
the shim's own test says has roughly 3x headroom. Path B needs a `ZIS_HUB_NYM` entry
whose gateway node is offline — a state the design explicitly plans for (the failover
list) and which the hub's own connect-failure fallback tends to produce — and its cost
is permanent until the shim is redeployed, which for an immutable attested enclave is a
~25-minute operator action per shim.

### Severity: why Medium

- Not High: no confidentiality breach, no attacker required, and the permanent case
  needs a specific (though designed-for and realistic) configuration state. The
  everyday cost is roughly a doubling of packets, which the system absorbs.
- Not Low or Info: it is a genuine unbounded-resource defect on the single serialised
  resource that carries every diverted migration, in a component that has already told
  the wallet the migration succeeded, with no monitoring that can see it, and the
  project fixed the identical problem on the other binary — so the omission is a
  deviation from its own established practice rather than an accepted residual.
- Not double-counted: the two confirmed flood issues own the *attacker-driven*
  starvation of the shim's egress; this issue owns the *no-attacker* and *permanent*
  cases and the missing cap, and Path C is noted only as an interaction, not as its own
  harm.

### False-positive checks applied

- *§1 Assumption an attacker cannot violate?* Not applicable; Paths A and B need no
  attacker.
- *§4 Test/debug code?* Checked and inverted: the shim's only `debug_config` is
  correctly gated out of production — that is precisely why the production path has no
  knob.
- *§5 Impractical resource exhaustion?* No resources are demanded of anyone; the drain
  is self-inflicted.
- *§6 Intentional design?* No. The hub's branch, the deploy plumbing and the shipped
  `HUB_ACK_WAIT_MS` show the project treats this as a defect worth fixing; the shim was
  simply not given the same treatment.
- *§9 Obviously broken functionality?* No — Path A degrades rather than breaks, and
  Path B needs a stale address plus a dead gateway node.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
