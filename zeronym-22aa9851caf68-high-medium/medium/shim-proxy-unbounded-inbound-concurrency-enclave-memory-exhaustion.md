# Nothing bounds the shim's *total* inbound buffering: ~4 MiB per in-flight `SendTransaction`, an uncapped number of concurrent requests, and no timeout of any kind on the wallet leg, against a fixed 2 GB enclave that nothing restarts

**Severity**: Medium
**Validation Status**: Confirmed
**Location**:
`audit-target/zeronym/shim/src/proxy.rs:478-553` (`serve_with_shutdown`'s accept loop — `tokio::spawn` per connection at `:516`, no semaphore, no counter),
`audit-target/zeronym/shim/src/proxy.rs:575-608` (`serve_connection`; the h2 server builder at `:602-606` sets only the two window sizes — no `max_concurrent_streams`, no `.timer()`, no keep-alive),
`audit-target/zeronym/shim/src/proxy.rs:592` (one `UpstreamPool` per inbound connection),
`audit-target/zeronym/shim/src/proxy.rs:174,177` (`STREAM_WINDOW` 2 MiB, `CONNECTION_WINDOW` 8 MiB),
`audit-target/zeronym/shim/src/proxy.rs:271-287` (`upstream_h2_builder`, the *outbound* leg that does get a timer and keep-alives),
`audit-target/zeronym/shim/src/intercept.rs:68-81` (`MAX_SEND_TX_BYTES` = 4 MiB, `MAX_TX_FILTER_BYTES` = 1 KiB and the comment that describes this exact attack),
`audit-target/zeronym/shim/src/intercept.rs:94-131` (`send_transaction`: the `Limited(...).collect()` at `:102`, the `to_bytes()` copy at `:108`),
`audit-target/zeronym/shim/src/intercept.rs:559-568` (`RawTransaction::decode`, the second full copy),
`audit-target/zeronym/shim/src/wire.rs:124,274-277` (`MAX_NYM_TX_BYTES` = 65,503 — the ceiling on anything the divert path can actually carry),
`audit-target/zeronym/shim/deploy/caution/caution.hcl.tmpl:29-36` (`cpu = 2`, `memory_mb = 2048`), `:51-55` (`ingress 0.0.0.0/0` on 8083), `:97-125` (the in-enclave Caddy that maps 443 onto it)
**Found by agent:** Local (file audit of `shim/src/proxy.rs`), merged at validation with the independently-filed `shim/src/intercept.rs` finding `send-transaction-4mib-buffer-memory-exhaustion.md`; validated 2026-08-18
**In scope of audit?** Yes

> **MERGE NOTE.** This file is the single owner of the shim's inbound
> memory-exhaustion finding. `issues/invalid/send-transaction-4mib-buffer-memory-exhaustion.md`
> was filed independently from the `intercept.rs` side of the same defect; its
> substance was validated and found **real**, and it now lives in `invalid/`
> **for bookkeeping only** so the harm is not counted twice. Its three unique
> contributions — the ~8 MiB completion peak, the 64x gap between
> `MAX_SEND_TX_BYTES` and `MAX_NYM_TX_BYTES`, and the fact that the project
> already wrote this attack down and fixed only the other half of it — are
> folded in below.

## Description

The shim buffers a whole `SendTransaction` body before it can classify it. That
buffer is capped **per stream** at `MAX_SEND_TX_BYTES = 4 MiB`
(`intercept.rs:71`, `:102`). Nothing caps the aggregate:

- **No connection cap.** The accept loop spawns a task per accepted socket with
  no semaphore, no counter and no admission control (`proxy.rs:478-553`, spawn
  at `:516`). `grep -rn "Semaphore\|max_concurrent_streams\|MAX_CONN" shim/src/`
  returns nothing.
- **No aggregate byte budget.** There is no process-wide accounting of how much
  all in-flight requests are holding. The only number in the system is the
  per-stream constant.
- **No timeout of any kind on the wallet leg.** The server h2 builder
  (`proxy.rs:602-606`) sets only `initial_stream_window_size` and
  `initial_connection_window_size`. There is no request deadline, no idle
  timeout, no keep-alive and no timer. `Limited` is a *byte* cap, not a
  *duration* cap: a peer that sends 4 MiB minus one byte and then stops without
  `END_STREAM` leaves `collect()` pending forever, holding every byte it
  received.

Two multipliers make the per-stream figure worse than 4 MiB:

1. **The completion peak is ~8 MiB, not 4 MiB.** When a body does complete,
   `Collected::to_bytes()` (`intercept.rs:108`) allocates a fresh contiguous
   buffer of the full length *before* draining the chunk list
   (`http-body-util-0.1.4/src/util.rs`, `BufList::copy_to_bytes` →
   `BytesMut::with_capacity(len)`), so both live at that instant. Then
   `RawTransaction::decode` allocates `raw.data` as a second full copy
   (`intercept.rs:559-568`) while `frame` is still held for the pass-through
   replay (`intercept.rs:129`) or the divert. An attacker who parks N streams at
   4 MiB and then sends the last byte of all of them at once turns N x 4 MiB
   resident into ~N x 8 MiB in one step, with no extra upload.
2. **4 MiB is 64x larger than anything the divert path can carry.** The mixnet
   transport refuses any transaction over `MAX_NYM_TX_BYTES` = 65,503
   (`wire.rs:124`, `:274-277`), so an Orchard-touching body above ~64 KiB is
   buffered at up to 4 MiB, copied twice, and *then* refused with
   `RESOURCE_EXHAUSTED` (`intercept.rs:167-179`). Only the pass-through path
   needs headroom at all, and the constant's own doc comment cites the ~2 MB
   Zcash transaction limit as the number it is "well above" — i.e. it is
   deliberately double what it needs to be.

**The project already wrote this attack down and closed only the other half of
it.** `MAX_TX_FILTER_BYTES` was introduced at 1 KiB for the `GetTransaction`
path with this comment (`intercept.rs:73-81`):

> This used to share `MAX_SEND_TX_BYTES`, which was 4000x looser than the request
> can ever legitimately be, and the looseness had a price: hyper allows ~200
> streams per connection and connections are uncapped, so a hostile client
> trickling near-4 MiB bodies on many streams could pin gigabytes in an enclave
> whose memory is mostly EnclaveOS. A kilobyte refuses nothing a wallet sends and
> takes that lever away.

The lever was taken away on the path that never needed a large buffer. On
`SendTransaction` — the path that *must* buffer, and therefore the one a
per-stream constant cannot fix — it is untouched, and line 70's claim that "a
hostile client cannot make the shim buffer unbounded memory" is true per stream
and false in aggregate.

## Attack Scenario and Steps

1. Connect to the shim's public endpoint. In the shipped deployment that is the
   in-enclave Caddy on 443, which terminates TLS and forwards h2c to the shim on
   8083 (`caution.hcl.tmpl:97-125`); `ingress` is `0.0.0.0/0` (`:51-55`). No
   credentials, no wallet, no valid transaction.
2. Open many concurrent HTTP/2 streams, each a
   `POST /cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction`. `route_for`
   matches on path only, so this reaches `intercept::send_transaction`
   unauthenticated.
3. On each stream send just under 4 MiB of DATA and then **stop**, without
   `END_STREAM` and without a `RST_STREAM`. `Limited` only errors when the limit
   is *exceeded*, so nothing errors; `collect()` stays pending and holds every
   byte. HTTP/2 flow control does not bound this: hyper releases receive capacity
   as soon as it hands each chunk to the body
   (`hyper-1.11.0/src/body/incoming.rs:245`, `h2.flow_control().release_capacity(bytes.len())`),
   so the 2 MiB stream / 8 MiB connection windows only pace the upload, they do
   not cap it.
4. Add streams and connections until the enclave's memory is gone. Optionally,
   send the final byte of every parked stream simultaneously to double the peak
   (mechanism 1 above) instead of uploading twice as much.
5. Nothing reaps the stalled streams. Once uploaded, the memory is pinned for
   free for as long as the attacker leaves the sockets open.

**Attack Requirements and Assumptions:**

- **Access needed:** the ability to open TCP connections to a public endpoint.
  Unauthenticated and remote.
- **Concurrency is not capped anywhere on the path.** hyper 1.11.0 caps the
  *shim* at 200 streams per connection — verified, not assumed:
  `Config::default()` sets `max_concurrent_streams: Some(200)`
  (`hyper-1.11.0/src/proto/h2/server.rs:69`) and applies it to the h2 builder
  (`:143-144`). But nothing caps *connections*, and in the shipped topology the
  shim's peer is Caddy, not the attacker: Caddy's Go HTTP/2 transport opens
  additional backend connections once a backend's stream limit is reached, and
  Caddy's own server accepts an uncapped number of client connections. So the
  attacker's aggregate in-flight request count is bounded only by what they
  choose to open.
- **Nothing in front of the shim absorbs it, and this was checked rather than
  assumed.** The Caution platform renders the in-enclave Caddyfile from
  `src/enclave-builder/templates/run.sh.template:107-121`; it is three `handle`
  blocks and a bare `reverse_proxy {{CADDY_UPSTREAM}}` with **no `request_body`
  size limit, no rate limit, and no timeout directives**. Caddy's
  `reverse_proxy` streams request bodies by default (`request_buffers` unset), so
  it relays a slow, incomplete body rather than absorbing it. Nor is there a
  second way in that bypasses Caddy: under `e2e_encryption { mode = "tls" }` the
  builder *excludes* the `http_port` from the per-port vsock relays
  (`src/enclave-builder/src/build.rs:421-435`), so 8083 is reachable only through
  Caddy — and Caddy does not change the conclusion.
- **Cost ratio.** This is a bandwidth-priced flood, not an amplifier: roughly one
  uploaded byte per one to two bytes of enclave memory at the peak. Pinning
  ~1.6 GB costs ~0.8-1.6 GB of upload, once, from a single ordinary host — tens
  of seconds on a commodity VPS — after which the hold is free and can be
  repeated at will. This ratio is the single reason this issue is Medium rather
  than High; see the severity note in the Validation Information.
- **What is *not* required:** no wallet, no valid transaction, no position on the
  mixnet, no operator privilege, no interaction with the classifier.

## Impact on Users

The enclave is `memory_mb = 2048` with no swap and no autoscaling
(`caution.hcl.tmpl:29-36`), and its own comment says "2 GB is almost entirely
EnclaveOS; the process itself sits in single-digit MB". Exhausting it means an
allocation failure (Rust aborts) or an OOM kill of the shim.

- **The shim does not come back on its own.** The shim's process is the last
  command in the enclave's `run.sh`, so when it dies the script exits and the
  enclave terminates. On the parent, the `nitro-enclave.service` unit runs
  `nitro-cli run-enclave ... && tail -f /dev/null` with `Restart=on-failure`
  (Caution platform, `terraform/modules/aws/nitro-enclave/user-data.sh:157-175`):
  `run-enclave` returns as soon as the enclave is launched and `tail -f` keeps
  the unit alive forever, so systemd never observes the enclave's death and never
  restarts it. There is no watchdog elsewhere in the platform tree. **A
  successful attack is therefore an outage that lasts until a human notices** —
  and what they must then do is redeploy an immutable attested enclave, which on
  this project spends a certificate issuance and re-registers a mixnet identity.
- **Users are pushed off the protected path, permanently and on-chain.** A shim
  that is down answers nothing. A user who must complete a mandatory
  Orchard->Ironwood migration and cannot will point their wallet at a different,
  unprotected indexer — which is the exact linkage the product exists to prevent,
  and it is written to the public chain forever. This is the "force fallback
  behaviour" attacker goal in the threat model, reached with no privileged
  position at all.
- **Acknowledged migrations in flight at that instant are destroyed silently.**
  `shim/src/hub.rs:236-240` returns `Submit::Accepted` at mixnet hand-off, and
  its own comment says a hub refusal "is never surfaced here". A shim that dies
  with frames in its pipeline loses them with the wallet already holding
  `error_code 0` and a txid. (That harm is owned in detail by
  `shim-nym-driver-every-teardown-path-silently-destroys-acknowledged-submits.md`;
  it is named here because it is what makes this a privacy/integrity failure
  rather than plain downtime.)
- **The adversary chooses the moment**, cheaply and repeatedly — including the
  operator, who has a direct incentive to make the private path look unreliable
  and who can do this from anywhere without touching their own infrastructure.

## Technical Details / Code Analysis

**The buffer, and the two copies** (`shim/src/intercept.rs:94-131`):

```rust
 94 pub(crate) async fn send_transaction(
 95     req: Request<Incoming>,
 96     pool: Arc<UpstreamPool>,
 97     diversion: Option<Arc<Diversion>>,
 98 ) -> Result<Response<ProxyBody>, BoxError> {
 99     let (parts, body) = req.into_parts();
100
101     // The only buffering in the entire shim, and it is bounded.
102     let collected = match Limited::new(body, MAX_SEND_TX_BYTES).collect().await {
103         Ok(collected) => collected,
104         Err(err) => return Ok(body_read_failed(err)),
105     };
106
107     let trailers = collected.trailers().cloned();
108     let frame = collected.to_bytes();
109
110     let (inspection, tx_data) = inspect(&parts.headers, &frame);
...
128     let upstream = pool.get().await?;
129     let replay = ReplayBody::new(frame, trailers).boxed();
```

The comment on line 101 is true of one stream and of nothing else.
`Limited::poll_frame` errors only when a frame's payload *exceeds* what is left
(`http-body-util-0.1.4/src/limited.rs:44-59`), so a peer that stops one byte
short never errors and `collect()` never returns.

`to_bytes()` is the first copy. `Collected::to_bytes` calls
`BufList::copy_to_bytes(remaining)`, which for a multi-chunk body takes the
`_ =>` arm and does `BytesMut::with_capacity(len)` before draining the chunks
(`http-body-util-0.1.4/src/util.rs`) — both allocations live at that moment.

`RawTransaction::decode` is the second (`shim/src/intercept.rs:559-568`):

```rust
559     match RawTransaction::decode(message) {
...
562         Ok(raw) => {
563             let evidence = classify_with_evidence(&raw.data);
564             (
565                 Inspection::Classified(evidence),
566                 Some(Bytes::from(raw.data)),
567             )
568         }
```

`raw.data` is a fresh `Vec<u8>` holding the whole declared transaction while
`frame` is still alive in the caller. A garbage 4 MiB body decodes as a valid
`RawTransaction`, classifies as `Unparseable`, fails safe toward diversion, and
is then refused by `encode_submit` for exceeding `MAX_NYM_TX_BYTES`
(`wire.rs:274-277`) — so the two copies are paid in full to produce a refusal,
and no mixnet traffic is generated.

**The accept loop, with nothing bounding it** (`shim/src/proxy.rs:478-543`,
abridged):

```rust
478     loop {
479         tokio::select! {
480             biased;
481             () = &mut shutdown => break,
482             accepted = listener.accept() => {
...
516                 tokio::spawn(async move {
517                     let _live = live;
518                     match tls {
519                         None => {
520                             serve_connection(stream, peer, backend, diversion, caution, status)
521                                 .await
522                         }
```

`live_tx` is a shutdown tracker, not a limiter: nothing is ever sent on it and
nothing acquires anything. Every accepted socket becomes an unbounded task.

**The server h2 configuration** (`shim/src/proxy.rs:602-608`):

```rust
602     if let Err(err) = server_h2::Builder::new(TokioExecutor::new())
603         .initial_stream_window_size(STREAM_WINDOW)
604         .initial_connection_window_size(CONNECTION_WINDOW)
605         .serve_connection(TokioIo::new(stream), service)
606         .await
607     {
608         tracing::debug!(%peer, %err, "client connection ended");
```

Two window sizes and nothing else. Compare the *outbound* builder in the same
file (`proxy.rs:271-287`), which was given a timer, an interval and a timeout,
with a doc comment explaining that `.timer()` is "load-bearing: hyper silently
disables keepalive (and every other timed behaviour) when no timer is
installed". The inbound leg got none of it.

**Why "just add `.timer()`" is not the fix** (verified against the pinned
crate, and this inverts the recommendation both original filings made): hyper's
HTTP/1 server has a `header_read_timeout` that a missing timer silently
disables — that is the hazard the hub's `server.rs:392-398` records and
correctly closes. **hyper's HTTP/2 server has no equivalent at all.** Its only
timed mechanism is the keep-alive PING, and `Config::default()` sets
`keep_alive_interval: None` (`hyper-1.11.0/src/proto/h2/server.rs:70-73`). So a
timer alone enables nothing; the shim needs a timer *plus* an explicitly
configured keep-alive, and separately an explicit deadline around the
`Limited(...).collect()` in `intercept.rs`, because neither hyper nor h2 will
ever impose one.

**A second, cheaper mechanism against a different shared resource.**
`serve_connection` constructs **one `UpstreamPool` per inbound connection**
(`proxy.rs:588-592`), so N inbound connections that each issue one trivial
pass-through request cause the shim to hold N open TCP (and, in the shipped
config, TLS) connections to the operator's indexer, 1:1 with attacker
connections and with no pool and no cap. That consumes enclave file descriptors
and indexer front-end slots. (The *privacy* consequence of the same code —
one upstream session per wallet session — is a separate finding,
`one-upstream-connection-per-wallet-connection-makes-the-operators-indexer-a-per-wallet-session-boundary-and-only-caddys-pooling-hides-it.md`;
only the resource consequence is claimed here.)

**The envelope** (`shim/deploy/caution/caution.hcl.tmpl:29-36`, `:51-55`):

```hcl
  resources {
    # The shim is stateless: ... 2 GB is almost entirely EnclaveOS; the process
    # itself sits in single-digit MB.
    cpu       = 2
    memory_mb = 2048
  }
...
    ingress {
      cidr_ipv4   = "0.0.0.0/0"
      port        = 8083
      ip_protocol = "tcp"
    }
```

## Recommendations

Ordered by how much each one removes, and corrected against the pinned crates.

1. **Add an explicit per-request deadline around the buffering step.** Wrap
   `Limited::new(body, MAX_SEND_TX_BYTES).collect()` in `intercept.rs:102` (and
   the `MAX_TX_FILTER_BYTES` one at `:249`) in a `tokio::time::timeout`, and
   answer an expiry with the existing `body_read_failed` shape. This is the
   single change that closes the hold; a byte cap can never do it, and hyper's
   h2 server will not do it for you.
2. **Add an aggregate byte budget.** A process-wide `Arc<AtomicUsize>` (or a
   `Semaphore` of buffering slots) charged on entry to `send_transaction` and
   released on exit, sized to what 2 GB minus EnclaveOS can actually hold, so
   the bound is on *total* buffered bytes rather than on a per-stream constant
   multiplied by an unbounded stream count. Refuse over-budget requests with
   `RESOURCE_EXHAUSTED`.
3. **Lower `MAX_SEND_TX_BYTES`** to just above the largest transaction the shim
   must pass through — the ~2 MB Zcash limit its own doc comment cites — rather
   than double it. Nothing divertible exceeds `MAX_NYM_TX_BYTES` = 65,503, so
   the extra headroom serves only the pass-through path and buys nothing there
   either.
4. **Cap concurrent inbound connections** in the accept loop
   (`proxy.rs:478-543`) with a `tokio::sync::Semaphore` whose permit is held by
   the spawned connection task; at minimum, count and log them so the failure
   mode is visible before it is fatal.
5. **Set `.max_concurrent_streams(...)` explicitly** on the server builder and
   pick it together with (2) and (3) so `max_streams x buffer` fits the enclave
   with margin. hyper's 200 is a default, not a contract, and the arithmetic
   above depends on it.
6. **Install `.timer(TokioTimer::new())` *and* an explicit
   `keep_alive_interval`/`keep_alive_timeout`** on the server builder, so a
   vanished (as opposed to merely slow) peer is reaped. Note the correction:
   `.timer()` alone is inert on hyper's h2 server — it is necessary but not
   sufficient, and it is not a substitute for (1).
7. Avoid the second copy where it is free to do so: `inspect` could classify
   from a borrowed slice of `frame` and only materialise `tx_data` on the divert
   path, which removes ~4 MiB of the completion peak per stream.

## Validation Information

**Verdict: CONFIRMED, Medium (top of Medium).** Validated 2026-08-18. This file
now carries the merged finding; `send-transaction-4mib-buffer-memory-exhaustion.md`
has been moved to `invalid/` for bookkeeping only, with a header stating that its
substance is real and is owned here.

### Why the two filings are one finding

They describe the same harm (unauthenticated inbound requests pin the shim
enclave's memory) from the two files that jointly cause it, and they share every
step of the attack, every precondition and most of the fix list. `intercept.rs`
supplies the multiplicand (4 MiB per stream, doubled at completion);
`proxy.rs` supplies the multiplier (no connection cap, no aggregate budget, no
deadline). Counting them separately would double-count one outage. The first
filing said so itself in its own opening paragraph and asked to be merged.

### Every mechanical claim re-verified

| Claim | Verified at |
|---|---|
| `SendTransaction` is routed on path only, unauthenticated | `shim/src/proxy.rs:744-782` (`route_for`), `:70` (`SEND_TRANSACTION`) |
| The body is buffered whole, capped per stream at 4 MiB | `shim/src/intercept.rs:71`, `:102` |
| `Limited` errors only on *exceeding* the cap, so stopping one byte short never errors | `http-body-util-0.1.4/src/limited.rs:44-59` |
| Flow control does not bound the total: capacity is released as each chunk is handed to the body | `hyper-1.11.0/src/body/incoming.rs:245` |
| `to_bytes()` allocates a full second buffer before draining the chunks | `http-body-util-0.1.4/src/util.rs`, `BufList::copy_to_bytes`, `_ =>` arm |
| `RawTransaction::decode` allocates a second full copy while `frame` is live | `shim/src/intercept.rs:559-568`, `:108`, `:129` |
| hyper 1.11.0 really does cap the shim at 200 streams/connection | `hyper-1.11.0/src/proto/h2/server.rs:69`, `:143-144` (`max_concurrent_streams: Some(200)`) |
| hyper's h2 server has no header/request-read timeout at all; keep-alive defaults to `None` | `hyper-1.11.0/src/proto/h2/server.rs:70-73` |
| No connection cap, no semaphore, no counter in the accept loop | `shim/src/proxy.rs:478-553`; `grep -rn "Semaphore\|max_concurrent_streams\|MAX_CONN" shim/src/` is empty |
| No timer, no keep-alive, no stream cap on the server builder | `shim/src/proxy.rs:602-606`, contrasted with `:275-287` |
| One `UpstreamPool` per inbound connection | `shim/src/proxy.rs:588-592` |
| Anything divertible is <= 65,503 bytes, so 4 MiB is 64x more headroom than the divert path can use | `shim/src/wire.rs:124`, `:274-277`; refusal at `shim/src/intercept.rs:167-179` |
| Enclave is 2 vCPU / 2 GB, internet-reachable, TLS terminated by an in-enclave Caddy | `shim/deploy/caution/caution.hcl.tmpl:29-36`, `:51-55`, `:97-125` |

Facts established from the Caution platform tree (out of audit scope, used only
to test whether infrastructure absorbs the attack, and marked as such):

| Claim | Verified at |
|---|---|
| The in-enclave Caddyfile is a bare `reverse_proxy` — no body limit, no rate limit, no timeouts | `src/enclave-builder/templates/run.sh.template:107-121` |
| Port 8083 gets **no** direct vsock relay under `mode = "tls"`, so Caddy is the only way in | `src/enclave-builder/src/build.rs:396-435` |
| An enclave that exits is not restarted: `run-enclave` returns and `tail -f /dev/null` keeps the unit alive, so `Restart=on-failure` never fires | `terraform/modules/aws/nitro-enclave/user-data.sh:157-175` |

### `AVOIDING-FALSE-POSITIVES.md` §5 applied honestly, in both directions

§5's canonical false positives are *"unlimited concurrent connections … each
connection uses minimal resources"* and *"large file upload DoS … requires the
attacker to have 1 GB of upload bandwidth per attempt; CDN/proxy usually limits
to much less."* This issue sits between §5's two poles and I am recording which
half applies where rather than picking the flattering reading:

**Against the false-positive pattern (why this is real):**

- A connection here does **not** use minimal resources: one connection can hold
  800 MiB, and 200 simultaneous completions on it peak at ~1.6 GB. §5 names
  *"single connection consuming unbounded resources"* as the real-issue shape.
- The "CDN/proxy limits it to much less" mitigation was checked against the
  actual platform template and **does not exist**: no body cap, no rate limit,
  no timeout, and no route that bypasses Caddy either way.
- "OS limits apply first" is not a mitigation here — the limit that applies
  first is a hard 2 GB with no swap, and reaching it is the attack.
- The memory is retained after the upload at zero marginal cost, so it is not
  "per attempt"; and the target does not restart itself, so one success is a
  standing outage.

**For the false-positive pattern (why this is not High):**

- The cost ratio is ~1:1 to ~1:2, not §5's "1 KB request causing 1 GB
  allocation". The attacker must actually push on the order of a gigabyte. That
  is trivially affordable, but it is a bandwidth-priced flood, not an amplifier,
  and it is a genuinely different economic class from the two confirmed Highs on
  this same listener (`junk-sendtransaction-flood-…` holds the divert pipeline
  permanently full at ~1 byte/second; `gettransaction-flood-…` converts ~100
  bytes into 61 sphinx packets).
- Those two confirmed Highs already take migration diversion down for less
  money. The incremental harm this issue adds is "hard process death with no
  self-recovery" rather than "starvation", which is worse per event but is not a
  new class of victim.
- How much of the 2 GB is actually free could not be measured here; the enclave
  image (Caddy, socat, bootproofd, busybox, the shim) is loaded into that same
  RAM, so the true free figure is smaller than 2 GB — which makes the attack
  cheaper, not more expensive, but the exact number is an inference.

Net: a cheap, unauthenticated, remote, repeatable DoS whose terminal state is a
privacy failure rather than mere downtime, priced in bandwidth rather than in
amplification. **Medium, at the top of Medium.**

### Corrections applied to the two filings

1. **The `max_concurrent_streams` caveat is struck.** The original
   `intercept.rs` filing said the ~200 figure "should be verified rather than
   relied on; if the default advertises no `SETTINGS_MAX_CONCURRENT_STREAMS`,
   the per-connection stream count is bounded only by the peer." It is verified:
   hyper 1.11.0 sets `Some(200)`. (h2 0.4.15 alone would indeed be `usize::MAX`
   — `frame/settings.rs` leaves it `None` and `Counts::new` uses
   `unwrap_or(usize::MAX)` — but hyper sets it before handshaking.) The comment
   at `intercept.rs:74-80` is correct as written.
2. **Both filings' `.timer()` recommendation is inverted.** Recommendation 3 in
   each ("install `.timer()` … so a stalled client is reaped") does not work:
   hyper's h2 server has no `header_read_timeout` analogue, and its only timed
   mechanism defaults to off. The fix is an explicit request deadline; the timer
   is only a prerequisite for the keep-alive half. This is now recommendation 1
   with the timer demoted to 6.
3. **"800 MiB from a single TCP connection" is kept but scoped.** It is exact for
   the shim's h2 peer. In the shipped topology that peer is Caddy, not the
   attacker, so the attacker's own connection count and the shim's are
   decoupled; the accurate statement is that neither hop caps aggregate
   concurrency, which the text now says.
4. **The "a validator should confirm the deployed Caddy configuration" caveat is
   discharged** with the platform template quoted above, and the "could Caddy
   absorb it?" question answered no. The stronger new fact — that 8083 has no
   direct vsock relay under `mode = "tls"` — is also recorded, because it
   narrows the ingress to one hop rather than two.
5. **The `UpstreamPool` leg is kept as a resource claim only**, with its privacy
   half explicitly deferred to the file that owns it, so the two are not
   double-counted.
6. **A "1.6 GB momentary" figure in the original `proxy.rs` filing is restated
   correctly.** It read as though completing 200 bodies needs 1.6 GB *in addition
   to* the 800 MiB; it does not — the 800 MiB is one of the two copies. The peak
   is ~1.6 GB total, reached from 800 MiB of upload, which is the sharper claim.
7. **Recommendation 7 (classify from a borrowed slice) is new**, and is the
   cheapest single change that removes half the completion peak.

### What this issue does *not* claim

- It does not claim an amplification attack. The ratio is stated as ~1:1-1:2 and
  the severity rests on that.
- It does not claim the empty-`DATA`-frame trick is a memory attack.
  `Collected::push_frame` explicitly skips empty data frames
  (`http-body-util-0.1.4/src/collected.rs:41-52`), so zero-length frames hold a
  stream (~1 KB of h2 state) without accumulating bytes. That is a duration hole
  worth a sentence, already covered by recommendation 1, and it is not part of
  the arithmetic here.
- **It does not claim the response direction.** A lead worth a separate pass and
  deliberately excluded from this confirmed finding: nothing bounds the
  *outbound* side either — hyper's server `max_send_buffer_size` defaults to
  400 KB per stream (`hyper-1.11.0/src/proto/h2/server.rs:39`, `:75`) and the
  shim's upstream connection window is 8 MiB, so a ~100-byte `GetBlockRange`
  whose response the client never reads may pin hundreds of kilobytes of
  *indexer-supplied* bytes per stream at essentially zero cost to the attacker.
  That would be a genuine amplifier, but whether an attacker can drive it through
  the in-enclave Caddy's own buffering could not be established here, so it is
  recorded in `BRAINSTORM.md` as a lead rather than asserted.
- It does not re-argue the acknowledged-migration loss on shim death; that is
  owned by
  `shim-nym-driver-every-teardown-path-silently-destroys-acknowledged-submits.md`
  and referenced only as the reason this is not plain downtime.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
