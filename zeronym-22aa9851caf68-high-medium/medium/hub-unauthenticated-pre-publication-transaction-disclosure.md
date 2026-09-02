# Unauthenticated transaction lookup discloses queued, not-yet-published migrations and lets a third party steal the broadcast

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/server.rs:429-458` (`handle`, routing), `:487-505` (`lookup`), `:508-517` (`found`), `:296-322` (`Hub::lookup`); `audit-target/zeronym/hub/src/queue.rs:328-348` (`Queue::find_by_txid`); `audit-target/zeronym/hub/src/nym.rs:249-300` (`build_lookup_reply`, the same core over the mixnet); `audit-target/zeronym/hub/src/chain.rs:513-533` (`classify_publish_error`); `audit-target/zeronym/hub/src/batcher.rs:361-378` (`flush`); `audit-target/zeronym/hub/deploy/caution/caution.hcl.tmpl:51-55, 96-105` (`ingress 0.0.0.0/0` + the Caddy-proxied `http` block)
**Found by agent:** Brainstorming Agent; validated and substantially revised by the Issue Validator
**In scope of audit?** Yes

## Description

The hub answers a transaction lookup from its **queue first**, and a queue hit
returns the **raw bytes of a diverted migration that has not yet been broadcast
anywhere**, with `x-tx-height: 0`. The lookup is unauthenticated on **both**
transports:

* **Clearnet.** `POST /transaction` is served unconditionally. Unlike the submit
  path `POST /`, which is gated behind `ServeOptions::http_submit` (off by
  default) and falls through to a `404`, the lookup path has **no gate at all**
  (`server.rs:437-445`). The enclave declares `ingress { cidr_ipv4 = "0.0.0.0/0" }`
  and the platform maps the hub's TLS domain onto it, so the endpoint is
  reachable by anyone on the internet.
* **Mixnet.** `LookupV1` frames are answered through *exactly the same*
  `Hub::lookup` core (`nym.rs:249-300`), the hub's Nym address is published by
  design at `GET /nym-address`, and there is no submitter ACL. **Gating the
  clearnet path therefore does not close this**; it only raises the cost from
  `curl` to a Nym client.

Three distinct capabilities follow, with very different preconditions:

1. **Pre-publication disclosure of the transaction body**, to anyone who holds a
   candidate txid, up to ~25 minutes before the transaction exists anywhere else
   on the network.
2. **A residency oracle.** `200` + `x-tx-height: 0` means "queued here,
   unbroadcast"; `404` means "not here"; the 200→404 transition timestamps the
   flush. A queue hit also returns in microseconds while a miss pays a full
   indexer round trip (up to `RPC_TIMEOUT` = 10 s, `chain.rs:48`), so a latency
   oracle survives even if the body and the header were removed. This leg needs
   **no txid at all** if the attacker probes with their own submissions.
3. **Broadcast theft.** Whoever retrieves the bytes can broadcast them
   themselves, from a node of their choosing, at an instant of their choosing.
   The hub's later flush is then answered "already known", which
   `classify_publish_error` maps to `Publish::AlreadyKnown`, which `flush` counts
   as **achieved** and drops. Nothing anywhere records that the transaction did
   not enter the network from the hub, at the hub's cadence, with the batch.

Leg 3 is the one that defeats a claimed protection: the product's mechanism is
that the transaction is published **by the hub, on a fixed cadence, together
with others**. This lets a third party choose the publisher and the moment,
silently.

> **CORRECTION carried forward (coordinator, 2026-08-17), and now the settled
> reading.** An earlier revision of this issue also claimed the Nitro **parent
> host** could read these bodies off the wire with a packet capture. **That claim
> is withdrawn.** `caution.hcl.tmpl:96-105` declares
> `http { domain = ...; port = 8083; e2e_encryption { mode = "tls" } }`, which is
> Caution's *in-enclave* TLS termination: the platform runs Caddy inside the
> enclave, holds the private key there, and forwards plaintext only over the
> enclave-internal loopback to `ZIH_LISTEN=0.0.0.0:8083`. The parent host sees
> ciphertext on this hop. The attacker set for this issue is therefore
> **callers** — which is still the entire internet, unauthenticated. The
> withdrawn claim does **not** affect legs 2 and 3, which never depended on it.
> It also does **not** apply to the hub's *outbound* hop to its indexer, where
> `ZIH_INDEXER_TLS` really is the only thing keeping the parent host out of every
> batch (see `caution.hcl.tmpl:128-131` and the separate issues on that hop).

## Attack Scenario and Steps

**Leg 3 (the damaging one): steal the broadcast of a user whose txid you hold.**

1. The wallet is handed the txid the instant it submits: the shim answers
   `SendTransaction` with a synthesized `SendResponse { error_code: 0,
   error_message: <txid> }` computed locally (`shim/src/hub.rs:238-240`,
   `shim/src/intercept.rs:179-186`). The user therefore has, and can share, the
   txid ~25 minutes before the transaction is public.
2. The attacker obtains that txid out of band — a counterparty or merchant given
   it as a payment reference, a support channel, a screenshot, wallet telemetry,
   or another app on the device.
3. `POST /transaction` to the hub with the 32-byte wire-order hash (or the
   equivalent `LookupV1` frame over Nym). The hub answers `200`,
   `content-type: application/octet-stream`, `x-tx-height: 0`, body = the full
   raw transaction.
4. The attacker submits those exact bytes to a Zcash node of their choosing, at a
   moment of their choosing, before the hub's next 20-block flush.
5. At the flush, `chain::broadcast_batch` offers the same bytes to the hub's
   indexer, the node answers already-known, `classify_publish_error` returns
   `AlreadyKnown`, `flush` increments `achieved` and drops the entry
   (`batcher.rs:367`). `achieved_batch_size` logs a normal, healthy flush.
   *(If the node's wording is one `classify_publish_error` does not match, the
   entry is classed `Rejected` and dropped instead — the outcome for the user is
   identical; only the hub's counter differs.)*

**Leg 2 (no txid needed): measure the flush.** Submit a transaction of your own
(the mixnet submit path is unauthenticated and the hub's address is published),
compute its txid locally from your own bytes, and poll the lookup. The 200→404
transition gives the flush instant to the second.

**Attack Requirements and Assumptions:**

- **Network access only.** No credential, no enclave compromise, no mixnet
  position for the clearnet leg; a Nym client and the published hub address for
  the mixnet leg.
- **Legs 1 and 3 require a candidate txid**, which is not guessable
  (256-bit) and cannot be enumerated. Under ZIP 244 a txid is computable *from*
  the transaction bytes, so nobody derives it from the chain before publication.
  The realistic holders are the user and anyone the user tells within the ≤25
  minute window. **This is a targeted attack, not a mass one.**
- **Leg 2 requires nothing** beyond the ability to submit and to look up.
- **What makes it realistic:** the endpoint is on by default, ungated, and
  reachable from the whole internet; it has **no legitimate caller in the
  deployed topology at all**, because `HubTransport` is clearnet XOR mixnet
  (`shim/src/hub.rs:219-227`) and `deploy.env.example:18` configures the mixnet;
  and `smoke.sh:315-327` asserts only that `POST /` is closed, never checking
  `POST /transaction`.
- **What bounds it:** the marginal value of the *body* over the txid alone is
  small — the same bytes are public on chain within ~25 minutes, and
  `hub/REVIEW.md:177` already concedes that batch membership is publicly
  enumerable after the fact. The genuinely new capability is leg 3.

## Impact on Users

- **For a targeted user whose txid reaches a hostile party inside the window:**
  the core protection is nullified and nothing notices. Their transaction is
  published by an attacker-chosen node at an attacker-chosen instant, so
  "published by the hub, on a cadence, mixed with others" does not hold for them,
  and the ~25-minute delay that decorrelates their submission from the on-chain
  appearance can be collapsed to seconds. Because `AlreadyKnown` counts as
  achieved, **no component reports anything wrong**: not the wallet (told
  `error_code 0` at submit), not the hub's telemetry, not the operator, not the
  user.
- **The batch loses that member**, so every other user in that flush gets a
  smaller on-chain anonymity set than the hub believes it delivered.
- **For anyone:** a live residency and flush-timing oracle. Its practical value
  is bounded — the cadence is public by design and `hub/REVIEW.md:177` states
  that the batch is publicly identifiable from chain data anyway — but it
  contradicts the hub's own stated invariant that read-only endpoints must not
  become an anonymity-set oracle (`server.rs:22-25`, `queue.rs:297-299`).
- **Pre-publication body disclosure** is real but mostly costs earliness: value
  balance, anchor, expiry and action count all become public on chain shortly
  afterwards.

## Technical Details / Code Analysis

**1. The lookup path is ungated while the submit path is gated** — `hub/src/server.rs:437-445`:

```rust
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

