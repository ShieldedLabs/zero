# A five-byte unauthenticated request buys 45 Sphinx packets of a shim's entire mixnet egress, so at about one byte per second an anonymous attacker holds any chosen operator's divert pipeline permanently full — which breaks every wallet's `GetTransaction` through that shim, spends the design's `MAX_DELIVERY_LAG` budget, can convert admitted migrations into silent `expiry_too_tight` refusals, and removes the one stated mitigating factor of the driver-teardown loss

**Severity**: High
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/shim/src/proxy.rs:70` (`SEND_TRANSACTION`), `:743-782` (`route_for`, path-only, no method check, no authentication); `audit-target/zeronym/shim/src/intercept.rs:94-132` (`send_transaction`), `:495-568` (`inspect`, and specifically the `RawTransaction::decode` `Ok` arm at `:559-568`), `:137-215` (`divert`), `:481-490` (`Inspection::treat_as_migration`); `audit-target/zeronym/shim/src/classify.rs:99-101` (`Class::treat_as_migration`), `:332` and `:337` (the project's own unit tests that empty and garbage input are `Unparseable`); `audit-target/zeronym/shim/src/wire.rs:273-286` (`encode_submit` always produces `FRAME_BYTES`); `audit-target/zeronym/shim/src/nym.rs:595-690` (`NymHandle::submit`), `:1085-1115` (the crate's own emission arithmetic); `audit-target/zeronym/shim/src/main.rs:335-336` (channel capacities 32 and 8); `audit-target/zeronym/shim/src/nym_driver.rs:253-272` (one send in flight), `:113-125` (the throttled rate); `audit-target/zeronym/shim/src/hub.rs:231-240` (the anchor: `Submit::Accepted` at hand-off); `audit-target/zeronym/hub/src/queue.rs:199-204`, `:380-393` (`survives_next_flush` / `ExpiryTooTight`); `audit-target/zeronym/hub/src/batcher.rs:46-55` (`MAX_DELIVERY_LAG`, `MIN_WALLET_EXPIRY`), `:93-117` (`BatchParams::validate`); `audit-target/zeronym/hub/src/nym_driver.rs:187-201` (the measured enclave retransmission behaviour)
**Found by agent:** Global (focus area G7, "loss of a wallet-acknowledged migration"); validated 2026-08-18
**In scope of audit?** Yes

## Description

The shim's wallet-facing listener is unauthenticated and internet-reachable
(`caution.hcl.tmpl` declares `ingress 0.0.0.0/0`), and `route_for` is a pure
function of the request path with no method check
(`proxy.rs:743-748`). Anything posted to
`/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction` therefore reaches
`intercept::send_transaction`.

Three facts, each individually intended, compose into a very cheap amplifier on
the one resource the whole divert path is bottlenecked on:

1. **Unparseable input is diverted, by design.** `inspect` decodes the gRPC frame
   and the `RawTransaction` protobuf; if that succeeds it hands `raw.data` to the
   classifier and returns `(Inspection::Classified(evidence), Some(Bytes::from(raw.data)))`
   (`intercept.rs:559-568`). `classify` returns `Class::Unparseable` for bytes
   that are not a transaction, and `Class::Unparseable.treat_as_migration()` is
   `true` on purpose (`classify.rs:99-101`). The crate's own unit tests pin
   exactly the attacker's payload: `assert_eq!(classify(&[]), Class::Unparseable)`
   (`classify.rs:332`) and `assert_eq!(classify(&[0xff; 64]), Class::Unparseable)`
   (`classify.rs:337`). So an *empty* `RawTransaction` is diverted, and `divert`'s
   fail-closed arm does not catch it, because that arm only fires when `tx_data`
   is `None` (`intercept.rs:146-155`) and here it is `Some(<empty>)`.

2. **Every diverted payload becomes a full fixed-size frame, whatever its length.**
   `wire::encode_submit` allocates `vec![0u8; FRAME_BYTES]` and zero-pads
   (`wire.rs:280-285`). A zero-byte transaction and a 65,503-byte transaction cost
   the mixnet exactly the same: 65,536 bytes.

3. **The mixnet egress is a single, serialized, severely rate-limited resource.**
   The driver holds **one send in flight at a time** (`nym_driver.rs:253-272`) at
   the rate a real gateway's backpressure imposes. The crate computes this itself
   in `shim/src/nym.rs:1085-1115`: `THROTTLED_PACKETS_PER_SEC = 1000.0 / 120.0`
   (8.33 packets/s), `PACKET_BYTES = 2 KiB`, so one submit is
   `packets(FRAME_BYTES) + SUBMIT_REPLY_SURBS = 32 + 13 = 45` packets ≈ **5.4 s of
   the shim's entire outbound mixnet capacity**. With the two-address failover
   list the design assumes (`nym.rs:602-641`, submit goes to *every* address) it
   is ~10.8 s.

The attacker's request body is five bytes — `00 00 00 00 00`, a gRPC frame with
a declared length of zero wrapping a default-valued `RawTransaction`. Five bytes
in; 45 Sphinx packets and 5.4 seconds of a non-substitutable shared resource out.

### Why this is a *loss* finding and not just a throughput one

`NymHandle::submit` returns `Ok(())` — and therefore the wallet is answered
`error_code 0` with a txid (`hub.rs:231-240`, `intercept.rs:180-202`) — the
moment the request is accepted into the shim's in-process `requests` channel
(`nym.rs:660-661`), whose capacity is 32 (`main.rs:335`). Behind it sit the
8-slot `out_frames` channel (`main.rs:336`, from which `correlate`'s reserved
permit is taken rather than added) and one in-flight send: **~41 frames of
acknowledged-but-unemitted work**.

At the crate's own throttled rate that is **~3.7 minutes** (single hub address)
or ~7.4 minutes (two). That figure is a floor rather than a ceiling: behind the
driver's hand-off sits the SDK's own *unbounded* transmission buffer
(`nym_driver.rs:418-432`), so to the extent the SDK accepts faster than it
emits, the backlog accumulates there with no bound and no visibility.

At the rate the project *measured from a deployed enclave* it is far worse: `hub/src/nym_driver.rs:187-201` records that every
deployed enclave's mixnet client produced **15-25 duplicate fragments** per
32-packet message, "which is how a 32-packet reply that should take ~5 s took
45-90 s from every enclave and timed out". The hub was given
`ZIH_ACK_WAIT_ADDITION_MS` to fix that; **the shim has no equivalent knob and
sets no `DebugConfig` at all in a production build** (`nym_driver.rs:104-157`),
and its submit frame is *larger* than the reply that was measured. On those
numbers a full pipeline is **31 to 62 minutes** of acknowledged, unemitted
migrations — longer than the entire admission window in (a) below.

Three distinct harms follow. (c) is unconditional; between (a) and (b) the
attacker chooses, and can cause both:

**(a) Silent conversion into `expiry_too_tight`** — conditional at the crate's
optimistic emission model, certain at the rate the project measured from
deployed enclaves; see the validation section for the split. The hub admits a migration
only if it provably survives the next flush: `expiry >= next_flush_height(tip, 20) + 4`
(`queue.rs:380-393`), i.e. the transaction must reach the hub while the tip is
still 5 to 24 blocks below its expiry height. A librustzcash-default wallet sets
`expiry = build_tip + 40` (`batcher.rs:50-55` names exactly this default), so the
migration has a **16-to-35-block (20-44 minute) window** from being built to
being admitted. The design budgets 6 blocks (7.5 min) of that for delivery —
`MAX_DELIVERY_LAG` (`batcher.rs:46-48`, "wallet-to-shim lag, Nym round trips,
acknowledgement retries and hub failover"), and `BatchParams::validate` asserts
`20 + 4 + 6 <= 40` at startup as though delivery lag were a constant
(`batcher.rs:93-117`). **It is not a constant. It is an attacker-controlled
quantity**: about a byte per second of attacker traffic against a public
endpoint consumes roughly half of that budget at the crate's own optimistic rate
(a ~3.7 min pipeline against a 7.5 min allowance), and all of it and the whole
20-44 minute admission window at the 45-90 s per-frame rate measured from
deployed enclaves. When the frame finally arrives
the hub answers `Refusal::ExpiryTooTight` — into an `AckV1` that the shim
constructs a receiver for and immediately drops (`nym.rs:652`), so nothing
surfaces. The wallet was told success tens of minutes earlier.

**(b) Removing the stated mitigation of the driver-teardown loss.** The already
filed `shim-nym-driver-every-teardown-path-silently-destroys-acknowledged-submits.md`
records as its principal *limiting* factor: *"at the documented mainnet rate of
~0.77 Orchard-touching transactions per block the pipeline is usually empty, so a
randomly-timed restart usually destroys nothing."* This flood makes the pipeline
**permanently full**, at will, from anywhere. Every ordinary redeploy, every
SIGTERM, every SDK client death, every gateway blip that triggers a rebuild —
each of which already discards everything in flight with no drain and no
accounting — now destroys up to ~41 migrations that wallets were told had
succeeded, instead of zero. An anonymous third party creates the condition; an
entirely routine operator action pulls the trigger.

**(c) A certain, unconditional harm: every wallet's transaction lookups through
that shim stop working.** With a hub configured the shim routes EVERY
`GetTransaction` to the hub and none to the operator (`intercept.rs:237-247`),
and a lookup travels through the SAME `requests` channel as a submit
(`nym.rs:695-713` — "the request channel carries one type, so it travels in the
same buffer"). One deadline covers both acceptance and reply
(`nym.rs:756-772`) and it is `REQUEST_TIMEOUT = 90 s` (`nym.rs:71`). A
3.7-minute emission backlog exceeds 90 s, so every lookup times out, sweeps its
address list, and fails closed with `UNAVAILABLE`. This depends on no assumption
about wallet expiry, enclave retransmission, or operator restarts: it follows
from the pipeline depth and the timeout alone, at the same ~1 byte/s.

**Loud versus silent is the attacker's choice, not automatic.** When the
`requests` channel is exactly full, a genuine submit blocks and then fails closed
after `SUBMIT_DISPATCH_TIMEOUT = 5 s` (`nym.rs:80`, `:660-678`) — the wallet sees
`UNAVAILABLE`, which is the safe outcome already described by
`gettransaction-flood-starves-migration-diversion.md`. The *silent* variant needs
the attacker to pace injections so a slot is free when the victim arrives: the
victim is then accepted, told `error_code 0`, and left to rot behind the junk.
That variant is strictly better for an attacker and is the new behaviour here.

> **Correction, 2026-08-18, from the sibling validation of
> `gettransaction-flood-starves-migration-diversion.md`.** The silent variant does
> **not** in fact require careful pacing, and is the *default* outcome rather than
> the harder one. `send_message` returns when the SDK **accepts** the message, not
> when it is emitted: capacity-1 `InputMessage` channel
> (`client-core/base_client/mod.rs:1013`) → 8-slot batch channel
> (`real_messages_control/mod.rs:150`) → `poll_poisson`, which stores one whole
> message and emits **one packet** per Poisson tick
> (`real_traffic_stream.rs:443-478`) into an uncapped `TransmissionBuffer`. So the
> 32-slot `requests` channel drains at ~8-50 messages/s — far faster than the
> 0.19 submits/s the transport emits — and is usually **not** full. A victim
> arriving during a sustained flood is therefore normally *accepted*, told
> `error_code 0`, and left to rot; the loud `UNAVAILABLE` outcome is the one that
> needs the most attacker concurrency (of order `90 × drain rate` concurrent live
> requests), not the least. This confirms the "41 is a floor, not a ceiling" note
> in Correction 1 below and resolves it: the backlog does accumulate in the SDK's
> unbounded buffer, so the acknowledged-but-unemitted depth is bounded by nothing
> the shim owns. The arithmetic is derived in full in the sibling issue.

## Attack Scenario and Steps

1. The attacker picks a target: any operator running a zeronym shim. The endpoint
   is a public DNS name serving wallets (`ZIS_TLS_DOMAIN`; `deploy.env.example`
   uses `shieldedinfra.net`), and there is no authentication of any kind on it.
2. The attacker opens one HTTP/2 connection and issues
   `POST /cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction` with the
   five-byte body `00 00 00 00 00` and no `grpc-encoding` header.
3. `route_for` returns `Route::Intercept`; `inspect` reads `declared = 0`,
   `message = &frame[5..5]`, `RawTransaction::decode(&[])` succeeds with
   `data = vec![]`; `classify_with_evidence(&[])` returns `Class::Unparseable`;
   `treat_as_migration()` is true; `divert` is entered with `tx_data = Some(<empty>)`.
4. `NymHandle::submit` mints a nonce, `wire::encode_submit` produces a 65,536-byte
   frame, the request is accepted into `requests`, and the attacker is answered
   `error_code 0`.
5. The attacker repeats until ~41 frames are outstanding, then sends one more every
   ~5.4 s (or ~45-90 s at the measured enclave rate) to hold the pipeline full.
   Sustained cost: **five bytes per five seconds**, roughly one byte per second.
6. Every genuine migration diverted through that shim from then on is answered
   `error_code 0` and then queued behind the junk. It is destroyed if
   (a) it reaches the hub after its admission window has closed
   (`Refusal::ExpiryTooTight`, ack discarded), or
   (b) the driver tears down for any reason while it is still queued
   (`Step::Stop` / `Step::Rebuild` / `Step::Died`, all of which discard without a
   drain).
7. The attacker can raise the odds of (b) directly by also blackholing or simply
   flooding the enclave's gateway path, or — if the attacker is the operator —
   by restarting the enclave, which is a fully deniable action.

**Attack Requirements and Assumptions:**

- **Access needed:** the ability to make TCP connections to a shim's public
  wallet-facing listener. No credentials, no wallet, no Zcash funds, no Nym
  client, no knowledge of the hub's address, no on-chain activity.
- **Cost:** ~1 byte/second sustained per targeted shim. This is the cheapest
  migration-destruction lever found in this audit; the previously filed hub queue
  fill requires the attacker to push 64 MiB per 25-minute epoch through their own
  throttled Nym clients, and the `GetTransaction` flood requires sustained
  concurrency.
- **What makes it realistic:** the payload is not a corner case the developers
  overlooked — the shim's own unit tests assert that exactly these bytes take the
  divert path, because failing safe toward diversion is the correct privacy
  decision. The amplifier is the *consequence* of that correct decision meeting a
  fixed-size frame and a serialized 8-packet-per-second transport.
- **What bounds it:** harm (a) requires a wallet expiry near the librustzcash
  40-block default. A ZIP 318 migration carries a 30-to-60-*day* expiry
  (`audit-state/SPEC-NOTES.md` §3) and cannot be pushed out of its admission
  window this way, so for the acute use case only harm (b) applies. **The clause
  that followed here — *"for ZIP 318 traffic harm (b) is worse, because the
  wallet's recovery clock is then 30-60 days"* — was REFUTED 2026-08-18 and is
  struck; it had the polarity backwards.** 30-60 days is the wallet's automatic-
  **retry horizon**, so ZIP 318 traffic recovers from harm (b) *better* than the
  ZIP 203-default traffic this shim also diverts, whose horizon is ~50 minutes.
  What makes harm (b) severe here is that a *sustained* flood defeats retries of
  either class — which is exactly this issue's mechanism (~1 byte/s to hold the
  pipeline full). See
  `issues/invalid/zip318-canonical-expiry-is-the-only-recovery-clock-and-a-lost-migration-freezes-the-users-notes-for-30-to-60-days.md`.
- The attack does not need the operator's cooperation, but the operator is
  strictly better placed to run it and to trigger (b) at a moment of their
  choosing.

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

A user's wallet is told the mandatory Orchard migration was broadcast and is
handed a txid it displays and records. The transaction reaches no mempool.
Nothing anywhere retains it: the shim keeps no per-migration state by design
(`lib.rs:30-35`), the hub either never received it or refused it and holds no
copy, confirmation tracking is "designed, no code yet", and the ack that carries
the refusal is discarded at construction.

Because the shim's `SendTransaction` path fails *open* (success) while its
`GetTransaction` path fails *closed* (`UNAVAILABLE`), the user's experience
during an attack is precisely inverted from the truth: sends appear to work and
lookups appear broken.

The funds are in a pool NU6.3 has closed to new value, the migration is the
user's only route out, and the wallet believes it is done — so the wallet will
not re-broadcast, and will hold the notes as pending-spent until the transaction
expires.

## Technical Details / Code Analysis

**The routing predicate admits anyone.** `shim/src/proxy.rs:743-748`:

```rust
pub fn route_for(path: &str) -> Route {
    if path == SEND_TRANSACTION {
        return Route::Intercept;
    }
```

There is no method check (deliberately — rule 3), no authentication, and no rate
limit anywhere between the socket and `intercept::send_transaction`.

**An empty `RawTransaction` is diverted with `Some` bytes.**
`shim/src/intercept.rs:559-568`:

```rust
    match RawTransaction::decode(message) {
        // `data` is the serialized Zcash transaction: the only value the
        // classifier ever sees, and the exact bytes the hub broadcasts.
        Ok(raw) => {
            let evidence = classify_with_evidence(&raw.data);
            (
                Inspection::Classified(evidence),
                Some(Bytes::from(raw.data)),
            )
        }
```

and `shim/src/classify.rs:99-101`:

```rust
    pub fn treat_as_migration(self) -> bool {
        matches!(self, Class::Migration | Class::Unparseable)
    }
```

with the crate's own test at `shim/src/classify.rs:332`:

```rust
        assert_eq!(classify(&[]), Class::Unparseable);
```

`divert`'s fail-closed guard is `let Some(tx_data) = tx_data else { ... }`
(`intercept.rs:146-155`) — `Some(<empty>)` passes it.

**The frame is fixed-size regardless.** `shim/src/wire.rs:273-286`:

```rust
pub fn encode_submit(nonce: &Nonce, tx: &[u8]) -> Result<Zeroizing<Vec<u8>>, WireError> {
    if tx.len() > MAX_NYM_TX_BYTES { ... }
    let mut frame = Zeroizing::new(vec![0u8; FRAME_BYTES]);
    frame[0..4].copy_from_slice(&SUBMIT_MAGIC);
    frame[4..20].copy_from_slice(nonce);
    frame[20..24].copy_from_slice(&(tx.len() as u32).to_be_bytes());
    frame[SUBMIT_HEADER_BYTES..SUBMIT_HEADER_BYTES + tx.len()].copy_from_slice(tx);
    Ok(frame)
}
```

**The crate itself computes the cost of that frame.**
`shim/src/nym.rs:1087-1115`:

```rust
    const PACKET_BYTES: usize = 2 * 1024;
    const THROTTLED_PACKETS_PER_SEC: f64 = 1000.0 / 120.0;
    ...
    fn a_submit_fits_its_dispatch_budget_at_the_throttled_rate() {
        let on_wire = packets(wire::FRAME_BYTES) + SUBMIT_REPLY_SURBS as usize;
        let secs = seconds_to_emit(on_wire);
        assert!(secs < 30.0, ...);
    }
```

`packets(65536) = 32`, `+ 13 = 45`, `45 / 8.33 = 5.4 s`. The test asserts only
that this is "not absurd"; nothing anywhere asserts that the resource cannot be
consumed by a party other than a wallet.

**The wallet is answered before anything leaves the process.**
`shim/src/nym.rs:660-689`:

```rust
            match tokio::time::timeout_at(deadline, self.requests.send(request)).await {
                Ok(Ok(())) => dispatched += 1,
                Ok(Err(_)) | Err(_) => break,
            }
        }
        if dispatched > 0 { Ok(()) } else { Err(NymError::TransportGone) }
```

and `shim/src/hub.rs:231-240`:

```rust
            HubTransport::Nym(handle) => match handle.submit(tx_bytes).await {
                // ... There is no Refused arm: the hub's verdict is a full round trip
                // away and is deliberately not waited for ... so a refusal is never
                // surfaced here.
                Ok(()) => Ok(Submit::Accepted { txid: crate::nym::local_txid(tx_bytes) }),
```

**The pipeline that fills.** `shim/src/main.rs:335-336`:

```rust
    let (req_tx, req_rx) = mpsc::channel(32);
    let (out_tx, out_rx) = mpsc::channel(8);
```

**The hub's admission rule the delay defeats.** `hub/src/queue.rs:380-393`:

```rust
pub fn survives_next_flush(expiry: Option<u32>, tip: u32, flush_interval: u32, mining_margin: u32) -> bool {
    match expiry {
        None => true,
        Some(expiry) => {
            let deadline = next_flush_height(tip, flush_interval).saturating_add(mining_margin);
            expiry >= deadline
        }
    }
}
```

**The budget the design asserts as a constant.** `hub/src/batcher.rs:46-48` and
`:93-117`:

```rust
/// Blocks reserved for wallet-to-shim lag, Nym round trips (measured 9 to 10 s
/// unary), acknowledgement retries and hub failover.
pub const MAX_DELIVERY_LAG: u32 = 6;
```

```rust
    pub fn validate(&self) -> Result<(), BoxError> {
        let spent = self.flush_interval
            .saturating_add(self.mining_margin)
            .saturating_add(self.delivery_lag);
        if spent > self.min_wallet_expiry { ... }
```

Six blocks is 7.5 minutes. A full request pipeline is ~3.7 minutes at the
crate's own optimistic rate — half the allowance — and 31-62 minutes at the rate
the project measured from a deployed enclave, which exceeds the whole 20-44
minute admission window. The attacker sets the depth; the network sets which of
the two rates applies.

**The measured enclave behaviour the shim never received a fix for.**
`hub/src/nym_driver.rs:187-201`:

> *"Measured 2026-08-17: a local hub's replies reached a shim with ~1 duplicate
> fragment per lookup; every DEPLOYED hub's reached the same shim with 15-25 --
> and each duplicate is a full send slot at the throttled rate, which is how a
> 32-packet reply that should take ~5 s took 45-90 s from every enclave and timed
> out."*

The shim's `build_client` (`shim/src/nym_driver.rs:104-157`) applies no
`DebugConfig` in a production build, so its 45-packet submits carry the
unmitigated version of exactly this behaviour.

## Recommendations

1. **Do not spend a mixnet frame on a payload the shim knows is not a
   transaction.** `Class::Unparseable` must keep failing safe (never forwarded to
   the operator), but "fail safe" does not have to mean "spend 45 Sphinx packets".
   Refuse `Unparseable` bodies below a plausible transaction size — or all of them
   — with `UNAVAILABLE`/`INVALID_ARGUMENT`, which is fail-closed and costs the
   transport nothing. At minimum, refuse a zero-length `RawTransaction.data`
   outright; a wallet never sends one.
2. **Bound and account for the divert pipeline.** Publish the current depth of
   `requests` + `out_frames` on `/nym-status`, and refuse (fail closed, so the
   wallet retries) rather than accept once the depth exceeds what
   `MAX_DELIVERY_LAG` can absorb. Accepting work the transport cannot deliver
   inside the design's own budget is the defect; a visible `UNAVAILABLE` is
   strictly better for the user than a silent success.
3. **Rate-limit the wallet-facing `SendTransaction` path per source.** The shim
   already terminates TLS itself and sees the peer address; a token bucket at a
   few requests per minute per source refuses nothing a wallet does.
4. **Give the shim the `ack_wait_addition` fix the hub received.** The
   measurement at `hub/src/nym_driver.rs:187-201` applies verbatim to the shim's
   larger frames, and without it every real-world estimate in this issue should
   use the 45-90 s figure rather than the 5.4 s one.
5. **Surface the ack.** `intercept.rs:188` is the only line in the shim that can
   report a hub refusal to a wallet and it is unreachable on the deployed
   transport. Reading the `AckV1` — even asynchronously, into a counter on
   `/nym-status` — turns every silent loss in this issue into an observable one.
   The nonce, the waiter and the refusal codes all already exist.

## Validation Information

**Verdict: CONFIRMED. Severity: High (as filed).** This is the cheapest attack
found anywhere in the audit and every step of the mechanism was reproduced by
reading the code. Four corrections were applied to the body — the pipeline depth,
the certainty of harm (a), the loud-versus-silent choice, and the promotion of a
harm that is *certain* and was under-stated in the filing.

### The five-byte path, step by step

| Step | Verified at |
|---|---|
| The listener is internet-reachable and unauthenticated | `shim/deploy/caution/caution.hcl.tmpl` — `ingress { cidr_ipv4 = "0.0.0.0/0", port = 8083 }` behind a public TLS domain |
| Routing is a pure function of the path, no method check, no auth, no rate limit, no connection cap | `shim/src/proxy.rs:744-747`; `serve_connection` (`:575-609`) and `handle` (`:614-692`) contain no limiter of any kind, and hyper is configured only with window sizes |
| Body buffered under a 4 MiB cap | `shim/src/intercept.rs:101-104` |
| `frame.len() = 5 ≥ GRPC_PREFIX_LEN`, `frame[0] = 0`, `declared = 0` | `shim/src/intercept.rs:516-528` |
| `message = frame.get(5..5) = Some(&[])` and `message.len() == frame.len() − 5` | `shim/src/intercept.rs:529-551` |
| `RawTransaction::decode(&[])` succeeds — proto3 has no required fields, so prost returns the default message | `shim/src/intercept.rs:559-568` |
| `classify(&[]) == Class::Unparseable`, pinned by the crate's own test | `shim/src/classify.rs:330-332` |
| `Unparseable.treat_as_migration() == true`, on purpose | `shim/src/classify.rs:99-101` |
| `divert`'s fail-closed guard passes, because `tx_data` is `Some(<empty>)` not `None` | `shim/src/intercept.rs:146-155` |
| `encode_submit` allocates and pads to `FRAME_BYTES = 65,536` regardless of `tx.len()` | `shim/src/wire.rs:273-286` and `:72` (`FRAME_BYTES`) |
| The wallet (here, the attacker) is answered `error_code 0` at mixnet hand-off | `shim/src/hub.rs:231-240`, rendered at `shim/src/intercept.rs:180-205` |

**Five bytes really is the minimum that buys a frame**, which is worth stating
because it shows the boundary is exact rather than approximate: a *zero*-byte
body takes `frame.len() < GRPC_PREFIX_LEN` at `intercept.rs:516-521`, yields
`tx_data = None`, and hits the fail-closed arm at `:146-155`, spending no frame
at all. The 5-byte `00 00 00 00 00` is the smallest input that reaches
`NymHandle::submit`.

### The emission arithmetic, verified against `shim/src/nym.rs:1085-1115`

The crate's own `throughput_budget` module supplies every constant:
`PACKET_BYTES = 2 * 1024` (`:1092`), `THROTTLED_PACKETS_PER_SEC = 1000.0 / 120.0`
(`:1094`), `packets(bytes) = bytes.div_ceil(PACKET_BYTES)` (`:1096-1098`),
`SUBMIT_REPLY_SURBS = 13` (`:96`). So

    packets(65_536) = 32;  32 + 13 = 45;  45 / 8.333… = 5.4 s

per submit — **5.4 seconds of the shim's entire outbound mixnet capacity for
five attacker bytes**, ~13,000× amplification measured in bytes. The egress is
genuinely serialized and non-substitutable: the driver holds exactly one send in
flight (`shim/src/nym_driver.rs:258-272` and the guard at `:362-369`), and
`correlate` reserves capacity ahead of accepting each request
(`shim/src/nym.rs:853-880`).

Holding the pipeline full then costs one 5-byte request per 5.4 s ≈ **1 byte per
second** of payload (a few tens of bytes/s including HPACK-compressed HTTP/2
headers on a single reused connection).

### Correction 1 — the pipeline is ~41 frames, not 42

`requests` is `mpsc::channel(32)` and `out_frames` is `mpsc::channel(8)`
(`shim/src/main.rs:335-336`). The permit `correlate` holds
(`shim/src/nym.rs:853-863`) is taken *from* the 8, not in addition to it, so the
acknowledged-but-unemitted depth is 32 + 8 + 1 in flight = **~41 frames**, i.e.
~3.7 min at 5.4 s each. Immaterial to the conclusion; corrected for accuracy.

Note also, in the other direction, that 41 is a *floor*, not a ceiling. The
driver's send future completes when the **SDK accepts** the message, and the
project's own comment at `shim/src/nym_driver.rs:418-432` describes what sits
behind that acceptance: *"its one-slot input, an 8-deep batch channel, and an
**unbounded transmission buffer** drained at the throttled rate. There is no
drain-then-disconnect in the SDK. Frames in there may include SUBMITS ALREADY
ANSWERED SUCCESS to a wallet."* To the extent the SDK accepts faster than it
emits, the attacker's backlog accumulates there instead, with no bound and no
visibility — which makes both harms below worse, never better.

### Correction 2 — harm (a) is conditional at the optimistic rate, certain at the measured one

The admission window is as filed: `survives_next_flush` requires
`expiry >= next_flush_height(tip, 20) + 4` (`hub/src/queue.rs:380-393`), so a
librustzcash-default `expiry = build + 40` must reach the hub while the tip is
16 to 35 blocks past the build height — a 20-to-44-minute window.

At the crate's own throttled-rate model a full pipeline is **~3.7 minutes**.
That does not by itself exhaust a 20-44 minute window; what it does do is
consume about **half of `MAX_DELIVERY_LAG`** — the 6 blocks (7.5 min)
`batcher.rs:46-48` reserves for exactly this and that `BatchParams::validate`
(`:93-117`) asserts at startup as if it were a constant. So at the optimistic
rate harm (a) destroys migrations that were already within ~4 minutes of their
deadline, rather than all of them. The filing implied certainty; it is
conditional.

At the emission rate the project **measured from deployed enclaves** it is
certain. `hub/src/nym_driver.rs:187-201` records 15-25 duplicate fragments per
32-packet message from every deployed enclave, *"which is how a 32-packet reply
that should take ~5 s took 45-90 s from every enclave and timed out."* At
45-90 s per frame a full pipeline is **31 to 62 minutes**, which exceeds the
entire admission window. Two facts make this the right rate to plan against, and
one caveat keeps it honest:

- The mitigation the hub was given for it, `ZIH_ACK_WAIT_ADDITION_MS`
  (`hub/src/nym_driver.rs:198-217`), **has no shim equivalent**: verified by
  grep, the only `debug_config` call in the shim is behind
  `#[cfg(feature = "mixnet-localnet")]` (`shim/src/nym_driver.rs:126-140`), so a
  production shim applies no `DebugConfig` at all.
- The mechanism — a fixed retransmission timer against a slow or lossy enclave
  ack path — is a property of the enclave's network path, and the shim's submits
  are the same 32 packets from the same platform.
- *Caveat:* the measurement is of the hub's **replies**, not of the shim's
  sends. Nobody has measured the shim's direction. The issue should not be read
  as claiming they have.

### Correction 3 — the strongest harm is certain and was under-stated

Independent of wallet expiry and of enclave retransmission behaviour, holding
the pipeline full **breaks every wallet's transaction lookups through that
shim**:

- With a hub configured, the shim routes **every** `GetTransaction` to the hub
  and none to the operator (`shim/src/intercept.rs:237-247`).
- A lookup travels through the **same** `requests` channel as a submit
  (`shim/src/nym.rs:695-713`, "the request channel carries one type, so it
  travels in the same buffer").
- One deadline covers both acceptance and reply (`shim/src/nym.rs:756-772`), and
  it is `REQUEST_TIMEOUT = 90 s` (`:71`).

A 3.7-minute emission backlog exceeds 90 s, so the lookup times out whether or
not it is accepted, `each_target` exhausts its addresses, and the shim fails
closed with `UNAVAILABLE`. This costs the attacker the same ~1 byte/s, requires
no assumption at all, and degrades ordinary wallet function for every user of
the targeted shim for as long as the attacker cares to pay.

### Correction 4 — loud versus silent is a choice, not automatic

When the `requests` channel is *exactly* full, a genuine submit blocks and then
fails closed after `SUBMIT_DISPATCH_TIMEOUT = 5 s` (`shim/src/nym.rs:80`,
`:660-678`), which the wallet sees as `UNAVAILABLE` — loud, and safe. The silent
variant needs the attacker to pace injections so a slot is free when the victim
arrives, so the victim is *accepted* (told `error_code 0`) and then queued behind
~41 junk frames. Both are available at the same cost and the attacker picks; the
filing read as though the silent variant were automatic.

### `AVOIDING-FALSE-POSITIVES.md` §5 applied rigorously

§5's own statement of the real-vulnerability shape is *"amplification attacks
where small input causes disproportionate resource use"*, and its contrasting
"Real Issue" lines are *"1 KB request causing 1 GB memory allocation"* and
*"single connection consuming unbounded resources"*. This is that shape, at an
unusually extreme ratio: **5 bytes in, 65,536 bytes and 5.4 s of a serialized
~8-packet-per-second resource out**, sustained at ~1 byte/s.

*What resources would the attacker need?* A single TCP connection and about a
byte per second. No credential, no wallet, no Zcash funds, no Nym client, no
knowledge of the hub's address, no on-chain activity.

*What would stop them?* Nothing in the target and nothing in the deployment.
There is no authentication, no rate limit, no per-source accounting and no
connection cap on the shim's wallet-facing listener; the platform terminates TLS
and forwards, and no limiter is configured anywhere in `caution.hcl.tmpl`. The
one structural bound is the pipeline depth itself, which converts the attack
from unbounded delay into ~3.7 min (optimistic) or ~31-62 min (measured-enclave)
of delay — and both of those are enough for the harms above.

*And the inversion this target creates.* The guide would normally cap a
throughput attack at Medium. It cannot here, because the shim answers
`error_code 0` at an in-process channel send (`shim/src/hub.rs:231-240`) — so the
"denial" is not visible as a denial. It is a migration the user was told had
succeeded, which the shim does not retain (stateless by design,
`shim/src/lib.rs:32-34`) and which may never reach a mempool. The resource cost
is bytes per second; the damage is silent destruction of transactions the user
believes are spent.

### The amplifier is the fail-safe working correctly — do not "fix" the classifier

`Class::Unparseable => treat_as_migration() == true` is right and must stay
right: diverting a body the shim could not read is what prevents leaking one it
could not read. The defect is not the fail-safe; it is that the fail-safe spends
a full fixed-size mixnet frame on a payload the shim already knows is not a
transaction. Recommendation 1 in this issue (refuse a zero-length
`RawTransaction.data`, or `Unparseable` bodies below a plausible transaction
size, with a gRPC error) is fail-*closed* and costs the transport nothing, so it
does not weaken the privacy property at all.

### Severity justification — High

*Impact:* for a targeted operator, the privacy-critical divert path and the
whole `GetTransaction` path are disabled, and the `SendTransaction` failure mode
is a **false success** — the user's wallet records a txid for a transaction
that may reach no mempool, in a pool NU6.3/ZIP 258 has closed to new value.
Because the pipeline is held permanently full, every ordinary operator action
that tears the driver down (redeploy, SIGTERM, SDK death, gateway churn) now
discards up to ~41 acknowledged migrations instead of ~0 — a condition an
anonymous third party creates and a routine operator action triggers.

*Likelihood:* an unauthenticated request to a public DNS name, at ~1 byte/s,
against any operator the attacker chooses, with no detection surface (the shim
publishes no queue depth; `/nym-status` is client-lifecycle only, and
`/nym-diag`'s `sends_dispatched` counts SDK acceptance, so it over-counts in
exactly the loss cases).

*Why not Critical:* no funds are stolen and no key is compromised; the loss
requires either a wallet expiry near the librustzcash 40-block default or a
teardown to coincide; and ZIP 318 migrations — the acute use case — carry a
30-to-60-day expiry and **cannot** be pushed out of their admission window this
way, so for them only the teardown harm and the lookup outage apply.

*Why not Medium:* the cost is ~1 byte/s from anywhere on the internet with no
credential, the target is chosen by the attacker, and the primary failure mode
is a false `error_code 0` rather than a visible outage.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
