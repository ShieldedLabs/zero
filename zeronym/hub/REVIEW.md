# Adversarial review of the hub batching design

Run before implementing the queue and the flush, on the reasoning that the flush
logic **is** the anonymity mechanism: getting it wrong is not a bug that shows up
as a failure, it is a privacy loss that shows up as nothing at all.

Method: five independent attackers over separate lenses (timing and cadence, an
active attacker who can submit and run their own shim, metadata left in the
clear, Rust implementation pitfalls, and failure or low-volume degradation),
then a refutation pass in which a skeptic argued each serious finding was wrong,
defaulting to refuted when uncertain. Only what survived is recorded here.

    30 raw findings -> 27 critical/high -> 14 survived refutation

The findings are the reviewers' and are recorded as given, lightly ordered. They
have not all been independently re-derived; where one contradicts something in
the book, the contradiction is the point and is flagged for humans below.

## Read this first

The lead finding is not a bug to fix. At the measured mainnet rate the modal
published batch is 0 or 1 transactions, and at batch size 1 the anonymity set is
the transaction itself, which makes the shuffle, the simultaneous publish, Nym
and the TEE all irrelevant *to that transaction*. It is a property of volume,
not of code, and no amount of implementation care removes it. It changes what
the product may honestly claim, so it belongs in front of Mark, Zooko and Taylor
rather than in an engineering backlog.

## Design changes required before implementing

### 1. Change the wire message to `SubmitMigration { ciphertext }` and nothing else. The sealed plaintext is `{ request_nonce: [u8;16], tx_bytes }`. The hub recomputes the txid with `zebra_chain::transaction::Transaction::hash()` (transaction.rs:220) and reads expiry with `tx.expiry_height() -> Option<Height>` (transaction.rs:510). The Ack echoes `request_nonce`, never a txid.

**Why.** Every cleartext field the submitter supplies is a control input the adversary writes. `txid` in the clear is a dedup-poisoning key and a metric-inflation lever; `expiry_height` in the clear is a scheduling oracle. Both are recomputable from bytes the hub already parses, so carrying them buys nothing and creates two attack classes. Echoing a computed txid instead of the submitter's would also break the shim's Ack correlation whenever the two disagree, which is exactly the version-skew case that ends in direct broadcast; a request nonce cannot disagree.

**Replaces.** `SubmitMigration { ciphertext, txid, expiry_height }` with txid and expiry_height in the clear, and `Ack { txid }`.

### 2. Delete the early-expiry flush trigger entirely. Replace it with admission control at Ack time: admit iff `expiry.is_none() || expiry >= next_flush_height(H) + mining_margin`, where `next_flush_height(h) = ((h / N) + 1) * N`. Refuse with a typed `ExpiryTooTight` so the shim holds and retries rather than broadcasting.

**Why.** `any queued expiry <= H + margin` plus `flush ALL pending` is a one-transaction, zero-fee, attacker-operated flush clock: the hub's re-validation is stateless so consensus-invalid Orchard-touching junk passes every check, and one such tx per block collapses every window network-wide, permanently. Admission control makes the trigger unreachable rather than rate-limited: if every admitted entry provably survives the next scheduled flush, no entry can ever be urgent. Bounding against `next_flush_height(H)` rather than `H + N` is strictly more permissive (the wait is uniform 0..N, not always N) and equally safe, which matters because Brave's 20-block expiry is the binding constraint.

**Replaces.** "flush at every multiple of N, OR earlier if any queued migration has expiry_height <= H + safety_margin", and the "take all pending" early-flush action.

### 3. Fix the parameters against the expiry ceiling explicitly: `N = 10`, `mining_margin = 4`, and enforce the invariant `N + mining_margin + max_delivery_lag <= min_wallet_expiry (20)`, which leaves 6 blocks (~7.5 min) for wallet-to-shim lag, Nym round trips (measured 9-10 s unary), Ack retries and hub failover. Write the inequality into the code as a startup assertion over the configured values.

**Why.** The 20-block ceiling is real and every mitigation in this space silently spends it. Making the budget an explicit assertion means a future N or margin change fails at startup instead of quietly pushing a percentage of real traffic into last-resort direct broadcast.

**Replaces.** N and safety_margin as independently chosen constants (N=10, margin~5) with no stated arithmetic against the 20-block wallet expiry.

### 4. Key the queue on `sha256(decrypted tx_bytes)`, not on the txid. Keep the computed txid as a separate `Option<transaction::Hash>` field used only for confirmation tracking.