`hub/src/server.rs:487-505` buffers up to `MAX_LOOKUP_BYTES` (64) and rejects
only an *empty* key — no length check, no authentication, no rate limit:

```rust
async fn lookup(req: Request<Incoming>, hub: Hub) -> Result<Response<Full<Bytes>>, Infallible> {
    let collected = match Limited::new(req.into_body(), MAX_LOOKUP_BYTES).collect().await { ... };
    let wire_hash = collected.to_bytes();
    if wire_hash.is_empty() {
        return Ok(text(StatusCode::BAD_REQUEST, "empty lookup key"));
    }
    match hub.lookup(&wire_hash).await {
        LookupOutcome::Found { data, height } => Ok(found(&data, height)),
        LookupOutcome::NotFound => Ok(text(StatusCode::NOT_FOUND, "transaction not found")),
        LookupOutcome::Unavailable => Ok(text(StatusCode::BAD_GATEWAY, "indexer unavailable")),
    }
}
```

**2. The queue is consulted first and a hit returns unpublished plaintext** —
`hub/src/server.rs:296-303`:

```rust
    pub async fn lookup(&self, wire_hash: &[u8]) -> LookupOutcome {
        if let Some(bytes) = self.queue.find_by_txid(wire_hash) {
            tracing::debug!(source = "queue", "transaction lookup answered");
            return LookupOutcome::Found { data: bytes, height: 0 };
        }
```

