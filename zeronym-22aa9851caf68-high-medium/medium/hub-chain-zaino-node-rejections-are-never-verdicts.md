# Against a Zaino indexer the hub can never obtain a `Rejected` verdict, so a refused transaction is re-published at every flush forever — the queue has no other eviction path, and one unauthenticated junk fill becomes permanent

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/chain.rs:491-501` (`classify_publish_failure`), the claim at `:432-437` (`classify_send_response`'s doc: "lightwalletd's convention, **which zaino follows**"), the rationale at `:447-473` (`best_of`) and `:503-512`, and the in-file test at `:589-607` that pins `"13"` as retryable. Consumed at `audit-target/zeronym/hub/src/batcher.rs:365-390`; the requeue with no eviction rule is `audit-target/zeronym/hub/src/queue.rs:279-295`. Backend behaviour (read directly from the vendored source, not inferred): `audit-context/zero/zaino/packages/zaino-state/src/indexer/node_backed_indexer.rs:1561-1570`, `.../chain_index.rs:2912-2922`, `.../error.rs:118-168` and `:525-531`, `audit-context/zero/zaino/packages/zaino-serve/src/rpc/grpc/service.rs:25-40,159-177`.
**Found by agent:** Local (file audit of `hub/src/chain.rs`)
**In scope of audit?** Yes

## Description

`chain.rs` splits publish failures into two classes and the batcher acts on the
split: `Rejected` is dropped permanently, `Retryable` is requeued for the next
flush. The split rests on an assumption stated as fact in the code
(`chain.rs:434-437`):

> lightwalletd's convention, **which zaino follows**: `error_code == 0` means
> success and `error_message` carries the txid. A non-zero code carries the
> node's rejection text …

The first half is true of Zaino. **The second half is not.** In the whole of
Zaino's non-generated source there is exactly **one** construction of a
`SendResponse`, and it hardcodes success
(`zaino/packages/zaino-state/src/indexer/node_backed_indexer.rs:1561-1570`):

```rust
    /// Submit the given transaction to the Zcash network
    async fn send_transaction(&self, request: RawTransaction) -> Result<SendResponse, Self::Error> {
        let hex_tx = hex::encode(request.data);
        let tx_output = self.send_raw_transaction(hex_tx).await?;

        Ok(SendResponse {
            error_code: 0,
            error_message: tx_output.hash().to_string(),
        })
    }
```

The `?` sends every node refusal down the error path instead, and that path
flattens to gRPC status **13 (INTERNAL)** with a fixed message. Traced end to
end in the vendored tree:

`ChainIndex::send_raw_transaction` maps *any* backing-node error through
`ChainIndexError::backing_validator` (`chain_index.rs:2912-2922`), which is
`kind: InternalServerError, message: "InternalServerError: error receiving data
from backing node", source: Some(<the real RPC error>)` (`error.rs:525-531`).
The `From<NodeBackedIndexerServiceError> for tonic::Status` impl then renders
that as `tonic::Status::internal(err.message)` (`error.rs:132-137`) — **using the
fixed message and discarding the `source` chain**. The gRPC handler propagates it
verbatim (`zaino-serve/src/rpc/grpc/service.rs:29-40`, `:159-177`).

So against Zaino a node rejection arrives at the hub as a `GrpcStatusError` with
`code == "13"` and `message == "InternalServerError: error receiving data from
backing node"`, and `classify_publish_failure` treats only codes 3 and 9 as
verdicts (`chain.rs:491-501`):

