# Zeronym shim + hub: build plan

Build plan for the near-term migration-privacy system (`zero-indexer-shim` +
`zero-broadcaster` hub + Nym between them). Read [README.md](README.md) first for the
strategy, threat model, and honest limits. This doc is grounded in the current tree
(anchors below) so the build does not have to be re-derived.

> **Status:** design. The README marks the build as held on the threat-model doc
> (Taylor + Zooko). This plan is the head start Mark asked for; the classification
> definition (section 3) and the correctness issues (section 8) are exactly what that
> threat-model review needs to settle before code lands.

---

## 1. Recap: what we are building

Two small, attested-TEE binaries plus a transport:

- **`zero-indexer-shim`**: a transparent proxy each operator runs in front of their
  existing light-wallet backend. Passes all traffic through untouched, except it
  intercepts a `SendTransaction` carrying an Orchard to Ironwood **migration** tx,
  which it isolates and routes over Nym to the hub.
- **`zero-broadcaster`** (the hub): one central attested-TEE service that accumulates
  migration txs from every shim, batches them, and publishes them together on a strict
  block cadence.
- **Nym** between shim and hub only (the "zeroith step").

The whole point: the operator never sees the migration tx cleartext, the network
cannot link it to a source IP, and batched publish breaks timing correlation.

---

## 2. `zero-indexer-shim`: the proxy

### 2.1 Architecture: byte-level HTTP/2 reverse proxy

A hyper / `tower` HTTP/2 reverse proxy (optionally an `axum::Router` with a
byte-forwarding fallback plus one explicit route). It matches the HTTP/2 `:path`
pseudo-header and **decodes only** the `SendTransaction` path; every other method,
stream, and unknown service passes through as opaque DATA frames.

Chosen over a `tonic` server that re-exports the full `CompactTxStreamer` trait
because: it touches exactly one message type (smallest auditable TEE surface), stays
base-agnostic (Zaino and lightwalletd share the identical method and path; unrelated
services like `darkside.proto` / `proposal.proto` just pass through), and does not
have to track ~20 methods and their streams.

- Intercept path constant: `/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction`
  (service `cash.z.wallet.sdk.rpc.CompactTxStreamer`).
- Request message `RawTransaction { bytes data = 1; uint64 height = 2; }`; `data` is
  the serialized tx. Response `SendResponse { int32 errorCode; string errorMessage; }`.
  Reuse the generated prost types from the `zaino-proto` crate rather than re-vendoring
  the `.proto`.
- Frame handling on the intercept path: strip the 5-byte gRPC prefix (byte 0 =
  compression flag, bytes 1..5 = big-endian uint32 length), then prost-decode
  `RawTransaction`. Advertise only `identity` encoding; if the compression flag is set,
  forward without inspection rather than mis-parse. Forward HTTP/2 trailers
  (`grpc-status`, `grpc-message`) on pass-through, and synthesize them when the shim
  answers directly.
- Reuse the bind / TLS / graceful-shutdown template from
  `zaino/packages/zaino-serve/src/server/grpc.rs:40,75-97` for the listener side.
- Optional JSON-RPC intercept (if the shim also fronts JSON-RPC): match
  `"method":"sendrawtransaction"` in the HTTP/1.1 POST body, hex-decode `params[0]`,
  same classification; forward all other methods. This is simpler than gRPC.

### 2.2 Decision logic

```
on request:
  if :path == ".../SendTransaction":
      tx_bytes = decode RawTransaction.data
      if is_migration(tx_bytes):            # section 3
          encrypt tx to the hub enclave key   # section 5
          hand off to the Nym client -> hub   # section 6 (fire-and-forget + ack)
          return synthesized SendResponse{errorCode: 0}   # see section 8 for the honesty cost
      else:
          forward verbatim to the operator backend
  else:
      forward verbatim (all queries, streams, other methods)
```

Everything that is not a migration broadcast, including all queries, reaches the
operator's unchanged backend exactly as today. The shim adds no query privacy (by
design; see the README threat model).

---

## 3. Classifying a migration transaction (the crux)

