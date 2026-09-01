# The indexer decides which members of a flushed batch reach the chain, so the on-chain batch size is chosen by that party regardless of how large the hub's batch is

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/batcher.rs:341-390` (`flush`: drain, `broadcast_batch`, the achieved/rejected/requeue split), `:396-420` (the telemetry); `audit-target/zeronym/hub/src/chain.rs:176-210` (`broadcast`, `broadcast_batch`), `:269-337` (`unary`/`unary_inner`: one fresh connection and one 10 s budget per call), `:459-474` (`best_of`), `:491-501` (`classify_publish_failure`); `audit-target/zeronym/hub/src/queue.rs:279-295` (`requeue`), `:328-348` (`find_by_txid`); `audit-target/zeronym/hub/src/server.rs:296-303` (`Hub::lookup` answers a queued entry with the mempool sentinel). Deployed endpoint count: `audit-target/zeronym/deploy.env.example:22`. Claims contradicted: `audit-target/zeronym/hub/src/batcher.rs:8-22`, `audit-target/zeronym/README.md:27` and `:34`, `audit-target/zeronym/hub/REVIEW.md` #2 and #8.
**Found by agent:** Global (focus areas G2 "the flush clock as an attack surface" / G14 "admission→flush→publish as one system" / G26 "isolate a target's migration into a batch of one")
**In scope of audit?** Yes

## Description

Every defence in the hub's batching design protects the **decision** to publish.
`batcher.rs:8-22` and `hub/REVIEW.md` #2 and #8 are entirely about making the
moment of the flush unreachable by an attacker: no count trigger, no expiry
trigger, no stale-tip trigger, max-over-nodes tip acquisition, admission control
instead of an early-expiry flush.

Nothing protects the **act** of publishing. Between `flush()` deciding to publish
and the transactions entering a mempool sits the indexer named in `ZIH_INDEXERS`,
and that party's **per-transaction** answer decides, individually and
independently for each batch member, whether that member reaches the network at
this flush or is put back in the queue for a later one.

The mechanism is `Publish::Retryable`. `flush` splits the batch three ways
(`batcher.rs:365-378`): `Accepted`/`AlreadyKnown` are dropped as delivered,
`Rejected` is dropped as destroyed, and **`Retryable` is requeued and offered
again at the next flush**. `classify_publish_failure` (`chain.rs:491-501`) maps
every gRPC status except `INVALID_ARGUMENT` (3) and `FAILED_PRECONDITION` (9) to
`Retryable` — including `UNAVAILABLE` (14), the status a healthy,
correctly-implemented gRPC service returns when momentarily overloaded, and
therefore the least suspicious answer available.

So an indexer that wants exactly one transaction from a batch of `k` to appear on
chain answers `SendResponse { error_code: 0, error_message: "<txid>" }` for that
one and relays it, and gRPC `UNAVAILABLE` for the other `k-1` and relays nothing.
The hub publishes a batch of `k`; the chain receives a batch of **1**. The other
`k-1` come back at the next flush, where the choice can be repeated.

This is the isolation attack `REVIEW.md` #2 was written to prevent
("Count-based flushing lets an attacker submit 99 of their own migrations to
isolate a target's", `batcher.rs:9-11`), reached by a route neither #2 nor #8
considers, because both reason about the queue and the clock and neither reasons
about the hop after them.

**It is adoption-proof.** `README.md:34` states the residual as "the modal batch
is zero or one … The lever is adoption, not code." Raising adoption raises `k`;
this sets the on-chain batch to 1 for any `k`.

## Attack Scenario and Steps

Attacker: the operator of the indexer the hub broadcasts through, or anyone who
compromises or compels it. The audit's threat model names the capability
explicitly — *"hub → indexer: sees the whole batch seconds before it is public;
can lie about the tip and **about publish verdicts**"*. With the shipped
`INDEXERS=66.241.124.200:443` (`deploy.env.example:22`) there is exactly one such
party and `best_of` has one verdict to fold.

1. The cadence fires. `flush` drains the queue and calls
   `chain.broadcast_batch(&payloads)` (`batcher.rs:342`, `:355`).
2. `broadcast_batch` issues one `broadcast` per transaction concurrently
   (`chain.rs:208-210`), and each `broadcast` issues one `unary` per endpoint
   (`chain.rs:181-197`). `unary_inner` dials a **fresh TCP + TLS + h2 connection
   per call** (`chain.rs:300-334`), so the indexer receives `k` separate,
   simultaneous `SendTransaction` requests, each carrying one raw transaction in
   the clear.
3. Each call has a 10 s budget (`chain.rs:48`, `:280-291`). The indexer therefore
   holds the entire batch, with up to ten seconds to decide, before it has to
   answer any of them. It selects a victim by content — value balance, action
   count, length, `anchorOrchard`, `nExpiryHeight` — or by a txid supplied out of
   band.
4. It relays the victim's transaction to its node and answers
   `SendResponse { error_code: 0, error_message: "<the real txid>" }`.
   `classify_send_response` (`chain.rs:438-445`) makes that `Publish::Accepted`,
   and `flush` counts it achieved and drops the entry.
5. For every other member it answers a trailers-only `grpc-status: 14` and relays
   nothing. `round_trip` turns that into `GrpcStatusError { code: "14" }`
   (`chain.rs:391-401`), `classify_publish_failure` makes it `Publish::Retryable`
   (`chain.rs:491-501`), and `flush` pushes the entry into `unplaced` and calls
   `queue.requeue` (`batcher.rs:369-372`, `:390`).
6. On chain, at the height of this flush, **exactly one** Orchard-touching
   transaction appears from this hub. Its anonymity set is itself, and it is
   visible as such to **anyone watching the chain or the mempool**, not only to
   the indexer performing the selection.
7. At the next flush the attacker repeats with a different victim — publishing
   every migration alone, forever — or releases the remainder.

**Nothing anywhere observes the difference.** Three detection channels all fail:

- **The hub's own telemetry.** `batcher.rs:396-410` logs
  `flush_size = k, achieved_batch_size = 1, rejected = 0, requeued = k-1` plus a
  `warn!` whose `reason` field is *a string the attacker wrote*. That is
  byte-for-byte what a genuine ten-second indexer hiccup looks like, and the
  project's own test `a_transport_flavoured_grpc_status_is_held_but_invalid_argument_is_not`
  (`batcher.rs:763-775`) pins `UNAVAILABLE → hold` as correct behaviour. The one
  log line that names the harm — `"batch provides no batching anonymity at this
  size"` (`batcher.rs:412-420`) — is *expected to fire today* at current adoption,
  so it is pre-normalised as noise.