**Why.** Under ZIP 244 the v5/v6 txid is a digest over transaction effects and deliberately excludes authorizing data, so two different byte strings can legitimately share a txid with no hash break; v4 has classic malleability besides. A txid is therefore the wrong identity for a byte-level dedup, and any rule asserting "same txid with different bytes is impossible" would enshrine a false invariant. Payload-hash keying also makes the entry key unforgeable by the submitter, which kills the poison-suppression variant outright. Honest resends and cross-hub duplicates still collapse, because identical bytes hash identically.

**Replaces.** "Queue: in-RAM, keyed by txid for dedup."

### 5. The hub's re-parse and re-classification are telemetry, never a drop reason. The only permitted refusals are: AEAD authentication failure, frame not exactly `FRAME_BYTES` after unpad, byte budget exhausted (refused strictly before Ack), and the expiry admission rule. Unparseable payloads are queued with `expiry = None` and published; `sendrawtransaction` at the node is the only authority on validity.

**Why.** The shim fail-safes for privacy: `Class::Unparseable` routes exactly like `Migration` (classify.rs:99-101), and the shim's trailing-bytes check (classify.rs:248-257) makes its "parses" strictly stricter than the node's. So the transactions most likely to fail a hub parse are precisely the ones the shim deliberately diverted because it could not read them. A "reject malformed" rule inverts the shim's fail-safe into a leak, and an adversary who characterises the parser skew gets an on-demand direct-broadcast primitive. The cost of publishing junk is one wasted batch slot.

**Replaces.** "Hub decrypts in-enclave, re-parses with zebra-chain, re-runs the classifier, rejects malformed or already-expired transactions."

### 6. Delete the primary/standby asymmetry. Every shim submits every migration to ALL hubs. Combined with the unconditional deterministic cadence, both hubs hold the same queue and publish the same set at the same height, so a duplicate is a duplicate of the whole batch rather than an intersection of two disjoint ones. `ChainClient::broadcast` already classifies already-known as success (chain.rs:141, 196-217).

**Why.** Preferring a primary "for batch density" is exactly what creates the isolation oracle: a brief, targeted outage of the primary during one migration's retry window pushes that migration onto a standby that is otherwise idle, and it publishes in a batch of one. Send-to-all gives every hub the full density, which is a better answer to the density argument than concentration, and removes failover as an observable event. Cost is 2x Nym bandwidth and 2 channels per shim, negligible at two hubs. Do not replace it with height-derived hub rotation: the schedule is public so it mitigates no DoS, it makes density hostage to config consistency across independently-run operators, and it manufactures straggler batches of one at every boundary.

**Replaces.** "Shims prefer a primary hub for batch density and fail over to a standby only when primary is unreachable."

### 7. Add a hub-to-shim `Confirmed { request_nonce, txid, height }` message over the existing Nym tunnel. The shim must never issue a txid-specific query to its backing indexer for a diverted migration, and must additionally intercept `GetTransaction`, `GetTaddressTxids` and `GetMempoolTx(Exclude)` for diverted txids and serve them from the bytes it already holds, including after confirmation.

**Why.** "Retain until confirmed on chain" with no specified mechanism leads any engineer to `GetTransaction(txid)` against the operator's indexer, which hands the operator the one fact the whole system withholds. The operator already knows IP C submitted a migration at time T (the single request the shim does not forward); the txid completes the link and bypasses Nym, the TEE and the batch in one query. Post-confirmation matters too: `TransactionDataRequest::Enhancement(txid)` (librustzcash zcash_client_backend/src/data_api.rs:1112) fires once the wallet's compact-block scan sees the tx mined, so a shim that drops the txid on confirmation leaks it later. Do not implement passive compact-block matching instead: it violates the no-body-reading rules proxy.rs is built around, puts an operator-controlled parser inside the enclave, and has no coverage guarantee on a quiet endpoint.

**Replaces.** An unspecified "shim retains each migration until it observes the transaction confirmed on-chain".

### 8. Rewrite tip acquisition: query all nodes concurrently and take the MAX of the successes; enforce monotonicity with a small reorg allowance; record `last_advance` wall-clock. Declare the tip stale only when no node has advanced for 15 minutes. On a stale tip, stop admitting (typed `Unavailable`, wired into the shim's failover predicate) and keep the cadence running off a free-running wall-clock clock at 75 s/block from the last known good height. Never flush early because the tip is stale.