There is **no canonical on-wire migration marker.** Ironwood is a new shielded pool
introduced by NU6.3 (branch id `0x37a5165b`; `ShieldedPool::Ironwood`), reusing
Orchard's Action/Halo2 wire format but type-distinct. "Migration" is a ZIP 318 wallet
concept, so classification is **structural and heuristic**:

```
is_migration(tx) :=
     tx.version == V6                    # only V6 carries an Ironwood bundle
  && orchard_bundle present  && orchard_value_balance  > 0   # value LEAVING Orchard
  && ironwood_bundle present && ironwood_value_balance < 0   # value ENTERING Ironwood
```

Parsing APIs (pick one; both are in-tree):
- **zebra-chain** (recommended, has ready helpers): `Transaction::V6`,
  `orchard_value_balance()` (`zebra/zebra-chain/src/transaction.rs:1503`),
  `ironwood_value_balance()` (`:1520`), `ironwood_shielded_data()` (`:1129`),
  `expiry_height()` (`:510-529`).
- **zcash_primitives**: `orchard_bundle()`
  (`librustzcash/zcash_primitives/src/transaction/mod.rs:451`), `ironwood_bundle()`
  (`:455`), each bundle's `value_balance()` (sign: positive = value leaving that pool),
  `expiry_height()` (`:435`).

