# The hub's unauthenticated HTTP lookup path has no concurrency bound, so a ~100-byte request buys a fresh TCP+TLS+HTTP/2 dial to the indexer — the exact flood the sibling mixnet path caps at 64

**Severity**: High
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/server.rs:442-445` (the ungated `TRANSACTION_PATH` arm of `handle`), `:487-505` (`lookup`), `:296-322` (`Hub::lookup`), `:354-408` (`serve`, unbounded `tokio::spawn` per connection); the work it triggers is `audit-target/zeronym/hub/src/chain.rs:221-266` (`get_transaction`) and `:300-337` (`unary_inner`, a fresh `TcpStream::connect` + TLS + h2 handshake **per call**, `:310`); the bound that exists on the equivalent mixnet arm is `audit-target/zeronym/hub/src/nym.rs:38-54` (`MAX_CONCURRENT_LOOKUPS = 64`) and `:171-196` (`try_acquire_owned`, drop-on-full); the collateral victims are `audit-target/zeronym/hub/src/batcher.rs:290-296` (the tip poll) and `:341-355` (`flush`); the refusal that follows is `audit-target/zeronym/hub/src/server.rs:248-254` with `audit-target/zeronym/hub/src/batcher.rs:62` (`TIP_STALE_AFTER`) and `:205-208` (`is_stale`); reachability from `audit-target/zeronym/hub/deploy/caution/caution.hcl.tmpl:51-55` and `:97-105` (`ingress 0.0.0.0/0` on 8083 and the in-enclave Caddy that maps 443 onto it), `:39-40` (`cpu = 2`, `memory_mb = 2048`)
**Found by agent:** Local (file audit of `hub/src/server.rs`); validated 2026-08-18
**In scope of audit?** Yes

## Description

`hub/src/nym.rs:38-54` states the rule for the hub's lookup arm and the reason
for it, in the code's own words:

> How many lookups may be in flight at once: dialling the operator's indexer,
> framing the reply, or waiting to hand it to the driver. ... Generous enough
> that honest polling never queues behind it, **small enough that a flood cannot
> open unbounded connections** or park an unbounded pile of 64 KiB reply frames
> behind a slow driver.
>
> ... When every slot is held the lookup is **dropped, not parked**: parking it
> in a task is the unbounded pile this bound exists to prevent.

That bound is implemented on the **mixnet** ingress only. The **HTTP** ingress
reaches the identical core, `Hub::lookup`, with **no semaphore, no rate limit,
no connection cap and no per-peer accounting**. `handle` routes
`TRANSACTION_PATH` unconditionally — it is not behind `http_submit`, unlike
`POST /` — and `serve` spawns one unbounded `tokio::spawn` per accepted
connection.

Each lookup that misses the in-RAM queue calls `ChainClient::get_transaction`,
which for **every** configured endpoint runs `unary_inner`
(`hub/src/chain.rs:310`):

```rust
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
```

There is no connection pool anywhere in `chain.rs`; `ChainClient` holds only
`endpoints` and `tls` (`chain.rs:123-129`). Every single call performs a fresh
TCP connect, a full TLS handshake with certificate-chain verification
(`ZIH_INDEXER_TLS` is a required production setting —
`caution.hcl.tmpl:126-132`, "WITHOUT THIS THE HOP IS PLAINTEXT"), and a fresh
HTTP/2 handshake, under a 10 s budget (`RPC_TIMEOUT`, `chain.rs:48`).

So the cost ratio on a 2 vCPU / 2 GB enclave is:

| Attacker pays | Hub pays, per request |
|---|---|
| ~100 bytes on an already-open connection: `POST /transaction` with a 32-byte random body | 1 outbound socket, 1 ephemeral source port held for the connection plus 60 s of `TIME_WAIT`, 1 TCP connect, **1 full TLS handshake with X.509 chain verification**, 1 HTTP/2 handshake, 1 gRPC round trip, up to 10 s of holding — *per configured indexer endpoint* |

A random 32-byte body always misses the queue (`Queue::find_by_txid` compares
64-character hex txid strings, `queue.rs:328-348`), so the attacker chooses with
certainty that the expensive branch runs.

**The damage is not the flood itself; it is that the hub's own outbound path
shares every ceiling the flood reaches.** `batcher::run` polls the tip through
the same `ChainClient` (`batcher.rs:290`) and `flush` publishes through it
(`batcher.rs:341-355`). Whichever limit binds first — the process's descriptor
limit, the ephemeral port range for the single indexer destination, or the two
vCPUs — the batcher's `TcpStream::connect` fails at the same moment the
attacker's lookups do. Fifteen minutes of failed tip polls make `is_stale()`
true (`batcher.rs:62`, `:205-208`), after which `Hub::admit` refuses **every**
submission on **both** ingress paths (`server.rs:248-254`), and on the deployed
mixnet transport a refusal reaches nobody: the shim answered the wallet
`error_code 0` at mixnet hand-off (`shim/src/hub.rs:231-240`).

## Attack Scenario and Steps

Attacker is anyone on the internet. The hub's HTTP listener is reachable at
`https://<hub-domain>/transaction`: `caution.hcl.tmpl:97-105` declares
`http { domain, port = 8083, e2e_encryption { mode = "tls" } }`, which the
platform implements as an **in-enclave Caddy terminating TLS on 443 and
reverse-proxying to `127.0.0.1:8083`** (settled in PROGRESS.md item 5/6). Caddy
ships with no request-rate limit and no upstream concurrency cap, so it relays
whatever arrives.

