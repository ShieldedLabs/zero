# CompactTxStreamer endpoint classification

How the diverting shim must handle each gRPC method so the operator cannot link a
wallet/IP to its migration. This is the design for the FULL shim; the current PoC
only classifies + logs `SendTransaction` and forwards everything.

Derived from an adversarial pass over all 20 methods (classify, then attack every
"safe" verdict to construct a leak), then curated. The raw pass, told to maximise
leak-finding, said "intercept almost everything." It was right that most methods
can leak and wrong about the fix: it assumed intercepting meant serving from
inside the enclave (the 400-500 GB design the project rejects). The **Zeronym
indexer** decision below resolves that: the leaky methods are served from a full
indexer that runs *outside* the enclave, so the aggressive read is largely
vindicated without bloating the enclave.

## The one distinction that matters

A method is a leak in one of two fundamentally different ways:

- **By argument** - the request *names* the migration: its txid, an address it
  touches, or its confirmation height. The operator reads the reference directly.
  These the shim recognises and routes away from the operator.
- **By timing/pattern** - the request names nothing, but *when* or *how* the
  wallet calls it, while a migration is pending, correlates. These the shim cannot
  fix by inspecting one request; they are closed only by routing whole method
  classes to the Zeronym indexer (see the routing decision) or left as residuals.

Do not treat these the same. The first is a bounded recognition-and-route table;
the second is a routing-breadth choice and honesty in the threat model.

## Three handling classes

- **FORWARD** - pass to the operator's indexer unchanged.
- **DIVERT** - do not forward; encrypt and send to the hub over Nym.
- **INTERCEPT** - do not forward to the operator; answer from the **Zeronym
  indexer** instead (see below), reached over the same Nym channel as the hub.

## The Zeronym indexer changes what INTERCEPT can do

The original plan for INTERCEPT was for the shim to answer from the tiny state it
holds: the buffered migration bytes plus the hub's `Confirmed{txid, height}`.
That was enough to fake a confirmation reply but not much else, and it left three
problems open: a **reused** tainted address (the shim holds only the migration's
own vout/vin, not the address's other history), the **durability gap** (the shim
is diskless and drops migration state at confirmation, so a later query cannot be
answered), and an **isolating** block/tree-state request (the shim does not hold
arbitrary block data).

**Decision (Mark, 2026-08-05): run a Zeronym-operated, non-enclaved indexer
alongside the hub, and route the INTERCEPT queries to it over Nym.** It is a
normal lightwalletd/zaino with the full chain, so it can answer *completely*:
full address history for a reused address, any block for an isolating request,
tree state, a real confirmation. INTERCEPT stops meaning "fake it from held
bytes" and starts meaning "serve it from a full indexer the operator does not
run." All three open problems above close.

Why this is not "the indexer in the enclave" (the 400-500 GB design the project
rejects): the Zeronym indexer is **outside** the enclave. The enclave stays
small. The indexer is not attested, and it does not need to be, because of where
it sits in the trust model:

- The **operator** never sees these queries: they leave the shim over Nym, not
  toward the operator's own indexer. That is the leak this closes.
- The **Zeronym indexer operator** sees the query *content* (which address, which
  txid) but **not the source IP** (Nym blinds it), exactly the posture the hub
  already has for migration content. So this shifts migration-follow-up-query
  visibility from the untrusted operator to the same semi-trusted, IP-blinded
  Zeronym boundary that already holds migration content. It is a real trust
  shift, not a free lunch (see residuals).
- The enclave (which sees migration *content*) and the indexer (which sees
  *queries*) are separate services and must stay that way; if they shared state,
  Zeronym could join a migration to its follow-up queries. Neither alone can.

---

## FORWARD

Bulk chain data, identical for every client, argument is a block id/range or
nothing. The operator already knows the source IP is a syncing light wallet;
these add no migration reference.

| method | note |
|---|---|
| `GetBlock`, `GetBlockRange` | The block-range sync stream is the SAFE channel by which a wallet legitimately receives its own migration's block once the hub publishes it. A wide range spanning the confirmation height H is fine; intercepting it would break sync. |
| `GetBlockNullifiers`, `GetBlockRangeNullifiers` | Deprecated twins. Still guard the isolating case below, because a backend that routes them acts on them. |
| `GetTreeState` | Tree frontier at a block; response identical for all callers. |
| `GetSubtreeRoots` | Structural roots. See the first-fetch residual. |
| `GetLatestBlock`, `GetLightdInfo` | Chain tip. See the tip-poll residual. |
| `Ping` | The only method with no argument leak, no reaction leak, and no place in any sync-or-confirm loop. Unconditionally safe. |