**Why.** `tip_height` today returns the first node that answers (chain.rs:98-113), so one lagging or hostile node is a second, independent lever on the flush clock: a stalled tip freezes flushes (everything falls out to shim direct broadcast) and an advanced tip drains the queue. Requiring agreement between nodes is the wrong fix, because tips legitimately differ by a block during propagation and a lagging node would then stall scheduling. A 3-block (~4 min) staleness threshold is also wrong: block arrivals are Poisson at 75 s, so P(gap > 225 s) = e^-3 ~ 5%, roughly 57 false stalls per day. 15 minutes gives e^-12 ~ 6e-6, about one false positive every 140 days. And flushing on a stale tip hands the adversary the trigger back: a few minutes of packet interference against the hub's node endpoint would force a near-empty batch containing the targeted transaction. The wall-clock fallback keeps batches whole and errs early relative to true expiry, since during a real stall blocks arrive slower than 75 s.

**Replaces.** "Hub tracks chain height H from a full node", implemented as first-answer-wins with no staleness notion.

### 9. Do not implement a k-anonymity floor as a runtime gate. Compute `achieved_batch_size` per flush, export the distribution to the hub operator, and gate LAUNCH on a measured distribution rather than gating each batch.

**Why.** A floor has exactly three possible behaviours and all three are worse than publishing. Holding is impossible: at 0.77 Orchard-touching tx/block network-wide and single-digit operator adoption you would need roughly 100 blocks to accumulate k=8, and the expiry budget gives you one window plus a fragment. Padding requires decoys, which do not work (see the decoy item). Refusing routes the migration to the shim's direct broadcast, which is strictly worse than a size-1 batch because the operator then publishes it itself, or to a wallet error, which hands any DoS-capable attacker a total availability kill. A floor met by attacker fill is also worse than no floor: at k=8 an attacker supplies 7 for 7 ZIP-317 fees, cheaper than the 99 that got count-based flushing rejected.

**Replaces.** Proposed hard `k_min` batch-size floors with hold-or-pad fallbacks.

### 10. Drop hub-generated decoys from v1 and say so in the docs, correcting the stated reason. The cheap decoy (an all-dummy Orchard bundle; orchard/src/builder.rs:1243-1246 pads a dummy spend with a zero-valued output to the dummy's own address) is not blocked by cost: it is blocked because it necessarily carries `orchard_value_balance == 0`, and orchard_vb is public on every diverted transaction.

**Why.** An observer partitions any batch by orchard_vb. Real Orchard exits have orchard_vb > 0; all-dummy decoys have orchard_vb == 0, so they cover only the net-zero-shuffle subclass and give exits, the class that most needs cover, nothing. Making a decoy with orchard_vb > 0 means spending a real Orchard note, and NU6.3 closes the pool to new value, so that stock is finite and unreplenishable. A transparent-funded decoy is additionally linkable by funding provenance. The one sustainable design is an all-dummy Orchard bundle whose fee is paid from Ironwood (refillable, shielded, no transparent trail), and it still only covers the zero-balance class, and it still needs commitment-tree and witness state rebuilt on every cold boot inside a diskless enclave. That is a subsystem, not a fallback, and it is not the ~Aug 10 deliverable.

**Replaces.** "Decoys cost real on-chain value, so they are a last resort" as the stated reason, and every mitigation that leans on decoys to make a floor reachable.

### 11. Make the shim's last-resort direct broadcast off by default, behind an explicitly named config flag, and make hub REJECTION never a trigger for it (only hub unreachability, after retries and after all hubs have been tried). When enabled it must go over Nym, must not wait for a cadence boundary, and must be counted and reported.

**Why.** Nearly every attack in this set converges on the same final step: get the shim to broadcast directly. That is only possible because the shim fails OPEN on privacy. Making it fail closed removes the whole class in one change, whereas each hub-side fix removes one route into it. Waiting for a flush boundary is actively wrong: the fallback fires when expiry is close and the next multiple of N can be up to 10 blocks out, so aligning converts a privacy degradation into a lost transaction; during a hub outage there is also no other traffic on that cadence, so the alignment creates no cover and just makes the fallback more fingerprintable.

**Replaces.** "LAST-RESORT DIRECT BROADCAST by the shim before expiry" as unconditional default behaviour.