```rust
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

`13` falls to the `_` arm and becomes `Retryable`. The file's own unit test
asserts exactly this (`chain.rs:598`: `for code in ["14", "4", "8", "2", "13"]`
… *"must be retryable"*).

**Consequence: with a Zaino backend, `Publish::Rejected` is unreachable for a
node rejection**, by both routes — `classify_send_response`'s non-zero branch is
dead because Zaino never builds a non-zero `SendResponse`, and
`classify_publish_failure` cannot produce a verdict because Zaino's
`send_transaction` path emits neither code 3 nor code 9. Every transaction the
network refuses is requeued and offered again at every subsequent flush.

**Nothing else evicts it.** `Queue::requeue` (`queue.rs:279-295`) re-inserts
unconditionally and re-charges the bytes; it applies no expiry check, no retry
counter and no deadline. `Entry::received_height` exists and its doc comment says
*"Drives the confirmation deadline"* — a repository-wide grep shows it is read
**nowhere** (`queue.rs:135` and `:235` are its only two occurrences). `admit`'s
`survives_next_flush` check runs at admission only. So the *only* thing that ever
removes an entry from the queue is a terminal verdict from the indexer, and
against Zaino no terminal verdict except success exists.

The code says so itself, and the sentence is false against this backend
(`batcher.rs:383-389`):

> The entry keeps its original expiry; when the indexer answers again a stale one
> gets the node's verdict and leaves.

Against Zaino a stale entry gets `INTERNAL` and stays.

## Attack Scenario and Steps

1. The hub is configured against a Zaino `CompactTxStreamer`. This is a
   first-class supported configuration: the hub links `zaino-proto` as its RPC
   types (`hub/Cargo.toml:52`), speaks
   `/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction`
   (`chain.rs:53`), the product is described as sitting in front of the
   operator's existing indexer, and `chain.rs:434` names Zaino explicitly.
2. An attacker submits junk over the unauthenticated Nym submit path. A payload
   that does not deserialize is admitted with `txid = None, expiry = None`
   (`queue.rs:189-204`, REVIEW #5 — deliberate, because the shim diverts what it
   cannot parse). Distinct payloads defeat the payload-hash dedup at
   `queue.rs:206-218`.
3. At every flush, `flush` drains the junk, `broadcast_batch` offers it, Zaino's
   node refuses it, Zaino answers `INTERNAL`, `chain.rs` says `Retryable`, and
   `batcher.rs:390` puts it straight back.
4. The entry is immortal. Not because `expiry == None` — that only matters at
   admission — but because **no code path anywhere consults expiry after
   admission**. A *parseable but permanently invalid* migration is equally
   immortal.
5. Once `inner.bytes` reaches `MAX_QUEUE_BYTES`, `admit` returns
   `Refusal::Full` for every genuine migration (`queue.rs:223-225`) **until the
   process restarts** — and per the confirmed
   `hub-nym-driver-automatic-fresh-identity-…` / failover runbook, restarting the
   hub changes its Nym address and strands every shim for "well over an hour".
   Each flush's outbound fan-out also grows with the junk (one connection per
   transaction per endpoint, `chain.rs:176-210`).

**This is what the issue owns that its siblings do not:** on a lightwalletd/zebra
backend the same junk is answered with a node error string, classified
`Rejected`, and **dropped at the first flush** — which is why coordinator item
6u(f) could record "junk never reaches the chain and there is nothing to
subtract", and why the confirmed High `hub-queue-unauthenticated-fill-…` requires
the attacker to *sustain* the flood across every epoch. Against Zaino the same
attack is **one-shot and permanent**.

The non-attacker form needs no attacker at all: any genuinely invalid or expired
migration — a stale anchor, a bad signature, a double-spend — is re-offered to
the operator's indexer at every flush, forever, at one connection and one publish
per endpoint per flush.

**Attack Requirements and Assumptions:**
- Requires the hub to be pointed at Zaino rather than lightwalletd. Both are
  supported and the code names both; the shipped example endpoint
  (`deploy.env.example:22-23`, `INDEXER_TLS=na.zec.rocks`) was established by the
  sibling issue's validation to be **lightwalletd+zebra** today, with Zaino on
  separate `zaino.*.zec.rocks` hosts. So this is a supported-configuration
  finding, not a shipped-default one — but the operator has no way to learn it,
  because the code asserts the opposite.
- The junk form requires nothing else: hub submission is unauthenticated by
  design.
- The mapping was read directly from the vendored Zaino source at
  `audit-context/zero/zaino` (monorepo commit `62baea8`), not inferred from
  zeronym's comments about it.

## Impact on Users

**Genuine migrations are refused, permanently.** Once the byte budget is consumed
by immortal entries, `admit` answers `Full` and — because submit is dispatch-only
on the deployed transport — the shim has already told the wallet `errorCode 0`
with a txid (`shim/src/hub.rs:231-241`, `shim/src/intercept.rs:186`). Per the
threat model that is not merely availability: a wallet that cannot migrate
through the hub is a wallet whose user retries, changes indexer, or broadcasts in
the clear — the exact outcome the product exists to prevent — and with this bug
the attacker chooses when it starts and it does not end without a hub restart
that is itself a fleet-wide outage.

**The hub re-publishes the same refused bytes to the operator's indexer every 20
blocks, indefinitely.** For a *genuinely invalid but real* migration — a user's
transaction that failed for a stale anchor, say — that is a repeating,
per-transaction signal delivered to the indexer operator once per cadence for as
long as the process lives, which is precisely the "fresh timing signal tied to
one transaction" that `chain.rs:513-520` says this component exists to avoid
emitting.

## Technical Details / Code Analysis

The seam, with its own description of the property it is meant to hold
(`hub/src/chain.rs:475-490`):

```rust
/// Map a failed `SendTransaction` call (no `SendResponse` came back) onto
/// [`Publish`].
///
/// This is the seam between "the indexer judged the transaction" and "the
/// indexer was never really asked", and the batcher's requeue depends on it
/// being drawn honestly. Only INVALID_ARGUMENT and FAILED_PRECONDITION are
/// verdicts here: they are what a gRPC service returns when it read the request
/// and refuses its content. ...
```

"they are what a gRPC service returns when it read the request and refuses its
content" is the assumption that fails. Zaino returns `INTERNAL` for a node
refusal because, from Zaino's point of view, the failure came from its upstream
JSON-RPC call, not from the request's arguments.

Zaino *does* preserve the node's legacy error code — but only in the `source()`
chain, and only its **JSON-RPC** front end walks it
(`zaino-serve/src/rpc/jsonrpc/service.rs:525`,
`sendrawtransaction_error_object_from_indexer_error`, pinned by
`chain_index/tests/mockchain_tests.rs:1293-1327`). The gRPC front end the hub
talks to converts through `Into<tonic::Status>` and drops it. This matters for
the fix: text-matching the gRPC message is **not** a workaround here, because the
message the hub receives is the constant `"InternalServerError: error receiving
data from backing node"`.