**Guard on the block/tree methods: refuse or widen a degenerate *isolating*
request.** A lone `GetBlock(H)`, a narrow `GetBlockRange` bracketing H, or a
`GetTreeState(H)` where H is a *diverted confirmation height for this session*
singles out the wallet's interest in that one block. At ~0.77 Orchard tx/block an
observer can enumerate H's Orchard actions down to a tiny set. Near-term the shim
cannot serve these locally (it does not hold arbitrary block data), so it
**refuses or normalises to a wide range**; it does not forward the isolating form.
Normal wide sync forwards untouched. State needed: `DivertedHeights` (per session,
from hub `Confirmed`).

---

## DIVERT to the hub

| method | predicate |
|---|---|
| `SendTransaction` | **Content-conditional, not txid-conditional.** Strip the 5-byte gRPC frame, decode `RawTransaction`, run the pure `classify()` (`shim/src/classify.rs`). Divert iff `is_orchard_touching(tx)` (`orchard_shielded_data().is_some()`, a bundle with >=1 action). Everything else (transparent, Sapling-only, **Ironwood-only**) forwards. Ironwood is deliberately not a divert arm: new pool, time-sensitive commerce. **Fail safe toward divert**, never forward: unparseable body, gRPC compression flag set, `grpc-encoding != identity`, frame shorter than 5 bytes, length over/under-running the body. A single false negative here defeats the whole system. |

The divert arm is also where the shim's interception state is seeded: it buffers
the exact raw migration bytes, computes the txid, and records the tx's transparent
addresses. That write feeds `DivertedMigrations` and `TaintedAddrs` below.

---

## INTERCEPT: route to the Zeronym indexer

These name the migration by argument. Forwarding hands the operator the exact link
hub + Nym removed, so instead the shim routes them over Nym to the Zeronym indexer
(above), which answers in full. All are **conditional**: forward to the operator
for ordinary arguments, route to the Zeronym indexer only when the argument
references a diverted migration.

The `leak if forwarded` column is why each must not go to the operator. The
Zeronym indexer answers all of them completely, including the cases the old
held-bytes approach could not (reused addresses, arbitrary blocks).

| method | flip predicate | leak if forwarded to the operator |
|---|---|---|
| `GetTransaction` | `TxFilter` references a diverted txid, in **both** forms: `hash` in `DivertedMigrations`, or `block{height}+index` resolving to one. | "IP C wants migration T's full details" the instant the wallet checks confirmation. The canonical follow-up leak. |
| `GetTaddressTransactions`, `GetTaddressTxids` | queried address in `TaintedAddrs`. | Names the migration's transparent leg; the operator joins IP C to the on-chain batched tx once the hub publishes. Guard the deprecated `...Txids` too. |
| `GetAddressUtxos`, `GetAddressUtxosStream` | queried address in `TaintedAddrs`. | The deshield-confirmation poll: the wallet checks the destination for the arriving UTXO. |
| `GetTaddressBalance`, `GetTaddressBalanceStream` | any queried address in `TaintedAddrs`; **split** a mixed list, route tainted addresses to the Zeronym indexer and forward only clean ones. | The sharpest amount leak: a balance poll bracketing the flush yields post-minus-pre = the exact deshielded amount, turning "operator learns *that* a client migrated" into "amount Y". |
| `GetMempoolTx` | a `exclude_txid_suffixes` entry tail-matches a diverted txid. Handling is **surgical**: strip the offending suffix, forward the sanitised request. | Once the hub broadcasts, the migration enters the operator's own mempool; a matching exclude suffix says "IP C already holds T". |
| `GetMempoolStream` | content-conditional (arg is Empty). Forward the bulk stream, but source the diverted-migration element from the Zeronym indexer (or the held bytes) and suppress the operator-sourced copy. | Forwarding places the wallet's reaction to its own migration in the operator's view. |
| `GetLatestTreeState` | **INTERCEPT for all callers**, served from the Zeronym indexer on a shared cadence. | Anchor correlation, the strongest non-argument leak: this supplies the Orchard anchor the wallet spends against, and that anchor root is a public field of the published tx. Serving one shared, cadence-refreshed tree state to every wallet means they share an anchor and the operator sees no per-wallet anchor. This is the mechanism behind the aligned-anchor requirement already in the design (see the problem chapter). |