### 12. Pad every SubmitMigration record to a fixed 64 KiB frame at the shim, before the STEVE/AEAD layer. Emit Acks on a fixed 5 s tick per session, one fixed-ciphertext-length response per tick regardless of how many messages arrived. Correct the trust document: the hub host sees ciphertext plus its exact length and arrival time, not "only ciphertext".

**Why.** Without padding the parent host at the Nym exit defeats the shuffle and the simultaneous publish with no decryption at all: inbound record length is |tx| plus a constant, the outbound sendrawtransaction body is ~2|tx| hex, Orchard transaction sizes are highly variable (this repo's own fixtures are 11,994 and 6,010 bytes) and batches are single-digit, so the size match is a deterministic bijection. Shuffling permutes order, not sizes. Do not use a size ladder (it leaks the one bit) and do not pad to 2 MB (Nym is metered via ticketbooks and measured ~10x slower than clearnet). The Ack payload can carry the nonce and disposition freely since it is under AEAD; only ciphertext length and emission time need to be constant, and a contentless Ack would break the shim's retry and failover decision. Once inbound is padded there is no join key left, so the outbound needs no change and must NOT be dripped out.

**Replaces.** Unpadded records, immediate per-message Acks, and the claim that the hub host sees only ciphertext.

## Implementation rules

- Keep expiry as `Option<Height>` end to end and never `unwrap_or(0)`. `expiry_height() == None` means NO expiry, i.e. infinitely far away, not maximally urgent. Under a naive `expiry <= H + margin` comparison a `None` folded to 0 pins the hub into permanent early flush, and under `expiry >= bound` it rejects every legal no-expiry transaction. The same rule must reach the confirmation set, whose drop-when-expired arm otherwise never terminates for such an entry.

- `fn next_flush_height(h: u32, n: u32) -> u32 { ((h / n) + 1) * n }` with saturating arithmetic. Admission: `entry.expiry.map_or(true, |e| e.0 >= next_flush_height(tip, N) + MINING_MARGIN)`. Unit-test the boundaries at `h % N == 0` and `h % N == N - 1`.

- Queue entry: `{ key: [u8;32], txid: Option<transaction::Hash>, expiry: Option<Height>, tx_bytes: Vec<u8>, received_at: Instant, received_height: u32 }`. There is NO channel, session or contributor identifier on the entry, and there must never be one: an operator-to-migration mapping inside the enclave is precisely the linkage the system exists to destroy, and it becomes exposure to enclave compromise, side channels and legal compulsion.

- Account BYTES, not entries, with variable-size entries and a per-entry cap. Do not pre-allocate 2 MB slots: real Orchard migrations are 2-16 KB, so fixed 2 MB slots waste ~99% of the budget and hand the attacker a cheaper occupancy attack than the memory attack. `MAX_TX_BYTES = FRAME_BYTES - AEAD_AND_HEADER_OVERHEAD`; do not reuse the shim's `MAX_SEND_TX_BYTES = 4 * 1024 * 1024` (intercept.rs:53), which bounds a wallet's HTTP body into a shim and is unrelated.

- Reserve the byte budget and insert the entry BEFORE queuing the Ack. An Ack is a promise; never emit one for an entry that is not resident. Correspondingly, never evict an already-admitted entry: refuse at admission instead. Oldest-first eviction is the worst possible policy here because it evicts the entries closest to expiry, which is exactly the attacker's selection lever.

- Give the awaiting-confirmation set its own byte budget and a hard deadline (`received_height + 2N`), then drop and stop resubmitting. That set does not drain at flush and is the structure that actually accumulates. Entries with `expiry == None` need this deadline as their only GC path.

- Resubmit unconfirmed entries by folding them into the NEXT cadence flush's shuffle, never as their own event. A standalone resubmission is a singleton publish tied to one transaction, which is a fresh timing signal for exactly the transaction being protected.

- Shuffle with `rand::rngs::OsRng` via `SliceRandom::shuffle` on a `Vec` drained from the map. Do not iterate a `HashMap` and call it shuffled, do not `sort_by_key`, do not seed a `StdRng` from anything reproducible. `rand` is already declared a security dependency in hub/Cargo.toml:53-55; keep that comment true.

- Rewrite `ChainClient::broadcast` (chain.rs:122-149) to be concurrent. It currently loops `for node in &self.nodes` sequentially with a 10 s `RPC_TIMEOUT` per call, and the flush would loop over transactions on top of that; k transactions x n nodes with one hung node exceeds a 75 s block interval and reintroduces the ordering the shuffle removes. Spawn the full (tx, node) product with `join_all`, keep the per-call timeout, and assert total flush wall time stays well under one block.

- Rewrite `tip_height` (chain.rs:98-113): concurrent queries, `max()` over successes, monotonic with a small reorg allowance (10 blocks, logged loudly if hit), and a stored `(height, Instant)` so staleness is a wall-clock fact rather than an absence of answers.

- Speak TLS to the full nodes. `chain.rs:166` builds `http://{addr}/`, so today the parent host reads every `getblockchaininfo` and every `sendrawtransaction` body in the clear, which includes the whole batch before it is published. The shim already proves the rustls stack builds under StageX musl.

- Never log a txid, a transaction body, or a per-entry identifier at any level. In a Nitro enclave the tracing output reaches the parent via the console, so `tracing::info!(txid = %h, ...)` hands the host exactly what STEVE exists to withhold. Log counts, reasons and aggregate timings only.

- Add `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` to the queue and flush modules. A panic in a diskless enclave destroys the queue and the confirmation set, and every shim then walks its retry-failover-fallback ladder simultaneously; an enclave panic is a fleet-wide privacy event, not a crash.

- Zeroize decrypted transaction bytes on drop (`zeroize::Zeroizing<Vec<u8>>` or an explicit `Drop`), including the decrypt scratch buffer.

- Do NOT call `testmempoolaccept` (or any per-submission node query) at admission time to filter junk. It leaks each transaction to a node individually at arrival, which is precisely the per-transaction timing signal the batch exists to destroy, handed to the node operator.

- Count `achieved_batch_size` as node-accepted OR already-known, using the existing `Publish::AlreadyKnown` arm. With send-to-all-hubs the second hub's publish is already-known by construction, so counting only `Accepted` would report zero on one side of every honest batch.

- Export metrics to the hub operator only, aggregated and delayed: `flush_size`, `admission_refused{reason}`, `tip_stale_seconds`, `direct_broadcast_events` (reported by shims). Never return a batch-size or queue-depth number down the shim channel: that is a live anonymity-set-size oracle for any party running a shim, and a real-time "the fleet is unprotected right now" feed helps the adversary time the correlation.

- Do not merge the shim and hub workspaces. Both are standalone on purpose so each lockfile is authoritative for its own reproducible StageX build, and merging couples the attestation measurements (a shim TLS change would move the hub's build hash) while linking dependencies the hub must not carry. Instead pin the parse-critical crates identically across both lockfiles (they currently drift at orchard 0.15.4/0.15.5 and halo2_proofs 0.3.4/0.3.5) and add a differential test that runs both binaries' classifiers over one corpus and fails on any disagreement.

- Do not make attested build-id equality a runtime gate on the shim side. "Refuse a hub whose classifier build differs" is the unreachable-hub condition, so it converts a version skew into a fleet-wide fallback. Use a monotone compatibility epoch (hub epoch >= shim epoch), log mismatches, and fail closed rather than falling through to broadcast.

## Inherent limits: state these, do not paper over them

- At the measured mainnet rate of 0.77 Orchard-touching transactions per block (144-block sample at tip 3,433,105, 2026-08-01) and realistic launch adoption of one to a few operators, batch size at N=10 is Poisson with lambda well under 1. The modal published batch is 0 or 1. For a size-1 batch the anonymity set is the transaction itself and shuffle, simultaneous publish, Nym and the TEE are all irrelevant to it. There is no fix at v1: holding needs a window the 20-block expiry does not permit, decoys do not cover the class that matters, and refusing to publish routes the transaction to a worse leak. State that the anonymity claim does not hold below a stated adoption threshold rather than presenting the batch property as unconditional.

- The batch is publicly identifiable. Simultaneous submission to a small set of known full nodes at a publicly known cadence is exactly what an observer clusters on, so anyone can enumerate batch membership and cardinality from public data. This is not fixable by jitter: the simultaneity is what hides intra-batch ordering, and at k=1 there is no ordering to hide, so a randomized offset is pure cost against an adversary who can already see the burst.

- Set subtraction is unfixable without a scarce, Sybil-resistant contributor identity. Any adversary who submits j migrations of their own into a window knows which members are theirs and subtracts them; the effective anonymity set is (batch minus attacker's own), and rejecting count-based flushing does not touch this. Under one-way STEVE over Nym the hub also cannot count contributors at all, so "distinct contributors" is not a computable metric and must not gate a privacy decision or be published as one.

- The operator still learns that client IP C submitted an Orchard-touching transaction at time T. It is the one SendTransaction the shim does not forward, so the absence is the signal. The system removes the txid from that link, not the link's existence. Intercepting per-txid queries for diverted migrations adds a second observable of the same kind ("this client polled for its migration"), which is within the same residual.

- The hub sees every migration in plaintext. Enclave compromise, a side channel, or legal compulsion against the hub is total for every migration in flight, and both hub instances share one key so there is no compartmentalisation between them.

- Nym is a hard availability dependency and its availability is a privacy property, not a liveness one. Any party who can degrade the shim-to-hub path for the length of a migration's expiry slack chooses the moment that migration is published, and chooses whether it is published in a batch at all.

- The hub host sees ciphertext length and arrival time. Padding to a fixed frame reduces that to per-session record COUNTS, which remain: a session that sent one record owns one member of the batch. That residual is the intended anonymity-set-equals-batch property, so it is acceptable, but it must be written down rather than assumed away.

- Sustainable decoys and indistinguishable decoys are disjoint. An all-dummy Orchard bundle is cheap (one ZIP-317 fee, no Orchard notes) but necessarily carries orchard_value_balance == 0, which is public, so it covers only the net-zero-shuffle subclass. Covering an exit requires orchard_vb > 0, which requires spending a real legacy Orchard note, and NU6.3 makes that stock finite and unreplenishable. The diverted population is also heterogeneous in shape (deshields, Orchard-to-Sapling, Orchard-to-Ironwood, V5 and V6, varying action counts), so any batch member with a unique shape has its anonymity set collapse to 1 regardless of k.

- Transactions larger than the fixed 64 KiB frame cannot be privately batched. That is the price of leaking zero bits of length. It is a rare tail (64 KiB accommodates roughly 75 Orchard actions), and the shim must surface it as an error to the wallet rather than falling back to a direct broadcast.

## Decisions for humans

- The single highest-value lever is widening the migration expiry, and it is not a hub decision. 20 blocks is Brave's wallet default, not a consensus constant (librustzcash uses 40, Zingo 100). Widening it would let N grow past 10 and make batch size a real function of adoption instead of a fixed loss. But it must be an EPOCH-CANONICAL change adopted by all wallets in the epoch, in the ZIP 318 sense: a Zeronym-only longer expiry stamps a distinguishing expiryHeight permanently on chain on exactly the transactions being protected, which is worse than the problem it solves. Needs Zooko and the wallet consortium, not the hub team.

- Mutual STEVE plus consortium-issued shim enrollment. It is the only thing that makes contributor counting, per-contributor share limits, or a j-distinct-channels floor mean anything, because a Nitro attestation document attests the CODE, not the instance, so every shim running the published image produces an equally valid one. This is a governance and admissions question (who gets a credential, who revokes) on an ~Aug 10 deadline; decide it as governance or explicitly defer it, do not design around it.

- Fail-closed is a product decision with a real cost. Making the shim's direct broadcast off by default means a permanent on-chain privacy leak is never traded for a recoverable availability failure, which is right on the merits, but it hands any DoS-capable attacker a total availability kill against every participating operator. Mark and Zooko should sign this off explicitly rather than letting it fall out of a hub-side flag, and the wallet-facing error text is part of the decision.

- Whether to gate launch on a measured batch-size distribution rather than on the date. The engineering answer is yes; the ~Aug 10 deadline is a business constraint. If the answer is ship anyway, the adoption threshold below which the anonymity claim does not hold must be stated publicly in the same release.

- Correct the public threat table before anything ships. "Timing correlatable: No" and "does not learn which on-chain transaction or what amount" are false at k=1 and contradict what the trust chapter already concedes. Taylor and Anton should review the corrected wording; an overclaim here is worse than the underlying limit.

- Whether to fund a v2 decoy subsystem at all, knowing its ceiling: an Ironwood-funded all-dummy Orchard bundle is the only sustainable and unlinkable decoy, and it covers only the orchard_vb == 0 class. It needs Ironwood note, witness and commitment-tree state rebuilt on every cold boot in a diskless enclave, plus a per-hub note partition rule the shared-hub-key design does not currently have.

- Whether the hub's added chain load is acceptable to the wider community if decoys ever do land: a few thousand extra transactions per day is a visible externality that should be costed and disclosed rather than discovered.

