# The hub's `GetTransaction` fall-through delivers every wallet's txid to whichever indexer the hub is pointed at, and the shipped example points it at the same host the shim fronts

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/hub/src/server.rs:296-322` (`Hub::lookup`, queue-then-indexer, no cache); `audit-target/zeronym/hub/src/chain.rs:212-249` (`get_transaction` sends the wallet's `TxFilter.hash` to every configured endpoint); `audit-target/zeronym/shim/src/intercept.rs:229-236, 295-323` (every lookup is routed to the hub, no cache); `audit-target/zeronym/deploy.env.example:16-17` (`BACKEND`) versus `:22-23` (`INDEXERS`); `audit-target/zeronym/deploy.sh:110-116`; `audit-target/zeronym/smoke-local.sh:39-40`; `audit-target/zeronym/hub/deploy/caution/OPERATORS.md:281`. Claims contradicted: `audit-target/zeronym/README.md:28`, `audit-target/zeronym/shim/src/intercept.rs:231-232`, `audit-target/zeronym/hub/REVIEW.md:97-99` (REVIEW #7).
**Found by agent:** Global (focus area G26 "isolate a target's migration into a batch of one", reached from G14's end-to-end pipeline trace); validated and re-scoped by the Issue Validator
**In scope of audit?** Yes

## Description

The shim intercepts **every** `GetTransaction` and routes it to the hub. Its own
doc comment states the property this is supposed to buy
(`shim/src/intercept.rs:231-235`):

> With a hub configured, EVERY `GetTransaction` is answered via the hub and NONE
> reaches the operator's indexer.

and `README.md:28` sells it to users:

> **`GetTransaction`.** The shim routes every lookup to the hub, so the operator
> cannot watch a wallet fetch back the transaction it just diverted. This moves
> the lookup to the hub rather than making it private outright.

The hub does not hold the lookup. `Hub::lookup` checks its in-RAM queue first, and
**on a miss forwards the wallet's `TxFilter.hash` verbatim to every endpoint in
`ZIH_INDEXERS`** (`server.rs:305`, `chain.rs:221-249`). The queue answers only in
the window between admission and the next flush; after publication — the entire
period a wallet spends confirming, and the only period in which librustzcash's
`TransactionDataRequest::Enhancement(txid)` fires at all (`hub/REVIEW.md:99`) —
every lookup misses and is forwarded. Neither side caches: the shim is stateless
by design (`intercept.rs:193`) and `Hub::lookup` dials the indexer on every miss,
so `n` wallet polls produce exactly `n` upstream queries naming the same txid.

`chain.rs:212-220` states the consequence itself, accurately, while being silent
about who the endpoint is:

> The bytes are passed through unmodified … so behaviour is identical to a direct
> query by construction.

That is the problem. If the endpoint is an indexer the operator runs, the query
the shim refused to send them arrives at them anyway — byte-identical, one Nym
round trip later, with only the source IP removed.

**The shipped example makes the two the same host.** `deploy.env.example` — the
single file `deploy.sh` reads for *both* components (`deploy.sh:110-116`) — sets
`BACKEND=66.241.124.200:443` / `BACKEND_TLS=na.zec.rocks` (`:16-17`) and
`INDEXERS=66.241.124.200:443` / `INDEXER_TLS=na.zec.rocks` (`:22-23`). Same IPv4,
same port, same certificate name. `smoke-local.sh:39-40` defaults identically.
Nothing in `deploy.sh`, `hub/src/config.rs`, or any document requires them to be
different parties; `hub/deploy/caution/OPERATORS.md:281` describes `ZIH_INDEXERS`
only as *"your indexers; every batch member goes to all of them"* — a broadcast
description for a value that also receives every wallet's lookups.

This uses **no defect**. Every component behaves exactly as written.

## Attack Scenario and Steps

Attacker: the party that operates the indexer named in `ZIH_INDEXERS`, in the case
where that party also fronts a participating shim — i.e. owns the shim's Nitro
parent host and therefore sees wallet source IPs at the TCP layer, which the
threat model records as the primary adversary's standing capability. Wholly
passive; nothing is injected and no fee is paid.

1. Wallet `W` submits an Orchard-touching transaction. The shim diverts it, so the
   operator's indexer sees **no** `SendTransaction`. That absence, against a
   connection whose source IP the parent host reads, is the residual `README.md:33`
   and `hub/REVIEW.md:181` both concede: *the operator learns that client IP C
   submitted an Orchard-touching transaction at time T.*
2. The hub publishes the batch at the next cadence height — through
   `ZIH_INDEXERS`, i.e. through this same operator, who therefore already holds
   every batch member's plaintext (`chain.rs:176-210`; a conceded residual).
   What they do **not** have is which member is `W`'s.
3. `W`'s wallet begins fetching the transaction. Each fetch is a `GetTransaction`
   to the shim, diverted to the hub over Nym; post-flush every one misses the
   queue and becomes `GetTransaction(TxFilter { hash: T })` from the hub's fixed
   IP to the operator's indexer, over TLS the operator terminates.
4. The operator now holds two time series they own both ends of: on their own
   parent host, `W`'s request/response bursts inside the wallet's TLS; on their own
   indexer, per-txid lookup arrivals offset by one Nym round trip (measured 9–10 s
   unary, `batcher.rs:45-47`). Matching the two by **phase and count over repeated
   polls** names `T` as `W`'s transaction.
5. `T` is on chain; the operator reads `valueBalanceOrchard` off it. IP → transaction
   → balance, complete.

**Attack Requirements and Assumptions:**
- The operator must be both the shim's `BACKEND`/parent host **and** (one of) the
  hub's `ZIH_INDEXERS`. The shipped example sets `BACKEND` and `INDEXERS` to the
  same host, port and TLS name, and the product is aimed at exactly the handful of
  public indexer operators, so the conjunction is realistic — but it is a
  **deployment property under the hub operator's control**, not something the code
  forces.
- Conditional on wallets issuing `GetTransaction` at all. `server.rs:288-296` is
  the project's own model (*"Wallets poll on multi-second intervals and tolerate a
  transient NOT_FOUND"*), and `hub/REVIEW.md:99` cites librustzcash's
  `Enhancement(txid)` firing once the scan sees the transaction mined. Either way
  the query happens; the number of samples decides how sharp step 4 is.
- Retrospective: the operator's own logs plus the public chain suffice, so the join
  can be made at any later date.

**Honest bound on step 4, corrected during validation.** The filed issue claimed
the two envelopes share *"the same start, the same stop, the same period and the
same count"*. Start and stop are **not** discriminating: every member of a batch
becomes queryable at the same flush and confirms in the same block or nearby, so
all `k` lookup streams begin and end together. The discriminators are **poll phase
and period**, matched through Nym's jitter. With a handful of polls and low
concurrency this is decisive; with a large batch of same-app wallets it requires
averaging over many samples. It is a correlation channel with an error rate, not a
naming primitive.

## Impact on Users

- **The product's core linkage, reached without any defect.** Where the
  co-location holds, the operator obtains IP → on-chain transaction → balance for
  their own wallets, passively and retrospectively. `hub/REVIEW.md:97-99` (REVIEW
  #7) states the harm in the project's own words: *"the txid completes the link and
  bypasses Nym, the TEE and the batch in one query"*, and mandates that *"the shim
  must never issue a txid-specific query to its backing indexer for a diverted
  migration."* The shim obeys; the hub then issues that query to the same indexer
  on the shim's behalf.
- **Independent of the mechanisms the project is relying on to improve.**
  `README.md:34` states the remedy for the batching residual as *"the lever is
  adoption, not code"*. This channel names one transaction out of the batch by
  query correlation rather than by set-membership arithmetic, so raising `k`
  weakens it only through concurrency, not through anonymity-set size. It also
  **survives ZIP 318 conformance**: the already-filed core-linkage chain selects
  within a batch by `(length, anchor, expiry)`, and those become uniform exactly
  when wallets do what `README.md:69` asks; this channel uses no transaction shape
  at all.
- **`README.md:28` is an overclaim for the shipped configuration.** "The operator
  cannot watch a wallet fetch back the transaction it just diverted" is false when
  `ZIH_INDEXERS` is the operator: they see the fetch, just not the wallet's IP and
  not on the wallet's connection. The hedge that follows ("rather than making it
  private outright") does not tell a reader that the query may be delivered back to
  the party it was routed away from.
- **Undisclosed third-party export.** A shim run by operator `B` sends its wallets'
  lookups to the hub, which forwards them to whatever `ZIH_INDEXERS` names — a
  value `B` cannot observe, that is not in `B`'s attestation, and that no document
  discloses. `shim/ENDPOINTS.md:167-175` reasons about this trust shift for a
  *planned, Zeronym-operated* indexer and argues it is acceptable because it is *"a
  gain versus the operator (who additionally sees the on-chain publication and
  could correlate)"*. The shipped `ZIH_INDEXERS` is not that indexer, and can be
  precisely the operator that analysis excluded.

## Technical Details / Code Analysis

The fall-through (`hub/src/server.rs:296-312`):

```rust
    pub async fn lookup(&self, wire_hash: &[u8]) -> LookupOutcome {
        if let Some(bytes) = self.queue.find_by_txid(wire_hash) {
            tracing::debug!(source = "queue", "transaction lookup answered");
            return LookupOutcome::Found { data: bytes, height: 0 };
        }

        match self.chain.get_transaction(wire_hash).await {
            Ok(TxLookup::Found { data, height }) => { ... }
```

and what `chain.get_transaction` does with it (`hub/src/chain.rs:221-232`):

```rust
    pub async fn get_transaction(&self, wire_hash: &[u8]) -> Result<TxLookup, BoxError> {
        let calls = self.endpoints.iter().map(|addr| {
            let filter = TxFilter {
                block: None,
                index: 0,
                hash: wire_hash.to_vec(),
            };
```

`self.endpoints.iter()` — the hash goes to **every** configured endpoint, so
adding endpoints widens the disclosure linearly. This is the same `endpoints` list
`broadcast_batch` publishes through (`chain.rs:176-210`), so the lookup recipient
is by construction the party that already saw the whole batch in plaintext; the
new information is the *per-txid, per-poll timing*, not the content.

The window in which the queue answers, and why it is the minority of a
transaction's life (`hub/src/server.rs:288-296`):

> Note the flush-in-flight gap: `flush()` drains the queue before `broadcast_batch`
> has reached the indexer, so a lookup in that window gets a queue miss then an
> indexer NOT_FOUND … Wallets poll on multi-second intervals and tolerate a
> transient NOT_FOUND

No cache exists on either side. The shim's `get_transaction`
(`shim/src/intercept.rs:295`) calls `diversion.hub.get_transaction(&filter.hash)`
unconditionally for every request, and `intercept.rs:193` states *"Nothing is
recorded: the shim keeps no map of what it diverted."* `Hub::lookup` has no
memoisation of any kind.

The configuration, from the single file both components are deployed from
(`audit-target/zeronym/deploy.sh:110-116`):

```sh
  : "${BACKEND:?set BACKEND for a shim}"; : "${BACKEND_TLS:?set BACKEND_TLS for a shim}"
  set -- "$@" --backend "$BACKEND" --backend-tls "$BACKEND_TLS"
  ...
  : "${INDEXERS:?set INDEXERS for a hub}"; : "${INDEXER_TLS:?set INDEXER_TLS for a hub}"
  set -- "$@" --indexers "$INDEXERS" --indexer-tls "$INDEXER_TLS"
```

with `deploy.env.example:16-17` and `:22-23` both naming `66.241.124.200:443` /
`na.zec.rocks`. There is no check anywhere that `INDEXERS` and `BACKEND` are
disjoint, and no warning that they should be.

## Recommendations

- **Separate the publish endpoint from the lookup endpoint in configuration.** The
  hub needs a broadcast path (inherently operator-visible network) and a
  chain-query path (which is not). Two configuration values let an operator publish
  through a public indexer while answering lookups somewhere the fleet's wallets
  are not exposed to. This is the structural fix.
- **Say, where an operator will read it, that `ZIH_INDEXERS` must not be an indexer
  that fronts a participating shim.** At minimum make `deploy.env.example`'s
  `BACKEND` and `INDEXERS` visibly different values with a comment explaining why,
  and add the requirement to the `ZIH_INDEXERS` row of
  `hub/deploy/caution/OPERATORS.md:281` and to `shim/deploy/caution/OPERATORS.md`.
- **Coalesce the fall-through.** Answer repeated lookups for the same txid from a
  short-lived in-hub cache so `n` polls produce one upstream query; optionally add
  jitter or cross-wallet batching. All of these attack the phase-matching step,
  which is what makes step 4 work.
- **Correct `README.md:28`.** The accurate statement is that the operator never
  sees the query *on the wallet's connection*, and whether they see it at all
  depends on a hub-side configuration value the shim operator cannot observe.
- **Disclose `ZIH_INDEXERS` to participating shim operators**, since their wallets'
  lookup traffic terminates there and their own attestation says nothing about it.

## Validation Information

**Verdict: CONFIRMED at Medium** (downgraded from the filed High).

**Every factual claim was checked against the target:**

- **The fall-through exists and is verbatim.** `server.rs:296-303` returns from the
  queue only on a hit; `server.rs:305` calls `self.chain.get_transaction(wire_hash)`
  on every miss; `chain.rs:221-232` builds `TxFilter { hash: wire_hash.to_vec() }`
  and issues it to `self.endpoints.iter()` — every configured endpoint.
- **No cache on either side.** Shim: `intercept.rs:237-323` has no lookup memo and
  calls the hub on every request; `intercept.rs:193` states the shim records
  nothing. Hub: `Hub::lookup` (`server.rs:296-322`) has no memoisation. Confirmed
  as filed.
- **The shipped configuration claim is exactly right.** `deploy.env.example:16-17`
  = `BACKEND=66.241.124.200:443` / `BACKEND_TLS=na.zec.rocks`; `:22-23` =
  `INDEXERS=66.241.124.200:443` / `INDEXER_TLS=na.zec.rocks` — same host, port and
  TLS name. `smoke-local.sh:39-40` defaults to the same pair. `deploy.sh:110-116`
  reads both from that one file. No disjointness check exists anywhere.
  `OPERATORS.md:281` documents `ZIH_INDEXERS` in broadcast terms only.
- **It is independent of batch size and survives ZIP 318** — upheld. The channel is
  a per-transaction query correlation, not a set-membership argument, and uses no
  transaction shape, so the wallet-side `(length, anchor, expiry)` uniformity that
  closes the filed core-linkage chain leaves it open.
- **It uses no defect** — upheld. Every component does what its comments say.

**Corrections made during validation (the filed issue overstated the correlation):**

1. **"Same start, same stop, same period, same count" is wrong on two of four.**
   All batch members become queryable at the same flush and confirm at nearly the
   same height, so start and stop are shared across the whole batch and discriminate
   nothing. The real discriminators are poll **phase** and **period**, recovered
   through Nym's 9–10 s jittery offset over repeated samples. The issue now says so.
2. **The lookup recipient is always also the publish recipient**, because
   `ZIH_INDEXERS` is the same list used by `broadcast_batch`. So the fall-through
   never discloses migration *content* the endpoint did not already have; its
   marginal contribution is the per-wallet timing correlation. The "cross-operator
   export" harm is correspondingly narrower than filed and has been restated.
3. **`shim/ENDPOINTS.md:62-64` is not a shipped claim.** That sentence sits inside
   the design section for a *planned, Zeronym-operated, non-enclaved* indexer that
   does not exist in the code. It is still relevant — the residual analysis at
   `:167-175` explicitly assumes the query recipient is **not** the operator, which
   the shipped `ZIH_INDEXERS` can violate — but it is cited as an assumption the
   deployment breaks, not as a shipped promise that is false.

**Why Medium and not High:**

- The severe leg (complete IP → transaction → balance) needs the conjunction
  *"the hub's `ZIH_INDEXERS` is operated by the party that also fronts the victim's
  shim"*. That conjunction is invited by the shipped example and is realistic given
  how few public Zcash indexers exist, but it is a deployment choice the hub
  operator controls and can fix in one line — it is not forced by the code.
- The unconditional leg (a third-party indexer learns "someone asked about txid T",
  IP-blinded) is a genuine, undisclosed trust shift, but it is close to the residual
  `shim/ENDPOINTS.md:167-175` already writes down, and `README.md:28` does hedge
  ("rather than making it private outright").
- Step 4 is a traffic-correlation step with an error rate that grows with
  concurrency, not a direct read of the linkage.
- At today's adoption the modal batch is 0 or 1 (`hub/REVIEW.md:175`), so the
  incremental harm now is near zero; the finding matters for the adoption regime
  the project is designing toward.

**Why not Low, and why not invalid:**

- The mechanism is fully verified in code and in the shipped configuration file,
  requires no attacker capability beyond passively reading traffic the design
  delivers to them, and defeats a property the README lists under **Protected** and
  that the project's own REVIEW #7 identifies as the query that "completes the
  link". A privacy product handing a wallet's txid lookups back to the operator it
  routed them away from is a real architectural finding, not a theoretical one.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