- **The wallet's confirmation lookup.** The shim routes every `GetTransaction` to
  the hub, and `Hub::lookup` answers a queued entry from `Queue::find_by_txid`
  with `height: 0` — lightwalletd's mempool sentinel (`server.rs:296-303`,
  `server.rs:90-91`, `queue.rs:328-348`). A held-back migration therefore answers
  the wallet *"pending in the mempool"*, indefinitely and indistinguishably from a
  genuinely unmined transaction.
- **The hub itself.** There is no confirmation tracking (`REVIEW.md` #7 is a
  documented designed-not-built item), and `Entry::received_height` — declared at
  `queue.rs:135` as *"Drives the confirmation deadline"* — is written once
  (`queue.rs:235`) and **read nowhere in the crate**.

**Attack Requirements and Assumptions:**
- **Control of, compromise of, or compulsion over the indexer(s) the hub
  publishes through.** This is not an internet-reachable attack and not available
  to a stranger: it requires the configured, semi-trusted endpoint to misbehave.
- Cost: zero. It is a status code per request.
- Reliability: **deterministic** while `node_count() == 1`, which is the shipped
  configuration.
- **What makes it not work:** with two or more genuinely independent endpoints, an
  honest one answers `Accepted` and relays the transaction, and `best_of` ranks
  `Accepted` (3) above `Retryable` (0) (`chain.rs:459-474`), so the hold-back
  fails. This attack needs **all** endpoints hostile. Multi-endpoint deployment is
  a real, code-supported mitigation here — and it is the *opposite* of the
  direction the `max()`-based tip rule scales (see
  `tip-and-verdict-aggregation-scale-in-opposite-directions-so-adding-indexers-fixes-one-lever-and-aggravates-three.md`).

## Impact on Users

