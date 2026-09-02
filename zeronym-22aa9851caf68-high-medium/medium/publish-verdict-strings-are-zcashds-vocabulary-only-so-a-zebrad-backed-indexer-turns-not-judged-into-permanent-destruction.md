# `classify_publish_error` matches only zcashd's reject vocabulary, so behind the shipped example indexer (lightwalletd in front of zebrad) *every* node answer is a `Rejected` verdict — including the two that mean "the node never looked at this transaction", which the batcher then destroys permanently

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/chain.rs:503-533` (`classify_publish_error` and its doc comment), reached from `:438-445` (`classify_send_response`) via `:176-198` (`broadcast`) and `:208-210` (`broadcast_batch`); the requeue-vs-drop split it feeds at `audit-target/zeronym/hub/src/batcher.rs:365-390`, and the telemetry at `:396-402`, `:412-420`. Deployed indexer: `audit-target/zeronym/deploy.env.example:22-23`. Node-side strings (out of scope, reasoned about as an interface): `audit-context/zero/zebra/zebrad/src/components/mempool/error.rs:50,55,61,70,76`, `zebrad/src/components/mempool/storage.rs:106`, `zebrad/src/components/mempool/downloads.rs:127-139`, `zebrad/src/components/mempool.rs:330,419-452,1116-1123`, `zebra-rpc/src/methods.rs:1305-1360`, `zebra-rpc/src/server/error.rs:105-107`, `zebra-rpc/src/queue.rs:39,42,206-241`. Relay: `audit-context/zero/lightwalletd/frontend/service.go:548-580`, `frontend/rawrequest.go:150-161`. Contrast: `audit-context/zero/zcashd/src/rpc/rawtransaction.cpp:1318-1339`, `src/main.cpp:2103,2132`.
**Found by agent:** Global (focus area G9, cross-component invariant drift — dedicated re-run)
**In scope of audit?** Yes

## Description

`hub/src/chain.rs` decides, for every member of every flushed batch, whether the
indexer's answer is a **verdict** (the entry is dropped forever) or a **transport
failure** (the entry is requeued for the next flush). When the indexer answers
`OK` carrying a `SendResponse` with a non-zero `error_code`, that decision is made
by matching four substrings against the node's error text:

`hub/src/chain.rs:513-533`

```rust
fn classify_publish_error(message: &str) -> Publish {
    // Hyphens folded to spaces before matching. Bitcoin-derived nodes report
    // these as hyphenated reject reasons (`txn-already-known`) while the longer
    // prose forms use spaces, and matching only one shape silently misses the
    // other. ...
    let m = message.to_ascii_lowercase().replace('-', " ");
    if m.contains("already in block chain")
        || m.contains("already known")
        || m.contains("already in mempool")
        || m.contains("duplicate")
    {
        Publish::AlreadyKnown
    } else {
        Publish::Rejected {
            reason: message.to_string(),
        }
    }
}
```

Its own doc comment states why the match is on text at all (`chain.rs:503-512`):

> Matched on text because the error codes for these cases are not consistent
> **between zebrad and zcashd**, nor through an indexer that relays them. Kept in
> one place and deliberately conservative: anything unrecognised is a rejection,
> never a silent success.

**All four substrings are zcashd's vocabulary. None of them is zebrad's.** The
four map one-for-one onto `zcashd/src/rpc/rawtransaction.cpp:1336`
(`"transaction already in block chain"`), `zcashd/src/main.cpp:2103`
(`txn-already-in-mempool`), `:2132` (`txn-already-known`), and the family of
`bad-*-duplicate` reject reasons.

Every string zebra's `sendrawtransaction` can produce was extracted from the
vendored source and run through the exact predicate above
(`.to_ascii_lowercase().replace('-', " ")`, then the four `contains`). **Not one
matches.** Behind zebrad, `classify_publish_error` returns `Publish::Rejected`
for *every* answer the node can give:

| zebrad string (verbatim) | source | what it means | `classify_publish_error` |
|---|---|---|---|
| `mempool is disabled since synchronization is behind the chain tip` | `mempool/error.rs:76` (`Disabled`) | **not judged** — mempool not yet active | `Rejected` |
| `transaction dropped because the queue is full` | `mempool/error.rs:70` (`FullQueue`) | **not judged** — request ignored | `Rejected` |
| `transaction already exists in mempool` | `mempool/error.rs:55` (`InMempool`) | already known | `Rejected` |
| `transaction dropped because it is already queued for download` | `mempool/error.rs:61` (`AlreadyQueued`) | already known | `Rejected` |
| `… until a chain reset: transaction was committed to the best chain` | `mempool/error.rs:50` + `storage.rs:106` | already mined | `Rejected` |
| `transaction is already in state` | `mempool/downloads.rs:127` | already mined | `Rejected` |
| `error in state service: …` | `mempool/downloads.rs:130` | **not judged** — internal fault | `Rejected` |
| `transaction download / verification was cancelled` | `mempool/downloads.rs:136` | **not judged** — task cancelled | `Rejected` |
| `transaction did not pass consensus validation: …` | `mempool/downloads.rs:139` | genuinely refused | `Rejected` (correct) |
| `transaction is non-standard` |  `mempool/error.rs:80` | genuinely refused | `Rejected` (correct) |

Four of these are the node saying *it never evaluated the transaction*. The hub
converts all four into `Publish::Rejected`, which `batcher.rs:368` counts as
rejected and **drops permanently** — while the wallet was told `error_code 0`
with a txid at mixnet dispatch, minutes earlier.

This is the exact outcome `chain.rs:485-490` says the classification exists to
prevent:

> re-offering an entry costs one call per flush and stops at its expiry, while
> **dropping a valid migration on a misread error is unrecoverable, because the
> shim has already told the wallet it was sent**.

`classify_publish_failure` (`chain.rs:491-501`) applies that principle correctly
to gRPC-level failures — only `INVALID_ARGUMENT` and `FAILED_PRECONDITION` are
verdicts, everything else is `Retryable`. `classify_publish_error` inverts it for
`SendResponse`-level failures: everything unrecognised is a verdict. Against
zcashd that default is nearly harmless, because zcashd pre-checks the mempool and
the chain before `AcceptToMemoryPool` and so only reaches the error path after it
has actually judged the transaction (`rawtransaction.cpp:1318-1339`). Against
zebrad it is wrong for four of ten reachable answers, and the type-level
distinction `Publish` documents at `chain.rs:100-108` — *"`Retryable` means
nothing judged the transaction at all"* — is simply not delivered.

## Attack Scenario and Steps

This fires with **no adversary at all**, which is what separates it from
`hub-flush-destroys-migration-on-single-unverifiable-verdict.md` (a hostile
indexer choosing to lie). Routine operation, in the configuration the project
ships:

1. The hub's configured indexer is `INDEXERS=66.241.124.200:443` /
   `INDEXER_TLS=na.zec.rocks` (`deploy.env.example:22-23`) — a **zec.rocks**
   endpoint. zec.rocks publishes its own stack
   (<https://github.com/zecrocks/zcash-stack/tree/main/docker>): the core is
   **Zebra + lightwalletd**, with Zaino as an optional add-on, and the operators
   stated on the Zcash community forum (thread 50907, checked 2026-08-18) that as
   of June 2026 most production endpoints run *"LWD+zebra"* and only
   `zaino.unsafe.zec.rocks` test endpoints run Zaino. lightwalletd-in-front-of-
   zebrad is also a first-class, documented, CI-tested configuration
   (`zebra/book/src/user/lightwalletd.md`), and zcashd is end-of-life by the
   monorepo's own README (*"the long-term direction is the Zebra, Zaino and
   Zallet (Z3) stack"*, `audit-context/zero/README.md:31,45-46`).
2. lightwalletd relays the node's JSON-RPC error verbatim: it splits
   `"<code>: <message>"` and puts the message straight into
   `SendResponse.error_message` (`frontend/service.go:554-566`,
   `frontend/rawrequest.go:158-159`). zebra's `map_misc_error` puts the
   `MempoolError` `Display` string in unmodified
   (`zebra-rpc/src/server/error.rs:105-107`).
3. **That zebrad is restarted** — an upgrade, a host reboot, an OOM, a crash, a
   state-format migration, or a first deployment still doing its initial sync.
   Zebra constructs its mempool `Disabled` (`mempool.rs:330`) and only enables it
   once the syncer's last three response lengths average under 20 blocks
   (`sync/status.rs:27,72-99`), which cannot be true until several sync rounds
   have completed.
4. The hub's 20-block cadence fires. `broadcast_batch` publishes every held
   migration; every call returns
   `SendResponse{error_code: -1, error_message: "mempool is disabled since
   synchronization is behind the chain tip"}`.
5. `classify_publish_error` matches none of the four substrings →
   `Publish::Rejected` for **every** member. `best_of` (`chain.rs:459-474`) ranks
   `Rejected` above `Retryable`, so a second endpoint that is merely unreachable
   cannot rescue them; only a second endpoint that actually *accepts* can.
6. `flush` (`batcher.rs:368`) counts them `rejected` and drops them. `requeued`
   is 0, so the `warn!` at `:404-411` (which needs `requeued > 0`) does not fire.
   Nothing else in the system holds a copy: the shim answered the wallet
   `error_code 0` at mixnet dispatch (`shim/src/intercept.rs:181-202`,
   `shim/src/nym.rs:595-600`), keeps no per-migration state, and the enclave is
   diskless.

**The node-side mitigation, quantified — this is what bounds the finding.**
`zebra-rpc`'s `send_raw_transaction` pushes the transaction into zebra's *own*
retry queue **before** it asks the mempool (`methods.rs:1322-1324`), so even the
`Disabled` answer leaves a copy behind. That queue re-offers the transaction on
every tip change and drops it after
`NUMBER_OF_BLOCKS_TO_EXPIRE × spacing + 5 s = 5 × 75 + 5 = 380 s`
(`zebra-rpc/src/queue.rs:39,206-241`), with capacity 20 (`:42`). So:

> **A migration is destroyed only if zebra is still not close to the tip ~380 s
> after the flush.** A restart that reaches the tip inside 6.3 minutes is fully
> absorbed by zebra and nothing is lost.

That makes the realistic trigger a *long* node-behind-tip window: a cold start, a
node that was down for hours, an initial sync, a state-format upgrade, or a slow
host — not a fast service restart. The same 380 s queue also covers the
`error in state service` and `cancelled` rows, and the hub's own 10 s
`RPC_TIMEOUT` (`chain.rs:48`) means zebra's 73 s verification timeout string is
not reachable through the hub at all. `FullQueue` needs 500 concurrent pending
mempool downloads (`downloads.rs:107`) sustained across the same window, which is
a flood condition rather than an ordinary one.

**Attack Requirements and Assumptions:**
- Requires the hub's indexer to relay node errors in `SendResponse.error_code` —
  lightwalletd's documented convention. **Against Zaino this path is unreachable
  entirely**: `node_backed_indexer.rs:1561-1570` returns `error_code: 0` on
  success and a `tonic::Status` otherwise, so a node refusal never becomes a
  `SendResponse`. That is the separately filed
  `hub-chain-zaino-node-rejections-are-never-verdicts.md`, and the two issues
  partition the indexer space between them.
- Requires the node behind that relay to be zebrad. Established above for the
  shipped example endpoint; and the hub cannot tell either way, since the
  endpoint is operator-chosen and can change without the hub noticing.
- **A correction to the original filing:** zebra does *not* disable an
  already-active mempool when it falls behind. `Mempool::update_state` returns
  early on `(is_caught_up = false, is_enabled = true, _)`
  (`mempool.rs:448-450`), with the comment *"Sync status only gates initial
  activation. Once the mempool is active, this method does not disable it."* So
  `Disabled` is reachable **only from process start**, not from a running node
  drifting behind. That narrows the trigger to restarts and first deployments.
- No privileged access, no network position, no mixnet capability.
- **Deliberate variant.** Whoever runs the indexer — a third party here, not the
  hub operator — gets a whole-batch kill with total deniability by restarting
  their own node near a cadence boundary: the node tells the exact truth, the hub
  mistranslates it, and the hub's log records `rejected = N`. This adds
  deniability rather than new power over the already-filed hostile-indexer issue.

## Impact on Users

A wallet that migrated is told `error_code 0` with a correct txid, and the
transaction is never broadcast. The loss is silent on both sides: the wallet
believes it succeeded, and the hub logs a truthful-looking `rejected` count that
an operator reads as *"the node refused an invalid transaction"*, not *"I
destroyed a valid one"*.

Because `MempoolError::Disabled` is a property of the node rather than of any
transaction, the failure is **not per-entry**: one flush landing in one node's
post-restart window destroys the entire batch — every user who migrated in that
20-block window, across every shim in the fleet.

How long the user is stuck depends on which traffic class they are in, and the
two answers are very different:

- **Today's traffic** (ordinary Orchard spends built by librustzcash-family
  wallets, expiry `= tip + 40` per ZIP 203's Blossom default) — the transaction
  expires in ~50 minutes, the wallet's notes unlock, and the user can retry. The
  harm is a silent failed migration and an hour of confusion.
- **ZIP 318 conforming traffic** — the canonical expiry is a bucketed absolute
  height 30 to 60 days out (`SPEC-NOTES.md` §3). The wallet shows the migration
  pending and will not reuse those notes until it expires. That is the
  separately-filed
  `zip318-canonical-expiry-is-the-only-recovery-clock-and-a-lost-migration-freezes-the-users-notes-for-30-to-60-days.md`;
  this issue is one of the mechanisms that reaches it.

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


At present adoption the modal batch is 0 or 1 (`README.md:34`, `REVIEW.md:175`),
so a single event typically destroys 0–2 migrations; the per-event population
grows exactly as the product succeeds.

Separately, and with no user harm, `achieved_batch_size` — the number
`batcher.rs:335-337` calls *"the honest measure of the privacy the flush actually
delivered"* and which `hub/REVIEW.md` design change #9 makes the launch gate — is
under-reported whenever an already-known answer is misread as `rejected`. **This
is narrower than the original filing claimed.** On a healthy zebrad a first
publish returns `error_code 0` with the txid, so the normal case counts
correctly. The mis-mapping bites in two places: (i) after a `Retryable` requeue
where the node in fact already has the transaction, the next flush reads
`transaction already exists in mempool` as `rejected`; and (ii) the second of two
*simultaneously live* hubs, which `shim/src/nym.rs:618-635` states is not the
deployment model (*"sending to every address is therefore safe only while the
other addresses are DEAD"*). The original claim that this happens "on every
honest flush" is withdrawn.

## Technical Details / Code Analysis

**The full path from node string to dropped entry.**

`hub/src/chain.rs:176-198` — one `Publish` per endpoint, folded by `best_of`:

```rust
    pub async fn broadcast(&self, tx_bytes: &[u8]) -> Publish {
        let calls = self.endpoints.iter().map(|addr| {
            let raw = RawTransaction { data: tx_bytes.to_vec(), height: 0 };
            async move {
                match self.unary::<_, SendResponse>(*addr, SEND_TRANSACTION, raw).await {
                    Ok(resp) => classify_send_response(&resp),
                    Err(err) => classify_publish_failure(&err),
                }
            }
        });
        best_of(join_all(calls).await)
    }
```

`hub/src/chain.rs:438-445`:

```rust
fn classify_send_response(resp: &SendResponse) -> Publish {
    if resp.error_code == 0 {
        return Publish::Accepted { txid: resp.error_message.clone() };
    }
    classify_publish_error(&resp.error_message)
}
```

`hub/src/batcher.rs:365-377` — the drop:

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
```

`Rejected` increments a counter; the entry, moved out of `batch` by `into_iter()`,
is never pushed to `unplaced`, so `queue.requeue(unplaced)` at `:390` does not see
it. It is gone. The comment immediately below (`:379-388`) states the intent this
defeats:

> A transport failure goes back into the queue for the next cadence. This is the
> only place such a failure can be recovered … **Dropping it because the indexer
> restarted during the flush window would lose the migration outright while the
> wallet believes it was sent.**

The indexer restarting is handled. The *node behind the indexer* restarting is
not, because that failure arrives as an `OK` response carrying text.

**Where the strings come from, on the zebra side.**

`zebra/zebrad/src/components/mempool/error.rs:53-78`:

```rust
    /// Transaction rejected because the mempool already contains another
    /// transaction with the same hash.
    #[error("transaction already exists in mempool")]
    InMempool,

    /// The transaction hash is already queued, so this request was ignored.
    #[error("transaction dropped because it is already queued for download")]
    AlreadyQueued,

    /// The queue is at capacity, so this request was ignored.
    #[error("transaction dropped because the queue is full")]
    FullQueue,

    /// The mempool is not enabled yet.
    #[error("mempool is disabled since synchronization is behind the chain tip")]
    Disabled,
```

`zebra/zebrad/src/components/mempool.rs:1116-1123` — while the mempool is off,
every queued transaction gets `Disabled`:

```rust
                    Request::Queue(gossiped_txs) => Response::Queued(
                        iter::repeat_n(MempoolError::Disabled, gossiped_txs.len())
                            .map(BoxError::from)
                            .map(Err)
                            .collect(),
                    ),
```

`zebra/zebrad/src/components/mempool.rs:448-450` — and the reason `Disabled` is a
startup-only state:

```rust
            // TODO: only disable an already-active mempool when validated sync
            // state proves Zebra is behind a higher-work chain ...
            (false, true, _) => {
                return false;
            }
```

`zebra/zebra-rpc/src/methods.rs:1322-1348` — the transaction is pushed into
zebra's own retry queue first, then the mempool error is surfaced with its
`Display` string intact:

```rust
        // send transaction to the rpc queue, ignore any error.
        let unmined_transaction = UnminedTx::from(raw_transaction.clone());
        let _ = queue_sender.send(unmined_transaction);
        ...
        let queue_result = queue_results
            .pop()
            .expect("there should be exactly one item in Vec")
            .inspect_err(|err| tracing::debug!("sent transaction to mempool: {:?}", &err))
            .map_misc_error()?
```

`zebra/zebra-rpc/src/server/error.rs:105-107` — the message is the error's
`to_string()`, unmodified, under `LegacyCode::Misc = -1`:

```rust
    fn map_error(self, code: impl Into<ErrorCode>) -> Result<T, ErrorObjectOwned> {
        self.map_err(|error| ErrorObject::owned(code.into().code(), error.to_string(), None::<()>))
    }
```

`lightwalletd/frontend/service.go:554-566` — the relay splits `"<code>: <msg>"`
and puts the message in `SendResponse.error_message` verbatim:

```go
	if rpcErr != nil {
		errParts := strings.SplitN(rpcErr.Error(), ":", 2)
		...
		errMsg = strings.TrimSpace(errParts[1])
		errCode, err = strconv.ParseInt(errParts[0], 10, 32)
```

**Why zcashd hides the defect.** `zcashd/src/rpc/rawtransaction.cpp:1318-1339`
pre-checks the mempool and the chain *before* calling `AcceptToMemoryPool`:

```cpp
    bool fHaveMempool = mempool.exists(hashTx);
    bool fHaveChain = existingCoins && existingCoins->nHeight < 1000000000;
    if (!fHaveMempool && !fHaveChain) {
        ... AcceptToMemoryPool ...
    } else if (fHaveChain) {
        throw JSONRPCError(RPC_TRANSACTION_ALREADY_IN_CHAIN, "transaction already in block chain");
    }
    RelayTransaction(tx);
    return hashTx.GetHex();
```

An already-in-mempool transaction takes neither branch and returns the txid with
`error_code 0`, i.e. `Publish::Accepted`. So on zcashd the "already known" case is
handled by a path that never reaches `classify_publish_error`, and two of the four
substrings (`already known`, `already in mempool`) are close to dead code there.
The function's coverage of zcashd is better than it looks, and its coverage of
zebrad is nil.

**Nothing checks the coupling.** The four strings are a hard-coded model of
another component's error vocabulary, in a monorepo that ships that component's
source. `chain.rs:536-580`'s tests exercise only synthetic strings the author
chose; no test, script or CI job compares the list against any node's actual
output, and no CI runs `cargo test` at all (PROGRESS item 6n).

## Recommendations

Recommendations 1 and 2 **must land together**; 1 alone makes things worse, for
the reason given under 2.

1. **Invert the default for `SendResponse` errors, as `classify_publish_failure`
   already does for gRPC errors.** Treat an *unrecognised* message as
   `Retryable`, and reserve `Rejected` for messages that positively match a
   consensus-failure vocabulary (`bad-txns-*`, `bad-*-nullifiers-*`,
   `tx-overwinter-expired`, `insufficient priority/fee`, `Missing inputs`,
   `did not pass consensus validation`, `is non-standard`, …). The asymmetry is
   the one `chain.rs:485-490` already argues. This single change removes the
   `Disabled` / `FullQueue` / `state service` / `cancelled` destruction without
   needing to enumerate zebra's vocabulary correctly.
2. **Add zebra's already-known strings to the `AlreadyKnown` set at the same
   time**: `already exists in mempool`, `already queued for download`,
   `committed to the best chain`, `already in state`. Keep the existing zcashd
   strings. **This is not cosmetic once (1) is applied.** With (1) alone, an
   already-in-mempool or already-mined transaction becomes `Retryable`, and
   `Queue::requeue` has no expiry sweep and no GC path of any kind (confirmed in
   `hub-queue-requeue-ignores-byte-budget-unbounded-growth.md`; `REVIEW.md:145`
   specifies the deadline that was never implemented). Such an entry would
   therefore be **re-broadcast in every batch, forever**, holding queue bytes and
   emitting exactly the repeated per-transaction timing signal
   `chain.rs:517-521` says the `AlreadyKnown` branch exists to prevent. Applying
   (1) without (2) converts silent destruction into permanent republication.
   Landing the `REVIEW.md:145` deadline GC alongside is the belt-and-braces
   version.
3. **Split `already_known` from `achieved` in the counters** (`batcher.rs:367`,
   `:396-401`) rather than folding them, so the launch-gate number distinguishes
   "this hub put it on the network" from "someone else already had it".
4. **Pin the coupling with a test.** The monorepo already contains zebra's,
   zcashd's, lightwalletd's and Zaino's source as sibling subtrees. A
   table-driven unit test listing each node's actual error strings with its
   expected `Publish` turns a silently-drifting model of another component into
   something a change to that component can break loudly. Nothing about this
   requires a live node.
5. **Use `error_code` as a coarse pre-filter, with text as the refinement — but
   do not replace the text match with it.** The codes do carry signal (zebra:
   `-22` deserialization, `-25` verification refusal, `-1` for everything the
   mempool queue rejected without judging; zcashd: `-27` already-in-chain,
   `-25`/`-26` rejected), and "only `-22`/`-25`/`-26` may be a verdict" is a
   correct and cheap outer guard for both nodes. It is not sufficient on its own,
   because `-1` covers both "never judged" and "already known", which recommendation
   2 needs to tell apart.

## Validation Information

**Status: CONFIRMED. Severity Medium** (filed as Medium; kept, with the reasoning
below).

### What was verified, and how

**1. The predicate is exactly as quoted** — `hub/src/chain.rs:513-533`,
`:438-445`, `:459-474`, `:491-501`; `batcher.rs:341-390`. All line numbers in the
original filing check out against the target.

**2. Every zebra string was extracted from the vendored source and run through
the actual predicate.** The four `contains` tests were applied mechanically to
all 17 error strings reachable from
`zebrad/src/components/mempool/{error.rs,storage.rs,downloads.rs}`.
**Zero matches.** The claim "not one of them contains any of the four substrings"
is exact, not approximate. Verbatim line numbers: `error.rs:50,55,61,70,76,80`;
`storage.rs:100-116`; `downloads.rs:127,130,133,136,139`.

**3. The relay chain was verified end to end.** zebra's mempool-queue errors are
surfaced through `map_misc_error` (`methods.rs:1345`) → `map_error`
(`server/error.rs:105-107`) → `ErrorObject{code: -1, message: <Display string>}`;
lightwalletd's `RawRequest` returns `resp.Error` (a `btcjson.RPCError`, whose
`Error()` is `"<code>: <message>"`) at `rawrequest.go:158-159`, and
`service.go:554-566` splits it and copies the message into
`SendResponse.error_message` **verbatim**. The hub then hits
`classify_send_response` → `classify_publish_error`.

**4. The Zaino partition is confirmed, so the two issues do not double-count.**
`zaino/packages/zaino-state/src/indexer/node_backed_indexer.rs:1561-1570`
constructs `SendResponse { error_code: 0, .. }` on success and returns `Err(..)`
on failure, which becomes a gRPC status and is handled by
`classify_publish_failure`. `classify_publish_error` is genuinely unreachable
behind Zaino. Likewise the `"duplicate"` over-match issue is a *zcashd*-only
phenomenon — zebra's double-spend string is `"transaction inputs were spent, or
nullifiers were revealed, in the best chain"`, which contains no `duplicate`. The
three issues partition cleanly: the function is wrong for every backend, in a
different direction for each.

**5. Reachability is real, not hypothetical — this was the decisive check.**
- `deploy.env.example:22-23` ships `INDEXERS=66.241.124.200:443` /
  `INDEXER_TLS=na.zec.rocks`, and `smoke.sh:46` / `smoke-local.sh:40` use the
  same endpoint.
- zec.rocks publishes its own stack: `zecrocks/zcash-stack`'s `docker/` compose
  set is **Zebra + lightwalletd**, with `compose.zaino.yaml` as an optional
  add-on. Zaino is not the default.
- The zec.rocks operators stated on the Zcash community forum (thread
  "Zec.rocks Zcashd Deprecation Timeline", read 2026-08-18) that as of June 2026
  most endpoints run *"LWD+zebra"* and only `zaino.unsafe.zec.rocks` /
  `zaino.testnet.unsafe.zec.rocks` run Zaino.
- lightwalletd-on-zebrad is documented and CI-tested upstream
  (`zebra/book/src/user/lightwalletd.md`), and lightwalletd's own JSON-RPC client
  says it is *"a context-aware JSON-RPC function for zcashd **and zebrad**"*
  (`rawrequest.go:72-73`).
- zcashd is end-of-life in this very monorepo (*"a supported fork with a
  hardcoded end-of-life, as a transition path only"*, `audit-context/zero/README.md`),
  so the vocabulary the function speaks belongs to the node that is being retired
  and not to the one that is replacing it.

**Conclusion on reachability: this is the shipped example configuration, and no
operator has to choose anything unusual to be in it.**

### Corrections made during validation

- **The trigger was narrowed.** The original filing said zebra disables its
  mempool whenever it "falls behind the tip for any reason". It does not:
  `mempool.rs:448-450` returns early for `(not caught up, already enabled)` with
  the explicit comment that sync status *"is strong enough to delay initial
  activation but not to shut down a working mempool"*. `Disabled` is a
  **startup-only** state (`mempool.rs:330`). Rewritten accordingly.
- **The node-side mitigation was quantified and promoted from a footnote to a
  bound on the finding.** `send_raw_transaction` enqueues the transaction into
  zebra's own retry queue *before* asking the mempool (`methods.rs:1322-1324`),
  and that queue retries on every tip change for
  `5 × 75 s + 5 s = 380 s` (`queue.rs:39,206-241`). So loss requires zebra to
  still be behind the tip ~6.3 minutes after the flush. Short restarts lose
  nothing. This is the single largest reason the issue is Medium rather than
  High.
- **Two more "never judged" rows were added** that the original filing missed —
  `error in state service: …` and `transaction download / verification was
  cancelled` (`downloads.rs:130,136`) — and one that was implied but is *not*
  reachable was excluded: zebra's `"timeout waiting for verification result"`
  fires at 73 s (`downloads.rs:496,514`, `crawler.rs:84`), long after the hub's
  own `RPC_TIMEOUT = 10 s` (`chain.rs:48`) has already produced a (correct)
  `Retryable`.
- **The "milder half" was cut back.** The claim that `achieved_batch_size = 0`
  and `rejected = N` "on every honest flush at the second hub" requires two
  simultaneously live hubs, which `shim/src/nym.rs:618-635` states is not the
  deployment model. On a healthy zebrad the first publish is `error_code 0` and
  counts correctly. The two cases where the mis-mapping does bite are named
  explicitly in the impact section.
- **Recommendation 1 was found to be dangerous on its own, and this is now stated
  in the issue.** Inverting the default without also extending the `AlreadyKnown`
  set turns every already-in-mempool and already-mined answer into `Retryable`;
  `Queue::requeue` has no expiry sweep and no GC path (`queue.rs:279-295`;
  independently confirmed in
  `hub-queue-requeue-ignores-byte-budget-unbounded-growth.md`, and
  `REVIEW.md:145` specifies the deadline that was never built), so those entries
  would be re-broadcast in every batch forever — the exact repeated timing signal
  `chain.rs:517-521` invokes as the reason the `AlreadyKnown` branch exists.
  Recommendations 1 and 2 are now explicitly coupled.
- **Recommendation 5 was corrected.** "Prefer the code over the text" as
  originally written would be wrong: zebra returns `-1` (`LegacyCode::Misc`) for
  *both* "never judged" (`Disabled`, `FullQueue`) and "already known"
  (`InMempool`, `Mined`), so the code cannot replace the text match. It is a
  useful outer guard, not a replacement.

### Severity: why Medium, not High or Low

**Not Low.** This is a real correctness defect in the requeue-vs-drop split — the
seam `chain.rs:479-490` calls out as the one that must be *"drawn honestly"* —
and it silently and permanently destroys migrations the wallet was already told
had succeeded, in the configuration the project ships as its example, with no
adversary present. The failure is per-node rather than per-transaction, so one
occurrence takes the whole batch, and the hub's own log (`rejected = N`) actively
misleads the operator about what happened.

**Not High.** Four things bound it, and all four were checked rather than assumed:
(i) zebra's own 380 s RPC retry queue absorbs any restart that reaches the tip
inside ~6 minutes, which is most of them; (ii) `Disabled` is reachable only from
process start, not from a running node drifting behind, so the trigger is
restarts and first deployments rather than any lag; (iii) at present adoption the
modal batch is 0–1, so a single event destroys 0–2 migrations; (iv) for today's
traffic class the wallet's automatic-retry horizon is the ~50-minute ZIP 203
expiry, not the 30–60 day ZIP 318 one. **NOTE, corrected 2026-08-18 (PROGRESS item
8a): this cuts the opposite way from how it was originally written.** The short
horizon is the *worse* one, so today's traffic is the harder case, not the easier
one; the deflation to Medium survives on (i)–(iii) alone, because zebra's 380 s
retry queue and the wallet's own ~20 s resubmissions both sit well inside 50
minutes. The deliberate variant (an indexer
operator restarting their node at a cadence boundary) adds deniability but no
capability beyond the separately-filed hostile-indexer issue, so it must not be
counted twice.

**Severity would rise to High** if any of these changed: adoption rises so a batch
holds tens of migrations, or the hub is pointed at an indexer whose node is
routinely far from the tip. **One escalation trigger was STRUCK 2026-08-18** —
*"ZIP 318 conforming wallets ship (the recovery clock becomes 30–60 days)"* rested
on a premise refuted during validation of
`issues/invalid/zip318-canonical-expiry-…md` (PROGRESS item 8a). ZIP 318's long
canonical expiry is a long **automatic-retry horizon**, not a long wait, so
conforming wallets shipping makes this issue's outcome *better*, not worse: the
wallet keeps resubmitting for 30–60 days instead of giving up after ~50 minutes.
Do not escalate on it.

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


### Scope note

Kept strictly to the zebrad-backed-indexer direction. The Zaino direction
(`hub-chain-zaino-node-rejections-are-never-verdicts.md`), the `"duplicate"`
over-match (`hub-chain-duplicate-nullifier-rejection-counted-as-published.md`),
and the hostile-indexer verdict question
(`hub-flush-destroys-migration-on-single-unverifiable-verdict.md`) are separate
issues with separate mechanisms; nothing here restates them.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
