# Zeronym shim + hub: build plan

Build plan for the near-term turnstile-privacy system (`zero-indexer-shim` +
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
  intercepts a `SendTransaction` carrying a transaction, classifies it, and isolates
  only the **migration** case (cross-pool shielded move), routing it over Nym to a hub;
  deshields, shields, and everything else pass through.
- **`zero-broadcaster`** (the hub): an attested-TEE service (run as >=2 with failover) that accumulates
  migrations from every shim, batches them, and publishes them together on a strict
  block cadence.
- **Nym** between shim and hub only (the "zeroith step").

The whole point: the operator never sees the migration cleartext, the network
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
      if is_migration(tx_bytes):              # section 3 (the migrate case)
          encrypt tx to a hub enclave key     # section 5
          hand off to the Nym client -> a hub # section 6 (failover + retry, section 4)
          return synthesized SendResponse{errorCode: 0}   # see section 8 for the honesty cost
      else:                                   # deshields, shields, non-crossings, queries
          forward verbatim to the operator backend
  else:
      forward verbatim (all queries, streams, other methods)
```

Everything except a migration, including deshields, shields, all queries, and pure
transparent or intra-pool shielded payments, reaches the operator's unchanged backend
exactly as today. The shim adds no query privacy (by
design; see the README threat model).

---

## 3. Classifying a turnstile-crossing transaction (the crux)

The shim **classifies** every transaction that crosses a value-pool turnstile (value
moving between the transparent pool and a shielded pool, or between two shielded pools).
Near-term it **batches only the `migrate` case** (a cross-pool shielded move, e.g.
Orchard to Ironwood); deshields and shields are detected but pass straight through
(section 2.2). The general predicate is still worth building: it dodges the value-pool
ambiguity a migration-only rule would have, and future-proofs the batched set as a
policy knob that cleanly separates `migrate` from the rest.

There is no on-wire "turnstile" or "migration" marker; classification is **structural
and fee-aware**. The fee subtlety: every shielded transaction has a small value balance
leaving its pool to pay the fee, so "nonzero shielded value balance" alone would
false-positive on ordinary shielded payments. The real signal is value crossing to a
**real destination on the other side**, which transparent input/output presence
discriminates:

```
is_turnstile_crossing(tx):
  vb_s, vb_o, vb_i = sapling / orchard / ironwood value_balance   # +ve = leaving that pool
  has_t_out = tx has transparent outputs with value
  has_t_in  = tx has transparent inputs
  leaving   = any shielded vb > 0
  entering  = any shielded vb < 0
  deshield  = leaving  && has_t_out         # shielded  -> transparent
  shield    = has_t_in && entering          # transparent -> shielded
  migrate   = a shielded pool leaving AND a DIFFERENT shielded pool entering  # e.g. Orchard -> Ironwood
  return deshield || shield || migrate
```

Why this is fee-robust without a magic threshold: a pure shielded payment (Orchard to
Orchard) has `vb_o = fee > 0` but **no transparent outputs**, so `deshield` is false;
and value that merely leaves a pool to pay the fee cannot appear as an *entering*
shielded pool, so `migrate` is false. The transparent in/out presence, not a value
threshold, is the discriminator. Fast-path all-transparent txs and pure-intra-pool
shielded txs (and pre-V6 txs, for the Ironwood case) straight to passthrough.

Parsing accessors (all `zebra/zebra-chain/src/transaction.rs`; `zcash_primitives` has
equivalents):
- transparent `inputs()` (:555), `outputs()` (:574)
- `sapling_value_balance()` (:1385), `orchard_value_balance()` (:1503),
  `ironwood_value_balance()` (:1520), combined `value_balance()` (:1561)
- `expiry_height()` (:510-529) for the hub's batching deadline
- Ironwood context: only `Transaction::V6` carries an Ironwood bundle (branch id
  `0x37a5165b`, NU6.3).

The three named subcases the predicate must all catch: **deshield** (shielded to
transparent), **shield** (transparent to shielded), and **migrate** (shielded to
shielded cross-pool, including Orchard to Ironwood). Grounding / fixtures:
`zaino/live-tests/e2e/tests/ironwood_activation.rs:173-254` for the migrate case and
`zaino/docs/notes/ironwood-activation-plan.md`; construct deshield / shield fixtures
similarly.

**Open definition questions (for the threat-model doc, section 8):**
1. **False negative = leak, false positive = delay.** A missed crossing broadcasts in
   the clear (privacy failure); a wrongly-flagged tx is merely delayed. Lean toward
   over-capture; let the threat model set the exact line.
2. **Discriminator edge cases.** Confirm against the wallet builders that
   transparent-output presence cleanly separates a deshield from a fee-only shielded
   payment, and that no common shape crosses a turnstile with neither a transparent
   output nor a second shielded pool.
3. **Coinbase / mining-reward shielding** is a shield (transparent coinbase input to a
   shielded output). Confirm it should be batched (probably yes).
4. **Network + activation.** NU6.3 / Ironwood has a testnet height (4,134,000) and no
   mainnet height in this tree; confirm the target network. Note deshield / shield
   crossings exist on mainnet today independent of Ironwood, so the shim protects real
   mainnet traffic from day one even before mainnet Ironwood.

---

## 4. `zero-broadcaster`: the hub

An attested-TEE service, run as at least two instances with shim failover (section 4
Resilience) so a hub outage never stalls migrations.

- **Ingest:** receives encrypted migrations from shims over Nym, decrypts them
  **inside the enclave** (only the attested hub software sees cleartext; the hub's host
  operator, Caution at launch, stays blind).
- **Accumulate:** holds pending migrations in RAM (the enclave is diskless), keyed
  by txid, deduplicated. Tracks each tx's `expiry_height`.
- **Batch + flush:** publishes all pending txs together on a strict block cadence.
  Because all crossings are batched, queued txs carry a **range** of expiry heights, so
  the hub must flush before the **tightest** queued expiry. **Flush every N blocks with
  N well under 20** (aim 10-15): Brave mints with a +20-block expiry, librustzcash +40,
  Zingo 100 (ZIP 318 aligns these). Precedent for the skip-if-would-expire rule: the
  Sprout to Sapling migration op (`MIGRATION_EXPIRY_DELTA = 50`,
  `zcashd/src/wallet/asyncrpcoperation_saplingmigration.cpp:18,125`).
- **Publish path:** broadcast via P2P relay to many nodes rather than a single
  lightwallet endpoint (Nate's point: P2P relay is on by default on all full nodes, a
  far larger anonymity set). Concretely, submit to one or more full nodes'
  `sendrawtransaction`, or speak P2P `tx` messages directly. Decide in section 8.
- **Resilience (a hub outage must not stall migrations):** run **at least two attested
  hubs with shim failover** (dedup by txid; a double-publish is a harmless on-chain
  duplicate). The shim holds and retries using each migration's expiry slack, and as a
  last resort broadcasts a near-expiry migration directly over Nym rather than let it
  expire. Degrade privacy at the margin, never liveness (README section 5).
- **Cover traffic (mitigation if a window is still thin):** optionally emit decoy
  migrations so a batch is never size 1. Density is naturally high during the acute
  migration window; cover traffic is the backstop otherwise. Design decoys to be valid
  or clearly discardable.

---

## 5. Encryption and key model (shim to hub)

Cleartext migrations must be visible only inside the two attested enclaves (shim and
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
  migration end-to-end to the hub key itself, so the shim would only route, not
  decrypt. Near-term wallets are naive TLS, so the shim classifies. Design the shim so
  this future path drops in.

---

## 6. Nym transport (shim to hub)

Per `deploy/caution-zaino/NYM.md`: nym-sdk `TcpProxy` binaries, `nym-proxy-client` on
the shim side and `nym-proxy-server` fronting the hub. The shim opens a Nym tunnel to
the hub's Nym address; the encrypted crossing tx rides inside it. Nym's cover traffic
is what makes shim-to-hub traffic unlinkable, so the hub cannot tell which region /
operator a given crossing came from. The Nym client can run outside the shim TEE
(untrusted byte mover) since the payload is already encrypted to the hub key.

---

## 7. Data flow (end to end)

```
wallet --SendTransaction(migration)--> shim(TEE)
   shim: decode, classify; migration -> encrypt to hub key
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
   is not in any mempool until the hub flushes (up to ~25 min later). A wallet
   polling "is my tx in the mempool / confirmed?" (via queries that pass through to the
   operator backend, which does not know about the queued tx) will see "not found" for
   minutes. This can confuse UIs or trigger resends.
   Migrations are not time-sensitive, so the delay itself is fine, but the pending-state
   UX must be defined: accept it, or have the shim answer pending-status for queued
   txids. **Bounded, since migrations are not urgent, but still real.**
2. **Resend / idempotency.** If the wallet resends (because the tx did not appear), the
   shim re-queues it. The hub must dedup by txid (it does), and the shim should be
   idempotent per txid within a window.
3. **Invalid-tx handling.** A normal `sendrawtransaction` validates synchronously and
   returns an error. The shim returns success before the hub publishes, so an invalid
   migration gets a false success and fails silently at flush. Mitigation: stateless
   pre-validation at the shim or hub, or accept and surface hub-side failures somehow.
4. **Classification false negatives leak.** A missed migration is broadcast in the clear
   (privacy failure), so the predicate should over-capture (section 3, question 2).
5. **Batch density.** At low migration volume a batch may be size 1 (no anonymity).
   Cover traffic (section 4) is the lever; report achieved batch size honestly.
6. **Hub liveness.** Addressed by the resilience design (section 4 and README section
   5): >=2 hubs with shim failover, shim retry using expiry slack, and a last-resort
   direct broadcast so a migration never expires.
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

1. **Classifier + fixtures.** `is_turnstile_crossing` over `zebra-chain` (or
   `zcash_primitives`), tested against deshield, shield, and migrate fixtures (the
   `ironwood_activation.rs` tx is the migrate case). Settle section 3's definition
   questions first.
2. **Shim proxy.** HTTP/2 pass-through with the single `SendTransaction` intercept;
   forward-everything-else; TLS listener from the grpc.rs template.
3. **Hub (run >=2 with failover).** Ingest (decrypt), dedup by txid, expiry-aware queue,
   flush-every-N-blocks, publish via P2P; plus shim-side retry and a last-resort
   direct-broadcast fallback (section 4).
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

- Turnstile classifier: `zebra/zebra-chain/src/transaction.rs:555` (`inputs`), `:574`
  (`outputs`), `:1385` (`sapling_value_balance`), `:1503` (`orchard_value_balance`),
  `:1520` (`ironwood_value_balance`), `:1561` (`value_balance`), `:510` (`expiry_height`);
  `librustzcash/zcash_primitives/src/transaction/mod.rs` has equivalents.
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