`hub/src/queue.rs:328-348` linear-scans the queue, matches **both** byte orders
of the supplied hash, and returns a copy of the entry's bytes. The height
sentinel is an explicit response header (`hub/src/server.rs:508-517`):

```rust
fn found(tx_bytes: &[u8], height: u64) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::copy_from_slice(tx_bytes)));
    resp.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    resp.headers_mut().insert(TX_HEIGHT_HEADER, HeaderValue::from(height));
    resp
}
```

`server.rs:90-91` documents `0` as "mempool (a queued, unflushed transaction),
matching lightwalletd's sentinel".

**3. The mixnet path is the same core, also unauthenticated** —
`hub/src/nym.rs:275-279`:

```rust
    let reply = match hub.lookup(&hash).await {
        LookupOutcome::Found { data, height } => LookupReply::Found { height, tx: data },
        LookupOutcome::NotFound => LookupReply::NotFound,
        LookupOutcome::Unavailable => LookupReply::Error,
    };
```

This is why gating `POST /transaction` alone is not a fix: the module header at
`nym.rs:17-18` states outright that admission and lookup are "the exact calls the
HTTP serving path uses", and the hub publishes its Nym address to everyone.

**4. Already-known is counted as success and the entry is dropped** —
`hub/src/chain.rs:513-533`:

```rust
fn classify_publish_error(message: &str) -> Publish {
    let m = message.to_ascii_lowercase().replace('-', " ");
    if m.contains("already in block chain") || m.contains("already known")
        || m.contains("already in mempool") || m.contains("duplicate")
    { Publish::AlreadyKnown } else { Publish::Rejected { reason: message.to_string() } }
}
```