1. Open one HTTP/1.1 or HTTP/2 connection to the hub's domain, or a few hundred.
   Nothing in `serve` caps the number.
2. Issue `POST /transaction` with a random 32-byte body, back to back, at
   whatever concurrency is wanted. Each request is ~100 bytes on the wire and
   costs the attacker no CPU. Caddy opens one upstream connection per concurrent
   request, so hub-side concurrency tracks the attacker's concurrency exactly.
3. Each request misses the queue and produces one fresh TCP + TLS + HTTP/2 +
   gRPC dial from the enclave to each configured indexer.
4. One of three ceilings is reached, and all three are shared with the hub's own
   work:
   - **Ephemeral source ports.** Every call dials the *same* destination
     (the shipped deploy configures one indexer), and the hub is the side that
     closes, so each completed call parks a local port in `TIME_WAIT` for ~60 s.
     On stock Linux settings (`ip_local_port_range` 32768–60999 ≈ 28,232 ports,
     `tcp_tw_reuse = 2`, i.e. loopback only) a sustained rate above roughly
     **470 connections per second** exhausts the range for that destination and
     `connect()` returns `EADDRNOTAVAIL` — for the batcher as well. 470 requests
     per second of ~100-byte bodies is about 50 KB/s of attacker bandwidth.
   - **File descriptors.** Each in-flight lookup holds one inbound descriptor
     (from Caddy) and one outbound per endpoint. The code already anticipates
     this ceiling: `serve` has an explicit `is_fd_exhaustion` arm that logs a
     warning and sleeps 100 ms (`server.rs:341-352`, `:381-385`), which keeps
     the listener alive while the hub is failing *its own* outbound connects.
   - **CPU.** `cpu = 2`. Each request forces one client-side TLS handshake
     including certificate-chain verification, on the same two workers that run
     the cadence loop, the mixnet driver and the in-enclave Caddy.
5. The hub's tip poll fails. `TipTracker::observe` only refreshes `last_advance`
   when a height *advances*, so after `TIP_STALE_AFTER = 15 min` — 30
   consecutive 30 s `POLL_INTERVAL` ticks — `is_stale()` becomes true.
6. `Hub::admit` then refuses every submission fleet-wide:

```rust
// hub/src/server.rs:248-254
    pub fn admit(&self, tx_bytes: &[u8]) -> Result<Option<String>, Refusal> {
        if self.tip.is_stale() {
            return Err(Refusal::TipStale);
        }
```

7. Because submit over Nym is dispatch-only (`shim/src/hub.rs:231-240`, and the
   project's own test `shim/tests/divert_nym.rs:235-267` asserts
   `error_code == 0` against a refusing hub), the wallet was already told the
   migration succeeded. The refusal reaches nobody. The migration exists
   nowhere.