Grounding / test vectors: `zaino/docs/notes/ironwood-activation-plan.md` (the domain
record and the "Orchard pool value non-increasing" predicate) and
`zaino/live-tests/e2e/tests/ironwood_activation.rs:173-254` (an executable "ZIP 318
migration shape" test that produces a real Orchard-spend-to-Ironwood tx: use it as the
classifier's fixture).

**Open definition questions (for the threat-model doc, section 8) that change the
predicate:**
1. **Value-pool ambiguity.** A v6 tx has one shared transparent value pool, so the
   per-bundle balances cannot prove specific Orchard sats flowed into Ironwood. A tx
   that exits Orchard to transparent AND shields transparent into Ironwood looks
   identical to a "pure" migration. Decide whether that counts as a migration (it
   probably should, but confirm).
2. **Scope: "Orchard exit" vs strictly "Orchard to Ironwood".** After NU6.3 Orchard is
   effectively exit-only, so a looser predicate ("any tx spending Orchard") is
   defensible and catches more. Stricter (the predicate above) has fewer false
   positives. This trades false positives (a normal tx wrongly delayed) against false
   negatives (a migration wrongly broadcast in the clear, which leaks). **A false
   negative is a privacy failure; a false positive is a UX annoyance.** Lean toward
   over-capture, and let the threat model set the exact line.
3. **Network + activation.** These pins give NU6.3 a testnet height (4,134,000) and
   **no mainnet height** in this tree; confirm the mainnet Ironwood activation the
   Aug-10 urgency is premised on, and which network the shim targets first.

---

## 4. `zero-broadcaster`: the hub

A single central attested-TEE service.

- **Ingest:** receives encrypted migration txs from shims over Nym, decrypts them
  **inside the enclave** (only the attested hub software sees cleartext; the hub's host
  operator, Caution at launch, stays blind).
- **Accumulate:** holds pending migration txs in RAM (the enclave is diskless), keyed
  by txid, deduplicated. Tracks each tx's `expiry_height`.
- **Batch + flush:** publishes all pending txs together on a strict block cadence.
  **Flush every N blocks with N well under 20** (aim 10-15) because Brave mints
  migration txs with a +20-block expiry (librustzcash +40, Zingo 100); a tx must
  publish before it expires. This aligns with ZIP 318. Precedent for expiry handling:
  the Sprout to Sapling migration op (`MIGRATION_EXPIRY_DELTA = 50`,
  `zcashd/src/wallet/asyncrpcoperation_saplingmigration.cpp:18,125`), including its
  rule to skip a round when a tx would expire across the window.
- **Publish path:** broadcast via P2P relay to many nodes rather than a single
  lightwallet endpoint (Nate's point: P2P relay is on by default on all full nodes, a
  far larger anonymity set). Concretely, submit to one or more full nodes'
  `sendrawtransaction`, or speak P2P `tx` messages directly. Decide in section 8.
- **Cover traffic (mitigation for low volume):** optionally emit decoy migration txs so
  a batch is never size 1. This is the main lever against the density problem (README
  risk 1). Design carefully so decoys are valid or clearly discardable.

---

## 5. Encryption and key model (shim to hub)

Cleartext migration txs must be visible only inside the two attested enclaves (shim and
hub), never to either host operator.

- The shim (a TEE) decodes the tx to classify it, so it necessarily sees cleartext, but
  it is attested, so the operator host does not.
- The shim then **encrypts the tx to the hub's enclave public key** before handing it to
  the (possibly untrusted) Nym client. The hub key is born inside the hub enclave and
  bound into the hub's attestation.
- **"Steve"** (Caution's enclave-to-enclave encrypted key-sharing protocol) is the
  mechanism that delivers the hub's public key to each shim enclave and, longer term,
  lets the key be governed by the multi-sig consortium (Caution / Nym / Shielded Labs /
  ZF). Steve is under review (Zooko, Nate, Taylor); treat its interface as a dependency.
- **Future (Nym-aware wallets):** a wallet that knows the hub could encrypt the
  migration tx end-to-end to the hub key itself, so the shim would only route, not
  decrypt. Near-term wallets are naive TLS, so the shim classifies. Design the shim so
  this future path drops in.

---

## 6. Nym transport (shim to hub)

Per `deploy/caution-zaino/NYM.md`: nym-sdk `TcpProxy` binaries, `nym-proxy-client` on
the shim side and `nym-proxy-server` fronting the hub. The shim opens a Nym tunnel to
the hub's Nym address; the encrypted migration tx rides inside it. Nym's cover traffic
is what makes shim-to-hub traffic unlinkable, so the hub cannot tell which region /
operator a given migration came from. The Nym client can run outside the shim TEE
(untrusted byte mover) since the payload is already encrypted to the hub key.

---

## 7. Data flow (end to end)

```
wallet --SendTransaction(migration tx)--> shim(TEE)
   shim: decode, classify migration, encrypt to hub key
   shim --SendResponse{0}--> wallet          (accepted; see 8.1)
   shim --encrypted tx over Nym--> hub(TEE)
      hub: decrypt, dedup by txid, queue with expiry
      ... accumulate across all shims, up to N(<20) blocks ...
      hub: flush -> publish batch via P2P / sendrawtransaction --> Zcash network
tx appears on-chain in the batch, unlinked from the wallet's IP
```

Non-migration traffic: `wallet -> shim -> operator backend -> network`, unchanged.

---

## 8. Correctness and UX issues to resolve (feed the threat-model doc)

These are the non-obvious risks the design must answer before shipping:

1. **Delayed-broadcast visibility.** The shim returns success immediately, but the tx
   is not in any mempool until the hub flushes (up to ~15 blocks later). A wallet that
   polls "is my tx in the mempool / confirmed?" (via queries that pass through to the
   operator backend, which does not know about the queued tx) will see "not found" for
   minutes. This can confuse UIs or trigger resends. Options: accept it (migrations are
   not time-sensitive), define an expected UX, or have the shim answer pending-status
   for queued txids. **This is the biggest UX risk.**
2. **Resend / idempotency.** If the wallet resends (because the tx did not appear), the
   shim re-queues it. The hub must dedup by txid (it does), and the shim should be
   idempotent per txid within a window.
3. **Invalid-tx handling.** A normal `sendrawtransaction` validates synchronously and
   returns an error. The shim returns success before the hub publishes, so an invalid
   migration tx gets a false success and fails silently at flush. Mitigation: stateless
   pre-validation at the shim or hub, or accept and surface hub-side failures somehow.
4. **Classification false negatives leak.** A missed migration is broadcast in the clear
   (privacy failure), so the predicate should over-capture (section 3, question 2).
5. **Batch density.** At low migration volume a batch may be size 1 (no anonymity).
   Cover traffic (section 4) is the lever; report achieved batch size honestly.
6. **Hub liveness = migration liveness.** If the hub is down, migrations stall (they are
   held, not broadcast). Need a fallback or a clear failure mode, and eventually >1 hub.
7. **Expiry across the flush window.** Never queue a tx whose expiry is within the next
   flush; publish it immediately or reject, mirroring the Sapling-migration skip rule.

---

## 9. Attestation and reproducible build

Both binaries are static-musl, built reproducibly (StageX, `SOURCE_DATE_EPOCH=1`), and
run in Nitro enclaves, following the existing `deploy/caution-zaino/combined/`
Containerfile pattern. Each enclave generates its key at boot and binds the public key
into the NSM attestation (the RA pattern from `NYM.md`). Verification UX: a wallet dev
reproduces the build, gets the hash, and matches it against the signed hash at the
attestation URL, then trusts the endpoint on users' behalf. The shim and hub each
publish an attestation; the consortium governs the hub key long-term.

---

## 10. Milestones and critical path (~Aug 10)

**Critical path (guard ruthlessly):** shim + hub + Nym-between-them + one operator
running the shim (Caution counts).

1. **Classifier + fixtures.** `is_migration` over `zebra-chain` (or `zcash_primitives`),
   tested against the `ironwood_activation.rs` migration tx. Settle section 3's
   definition questions first.
2. **Shim proxy.** HTTP/2 pass-through with the single `SendTransaction` intercept;
   forward-everything-else; TLS listener from the grpc.rs template.
3. **Hub.** Ingest (decrypt), dedup, expiry-aware queue, flush-every-N-blocks, publish
   via P2P / `sendrawtransaction`.
4. **Encryption + Steve.** Shim encrypts to hub key; wire up Steve for key delivery.
5. **Nym shim to hub.** nym-proxy-client / -server tunnel.
6. **Attestation + reproducible build** for both, in an enclave.
7. **One operator** fronts their testnet backend with the shim; end-to-end demo.

**Off the critical path (post-launch):** the attested Nym fleet, Option A, the
query-only/broadcast-only split, PIR, full consortium governance, cover-traffic tuning.

---

## 11. Repo layout (proposed)

```
zeronym/
  README.md            strategy (done)
  SHIM-HUB.md          this build plan
  shim/                zero-indexer-shim (Rust crate)
  hub/                 zero-broadcaster (Rust crate)
  deploy/              Containerfiles + Caution policy for shim and hub enclaves
```

Zero-owned, plain conventional commits. Reuse the `zaino-proto` crate for the
`RawTransaction` types and, if useful, `zebra-chain` for tx parsing (a real dependency
to weigh against a minimal hand-rolled V6/bundle parser for a smaller TEE surface).

---

## 12. Key anchors (verified in-tree)

- Migration classifier: `zebra/zebra-chain/src/transaction.rs:1503` (`orchard_value_balance`),
  `:1520` (`ironwood_value_balance`), `:1129` (`ironwood_shielded_data`), `:510`
  (`expiry_height`); or `librustzcash/zcash_primitives/src/transaction/mod.rs:451`
  (`orchard_bundle`), `:455` (`ironwood_bundle`), `:435` (`expiry_height`).
- Ironwood / NU6.3: `librustzcash/components/zcash_protocol/src/consensus.rs:606,749`
  (branch id `0x37a5165b`), `:529` (testnet height 4,134,000, no mainnet);
  `zebra/zebra-chain/src/ironwood.rs`; domain record
  `zaino/docs/notes/ironwood-activation-plan.md`; test
  `zaino/live-tests/e2e/tests/ironwood_activation.rs:173-254`.
- Interception: proto `zaino/packages/zaino-proto/proto/service.proto:253`
  (`SendTransaction`), `:58-88` (`RawTransaction` / `SendResponse`); path
  `/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction`; JSON-RPC
  `zaino/packages/zaino-serve/src/rpc/jsonrpc/service.rs:240` (`sendrawtransaction`).
  Listener template `zaino/packages/zaino-serve/src/server/grpc.rs:40,75-97`.
- Expiry precedent: `zcashd/src/wallet/asyncrpcoperation_saplingmigration.cpp:18,125`
  (`MIGRATION_EXPIRY_DELTA = 50`).
- Nym transport: `deploy/caution-zaino/NYM.md`.