and `hub/src/batcher.rs:365-378`:

```rust
    for (i, entry) in batch.into_iter().enumerate() {
        match outcomes.get(i) {
            Some(Publish::Accepted { .. }) | Some(Publish::AlreadyKnown) => achieved += 1,
            Some(Publish::Rejected { .. }) => rejected += 1,
            Some(Publish::Retryable { reason }) => { ...; unplaced.push(entry); }
            None => unplaced.push(entry),
        }
    }
```

Only `Retryable`/`None` are requeued, so an `AlreadyKnown` entry is neither
retried nor flagged; it increments the number the hub logs as
`achieved_batch_size`. The comment at `batcher.rs:356-358` explains why
already-known must count as success ("with every shim submitting to every hub,
the second hub's publish is already-known by construction") — sound reasoning
that cannot distinguish a sibling hub from an attacker who published first.

**5. Exposure** — `hub/deploy/caution/caution.hcl.tmpl:51-55` and `:96-105`:

```hcl
    ingress { cidr_ipv4 = "0.0.0.0/0"; port = 8083; ip_protocol = "tcp" }
    ...
    http { domain = "__TLS_DOMAIN__"; port = 8083; e2e_encryption { mode = "tls" } }
```

The hub process itself speaks plain HTTP/1.1 (`hub/src/tls.rs` is a *client* to
the indexer only); the TLS a caller sees is terminated by the in-enclave Caddy.
The exposure is the open, unauthenticated ingress — not a cleartext wire.

**6. The stated invariant this sits against** — `hub/src/server.rs:22-25` requires
that these read-only endpoints never become a live anonymity-set oracle, and
`hub/src/queue.rs:297-299` refuses to return queue depth for the same reason.

## Recommendations

In order of value:

1. **Do not serve an unpublished transaction's bytes to an unauthenticated
   lookup.** A queue hit can answer `x-tx-height: 0` with an empty body (enough
   for a wallet to render "pending"), or `NotFound`. This closes legs 1 and 3 on
   **both** transports at once and is the only recommendation that does.
2. **Break the "already known ⇒ achieved" equivalence for an entry this hub has
   not previously offered.** Record whether a prior flush of the same entry
   returned `Accepted`/`Retryable`, and surface a *first-flush* `AlreadyKnown` as
   a counter-level anomaly. Today that event is indistinguishable from success.
3. **Gate `POST /transaction` behind an explicit flag, defaulted off**, the way
   `POST /` already is. Note this is a *reduction in exposure, not a fix* — the
   mixnet lookup path remains open — but the clearnet endpoint has no legitimate
   caller in the deployed mixnet topology, so it is free to remove.
4. **Authenticate the hub's ingress** (the STEVE / mutual-attestation work
   `OPEN-QUESTIONS.md` §3 records as designed-not-built). Until it exists, every
   hub endpoint is hostile-reachable and should disclose nothing.
5. **Extend `smoke.sh`** to assert the disposition of `POST /transaction`
   alongside its existing `POST /` assertion (`smoke.sh:315-327`).

## Validation Information

**Verdict: CONFIRMED at Medium** (downgraded from the filed High).

**What was verified directly in the target:**

- `handle` routes `TRANSACTION_PATH` with no `options.*` guard while `SUBMIT_PATH`
  carries `if options.http_submit` — `server.rs:437-445`. `ServeOptions::http_submit`
  defaults false (`server.rs:198`), and `hub/tests/endpoints.rs:170-181`
  (`the_lookup_path_is_not_gated_by_the_submit_flag`) pins the ungated behaviour
  as intended.
- `Hub::lookup` consults `Queue::find_by_txid` before the indexer and returns
  `height: 0` on a hit — `server.rs:296-303`; `queue.rs:328-348` returns a copy of
  `entry.tx_bytes`, matching either byte order.