Steps 1–4 are certain from the code. Step 5 requires the flood to be sustained
for fifteen minutes, which is trivial. **Even without reaching step 5**, steps
1–4 alone contend with `flush`, which runs inline on the cadence task
(`batcher.rs:290-313`): a `broadcast_batch` that cannot get sockets returns
`Retryable` for the whole batch and requeues it into a later window, so an
anonymous outsider gets a lever on *when* a batch is published.

**Attack Requirements and Assumptions:**

- **Network access only.** No credential, no txid, no mixnet position, no Zcash
  knowledge, no funds. The lookup path is not gated by `http_submit`, so it is
  open in every deployment the repository ships, including the intended
  mixnet-only one.
- **What makes it realistic:** the request is content-free random bytes; the
  amplification is a full TLS handshake, a socket and an ephemeral port for
  ~100 bytes; the target has two vCPUs; and the identical arm on the mixnet
  transport is bounded at 64 with drop-on-full precisely because the authors
  identified this attack there.
- **What limits it:** the flood is noisy and originates from identifiable
  addresses — but there is no per-IP block list here to evade, because there is
  no per-IP anything, and no operator-visible signal (see Impact). The exact
  concurrency needed depends on the enclave's `RLIMIT_NOFILE` and
  `ip_local_port_range`, neither of which could be measured in this environment;
  the Containerfile and the manifest set neither, so both are the platform
  defaults. The *direction* is not in doubt: nothing in the application bounds
  concurrency, so the attacker reaches whichever ceiling is lowest, and every
  one of them is shared with the batcher.

## Impact on Users

- **Silent loss of transactions users were told had been sent.** Under
  dispatch-only submit the wallet already saw `error_code 0`. A `TipStale`
  refusal, or a repeatedly-failing flush, destroys the migration with no error
  surfaced anywhere the user can see. This is the same terminal state as the
  confirmed High `hub-queue-unauthenticated-fill-silently-destroys-migrations.md`,
  reached over clearnet at a small fraction of the cost and denying **100 %** of
  submissions rather than a bandwidth-proportional fraction.
- **A privacy failure, not only an availability one.** A hub that refuses
  admission makes every shim's divert fail closed; wallets retry, and a user
  whose transaction will not send eventually points their wallet at a different,
  unprotected light-wallet server and broadcasts the Orchard-touching
  transaction in the clear. That is precisely the leak the product exists to
  prevent, and the attacker chooses the moment it happens.
- **Batch timing becomes an outsider's input.** Making a flush's
  `broadcast_batch` fail defers a whole batch to a later window. Choosing when a
  batch is published, and keeping a hub from accepting during a chosen interval,
  is an anonymity-set attack: it shrinks the set any given migration is mixed
  with.
- **Nothing reports the condition.** `GET /healthz` returns 200 unconditionally
  (`server.rs:450-453`); `GET /nym-status` reports only mixnet-client lifecycle;
  the tip-poll failure is logged at `debug!` (`batcher.rs:293`) and
  `caution.hcl.tmpl:139` leaves `RUST_LOG` at the default `info`, so it is not
  even emitted. An operator monitoring the hub sees green throughout. (Filed
  separately as `hub-health-surface-blind-to-the-states-that-destroy-migrations.md`.)

## Technical Details / Code Analysis

**The route is unconditional and the handler is unbounded.**

```rust
// hub/src/server.rs:437-448
    match req.uri().path() {
        SUBMIT_PATH if options.http_submit => match method {
            Method::POST => submit(req, hub).await,
            _ => Ok(text(StatusCode::METHOD_NOT_ALLOWED, "POST only")),
        },
        TRANSACTION_PATH => match method {
            Method::POST => lookup(req, hub).await,
            _ => Ok(text(StatusCode::METHOD_NOT_ALLOWED, "POST only")),
        },
```

```rust
// hub/src/server.rs:487-505
async fn lookup(req: Request<Incoming>, hub: Hub) -> Result<Response<Full<Bytes>>, Infallible> {
    let collected = match Limited::new(req.into_body(), MAX_LOOKUP_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected,
        Err(_) => return Ok(text(StatusCode::PAYLOAD_TOO_LARGE, "lookup key too large")),
    };
    let wire_hash = collected.to_bytes();
    if wire_hash.is_empty() {
        return Ok(text(StatusCode::BAD_REQUEST, "empty lookup key"));
    }

    match hub.lookup(&wire_hash).await {
```

