# The hub buffers an indexer response with no size ceiling, so a ~100-byte unauthenticated lookup buys a multi-megabyte allocation it then throws away

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/chain.rs:373-377` (`round_trip`, the unbounded `collect()`) and `:415-430` (`unframe`, whose length check runs *after* the bytes are resident), reached from `:221-266` (`get_transaction`), `:176-198` (`broadcast`) and `:155-173` (`tip_height`). Consumers: `audit-target/zeronym/hub/src/server.rs:296-322` (`Hub::lookup`), `:487-505` (the HTTP handler), `:508-517` (`found`, which copies the body again into the response), `audit-target/zeronym/hub/src/nym.rs:249-303` (`build_lookup_reply`, which **discards** anything over ~64 KiB after buffering it). Enclave size: `audit-target/zeronym/hub/deploy/caution/caution.hcl.tmpl:39-40`. Contrast: every *inbound* body in the same crate is wrapped in `http_body_util::Limited` (`server.rs:488`, `:523`).
**Found by agent:** Local (file audit of `hub/src/chain.rs`); validated 2026-08-18
**In scope of audit?** Yes

## Description

`chain.rs` is the hub's only outbound network client, and every reply it reads
comes from a party the engagement's threat model designates untrusted:

> `hub → indexer`: sees the whole batch seconds before it is public; **can lie
> about the tip and about publish verdicts**
> — `audit-context/AUDIT-INSTRUCTIONS.md`, "Trust boundaries"

`round_trip` reads that reply with an unbounded `collect()`
(`chain.rs:373-377`):

```rust
    let collected = tokio::time::timeout(RPC_TIMEOUT, response.into_body().collect())
        .await
        .map_err(|_| -> BoxError { "reading the gRPC response timed out".into() })??;
    let trailers = collected.trailers().cloned();
    let body = collected.to_bytes();