The batch is the entire anonymity mechanism the hub provides; `queue.rs:1-6` says so
("The batch IS the anonymity set"). This reduces the on-chain batch to one
transaction at will, for a victim of the attacker's choosing, while every
component reports normal operation and the wallet is shown "pending".

The reason this matters despite the operator already holding stronger levers is
specific, and the report should carry it in this frame rather than as additional
independent linkage:

1. **It survives the fix for the shape-based selection channel.** The filed
   core-linkage chain selects within a batch by `(length, anchor, expiry)`, which
   works *because* diverted transactions differ in shape. ZIP 318 conformance plus
   wallet-side padding makes them uniform and closes that channel; this one uses no
   shape at all and is untouched. The two are anti-correlated in time: the shape
   channel dominates today, this one dominates the moment the wallet-side condition
   the project is designing toward is met.
2. **It reaches adversaries the operator-side attack cannot.** Publishing the
   victim **alone on the public chain** hands the result to a mempool watcher, a
   chain-analysis firm, or a party who can compel the indexer but not the enclave —
   all from public data. Anyone additionally holding the "IP C submitted an
   Orchard-touching transaction at time T" half (which `README.md:33` and
   `hub/REVIEW.md:181` concede the shim's operator holds) completes IP →
   transaction → balance. That is the linkage `README.md:27` says does not survive,
   "volume-independent".

Two secondary harms follow from the same mechanism:

- **Destruction of held-back transactions in the traffic class the hub is sized
  for.** A librustzcash-default wallet transaction carries `expiry = build_height +
  40` (`batcher.rs:49-55`). It was admitted only because it survived
  `next_flush_height(tip, 20) + 4` (`queue.rs:380-392`). Holding it back one full
  20-block interval pushes publication to the edge of or past its expiry, after
  which the node refuses it, `flush` classifies it `Rejected` and **drops it
  permanently** (`batcher.rs:368`) — for a migration the wallet was told at mixnet
  hand-off had been sent (`shim/src/hub.rs:238-240`). ZIP 318 migrations, with
  34,561–69,120 blocks of slack, survive indefinitely, which is what makes the
  *isolation* variant sustainable against exactly the population the product exists
  for: the attacker can serialise a whole batch one transaction per flush with
  nothing expiring.
- **Self-escalation into a fleet-wide admission outage.** Requeued entries are
  recharged to the byte budget but `requeue` never checks it (`queue.rs:279-295`,
  filed as `hub-queue-requeue-ignores-byte-budget-unbounded-growth.md`), so
  sustained hold-back grows the queue monotonically past `MAX_QUEUE_BYTES`
  (`queue.rs:65`), after which `admit` refuses every new submission `Full`
  (`queue.rs:224`) — which, on the deployed dispatch-only mixnet transport, is
  silent (`nym-submit-acks-are-never-read-so-every-hub-refusal-is-invisible.md`).

## Technical Details / Code Analysis

The verdict split, in full (`hub/src/batcher.rs:361-390`):

```rust
    let mut achieved = 0usize;
    let mut rejected = 0usize;
    let mut sample_failure: Option<String> = None;
    let mut unplaced = Vec::new();
    for (i, entry) in batch.into_iter().enumerate() {
        match outcomes.get(i) {
            Some(Publish::Accepted { .. }) | Some(Publish::AlreadyKnown) => achieved += 1,
            Some(Publish::Rejected { .. }) => rejected += 1,
            Some(Publish::Retryable { reason }) => {
                sample_failure.get_or_insert_with(|| reason.clone());
                unplaced.push(entry);          // <-- held back, offered again next flush
            }
            None => unplaced.push(entry),
        }
    }
    ...
    let requeued = queue.requeue(unplaced);
```

The verdicts are **positional and per transaction** — `broadcast_batch` returns one
`Publish` per input (`hub/src/chain.rs:208-210`):

```rust
    pub async fn broadcast_batch(&self, txs: &[Vec<u8>]) -> Vec<Publish> {
        join_all(txs.iter().map(|tx| self.broadcast(tx))).await
    }
```

and each `broadcast` is a separate gRPC call over a separate connection carrying
one transaction (`hub/src/chain.rs:176-198`, `:300-334`), so the indexer has a
distinct, individually answerable request per batch member and can discriminate on
the plaintext it is holding.