`MAX_LOOKUP_BYTES = 64` bounds the *body*, which is not the resource under
attack — it is what makes the attack cheap. There is no other bound on this
path.

**The accept loop spawns without limit.**

```rust
// hub/src/server.rs:390-407
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            if let Err(err) = http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(
                    io,
                    service_fn(move |req| handle(req, hub.clone(), options.clone())),
                )
                .await
```

The installed `TokioTimer` correctly re-enables hyper's 30 s header-read timeout
(the comment at `server.rs:392-396` explains why), but that only bounds *idle*
connections. A connection that keeps sending complete, valid requests is never
throttled, and there is no ceiling on how many such connections exist.

**A queue miss is guaranteed for random input, and the miss is the expensive
branch.**

```rust
// hub/src/server.rs:296-303
    pub async fn lookup(&self, wire_hash: &[u8]) -> LookupOutcome {
        if let Some(bytes) = self.queue.find_by_txid(wire_hash) {
            tracing::debug!(source = "queue", "transaction lookup answered");
            return LookupOutcome::Found { data: bytes, height: 0 };
        }

        match self.chain.get_transaction(wire_hash).await {
```

**Each miss is a fresh dial per endpoint.**

```rust
// hub/src/chain.rs:221-232
    pub async fn get_transaction(&self, wire_hash: &[u8]) -> Result<TxLookup, BoxError> {
        let calls = self.endpoints.iter().map(|addr| {
            let filter = TxFilter { block: None, index: 0, hash: wire_hash.to_vec() };
            async move {
                match self.unary::<_, RawTransaction>(*addr, GET_TRANSACTION, filter).await
```

```rust
// hub/src/chain.rs:310-334
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        ...
        let response = match &self.tls {
            Some(tls) => {
                let stream = tls.connect(addr, stream).await?;
                round_trip(stream, request).await?
            }
```

**The bound that exists on the other transport, and what it really bounds.**

```rust
// hub/src/nym.rs:54
const MAX_CONCURRENT_LOOKUPS: usize = 64;
```

```rust
// hub/src/nym.rs:171-176
            let permit = match lookups.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    tracing::info!(
                        in_flight = MAX_CONCURRENT_LOOKUPS,
```

The permit is taken before the task is spawned and released only after the reply
has been accepted by the driver's channel (`nym.rs:169-206`), so it does bound
**concurrent indexer dials at 64** — which is the resource at issue here. (The
G21 pass established that the same permit does *not* bound mixnet *emission*,
because `outgoing` accepts within seconds; that is a different resource and a
different, already-confirmed finding.)

`hub/src/nym.rs:17-22` claims the two ingress paths cannot drift:

> Admission is `crate::server::Hub::admit` and lookup is
> `crate::server::Hub::lookup`, the exact calls the HTTP serving path uses, **so
> the two ingress paths cannot drift.**

They share the *core* but not the *admission control around it*, and the
admission control is the security property.

**The collateral victim, and why it is silent.**

```rust
// hub/src/batcher.rs:290-296
        match chain.tip_height().await {
            Ok(height) => tip.observe(height),
            Err(err) => {
                tracing::debug!(%err, "tip query failed on every node");
            }
        }
```

```rust
// hub/src/batcher.rs:205-208
    pub fn is_stale(&self) -> bool {
        let state = self.read();
        !state.observed || state.last_advance.elapsed() > TIP_STALE_AFTER
    }
```

## Recommendations

1. **Bound concurrent indexer dials across *all* ingress.** Give `ChainClient`
   (or `Hub`) a single `Arc<Semaphore>` and acquire it in `Hub::lookup`, so the
   HTTP arm and the mixnet arm draw from one budget. On failure to acquire,
   answer `503` immediately rather than parking the request. This is the
   smallest change that closes the amplification and is the one the mixnet arm
   already models.