The batcher end of the seam (`hub/src/batcher.rs:365-390`):

```rust
    for (i, entry) in batch.into_iter().enumerate() {
        match outcomes.get(i) {
            Some(Publish::Accepted { .. }) | Some(Publish::AlreadyKnown) => achieved += 1,
            Some(Publish::Rejected { .. }) => rejected += 1,
            Some(Publish::Retryable { reason }) => {
                sample_failure.get_or_insert_with(|| reason.clone());
                unplaced.push(entry);
            }
            None => unplaced.push(entry),
        }
    }
    ...
    let requeued = queue.requeue(unplaced);
```

and the requeue that has no eviction rule (`hub/src/queue.rs:279-295`):

```rust
    pub fn requeue(&self, entries: Vec<Entry>) -> usize {
        ...
        for entry in entries {
            if inner.entries.contains_key(&entry.key) { continue; }
            inner.bytes = inner.bytes.saturating_add(entry.tx_bytes.len());
            inner.entries.insert(entry.key, entry);
            reinserted += 1;
        }
```

`best_of`'s doc (`chain.rs:447-458`) makes the *absence* of this bug its own
premise:

> if an unreachable endpoint could outvote a live one's verdict, a single dead
> endpoint in the list would keep every doomed transaction resident until it
> expired, and **an unparseable payload never expires** (queue.rs, REVIEW #5):
> junk plus one dead endpoint would fill the byte budget for everyone.

Against Zaino that safeguard never engages, because the verdict it protects never
exists — and the failure mode it describes is exactly what happens, without
needing a dead endpoint.

**A second, opposite defect in the same two lines, recorded as latent.**
`GRPC_FAILED_PRECONDITION` ("9") is classified as a permanent verdict, for which
the migration is *destroyed*. That reads the gRPC contract backwards:
FAILED_PRECONDITION denotes "the system is not in a state required for the
operation", i.e. a condition that may clear, whereas INVALID_ARGUMENT is about
the arguments themselves. Zaino uses code 9 for exactly such transient service
state — `tonic::Status::failed_precondition("zaino not yet synced")`
(`error.rs:163-165`) and `ChainIndexErrorKind::InvalidSnapshot`
(`error.rs:134-136`). Every producer of `UnavailableNotSyncedEnough` was checked
(`node_backed_indexer.rs:1051, 1089, 1175, 1544`) and **none is on the
`send_transaction` path**, so this is not live against Zaino today. It is
recorded because it is a real inversion that any front end or future backend
answering `9` for a transient condition would turn into permanent destruction of
a whole batch of migrations whose wallets were already told `error_code 0`.

Neither behaviour has any test coverage against a real indexer:
`hub/tests/live_chain.rs` is entirely `#[ignore]`d, gated on `ZIH_TEST_INDEXER`,
and exercises only `tip_height`; the in-module tests at `chain.rs:535-622` assert
the mapping the code implements rather than the mapping the backends produce; and
per coordinator item 6n no CI runs `cargo test` at all.

## Recommendations

1. **Give the queue a GC path, independently of the classification.** This is the
   fix that closes the issue whatever the backend does: no entry may be immortal.
   `Entry::received_height` is already stored and already documented as *"drives
   the confirmation deadline"* — read it. A bounded requeue count, or a
   `received_height + kN` deadline, satisfies `REVIEW.md`'s existing requirement
   that `expiry == None` entries need a deadline as their only GC path.
2. **Do not infer "the node judged this transaction" from a gRPC status code
   alone, and do not rely on the message either.** Against Zaino the message is a
   constant. If a verdict is wanted from Zaino, the honest options are to use its
   JSON-RPC front end (which does preserve the node's code), or to ask upstream
   for a status/message that distinguishes "the node refused these bytes" from
   "zaino could not reach its node" — a change worth requesting regardless, since
   any gRPC client of Zaino has this problem.
3. **Correct the doc comment at `chain.rs:434-437`:** Zaino follows lightwalletd's
   *success* convention only; it reports every failure as a gRPC status, so
   `classify_send_response`'s non-zero branch and `classify_publish_error` are
   dead code behind a Zaino backend.
4. **Reclassify `FAILED_PRECONDITION` as retryable**, matching the gRPC contract
   and Zaino's actual usage of it.
5. **Add an integration test that runs a flush against each supported backend and
   asserts the disposition of an invalid transaction.** The three sibling issues
   in this function exist because no such test does.

## Validation Information

**Verdict: CONFIRMED. Severity: Medium (as filed).**

### The backend behaviour was re-derived from the vendored source, end to end

Every hop was read in `audit-context/zero/zaino` (monorepo commit `62baea8`),
matching the standard set by the zebra sibling's validation:

1. **`SendResponse` has exactly one construction site in Zaino outside the
   generated proto crate.** `grep -rn "SendResponse" packages/ --exclude
   zaino-proto` returns seven hits: two `use` lines, the trait declaration
   (`indexer.rs:696-699`), the macro entry (`zaino-serve/.../grpc/service.rs:177`),
   and the single implementation at `node_backed_indexer.rs:1562-1570`, which
   hardcodes `error_code: 0`. **`classify_send_response`'s non-zero branch is
   therefore unreachable behind Zaino** — which is the same partition the zebra
   sibling's validation recorded from the other side.
2. **The error path.** `node_backed_indexer.rs:538-546` → `?` →
   `NodeBackedChainIndexSubscriber::send_raw_transaction`
   (`chain_index.rs:2912-2922`, `type Error = ChainIndexError` via
   `ChainIndexRpcExt: ChainIndex`, `chain_index.rs:547`, `:1660-1662`) →
   `.map_err(ChainIndexError::backing_validator)` → `error.rs:525-531`:
   `kind = InternalServerError`, `message = "InternalServerError: error receiving
   data from backing node"`, real cause in `source`.
3. **The conversion.** `error.rs:118-137`:
   `NodeBackedIndexerServiceError::ChainIndexError(err) => match err.kind {
   InternalServerError => tonic::Status::internal(err.message), … }`. **Status 13,
   constant message, `source` discarded.** Every other variant in that impl is
   also `internal(...)` except two `failed_precondition` cases.
4. **The handler.** `zaino-serve/src/rpc/grpc/service.rs:25-40` (`client_method_
   helper!`) does `.map_err(Into::into)?` and `:159-177` binds
   `send_transaction`, so the status reaches the wire unmodified.
5. **The hub's receiving end.** `chain.rs:356-401` reads `grpc-status` from a
   trailers-only response *and* from trailers after a body, so a tonic error
   status is correctly captured as `GrpcStatusError { code: "13", … }`;
   `classify_publish_failure` (`:491-501`) sends it to the `_` arm; the in-file
   test at `:589-607` pins `"13"` as retryable.
6. **`FAILED_PRECONDITION` reachability was checked, not assumed.** All four
   producers of `UnavailableNotSyncedEnough` are on `get_latest_block`,
   `get_block`-family and `get_transaction` paths, never `send_transaction`; and
   `InvalidSnapshot` is `#[allow(dead_code)]` and never constructed
   (`error.rs:483-484`). The filed "latent inversion" framing is correct and has
   been kept as such — it carries **no** severity here.

### The eviction claim was checked exhaustively, and is stronger than filed

The filing said the entry is immortal "because `expiry == None`". That reasoning
is wrong and has been corrected: `survives_next_flush` is consulted **only** at
`queue.rs:202`, inside `admit`. `requeue` (`:279-295`) has no expiry test, `flush`
(`batcher.rs:341-390`) has no expiry test, and `Entry::received_height` — whose
own doc comment says *"Drives the confirmation deadline"* — is read **nowhere**
in the crate (`grep -rn received_height hub/src/` returns exactly `queue.rs:135`
and `:235`). So against Zaino a **parseable, expired, permanently invalid**
migration is just as immortal as junk. The queue's only eviction is a terminal
verdict, and the backend that never produces one has no GC at all. That is why
recommendation 1 is now first: it is the backend-independent fix.

### Ownership — the three issues in this function partition cleanly

The zebra sibling's validation already recorded the partition from its side and
this pass confirms it from Zaino's:

| issue | backend | direction of the error |
|---|---|---|
| `publish-verdict-strings-are-zcashds-vocabulary-only-…` (**confirmed, Medium**) | lightwalletd + zebrad | zebra's strings match none of the four substrings, so *"never judged"* answers become `Rejected` → **destroyed** |
| `hub-chain-duplicate-nullifier-rejection-counted-as-published.md` (plausible, Low) | zcashd | `"duplicate"` over-matches a double-spend, so a refusal is counted `AlreadyKnown` → **reported as achieved** |
| **this issue** | **Zaino** | no answer is ever a verdict, so a refusal becomes `Retryable` → **immortal** |

**This issue owns the Zaino backend, and nothing else.** It must not be graded as
if it also covered the shipped lightwalletd+zebra endpoint, and the report should
present all three together as *"the function is wrong for every backend, in a
different direction each time"*.

### Severity: why Medium

*Impact, given Zaino:* one unauthenticated 64 MiB fill becomes **permanent**
rather than sustained, so the confirmed High queue-fill attack gets strictly
cheaper and its recovery becomes a hub restart, which is itself the fleet-kill
outage the runbook budgets at "well over an hour". Plus a no-attacker path (any
invalid migration re-broadcast forever) that leaks a per-transaction cadence
signal to the indexer operator.

*Likelihood:* bounded by the backend. Zaino is a supported, code-named,
same-monorepo indexer and the hub links its proto crate, but the shipped example
endpoint is lightwalletd+zebra (established by the sibling's validation from
zec.rocks' own published stack and operator statements). An operator choosing
Zaino has no way to discover the problem: `chain.rs:434` asserts the convention
holds, and no test in the repository exercises any real backend.

*Why not High:* it needs a configuration choice the shipped example does not
make, and the terminal harm — fleet-wide silent destruction after `Refusal::Full`
— is already owned at High by `hub-queue-unauthenticated-fill-silently-destroys-
migrations.md`. Item 6u(b)'s "do not stack the severities" applies: what is
graded here is the *escalation* (sustained → permanent) and the no-attacker GC
failure, not the terminal state.

*Why not Low:* the defect is in the one function that decides whether a
wallet-acknowledged migration is kept or destroyed; it disables the queue's only
garbage-collection mechanism entirely; it is invisible to every test and every
telemetry surface the project has; and it is asserted to be safe by a comment in
the code that is factually wrong about the backend it names.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