The isolating block/range/tree-state cases guarded under FORWARD also become
fully serveable now: rather than refuse or normalise, the shim routes the
isolating request to the Zeronym indexer, which returns the real block. Refusal
stays only as the fallback if the Zeronym indexer is unreachable.

### State the shim must keep

The Zeronym indexer supplies the *answers*, but the shim still has to *recognise*
which queries to route, so the recognition state stays; only the buffered bytes
become a fallback rather than the source of truth.

- `DivertedMigrations`: txid, plus hub `Confirmed{txid,height}`. Raw migration
  bytes still buffered (for the pre-publish window before the Zeronym indexer has
  the tx on-chain, and as a fallback).
- `TaintedAddrs`: address -> migration txid, from the tx's transparent vouts and
  vin-derived addresses (parsed with `zebra-chain` at divert time).
- `DivertedHeights`, `PendingMigration` (per session): from hub `Confirmed`.

All in RAM (the enclave is diskless). Recognition state is held for the retain
window; the Zeronym indexer, being a full node, has no such durability limit,
which is what closes the durability gap.

---

## Residual leaks (state them, do not pretend)

The Zeronym indexer closed the three that used to sit here (reused address,
durability gap, isolating request), because it holds the full chain. What remains
is timing, wallet behaviour, the new trust shift, and batch size.

- **The Zeronym indexer sees query content (new, from this decision).** Routing
  the INTERCEPT queries to the Zeronym indexer moves their content out of the
  operator's view but into the indexer operator's. Nym blinds the source IP, so
  the indexer learns "someone asked about address A / txid T", never "IP C did".
  This is the same IP-blinded, content-visible posture the hub already has for
  migration content, and it is only a gain versus the operator (who additionally
  sees the on-chain publication and could correlate). It is real, not free: it
  relies on the enclave (migration content) and the indexer (queries) staying
  separate services that do not pool state, and on the Nym cover holding.
- **Tip-poll and first-fetch timing.** `GetLatestBlock`/`GetLightdInfo` cadence
  speeds up while a migration is pending; a transparent-only wallet migrating into
  its first Orchard note starts fetching `GetSubtreeRoots` for the first time.
  Both are behavioural tells the operator can see even though the payloads are
  identical for everyone. Routing them to the Zeronym indexer during the pending
  window removes them from the operator; the residual is then only whether to do
  that always (see open decisions).
- **Fresh-address pre-announcement.** A wallet that queries a brand-new deshield
  destination *before* the migration confirms leaks the address and near-real
  submission time. Wallet-behaviour requirement (do not pre-announce), not a shim
  fix, unless the wallet is routing every address query to the Zeronym indexer.
- **Batch size.** Every intercept above is correct and the migrant's cover is
  still only the flush's batch size. A size-1 flush is no cover at all. This is the
  hub's problem (see `zeronym/hub/REVIEW.md`), restated here so the shim's
  correctness is not mistaken for sufficiency.

## Open decisions for humans

- **Precise routing vs broad routing (the big one the Zeronym indexer opens).**
  *Precise:* route to the Zeronym indexer only queries the shim *recognises* as
  migration-referencing (`TaintedAddrs`/`DivertedMigrations` hits). Keeps the
  drop-in model, minimal Zeronym-indexer load, but depends on recognition being
  complete, and recognition of a reused address is imperfect. *Broad:* route
  **every** query of the sensitive methods (all `GetTransaction`, all
  transparent-address methods, tree/tip) to the Zeronym indexer regardless, and
  forward only the bulk block sync to the operator. Closes the recognition problem
  and the tip-poll/first-fetch timing tells outright, at the cost of more
  Zeronym-indexer load and less of the operator's indexer being used. The far end
  of broad routing is "the operator serves only block sync", which starts to
  undo the drop-in premise (operator provides the infrastructure) and centralise
  cost onto Zeronym. Pick a point on this axis; it is the load-bearing choice now.
- **Does routing follow-up queries over the same Nym channel as the migration
  create a Zeronym-side correlation?** The enclave sees the migration, the indexer
  sees the query, close in time. Only a link if the two services pool state.
  Confirm they stay separate, and decide whether that separation is enforced or
  merely intended.
- **Deprecated methods** (`GetBlockNullifiers`, `GetBlockRangeNullifiers`,
  `GetTaddressTxids`): confirm the backend actually routes them, then guard-and-
  route vs hard-block at the shim.
- The tunable predicates here (the "narrow range" threshold, the retain window)
  are exactly the parameters the Taylor + Zooko threat-model sign-off must ratify;
  the build is gated on that doc.
