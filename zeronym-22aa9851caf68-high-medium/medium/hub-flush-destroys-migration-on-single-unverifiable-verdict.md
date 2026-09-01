# `flush` treats one endpoint's unverifiable `Rejected` as final and deletes the last copy of the migration, so a hostile or buggy indexer permanently destroys a transaction the wallet was already told had been sent

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/batcher.rs:361-390` (the requeue-vs-drop split in `flush`, the `Rejected` arm at `:368`, the rationale at `:379-389`) and `:333-340` (the claim about what the returned count measures); verdicts produced at `audit-target/zeronym/hub/src/chain.rs:438-445` (`classify_send_response`), `:446-474` (`best_of`), `:476-497` (`classify_publish_failure`), `:503-533` (`classify_publish_error`); the queue side at `audit-target/zeronym/hub/src/queue.rs:252-264` (`drain_shuffled`) and `:279-295` (`requeue`); the acknowledgement boundary at `audit-target/zeronym/shim/src/hub.rs:225-249`. Deployed endpoint count: `audit-target/zeronym/deploy.env.example:22`.
**Found by agent:** Local (file audit of `hub/src/batcher.rs`)
**In scope of audit?** Yes

## Description

`flush` splits the batch three ways after `broadcast_batch` returns
(`batcher.rs:361-390`):

* `Accepted` / `AlreadyKnown` — counted as achieved and **the entry is dropped**;
* `Rejected` — counted as rejected and **the entry is dropped**;
* `Retryable` / no verdict — the entry is requeued for the next cadence flush.

Only the third arm keeps a migration alive. Every one of those verdicts is
derived from a string or a gRPC status the indexer chose, and the hub verifies
none of it. With the shipped configuration there is exactly one indexer
(`deploy.env.example:22`, `INDEXERS=66.241.124.200:443`), so `best_of`
(`chain.rs:459-473`) folds a single endpoint's answer and there is no second
opinion. The threat model for this engagement names that party explicitly:
*"hub → indexer: sees the whole batch seconds before it is public; **can lie
about the tip and about publish verdicts**"*.

**One `Rejected` answer destroys a migration, unrecoverably, at the first
flush.** The entry is removed from the queue by `drain_shuffled`
(`queue.rs:252-264`, which drains the whole map) and is simply not put back, so
its `Zeroizing` buffer is dropped. No copy exists anywhere else in the system:
the shim answered the wallet `error_code 0` at mixnet hand-off and keeps no
per-migration state (`shim/src/hub.rs:230-240`), and the enclave is diskless.
`flush`'s own comment (`batcher.rs:379-389`) explains why a *transport* failure
must be requeued — "once the entry left this queue there is no other copy
anywhere that anyone will retry" — and then applies the opposite rule to a
verdict it cannot verify. `chain.rs:487-490` states the same principle even more
sharply, for the transport case only:

> re-offering an entry costs one call per flush and stops at its expiry, while
> **dropping a valid migration on a misread error is unrecoverable, because the
> shim has already told the wallet it was sent**.

Two properties make this worse than "the node said no":

1. **It is not fixed by adding endpoints, and gets a new failure mode from
   them.** `Rejected` (rank 1) outranks `Retryable` (rank 0) in `best_of`, and
   `Retryable` is the only outcome that keeps an entry alive. So during any
   outage of the honest endpoints — precisely when the requeue is the mechanism
   protecting the entry — one endpoint answering `Rejected` outranks their
   `Retryable` and the entry is destroyed. The publish path is n-of-n for
   *delivery* and **1-of-n for the retry**. The confirmed
   `tip-and-verdict-aggregation-scale-in-opposite-directions-…` names this
   "recovery veto" and delegates it here by name.
2. **It reaches the population the product exists for.** The other route to a
   `Rejected` drop — holding a transaction back until it expires, owned by the
   confirmed `indexer-chooses-which-batch-members-reach-the-chain-…` — works only
   against short-expiry traffic; that issue states plainly that ZIP 318
   migrations, with 34,561–69,120 blocks of slack, "survive indefinitely" under
   it. A direct `Rejected` answer destroys them on the first flush.

A second, smaller defect sits in the same function. `batcher.rs:335-336` calls
the returned count *"the honest measure of the privacy the flush actually
delivered"*, and `REVIEW.md` #9 makes the distribution of that number the
**launch gate** for the whole product. It is not a measure of what reached the
network; it is a tally of what the indexer *said*. Nothing ever checks the chain
afterwards — the confirmation tracking `REVIEW.md` #7 specifies is a documented
not-built item — and `flush` does not even compare the txid the endpoint returns
in `Publish::Accepted { txid }` against `Entry::txid`, which the hub computed
from the same bytes at admission (`queue.rs:125`, `Publish::Accepted`'s payload
discarded at `batcher.rs:367`).

The design's stated position (`REVIEW.md` #5) is that "`sendrawtransaction` at the
node is the only authority on validity". The implementation extends that authority
from a *node* to a third-party *indexer relay* in front of it — `chain.rs:17-21`
acknowledges the difference ("an indexer is a single funnel in front of a single
node") — and that relay is a member of the adversary class the product exists to
defend against.

## Attack Scenario and Steps

Attacker: the operator of the hub's indexer, or anyone who compromises or compels
it. It sees every batch member in plaintext seconds before publication (a stated
residual), so it can select by content — a value balance, an action count, a
length, an `anchorOrchard`, or a txid supplied out of band.

Targeted destruction:

1. The hub flushes a batch; the indexer receives all `k` members concurrently
   over `SendTransaction`, each on its own connection with a 10 s budget
   (`chain.rs:198-210`, `:269-337`), so it holds the whole batch before it has to
   answer any of it.
2. For the chosen member, the indexer answers gRPC `INVALID_ARGUMENT` (status 3),
   or `OK` carrying `SendResponse { error_code: -26, error_message: "16:
   bad-txns-orchard-binding-signature-invalid" }`. The first maps to
   `Publish::Rejected` at `chain.rs:491-496`; the second at `chain.rs:513-533`,
   because it matches none of the four `AlreadyKnown` substrings.
3. `best_of` has one outcome to fold, so the transaction's verdict is `Rejected`.
4. `flush` increments `rejected` and does **not** put the entry in `unplaced`, so
   `queue.requeue` never sees it (`batcher.rs:367-390`). The entry's `Zeroizing`
   buffer is dropped when the loop iteration ends.
5. It broadcasts every other member normally, so the hub's log line reads
   `flush_size = k, achieved_batch_size = k-1, rejected = 1` — a single count with
   no identifier, indistinguishable from one genuinely invalid transaction, and
   written to a console that does not exist in an attested deployment.

The wallet is never told. A resend by the user *would* be admitted (dedup is on
`sha256(tx_bytes)` and the entry is gone, so it is a fresh admission, and
`queue.rs`'s dedup makes a resend safe) — but nothing produces a
non-confirmation signal to prompt one, and a resend meets the same endpoint and
the same answer.

**Attack Requirements and Assumptions:**

- Requires control of, or a bug in, an indexer the hub publishes through — the
  shipped configuration has exactly one. **No mixnet access, no shim, and no
  chain observation are needed, and there is no internet-reachable path to this
  behaviour** (coordinator item 6p: the `Rejected` drop is a hub-trust /
  robustness defect, not a remote weapon).
- The same outcome arises without an adversary. An indexer that returns
  `INVALID_ARGUMENT` for a transaction version it does not recognise — a live
  concern across an NU6.3/v6 rollout — causes the hub to destroy every such
  migration rather than hold it. The parallel defect on the *other* verdict path,
  where the shipped example backend (lightwalletd + zebrad) produces text that no
  `AlreadyKnown` substring matches and "not judged" becomes destruction, is
  separately confirmed as
  `publish-verdict-strings-are-zcashds-vocabulary-only-so-a-zebrad-backed-indexer-turns-not-judged-into-permanent-destruction.md`
  and is not re-claimed here.
- Users cannot detect it: the wallet was told success, and the hub reports counts
  only, to a console that does not exist under `debug { enabled = false }`.

## Impact on Users

- A migration the wallet reported as sent never reaches the network and is never
  retried by anything. The user's value stays in a pool NU6.3 closed to new
  value, and the only automatic clock that can ever surface the failure is the
  transaction's own expiry — roughly 50 minutes for an ordinary Orchard spend and
  **30–60 days for the ZIP 318 migration the product exists for**
  (`zip318-canonical-expiry-is-the-only-recovery-clock-…`). Non-confirmation
  monitoring is asked of wallet vendors in `hub/deploy/caution/OPERATORS.md:190-195`
  and is guaranteed by nothing in this system.
- This is loss of a *submission*, not of funds: the note is not spent on chain and
  the user can build and send a replacement once they notice. Against a hostile
  endpoint the replacement is destroyed the same way, so the practical outcome is
  targeted, indefinite, silent censorship of one user's migration through the
  private path.
- Separately, the number `REVIEW.md` #9 designates as the launch gate for the
  product's privacy claim is a tally of the measured party's own assertions.

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


## Technical Details / Code Analysis

`hub/src/batcher.rs:361-390` — the split, in full:

```rust
    let mut achieved = 0usize;
    let mut rejected = 0usize;
    let mut sample_failure: Option<String> = None;
    let mut unplaced = Vec::new();
    for (i, entry) in batch.into_iter().enumerate() {
        match outcomes.get(i) {
            Some(Publish::Accepted { .. }) | Some(Publish::AlreadyKnown) => achieved += 1,
            Some(Publish::Rejected { .. }) => rejected += 1,          // entry falls out of scope here
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

and the comment that decides it (`batcher.rs:386-389`):

```rust
    // A Rejected verdict is not put back: the node said no, and re-offering the
    // same bytes buys the same answer every flush until expiry.
```

That reasoning is sound for an honest node and unsound for the party actually
answering, which is an indexer relay the threat model classes as able to lie.

`hub/src/chain.rs:459-473` — with one endpoint, `best_of` is the identity function
on that endpoint's verdict, and with several it is a 1-of-n veto on the retry:

```rust
fn best_of(outcomes: Vec<Publish>) -> Publish {
    fn rank(outcome: &Publish) -> u8 {
        match outcome {
            Publish::Accepted { .. } => 3,
            Publish::AlreadyKnown => 2,
            Publish::Rejected { .. } => 1,
            Publish::Retryable { .. } => 0,
        }
    }
    outcomes.into_iter().max_by_key(rank).unwrap_or(Publish::Rejected { … })
}
```

Its doc comment (`chain.rs:446-458`) argues the ordering carefully — a dead
endpoint must not keep a doomed transaction resident until expiry, and an
unparseable payload never expires — and closes with *"A verdict from any endpoint
that answered is final, exactly as it is today with one endpoint."* The argument
is correct for the case it considers and never considers a hostile answer.

`hub/src/chain.rs:491-496` and `:513-533` — the two roads to `Rejected`, one a
status code the endpoint picks, one a free-text string it writes:

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

fn classify_publish_error(message: &str) -> Publish {
    let m = message.to_ascii_lowercase().replace('-', " ");
    if m.contains("already in block chain") || m.contains("already known")
        || m.contains("already in mempool") || m.contains("duplicate") {
        Publish::AlreadyKnown
    } else {
        Publish::Rejected { reason: message.to_string() }
    }
}
```

`hub/src/queue.rs:135` declares `received_height` as "The tip when this was
admitted. **Drives the confirmation deadline.**" A repository-wide search shows
the field is written at `queue.rs:235` and **read nowhere** in `hub/src` or
`hub/tests`: there is no confirmation deadline and no awaiting-confirmation set of
the kind `REVIEW.md`'s implementation rules describe (`REVIEW.md:145`). The
absence of confirmation tracking is a documented limitation and is not the
finding; the dead field asserting otherwise, and the decision to delete the last
copy of an entry in its absence, are. (The same dead field is noted in the
validation of `hub-chain-zaino-node-rejections-are-never-verdicts.md`; it belongs
to the family, not to this file alone.)

## Recommendations

- **Do not treat a single unverifiable verdict as final.** Either requeue
  `Rejected` entries until their expiry, or require corroboration from at least
  two independent endpoints before destroying an entry. The bounded cost is one
  call per flush per held entry, which is what `chain.rs:487-490` already accepts
  for the transport case.
- **Sequence this with a GC path, or it regresses into the opposite defect.**
  Retaining rejected entries is only safe once the queue can evict on its own:
  `survives_next_flush` is consulted **only** in `admit` (`queue.rs:202`), so
  `requeue` and `flush` apply no expiry test, and an unparseable payload has
  `expiry = None` and would become immortal. `REVIEW.md:145` already specifies the
  mechanism (`received_height + 2N`, its own byte budget), and
  `hub-chain-zaino-node-rejections-are-never-verdicts.md` asks for the same GC
  path from the other direction. **Implement the GC first, then the retention.**
- **Note for the report's remediation ordering:** the standing advice "deploy more
  than one independent indexer" — correct for the hold-back and blackhole attacks
  — does **not** fix this direction and introduces the 1-of-n recovery veto. Ship
  the two together, and see
  `hub-one-tls-name-for-the-whole-indexer-list-…` for why "independent" is not
  expressible in today's configuration surface.
- **Cross-check the txid the endpoint returns** against `Entry::txid`, which the
  hub already computed from the same bytes. It is free and it catches a lazy or
  broken endpoint; it does not defeat a competent liar, which is why it is a
  sanity check and not the fix.
- Log rejections with enough aggregate detail for an operator to notice a pattern
  (a per-flush rejection *rate*, an `observed vs expected` counter), while keeping
  to the counts-only rule — and give it an egress path, since a `tracing` line
  reaches no console in an attested enclave.
- Either implement the confirmation check `REVIEW.md` #7 specifies, or change
  `batcher.rs:335-336` to say that `achieved_batch_size` measures what the indexer
  reported rather than "the privacy the flush actually delivered".
- Remove or implement `Entry::received_height`'s "drives the confirmation
  deadline" claim.

## Validation Information

**Verdict: CONFIRMED at Medium** (severity as filed).

**Every mechanical claim was re-derived in the target during validation:**

- `batcher.rs:361-390` — `unplaced` is fed by the `Retryable` and `None` arms
  only; the `Rejected` arm at `:368` increments a counter and lets `entry` fall
  out of scope. `queue.requeue` at `:390` therefore never sees it.
- `queue.rs:252-264` — `drain_shuffled` drains the entire map and zeroes `bytes`,
  so an entry not returned by `requeue` is gone from the process.
- `shim/src/hub.rs:230-240` — on the deployed mixnet transport a successful
  hand-off is reported to the wallet as `Submit::Accepted { txid: local_txid(...) }`,
  with an explicit comment that the hub's verdict is "deliberately not waited
  for". The shim retains nothing.
- `chain.rs:459-473` — `rank` is `Accepted 3 / AlreadyKnown 2 / Rejected 1 /
  Retryable 0`, so `Rejected` beats `Retryable` at any `n`; `max_by_key` on a
  one-element vector is the identity.
- `chain.rs:491-496` — status 3 and status 9 are the only codes mapped to
  `Rejected`; `chain.rs:513-533` — the four substrings are the only escapes from
  `Rejected` on the `OK`-with-non-zero-code path.
- `deploy.env.example:22-23` — one endpoint, `INDEXER_TLS=na.zec.rocks`.
- `received_height`: `grep -rn received_height hub/ shim/` returns exactly
  `queue.rs:135` (declaration, with the doc comment) and `queue.rs:235` (write),
  plus two `REVIEW.md` lines. It is read nowhere.
- `Publish::Accepted { txid }` is discarded at `batcher.rs:367`; `Entry::txid`
  exists at `queue.rs:125`. No comparison is made anywhere.

**Corrections applied against the filing:**

1. **"the migration ceases to exist" was softened.** What is destroyed is the
   hub's only copy and any prospect of the system retrying it. The wallet still
   holds the transaction and a user-initiated resend is admissible and safe (item
   7r established the same point for the runbook issue). The real defect is that
   nothing in *this system* produces a signal — but see the CORRECTION above:
   the wallet's own sync does, and both official SDKs resend automatically for the
   whole expiry window, so what this defect actually destroys is every submission
   made while the condition holds, not a single one.
2. **Loss-of-funds framing removed**, per the precedent set in item 7o: the note
   is not spent on chain, so this is silent destruction of a *submission*, not loss
   of funds. **The "30–60 day recovery clock" half of this sentence was REFUTED
   2026-08-18** (PROGRESS item 8a): 30–60 days is the wallet's automatic-**retry
   horizon**, during which it keeps resubmitting, not a period of waiting.
3. **The "silent blackhole" leg was demoted to evidence and its harm delegated.**
   An indexer answering `error_code: 0` for everything while broadcasting nothing
   is the *blackhole* attack, which the confirmed
   `indexer-chooses-which-batch-members-reach-the-chain-…` owns and which
   additional endpoints genuinely fix (an honest endpoint's real relay makes the
   lie irrelevant). What survives here is narrower and unowned elsewhere: the
   count `REVIEW.md` #9 makes the launch gate is a tally of the indexer's
   assertions, the code comment says the opposite, and a free txid cross-check is
   not performed.
4. **The "ordinary bug" leg was scoped.** `classify_publish_error`'s
   "anything unrecognised is a rejection" default against a real backend's real
   vocabulary is the confirmed zebrad Medium; this file keeps only the
   `classify_publish_failure` (status 3) instance and cross-references it.
5. Recommendation 1 now carries the **ordering constraint** (GC before retention)
   that the Zaino sibling makes necessary, and the "deploy more endpoints" advice
   is explicitly qualified rather than repeated unqualified as in the filing.

**What this issue owns after reconciliation with its four siblings — checked file
by file, because the risk of double-counting here is high:**

| Sibling | Direction | What it owns |
|---|---|---|
| `publish-verdict-strings-are-zcashds-vocabulary-only-…` (confirmed Medium) | zebrad: "not judged" → `Rejected` | the **vocabulary** of `classify_publish_error` against one real backend |
| `hub-chain-zaino-node-rejections-are-never-verdicts.md` (confirmed Medium) | Zaino: every refusal → `Retryable` | the **immortal-entry** direction and the GC ask |
| `hub-chain-duplicate-nullifier-rejection-counted-as-published.md` (plausible) | zcashd: double-spend → `AlreadyKnown` | the **false-success** direction |
| `indexer-chooses-which-batch-members-reach-the-chain-…` (confirmed) | `Retryable` → hold-back | **anonymity**: which members reach the chain, and destruction *via expiry* for short-expiry traffic only |
| **this file** | `Rejected` → immediate, terminal drop | the **decision that a single unverifiable verdict is final**, its 1-of-n recovery-veto form at `n > 1`, immediate destruction of the **ZIP 318 population** (which the hold-back route explicitly cannot reach), and `achieved_batch_size`'s misdescription |

**It is not subsumed, and the audit's own accounting already depends on it.** The
confirmed `tip-and-verdict-aggregation-scale-in-opposite-directions-…` was
deflated to Low with an ownership map that assigns "One unverifiable `Rejected`
destroys an entry" **to this file by name**, and notes that this file's
recommendation is also the fix for the `n > 1` retry-veto form. Merging it away
would orphan that harm and leave the recovery-veto unowned.

**Severity justification — Medium.**
*Why not High:* item 6p's bound applies exactly as it does to the four confirmed
tip findings — the attacker must control a configured `ZIH_INDEXERS` endpoint, so
this is a hub-trust and robustness defect, not an internet-reachable weapon; the
harm is loss of a submission with a recovery path the user can take once they
notice; and the accidental instances against real backends are separately graded.
*Why not Low:* it destroys wallet-acknowledged migrations belonging to the exact
traffic class the product exists for, on the first flush, chosen per victim from
plaintext the endpoint is handed by design; it is undetectable by the wallet, the
user and (in an attested enclave) the operator; the shipped configuration makes
one party's word final; and the code's own stated principle — never drop a valid
migration on an unverifiable error — is applied to the transport path and not to
the verdict path four lines away.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