```

There is no ceiling of any kind on how many bytes are accumulated: no
`Limited`, no `content-length` check, and no cap derived from the gRPC frame
header. `unframe` does check the declared length (`chain.rs:424-428`), but only
*after* the entire body is already resident, so that check cannot bound the
allocation. HTTP/2 flow control does not bound the total either: `collect()`
consumes eagerly, which releases receive-window capacity as fast as it arrives,
so the window governs bytes *in flight*, not bytes *accumulated*, and the peer
may keep sending for the whole 10 s `RPC_TIMEOUT`.

**Two facts make this a defect rather than a design choice.**

First, it is a gap specific to this file, not a house style. The same crate
applies `http_body_util::Limited` to *every* body it accepts from an untrusted
peer — `server.rs:488` (`MAX_LOOKUP_BYTES`, 64 bytes) and `server.rs:523`
(`MAX_TX_BYTES`, 64 KiB) — and `queue.rs:61` caps a stored transaction at
64 KiB. The indexer's response is the one untrusted input read without a limit.

Second, **the hub can never use more than ~64 KiB of it.** `nym.rs:295-302`
says so in as many words:

> The reply budget is nine bytes under the submit cap, so **an indexer can return
> a transaction that fits nowhere in a reply frame.** Fail closed rather than
> truncate.

So on the mixnet lookup arm the hub buffers a multi-megabyte body, decodes it,
discovers it will not fit a `LookupReplyV1` frame, and answers `error`. The
memory is spent to produce a refusal. The possibility of an oversized answer is
understood one file away; what is missing is the bound that would stop it being
buffered.

## Attack Scenario and Steps

Two paths reach it, and they have very different reachability. The distinction
matters for severity and is stated up front.

**Path A — an unauthenticated internet client and an entirely honest indexer.
This is the one that makes the finding.**

1. `POST /transaction` is served unconditionally — it is *not* behind
   `--http-submit` (`server.rs:442-445`) — on an enclave whose domain the
   in-enclave Caddy maps onto port 8083 from `0.0.0.0/0`
   (`caution.hcl.tmpl:51-55`, `:97-105`), with no ACL, no authentication and no
   rate limit. The same core is also reachable over the mixnet, whose address
   `GET /nym-address` publishes to anyone (`server.rs:446-449`).
2. The attacker picks the txid of a large Zcash mainnet transaction. This is
   public data. A transaction must fit in a block, and
   `MAX_BLOCK_BYTES = 2_000_000` (`zebra-chain/src/block/serialize.rs:24`), so a
   single transaction can be ~2 MB — **31× the hub's own `MAX_TX_BYTES` and
   31× the largest reply a Nym lookup can carry.** If no suitable transaction
   already exists on chain, the attacker can mint one once, for a few dollars of
   ZIP 317 fees, and reuse its txid forever.
3. The attacker sends `POST /transaction` carrying that hash — ~100 bytes.
4. Every request misses the queue (`server.rs:297`; it is not a diverted
   migration) and falls through to `chain.get_transaction()`, which fetches the
   full ~2 MB transaction from the honest indexer, once per configured endpoint.
5. Each in-flight lookup materialises the payload several times over: the
   collected body, the contiguous `to_bytes()` copy (`chain.rs:377`), the
   `RawTransaction.data` `Vec<u8>` prost decodes into (`chain.rs:429`, carried to
   `TxLookup::Found` at `:233-236`), and finally
   `Bytes::copy_from_slice(tx_bytes)` when the HTTP response is built
   (`server.rs:509`). Call it 4–6 MB transiently per concurrent lookup, of which
   ~2 MB is *retained* in the response until it has been written to the client.
6. `serve` spawns one task per accepted connection with **no concurrency
   semaphore** (`server.rs:354-408`), so the multiplier is limited only by the
   attacker's connection count. That missing bound is the separate confirmed
   issue `hub-http-lookup-path-has-no-concurrency-bound.md`; the defect owned
   *here* is the per-response byte ceiling, which is missing independently of it
   and which would bound the damage under any concurrency limit eventually
   chosen.
7. There is **no response-write timeout** anywhere on this path: the
   `TokioTimer` installed at `server.rs:397-398` enables hyper's *header-read*
   timeout only. A client that stops reading therefore parks its ~2 MB response
   buffer in the hub indefinitely, and the in-enclave Caddy streams rather than
   fully buffering, so the back-pressure lands on the hub.

Even the **bounded** mixnet arm is uncomfortable: 64 concurrent lookups
(`nym.rs:54`) × ~2 MB fetched and then discarded is ~128 MB of churn on top of a
64 MiB queue and whatever the HTTP arm is doing, on a 2 GB enclave.

**Path B — a hostile or compromised indexer.** The indexer answers any of the
three calls (`GetLightdInfo`, `SendTransaction`, `GetTransaction`) with an
arbitrarily large body inside the 10 s window. `GetLightdInfo` needs no attacker
action at all: `batcher.rs:290` polls it every 30 seconds. This is the only path
on which the response is *truly* unbounded. It requires control of a configured
`ZIH_INDEXERS` endpoint, which the audit treats as a hub-trust/robustness defect
rather than an internet-reachable weapon, and such a party already holds cheaper
levers (they receive every batch in plaintext). It is recorded because the
indexer is frequently a *third party* the hub operator does not control, and
because it is what makes "no ceiling" more than an aesthetic complaint.

**Attack Requirements and Assumptions:**
- Path A needs only network reach to the hub's published address and one public
  txid. No shim, no mixnet position, no credential, no funds beyond an optional
  one-off fee.
- Path B needs the configured indexer to be hostile or compromised — a party the
  threat model already treats as untrusted and able to lie.
- What makes Path A realistic: `POST /transaction` is reachable by design (it is
  how the shim answers `GetTransaction`), the ingress is `0.0.0.0/0`, there is
  no rate limit anywhere on it, and the in-enclave Caddy imposes none either.
- What limits it: with an honest indexer the per-response size is bounded by
  consensus at ~2 MB, so reaching a 2 GB enclave's ceiling needs several hundred
  concurrent lookups *and* an indexer willing and able to serve several hundred
  megabytes concurrently. Neither the enclave's `RLIMIT_NOFILE` nor the
  indexer's throughput could be measured in this environment.

## Impact on Users

Killing the hub process is not a restart, it is data loss. The queue is
deliberately RAM-only and the enclave is diskless
(`caution.hcl.tmpl:34-38`: "The queue is deliberately NOT persisted"), and
`server.rs:371-374` records exactly what that costs:

> the RAM-only queue -- every migration already acked to a wallet and waiting for
> the next flush -- went with it.

Every wallet whose migration is in the queue was already told `error_code 0` at
mixnet hand-off (`shim/src/hub.rs:231-240`, dispatch-only submit), and the shim
keeps no copy. An OOM therefore silently destroys migrations their owners
believe are on their way to the network, with no retry anywhere in the system.
An OOM is a `SIGKILL`, so even the hub's own `unpublished … they are lost`
accounting (`batcher.rs:325-331`) does not run.

Short of an OOM, the same requests are a straightforward memory- and
bandwidth-amplifier against an enclave that has 2 GB for everything: ~100 bytes
in, up to ~2 MB allocated and up to ~2 MB pulled across the indexer link that
the *operator* pays for.

Beyond the loss, an on-demand hub kill is a *privacy* primitive: with the hub
down, every shim in the fleet walks its retry/failover ladder simultaneously and
the whole fleet is unprotected in a window the attacker chose. A restart also
mints a fresh Nym identity (`nym_driver.rs`, `Ephemeral::default()` in a
diskless enclave), which is the terminal state of the confirmed
`hub-nym-driver-automatic-fresh-identity-permanently-invalidates-every-shim.md`.

## Technical Details / Code Analysis

The read path, in full (`hub/src/chain.rs:344-402`, abridged to the relevant
lines):

```rust
async fn round_trip<IO>(stream: IO, request: hyper::Request<Full<Bytes>>) -> Result<Bytes, BoxError>
{
    let (mut sender, conn) = http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await?;
    ...
    let collected = tokio::time::timeout(RPC_TIMEOUT, response.into_body().collect())
        .await
        .map_err(|_| -> BoxError { "reading the gRPC response timed out".into() })??;
    let trailers = collected.trailers().cloned();
    let body = collected.to_bytes();
```

`http2::Builder::new(...)` is used with defaults; hyper's HTTP/2 client exposes
no maximum response body size, and no window setting is applied here — but no
window setting would help, because a window bounds bytes in flight and
`collect()` releases it continuously.

The bound that does exist is applied too late (`hub/src/chain.rs:415-430`):

```rust
fn unframe<M: Message + Default>(body: &[u8]) -> Result<M, BoxError> {
    if body.len() < GRPC_PREFIX_LEN {
        return Err("gRPC response shorter than its frame header".into());
    }
    ...
    let declared = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    let message = GRPC_PREFIX_LEN
        .checked_add(declared)
        .and_then(|end| body.get(GRPC_PREFIX_LEN..end))
        .ok_or_else(|| -> BoxError { "gRPC frame length overruns the body".into() })?;
    Ok(M::decode(message)?)
}
```

`unframe` itself is memory-safe and panic-free (`checked_add`, `body.get`), and
its unit tests at `chain.rs:674-682` pin that. The defect is not in `unframe`; it
is that `body` is fully materialised before `unframe` ever sees it.

The lookup path an unauthenticated client drives
(`hub/src/server.rs:296-322`):

```rust
    pub async fn lookup(&self, wire_hash: &[u8]) -> LookupOutcome {
        if let Some(bytes) = self.queue.find_by_txid(wire_hash) { ... }

        match self.chain.get_transaction(wire_hash).await {
            Ok(TxLookup::Found { data, height }) => {
                LookupOutcome::Found { data: Zeroizing::new(data), height }
            }
            ...
```

the handler that reaches it, with an inbound limit but no outbound one
(`hub/src/server.rs:487-494`):

```rust
    let collected = match Limited::new(req.into_body(), MAX_LOOKUP_BYTES)
        .collect()
        .await
    { ... };
```

and the point at which the buffered bytes are proved useless on the mixnet arm
(`hub/src/nym.rs:295-302`):

```rust
    match wire::encode_lookup_reply(&nonce, &reply) {
        Ok(frame) => Some(frame),
        // The reply budget is nine bytes under the submit cap, so an indexer
        // can return a transaction that fits nowhere in a reply frame. Fail
        // closed rather than truncate.
        Err(err) => { ...; Some(error_reply(nonce)) }
    }
```

Note that `chain.rs` fans out one such read per endpoint per call
(`chain.rs:222-249` for lookups, `:181-195` for publishes), so the per-call
figure multiplies by `endpoints.len()`.

## Recommendations

- **Wrap the response body in `http_body_util::Limited` in `round_trip`**, with a
  ceiling passed in by the caller: a few kilobytes for `GetLightdInfo`, and
  `queue::MAX_TX_BYTES + GRPC_PREFIX_LEN` for `GetTransaction` and
  `SendTransaction`. Anything larger is already destined to be discarded
  (`nym.rs:295-302`), so the cap costs the hub nothing it can use. This is a
  handful of lines and closes both paths.
- **Reject on `content-length` before reading**, where the peer supplies one, so
  the common case never allocates at all.
- **Bound concurrency on the HTTP serving path** the way the mixnet path already
  is — see the confirmed `hub-http-lookup-path-has-no-concurrency-bound.md`.
  That issue and this one multiply; either fix alone leaves the product of the
  other two factors.
- **Add a response-write deadline** so a client that stops reading cannot park a
  materialised response body in the hub indefinitely.

## Validation Information

**Verdict: CONFIRMED. Severity confirmed at Medium**, and the two paths have
been separated because they do not have the same reachability and the filed text
ran them together.

### Every mechanical claim re-verified against the target

| Claim | Verified at |
|---|---|
| The response body is collected with no ceiling | `hub/src/chain.rs:373-377` — `response.into_body().collect()` with only a `tokio::time::timeout` around it |
| The only bound is time, not bytes | `hub/src/chain.rs:48` (`RPC_TIMEOUT = 10 s`), applied at `:291` and `:373` |
| `unframe`'s length check runs after full materialisation | `hub/src/chain.rs:415-430`, called at `:336` on the already-collected `response` |
| `to_bytes()` makes a second contiguous copy when the body arrived in multiple frames | `hub/src/chain.rs:377` |
| prost decodes `RawTransaction.data` into a fresh `Vec<u8>`, carried out as `TxLookup::Found` | `hub/src/chain.rs:232-236`, `:429` |
| The HTTP response copies it again | `hub/src/server.rs:508-509` — `Bytes::copy_from_slice(tx_bytes)` |
| Every *inbound* body in the same crate is `Limited`; this one is not | `hub/src/server.rs:488`, `:523`; `hub/src/queue.rs:61` |
| The hub cannot use more than ~64 KiB of the answer and discards the rest | `hub/src/nym.rs:295-302`; `hub/src/wire.rs` reply budget |
| `POST /transaction` is unauthenticated, ungated and internet-reachable | `hub/src/server.rs:442-445`; `hub/deploy/caution/caution.hcl.tmpl:51-55`, `:97-105` |
| No concurrency bound on the HTTP arm; 64 on the mixnet arm | `hub/src/server.rs:354-408`; `hub/src/nym.rs:54`, `:153` |
| No response-write timeout exists; the installed timer only re-enables the header-read timeout | `hub/src/server.rs:392-398` and its own comment |
| A Zcash transaction may be ~2 MB | `zebra-chain/src/block/serialize.rs:24` — `MAX_BLOCK_BYTES = 2_000_000`; a transaction must fit in a block |
| `GetLightdInfo` is polled every 30 s with no attacker involvement | `hub/src/batcher.rs:71` (`POLL_INTERVAL`), `:290` |
| The enclave has 2 GB and a RAM-only queue | `hub/deploy/caution/caution.hcl.tmpl:34-40` |
| A hub death destroys acked migrations | `hub/src/server.rs:370-374`; `shim/src/hub.rs:231-240` |

### `AVOIDING-FALSE-POSITIVES.md` §5 applied in both directions

§5's test is *what resources would the attacker need, and what would stop them?*

*What the attacker needs:* a TCP connection and a ~100-byte request naming a
public txid. That is the **inverse** of §5's canonical false positive
("must send 100 GB"): the ratio is roughly 1 byte in to 20,000 bytes allocated,
which is §5's own stated *real* pattern — *"1KB request → 1GB memory
allocation"*, *"single connection consuming unbounded resources"*.

*What would stop them:* nothing in the target and nothing in the platform. There
is no ACL, no rate limit, no per-response cap, no `content-length` check, no
concurrency bound on the HTTP arm, and no response-write deadline. The
in-enclave Caddy is a plain reverse proxy with none of these configured.

*Honest deflation, and it is why this is Medium and not High.* With an **honest**
indexer the per-response size is bounded by Zcash consensus at ~2 MB, so this
alone is a large-but-finite allocation; converting it into an OOM requires the
*separate* missing concurrency bound plus an indexer able to serve hundreds of
megabytes concurrently, and the enclave's descriptor limit may bind first.
Neither could be measured here. The **unbounded** case is genuinely unbounded but
needs a hostile configured indexer, which PROGRESS.md item 6p classifies as a
hub-trust defect rather than an internet-reachable weapon, and such a party
already sees every batch in plaintext. Medium reflects the union: a certain,
cheap, unauthenticated amplifier with an uncertain path to the worst outcome.

### Corrections made to the filed text

- **The two paths were re-ordered and re-scoped.** The filing led with the
  hostile indexer ("Path A") and called the whole thing unbounded. With an
  honest indexer it is bounded at ~2 MB by consensus; the text now says so, cites
  `MAX_BLOCK_BYTES`, and applies item 6p's bound to the hostile-indexer leg
  explicitly.
- **The HTTP/2 flow-control paragraph was corrected.** The filing asserted a
  specific 64 KiB default window. hyper 1.11.0 does not use h2's bare defaults
  and the exact figure is not load-bearing; the correct statement is that a
  window bounds bytes *in flight* while `collect()` releases capacity as it
  consumes, so no window setting bounds the total. Restated that way.
- **A mechanism the filing missed was added, and it is the strongest one:**
  there is no response-write deadline, so a client that stops reading parks the
  materialised response in the hub indefinitely rather than merely transiently.
- **The sharpest argument was added:** on the mixnet arm the buffered bytes are
  provably useless — `build_lookup_reply` discards anything that will not fit a
  reply frame — so the allocation is spent to produce a refusal.
- **The "a few hundred concurrent lookups is enough to pass 2 GB" claim was
  qualified** rather than asserted, because it depends on the enclave's
  descriptor limit and the indexer's throughput, neither of which is knowable
  from the tree.
- Severity left at Medium; the filed value was right.

### Relationship to other issues (stated so nothing is counted twice)

- The missing concurrency bound is `hub-http-lookup-path-has-no-concurrency-bound.md`
  (Confirmed, High). The two multiply — G21 §4.3 records the composition — and
  each file states it once. Neither claims the other's defect as its own.
- The flush-side `k × n` fanout and the per-flush copies of the batch belong to
  `hub-chain-connection-per-call-fanout-and-flush-memory-amplification.md`.
- The confidentiality of `POST /transaction` is the confirmed
  `hub-unauthenticated-pre-publication-transaction-disclosure.md`.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