2. **Pool or reuse indexer connections in `chain.rs`.** One long-lived HTTP/2
   connection per endpoint removes the per-call handshake, the per-call
   descriptor and the per-call ephemeral port in one change, and makes the tip
   poll robust under load. This is the single highest-value fix here, and it
   also addresses `hub-chain-connection-per-call-fanout-and-flush-memory-amplification.md`.
3. **Gate `POST /transaction` behind a config flag the way `POST /` is gated**,
   defaulting off. In the deployed topology the shim uses the mixnet `LookupV1`
   path, so the clearnet lookup is transitional exactly like clearnet submit.
   This also closes the confirmed
   `hub-unauthenticated-pre-publication-transaction-disclosure.md`.
4. **Cap concurrent connections in `serve`** with a semaphore acquired before
   `tokio::spawn`. A 2 GB, 2 vCPU enclave on an internet-facing ingress should
   not accept unbounded connections.
5. **Reject lookup bodies that are not exactly 32 bytes.** It does not fix the
   flood, but it removes a free variant and matches what a `TxFilter.hash` is.
6. **Reserve headroom for the hub's own egress.** Even with (1), the tip poll
   and `flush` should not compete with lookups for the last descriptors: give
   the batcher its own permit outside the lookup budget.
7. **Surface the condition.** Raise the tip-poll failure above `debug!`, and see
   `hub-health-surface-blind-to-the-states-that-destroy-migrations.md`; without
   it this attack has no detection signal at all.

## Validation Information

**Verdict: CONFIRMED. Severity raised from the filed Medium to High**, on the
same reasoning that put `hub-queue-unauthenticated-fill-silently-destroys-migrations.md`
at High: the attacker's marginal cost is ~100 bytes, the hub's is a socket, a
port and a TLS handshake, and the terminal state is not downtime but silent
destruction of migrations the wallet was told had succeeded.

### Every mechanical claim re-verified against the target

| Claim | Verified at |
|---|---|
| `POST /transaction` is routed unconditionally, with no `http_submit` guard and no auth | `hub/src/server.rs:437-448` — the `TRANSACTION_PATH` arm has no `if` |
| Body is capped at 64 bytes, so the request is tiny by construction | `hub/src/server.rs:84-87` (`MAX_LOOKUP_BYTES = 64`), `:488-494` |
| A random 32-byte body always misses the queue | `hub/src/queue.rs:328-348` — `find_by_txid` compares against 64-char hex `Entry.txid` strings |
| A miss dials the indexer once per endpoint | `hub/src/server.rs:302-306` → `hub/src/chain.rs:221-249` |
| No connection pool exists anywhere in `chain.rs` | `ChainClient` fields are `endpoints` + `tls` only (`chain.rs:123-129`); `unary_inner` calls `TcpStream::connect` on every invocation (`:310`) and `round_trip` performs a fresh h2 handshake (`:349-351`), whose sender is dropped on return |
| No concurrency bound, rate limit or per-peer accounting on the HTTP path | `hub/src/server.rs:354-408` — `tokio::spawn` per accepted connection, nothing acquired |
| The mixnet arm *does* bound concurrent dials at 64 | `hub/src/nym.rs:153` (`Semaphore::new(MAX_CONCURRENT_LOOKUPS)`), `:171-206` — permit taken before spawn, dropped only after `outgoing.send()` returns |
| The constant's own rationale names this attack | `hub/src/nym.rs:38-54` — *"small enough that a flood cannot open unbounded connections"* |
| The tip poll and the flush use the same `ChainClient` | `hub/src/main.rs:39` constructs one `Arc<ChainClient>`, passed to `batcher::run` at `:78-85` and to `Hub` at `:111-118`; `batcher.rs:290`, `:353` |
| 15 min without a tip advance closes admission for everyone | `hub/src/batcher.rs:62`, `:205-208`, `:161-190` (`observe` refreshes `last_advance` only on advance); `hub/src/server.rs:248-254` |
| The refusal never reaches the wallet | `shim/src/hub.rs:231-240` (returns `Submit::Accepted` at hand-off, *"a hub refusal is never surfaced here"*); pinned by `shim/tests/divert_nym.rs:235-267` |
| `/healthz` is unconditionally 200 | `hub/src/server.rs:450-453` |
| The enclave is 2 vCPU / 2 GB and internet-reachable on the HTTP port | `hub/deploy/caution/caution.hcl.tmpl:39-40`, `:51-55`, `:97-105` |
| The code already treats descriptor exhaustion as a reachable condition | `hub/src/server.rs:341-352` (`is_fd_exhaustion`), `:381-385` |