**The crux — the status classification that makes `UNAVAILABLE` a hold rather than
a verdict** (`hub/src/chain.rs:491-501`, with the constants at `:120-121`):

```rust
const GRPC_INVALID_ARGUMENT: &str = "3";
const GRPC_FAILED_PRECONDITION: &str = "9";
...
fn classify_publish_failure(err: &BoxError) -> Publish {
    let reason = err.to_string();
    match err.downcast_ref::<GrpcStatusError>() {
        Some(status)
            if status.code == GRPC_INVALID_ARGUMENT || status.code == GRPC_FAILED_PRECONDITION =>
        {
            Publish::Rejected { reason }
        }
        _ => Publish::Retryable { reason },
    }
}
```

`chain.rs:485-490` explains *why* everything else is retryable, and the reasoning
is sound in the direction it considers — dropping a valid migration on a misread
error is unrecoverable. The direction it does not consider is that the same rule
hands the party writing the status code a per-transaction publication switch.

`round_trip` produces that typed error from either the trailers or a trailers-only
HEADERS frame (`hub/src/chain.rs:360-401`), so a single `grpc-status: 14` header on
an otherwise well-formed HTTP/2 response is enough; no body is needed.

With one endpoint, `best_of` (`hub/src/chain.rs:459-474`) is the identity function
on that endpoint's answer, so there is no second opinion to override it.

The lookup path that hides the hold-back from the wallet
(`hub/src/server.rs:296-303`):

```rust
    pub async fn lookup(&self, wire_hash: &[u8]) -> LookupOutcome {
        if let Some(bytes) = self.queue.find_by_txid(wire_hash) {
            tracing::debug!(source = "queue", "transaction lookup answered");
            return LookupOutcome::Found { data: bytes, height: 0 };
        }
```

`height: 0` is documented at `server.rs:90-91` as *"`0` means mempool (a queued,
unflushed transaction), matching lightwalletd's sentinel"*. A wallet polling for
its migration therefore receives "in the mempool" for a transaction that is in no
mempool at all.

And the module claim this refutes (`hub/src/batcher.rs:8-22`):