- `found` writes the raw bytes as the body plus `x-tx-height` — `server.rs:508-517`.
- The enclave manifest opens `0.0.0.0/0` on 8083 and the platform maps the TLS
  domain onto that same port — `caution.hcl.tmpl:51-55`, `:96-105`. `smoke.sh`
  reaches `/nym-status`, `/nym-address`, `/healthz` and `POST /` over the public
  URL, so the listener is demonstrably internet-reachable.
- `classify_publish_error` → `AlreadyKnown` and `flush` counting it as `achieved`
  without requeue — `chain.rs:513-533`, `batcher.rs:365-378`.
- The shim hands the wallet a locally computed txid at submit time —
  `shim/src/hub.rs:231-240` (`Submit::Accepted { txid: crate::nym::local_txid(...) }`),
  rendered into the `SendResponse` at `shim/src/intercept.rs:179-190`. This is the
  mechanism by which a txid exists, and can leak, before publication.

**Corrections made to the issue during validation:**

- The withdrawn parent-host packet-capture claim has been **removed from the body
  entirely** rather than left as struck-through text, and the surviving legs
  restated on their own merits. The correction note is retained once, as the
  record of why.
- **The filed recommendation "gate `POST /transaction` … closes the whole issue
  for the deployed topology in one line" was wrong and has been corrected.**
  `hub/src/nym.rs:249-300` answers `LookupV1` through the identical `Hub::lookup`
  core, the hub's Nym address is published at `GET /nym-address` by design, and
  there is no submitter ACL — so the disclosure and the oracle both survive the
  clearnet gate. Only refusing to serve unpublished bytes closes them.
- Added that a mis-matched already-known message classes as `Rejected` and is
  *also* dropped, so leg 3 does not depend on `classify_publish_error`'s string
  list matching the node's wording.

**Why Medium and not High:**

- Legs 1 and 3 are gated on holding a 256-bit txid within a ≤25 minute window.
  Under ZIP 244 a txid is computed *from* the transaction, so no chain observer,
  mempool watcher, or the operator can derive one before publication; realistically
  only the user and parties the user tells hold it. That makes this a **targeted**
  attack against a user whose txid leaked, not a mass one — it fails the "many
  users" bar for High.
- The marginal value of the disclosed *body* over the txid alone is roughly 25
  minutes of earliness: the same transaction is public on chain shortly after, and
  `hub/REVIEW.md:177` already concedes batch membership is publicly enumerable.
- The freely-available leg (the residency/flush oracle) measures something the
  design already treats as public: the cadence is deliberately deterministic and
  publicly known (`batcher.rs:8-17`), and the batch is publicly identifiable
  (`REVIEW.md:177`). It is a real violation of `server.rs:22-25`'s stated
  invariant, but its independent user harm is small.

**Why not Low, and why not invalid:**

- Leg 3 is a genuine, silent defeat of a claimed protection for the user it hits:
  the transaction leaves the batch, the timing decorrelation is gone, and every
  detection channel reports success. That is a serious outcome available in
  realistic (if particular) circumstances, which is the definition of Medium.
- The exposure is gratuitous: there is no legitimate clearnet caller in the
  deployed topology, since `HubTransport` is clearnet **xor** mixnet
  (`shim/src/hub.rs:219-227`) and `deploy.env.example:18` configures the mixnet.

**Related issues, deliberately not restated here:** the unbounded concurrency /
upstream-amplification consequence of the same ungated endpoint is filed as
`hub-http-lookup-path-has-no-concurrency-bound.md`; the false rationale in the
test that certifies the path open is filed as
`hub-endpoints-test-certifies-the-ungated-clearnet-lookup-path-on-a-rationale-that-is-false-for-the-deployed-transport.md`;
the runbook omission is filed as
`hub-operators-runbook-endpoint-inventory-omits-the-unauthenticated-lookup-endpoint.md`.
Where the lookup *misses* the queue and is forwarded upstream, see
`hub-lookup-fall-through-hands-every-wallets-txid-to-whichever-indexer-the-hub-is-pointed-at-which-the-shipped-config-makes-the-operator.md`.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