### `AVOIDING-FALSE-POSITIVES.md` §5 applied in both directions

§5's canonical false positive is *"Unlimited concurrent connections … OS limits
(ulimit), nginx (worker_connections), firewall rules apply first; **each
connection uses minimal resources**"*. Three things take this issue out of that
shape, and they are the same three §5 names as the *real* pattern:

1. **The connection does not use minimal resources.** One ~100-byte inbound
   request causes one outbound TCP connect, one TLS handshake with chain
   verification, one h2 handshake and up to 10 s of holding, *per endpoint*.
   That is §5's own "Real Issue: single connection consuming unbounded
   resources" / "1 KB request causing disproportionate work".
2. **The infrastructure that normally absorbs a flood is absent by
   construction.** There is no proxy pooling the *outbound* leg —
   `chain.rs` deliberately dials fresh every call — and the in-enclave Caddy in
   front of the *inbound* leg is a plain reverse proxy with no rate limit and no
   upstream connection cap in the shipped manifest. Nothing sits between the
   attacker and `Hub::lookup`.
3. **"OS limits apply first" is the harm, not the mitigation.** The descriptor
   limit, the ephemeral-port range and the two vCPUs are shared with the hub's
   own outbound path. Reaching them stops the tip poll and the flush, which is
   the whole attack.

And the outcome is not the downtime §5 discounts. Because `shim/src/hub.rs:238-240`
acknowledges at mixnet hand-off, a hub that stops admitting is a hub that
silently destroys transactions users believe are spent — the *"violates
integrity guarantees"* row of the severity table, not the *"causes DoS"* row.

### Corrections made to the filed text

- **Severity Medium → High**, per the above.
- **The CPU arithmetic was replaced.** The filing's "order 100 connections … is
  ~5,000 handshakes/s of demand" conflated concurrency with rate and was not
  derived. The mechanisms are now stated in the order they actually bind, with
  ephemeral-port exhaustion (a hard, deterministic `EADDRNOTAVAIL` at ~470
  connections/s against a *single* destination on stock Linux settings) added as
  the sharpest one, and each marked as depending on platform defaults that the
  Containerfile and manifest do not set and that could not be measured here.
- **The transport premise was corrected.** The filing described the ingress as
  raw TCP on 8083. PROGRESS.md item 5/6 settled that `http { port = 8083 }`
  makes it Caddy-proxied from 443; that does not reduce reachability (Caddy has
  no rate limit or upstream cap here) but the text now says so accurately.
- **The mixnet comparison was made precise.** The G21 pass showed
  `MAX_CONCURRENT_LOOKUPS` fails to bound mixnet *emission*. It does bound
  concurrent *indexer dials*, which is the resource at issue here, so the
  comparison stands and is now stated with that scope.
- **The "composes with a cheaper operator variant" bullet was moved out.** The
  uncapped indexer response body is its own confirmed issue
  (`hub-chain-unbounded-indexer-response-body.md`); the composition is noted
  there rather than argued twice.
- The withdrawn amplification-loop premise recorded in PROGRESS.md item 7b-REFUTED is not relied on anywhere in this issue; the cost ratio here is a plain per-request one.

### What this issue does *not* claim

- It does not claim the enclave OOMs. Memory is the subject of
  `hub-chain-unbounded-indexer-response-body.md`; the multiplication of the two
  is real (G21 §4.3) and is noted in both, counted in neither.
- It does not re-argue the confidentiality of `POST /transaction`; that is the
  confirmed `hub-unauthenticated-pre-publication-transaction-disclosure.md`.
  Recommendation 3 is shared between them.
- It does not claim the flush-side `k × n` fanout, which belongs to
  `hub-chain-connection-per-call-fanout-and-flush-memory-amplification.md`.
  Recommendation 2 is shared.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