> **Why the cadence is unconditional (REVIEW #2, #8).** Every conditional trigger
> is a lever someone else can pull. […] A deterministic clock nobody can influence
> is the only shape with no lever on it […]

The clock is only half the mechanism. A deterministic clock firing into a
publication path a single untrusted party gates delivers a deterministic
*schedule* and an adversary-selected *content*.

## Recommendations

- **Alarm on the shape that has no benign explanation.** For a single endpoint
  answering `k` simultaneous requests, a flush with `achieved >= 1 && requeued >= 1`
  is either impossible (a real outage yields `achieved == 0`) or a selective
  hold-back. That is a one-line check in `flush` and is the cheapest fix in the hub.
- **Require, and validate, at least two operationally independent indexers before
  the hub claims batching anonymity.** `best_of`'s ranking already defeats this at
  `n >= 2` with one honest endpoint; the shipped `n = 1` is what makes it
  deterministic. Note this is the opposite of what the `max()` tip rule wants, so
  the two must be fixed together.
- **Implement the confirmation tracking `REVIEW.md` #7 specifies, or stop claiming
  what it would prove.** Re-querying the chain for a published txid one or two
  cadences later is the only mechanism that distinguishes "the indexer relayed it"
  from "the indexer said it did". Until then, `batcher.rs:333-340`'s description of
  `achieved_batch_size` as "the honest measure of the privacy the flush actually
  delivered" should say it measures what the indexer reported. `Entry::received_height`
  already exists and is unread; it is the natural place to hang this.
- **Distinguish "queued, never offered" from "offered and held back" in the lookup
  answer.** One boolean on `Entry` is the difference between a wallet that can
  eventually surface the failure and one that cannot.
- **State the residual.** `REVIEW.md`'s inherent-limits section records that a party
  degrading the *shim→hub* path chooses when a migration is published. It does not
  record that the party on the *hub→indexer* path chooses **whether a migration is
  published in a batch at all**, which is a strictly stronger capability over the
  same anonymity property.

## Validation Information

**Verdict: CONFIRMED at Medium** (downgraded from the filed High).

**The crux was verified directly, as required:**

- `hub/src/chain.rs:120-121` defines `GRPC_INVALID_ARGUMENT = "3"` and
  `GRPC_FAILED_PRECONDITION = "9"`; `classify_publish_failure` at `:491-501`
  returns `Publish::Rejected` **only** for those two codes and
  `Publish::Retryable` for everything else, including `UNAVAILABLE` (14). The
  doc comment at `:485-490` states this intent explicitly and names `UNAVAILABLE`.
- `round_trip` (`chain.rs:360-401`) reads `grpc-status` from trailers **or**
  headers and returns the typed `GrpcStatusError`, so a trailers-only `14` reply
  is sufficient — no body required.
- `flush` (`batcher.rs:361-390`) requeues exactly `Retryable` and `None`; the
  requeued entries return at the next cadence (`queue.rs:279-295`).
- `broadcast_batch` → `broadcast` → `unary` → `unary_inner` issues one
  `TcpStream::connect` per (transaction × endpoint) with a single 10 s
  `tokio::time::timeout` around the whole call (`chain.rs:208-210`, `:181-197`,
  `:280-291`, `:300-311`). The indexer therefore sees `k` independent, concurrent,
  individually answerable requests and has the whole budget to decide.
- `best_of` (`chain.rs:459-474`) ranks `Accepted` 3 > `AlreadyKnown` 2 >
  `Rejected` 1 > `Retryable` 0, so with `n = 1` it is the identity and with an
  honest endpoint present the hold-back fails — the filed "what makes it not work"
  bound is correct.
- The project's own test `a_transport_flavoured_grpc_status_is_held_but_invalid_argument_is_not`
  (`batcher.rs:763-775`) asserts `Status("14")` leaves the entry in the queue and
  `Status("3")` removes it, pinning the behaviour as intended.
- Detection claims checked: `batcher.rs:396-420` logs only aggregates plus the
  attacker-authored `reason` string; `Entry::received_height` appears only at
  `queue.rs:135` (declaration) and `:235` (write) and is read nowhere in `hub/src`
  or `hub/tests`; `Hub::lookup` answers a queued entry `height: 0`
  (`server.rs:296-303`). All three detection failures are as filed.
- `deploy.env.example:22` ships a single endpoint, so `node_count() == 1` in the
  shipped configuration.

**Why Medium and not High.** The coordinator's standing bound for indexer-dependent
findings applies here (PROGRESS.md item 6p: *"the `Rejected` drop and all tip
manipulation require control of a configured indexer endpoint — they are
hub-trust/robustness defects, not internet-reachable weapons"*), and three further
considerations cap the severity:

1. **The attacker is a configured, semi-trusted endpoint, not a stranger.** No
   internet-reachable path exists to this behaviour; it needs the indexer the hub
   publishes through to be hostile, compromised or compelled.
2. **Severities must not be stacked (PROGRESS.md item 6v).** Against the *shim's
   operator*, forcing `k = 1` adds little today: the already-filed core-linkage
   chain lets them select a target *within* a batch by `(length, anchor, expiry)`,
   and `hub/REVIEW.md:175` concedes the modal batch is already 0 or 1 at current
   adoption. The incremental harm to users **today** is near zero.
3. **A code-supported mitigation exists and is one configuration line:** two or
   more independent endpoints defeat it via `best_of`.

**Why it is nonetheless a real finding and not invalid or Info.** It earns its place
on the two grounds the global pass identified and this validation upholds: it is the
*successor* risk — it survives the wallet-side ZIP 318 + padding fix that closes the
shape-based channel, because it uses no shape — and it reaches adversaries that
channel cannot, because the victim lands **alone on the public chain**, so a mempool
watcher or chain-analysis firm gets the result from public data without ever
touching the enclave or the wallet leg. It also destroys wallet-acknowledged
migrations in the expiry-bounded traffic class as a side effect, with no signal to
the wallet, the operator or the hub.

**Nothing in the filed issue was found to be factually wrong.** The changes made are
framing only: the "adoption-proof isolation destroys anonymity" headline is now
stated together with the honest bound that today's batch is already 1, the attacker
must be the configured indexer, and `n >= 2` fixes it.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
