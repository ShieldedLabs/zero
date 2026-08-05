# CompactTxStreamer endpoint classification

How the diverting shim must handle each gRPC method so the operator cannot link a
wallet/IP to its migration. This is the design for the FULL shim; the current PoC
only classifies + logs `SendTransaction` and forwards everything.

Derived from an adversarial pass over all 20 methods (classify, then attack every
"safe" verdict to construct a leak), then curated. Where the raw pass said
"intercept everything," it was told to maximise leak-finding and over-rotated: it
flagged timing channels a thin proxy cannot close as if the shim should serve them
locally, which would mean reimplementing the indexer inside the enclave, the exact
400-500 GB design the project rejects. The separation below is the correction.

## The one distinction that matters

A method is a leak in one of two fundamentally different ways:

- **By argument** - the request *names* the migration: its txid, an address it
  touches, or its confirmation height. The operator reads the reference directly.
  These are **closeable at the shim** and are the real work.
- **By timing/pattern** - the request names nothing, but *when* or *how* the
  wallet calls it, while a migration is pending, correlates. A thin proxy mostly
  **cannot** close these; they are residuals, wallet-behaviour requirements, or a
  small shared cache, not per-request interception.

Do not treat these the same. The first is a bounded intercept table; the second
is honesty in the threat model.

## Three handling classes

- **FORWARD** - pass to the operator's indexer unchanged.
- **DIVERT** - do not forward; encrypt and send to the hub over Nym.
- **INTERCEPT** - do not forward; answer from state the shim holds (the buffered
  migration bytes + the hub's `Confirmed{txid, height}`).

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

## INTERCEPT and serve locally

These name the migration by argument. Forwarding hands the operator the exact link
hub + Nym removed. All are **conditional**: forward for ordinary arguments,
intercept only when the argument references a diverted migration.

| method | flip predicate | leak if forwarded |
|---|---|---|
| `GetTransaction` | `TxFilter` references a diverted txid, in **both** forms: `hash` in `DivertedMigrations`, or `block{height}+index` resolving to one. | "IP C wants migration T's full details" the instant the wallet checks confirmation. The canonical follow-up leak. |
| `GetTaddressTransactions`, `GetTaddressTxids` | queried address in `TaintedAddrs`. | Names the migration's transparent leg; the operator joins IP C to the on-chain batched tx once the hub publishes. Guard the deprecated `...Txids` too. |
| `GetAddressUtxos`, `GetAddressUtxosStream` | queried address in `TaintedAddrs`. | The deshield-confirmation poll: the wallet checks the destination for the arriving UTXO. |
| `GetTaddressBalance`, `GetTaddressBalanceStream` | any queried address in `TaintedAddrs`; **split** a mixed list, serve tainted locally and forward only clean addresses. | The sharpest amount leak: a balance poll bracketing the flush yields post-minus-pre = the exact deshielded amount, turning "operator learns *that* a client migrated" into "amount Y". |
| `GetMempoolTx` | a `exclude_txid_suffixes` entry tail-matches a diverted txid. Handling is **surgical**: strip the offending suffix, forward the sanitised request. | Once the hub broadcasts, the migration enters the operator's own mempool; a matching exclude suffix says "IP C already holds T". |
| `GetMempoolStream` | content-conditional (arg is Empty). Forward the bulk stream, but **inject** the migration element from held bytes and suppress the operator-sourced copy. | Forwarding places the wallet's reaction to its own migration in the operator's view. |
| `GetLatestTreeState` | **INTERCEPT for all callers**, from a shared shim cache. | Anchor correlation, the strongest non-argument leak: this supplies the Orchard anchor the wallet spends against, and that anchor root is a public field of the published tx. Serving one shared, cadence-refreshed tree state to every wallet means they share an anchor and the operator sees no per-wallet anchor. This is the mechanism behind the aligned-anchor requirement already in the design (see the problem chapter). |

### State the shim must keep

- `DivertedMigrations`: txid -> { raw migration bytes; hub `Confirmed{txid,height}`; resolved (height,index) }.
- `TaintedAddrs`: address -> migration txid, from the tx's transparent vouts and vin-derived addresses (parsed with `zebra-chain` at divert time).
- `DivertedHeights`, `PendingMigration` (per session): from hub `Confirmed`.

All in RAM (the enclave is diskless), held only for the retain-until-confirmed
window.

---

## Residual leaks (cannot be closed at the shim; state them, do not pretend)

- **Tip-poll and first-fetch timing.** `GetLatestBlock`/`GetLightdInfo` cadence
  speeds up while a migration is pending; a transparent-only wallet migrating into
  its first Orchard note starts fetching `GetSubtreeRoots` for the first time.
  Both are behavioural tells the operator can see even though the payloads are
  identical for everyone. Optionally softened by serving the tip / subtree roots
  from a shared shim cache; not fully closeable.
- **Fresh-address pre-announcement.** A wallet that queries a brand-new deshield
  destination *before* the migration confirms leaks the address and near-real
  submission time. This is a wallet-behaviour requirement (do not pre-announce),
  not a shim fix.
- **Address reuse = anonymity set of one.** If a tainted address was already used
  or queried publicly through the operator, the address<->IP bind already exists
  in the operator's logs; no follow-up query needed. Deshield to / fund exits from
  **fresh single-use addresses** is a wallet-side requirement.
- **Durability gap.** The shim holds migration state only until confirmation
  (diskless). A query about the migration *after* the shim drops it cannot be
  intercepted and would forward. Bounded by keeping the retain window long enough,
  never fully closed.
- **Batch size.** Every intercept above is correct and the migrant's cover is
  still only the flush's batch size. A size-1 flush is no cover at all. This is the
  hub's problem (see `zeronym/hub/REVIEW.md`), restated here so the shim's
  correctness is not mistaken for sufficiency.

## Open decisions for humans

- **Isolating block/range/tree-state request:** refuse with a clean gRPC error,
  normalise to a wide range, or `NOT_FOUND`? Refusing may break naive wallets;
  normalising over-fetches.
- **Reused (tainted-but-not-fresh) address:** the shim must not forward (names the
  address) but cannot fully answer (it holds only the migration's own vout/vin,
  not the address's other history). What does it return?
- **Serve the tip locally during a pending migration** (closes the tip-poll tell)
  vs accept it as a residual.
- **Deprecated methods** (`GetBlockNullifiers`, `GetBlockRangeNullifiers`,
  `GetTaddressTxids`): confirm the backend actually routes them, then guard-and-
  serve vs hard-block at the shim.
- The tunable predicates here (the "narrow range" threshold, the retain window)
  are exactly the parameters the Taylor + Zooko threat-model sign-off must ratify;
  the build is gated on that doc.
