# zero-indexer-hub (ZIH): design

The concrete engineering design for the hub (earlier called `zero-broadcaster`).
Context and threat model are in [../SHIM-HUB.md](../SHIM-HUB.md) and
[../README.md](../README.md); the shim design is in [../shim/DESIGN.md](../shim/DESIGN.md).
This doc and that one are meant to be reviewed together by Anton (Caution) and Zooko;
section 14 is the STEVE / cross-party agenda. **Decision:** marks a committed choice;
open forks are in section 14.

The ZIH is an attested-TEE service that receives encrypted **migration** transactions
from many shims over Nym, batches them, and publishes them to the Zcash network together
on a strict block cadence, so no party can link a migration to the source IP that
submitted it. It is run as **>=2 instances with failover**.

---

## 1. Topology and process model

```
   shim A ---.                          AWS Nitro enclave (attested, diskless)
   shim B ----\  Nym mixnet   +-------------------------------------------------+
   shim C -----+------------->| nym-proxy-server -> zero-indexer-hub            |
              /   (encrypted  |    decrypt (in enclave) -> validate -> queue    |
   shim N ---'    migrations) |    flush every <N blocks -> publish batch  -----+---.
                              |    hub key from keymaker M-of-N quorum          |   |
                              +---------------------|---------------------------+   | clearnet
                                                    | clearnet (tip + broadcast)    |
                                                    v                               v
                                       full node(s) (zebrad/zcashd)         Zcash P2P network
                                       getblockchaininfo + sendrawtransaction
```

- **Enclave contents (TCB):** the hub binary + rustls. It is **lightweight**, like the
  shim: it does NOT run a validator in-enclave (that is the 400-500 GB problem). It
  connects OUT to an existing full node for chain tip and for broadcasting.
- **Sidecars / egress:** `nym-proxy-server` fronts the inbound side (from shims);
  clearnet egress to full node(s) for tip + broadcast; egress to the keymaker quorum and
  (if needed) Nyx-RPC for Nym ecash. The Nym server can be parent-side (untrusted; the
  payload is already encrypted to the hub key).
- **Supervisor:** small PID-1 script, mirrors the shim's.

---

## 2. Language and dependencies

**Decision: Rust**, static-musl, reproducible (StageX), same reasons as the shim.

Key crates: `hyper`/`tower` or a minimal framed-TCP server for the inbound
`SubmitMigration` channel; `rustls`; `prost` for the tx bytes; `zebra-chain` to re-parse
and re-classify migrations (defense in depth, section 3); a JSON-RPC client (`jsonrpsee`)
for the node's `sendrawtransaction` / `getblockchaininfo` (section 6);
`aws-nitro-enclaves-nsm-api` for attestation; the keymaker/quorum client (section 7).

---

## 3. Inbound: receiving migrations

The hub is the server end of the shim's channel (shim/DESIGN section 6). Over the Nym
tunnel it accepts `SubmitMigration { ciphertext, txid, expiry_height }` and replies
`Ack { txid }`.

- **Channel + auth (STEVE).** The shim verifies the hub's attestation and derives a
  shared key (STEVE); migrations are encrypted to the hub. Whether the hub also
  authenticates the shim (mutual STEVE) is an open decision (section 14): one-way is
  enough for privacy, mutual would gate abuse.
- **Decrypt in-enclave.** Only the attested hub software sees cleartext; the hub host
  operator (Caution) and the Nym path see ciphertext.
- **Re-validate (do not trust the shim).** Parse with `zebra-chain`, **re-run the
  `is_migration` classifier** (shim/DESIGN section 5), and check the tx is well-formed
  and not already expired. This is stateless: full consensus validity (proofs,
  nullifiers) needs chain state the hub does not hold, and is confirmed at broadcast.
  Reject anything that is not a valid-looking, unexpired migration, to keep garbage out
  of the batch. **Rate-limit** per channel to bound abuse / resource use.

---

## 4. The batch queue

In-RAM (diskless enclave), keyed by **txid** for dedup. Each entry:
`{ txid, expiry_height, tx_bytes, received_at }`. The hub tracks the current chain
height `H` from its node connection (section 6) to schedule flushes and check expiries.
Duplicate submissions (same txid) collapse; this is also what makes cross-hub failover
safe (section 8).

---

## 5. Flush and publish (the core)

- **Flush trigger:** at every height that is a multiple of **N (N < 20; Decision:
  target ~10, well under Brave's 20-block migration expiry)**, OR earlier if any queued
  migration's `expiry_height <= H + safety_margin`. `safety_margin` covers not just
  expiry but expected time-to-mine, so a flushed tx actually confirms before it expires.
- **Publish "simultaneously."** On flush: take all pending migrations, **shuffle the
  order** (never leak arrival order), and submit them to the node(s) as close to
  simultaneously as possible (parallel `sendrawtransaction`), so they enter the mempool
  together and land in the same block window. An on-chain / mempool observer then sees N
  migrations appear together, unordered, from many shims. **Decision: randomize order +
  parallel submit**; do not drip them out.
- **Confirmation tracking.** Move flushed migrations to an "awaiting confirmation" set;
  watch the chain (section 6) until each is mined; **re-submit** if a tx is not seen
  within a few blocks (node dropped it, or a hub crash lost it). Drop from the set once
  confirmed or expired.
- **The anonymity set is the batch itself** (cross-operator), so batch size is the key
  metric; the hub logs achieved batch size honestly (SHIM-HUB section 5). Hub-generated
  decoys are a costly last resort, not the primary lever.

---

## 6. Chain connection (tip + broadcast)

**Decision: connect OUT to existing full node(s) (zebrad/zcashd) over clearnet**, do not
run a validator in-enclave.

- **Tip:** poll / subscribe `getblockchaininfo` (or `getbestblockhash` + height) to keep
  `H` current for flush cadence and expiry checks.
- **Broadcast:** `sendrawtransaction` for each tx in the flush. Connect to **>=2 nodes**
  for robustness; optionally speak Zcash **P2P `tx`** to many peers directly (Nate's
  point: P2P relay reaches a far larger node set than one lwd endpoint, a bigger
  anonymity set for the broadcast source). The hub's node IP is not user-linked, so
  clearnet is acceptable near-term; broadcasting over Nym to hide the hub's own IP is an
  optional enhancement (section 14).
- The node connection is a hard dependency (no tip -> cannot schedule; no node ->
  cannot broadcast), so >=2 nodes, and node-down is part of the hub's failure handling
  (section 12).

---

## 7. Key management (hub key, STEVE, keymaker quorum)

- **The hub key** (that shims encrypt migrations to) is generated in-enclave and
  **persisted via the keymaker/locksmith M-of-N quorum across 3-4 orgs** (Caution / Nym /
  Shielded Labs / ZF), reconstituted across cold boots and upgrades (better than
  KMS-seal-to-PCR, which breaks on upgrade). The consortium governs it.
- **Decision: a single shared hub key across all hub instances**, provisioned to each
  attested hub by the quorum. This is what makes failover clean: a shim encrypts to "the
  hub key" and any hub instance can decrypt, dedup, and publish. (Per-hub keys would
  force the shim to re-encrypt on failover and would strand a migration if its hub died
  mid-flight.)
- **STEVE** is the handshake by which a shim verifies a hub's attestation (which binds
  the hub's public key) and derives a session key. The hub's role: present its
  attestation and complete the handshake. The exact STEVE wire form over Nym, and
  whether it is mutual, are the main items for the Anton / Zooko discussion (section 14).

---

## 8. Failover and multiple hubs

- **Run >=2 hubs, shared key** (section 7). A shim prefers a **primary** hub (so batches
  converge there and stay dense) and **fails over** to a standby only when the primary is
  unreachable. Standbys are hot but mostly idle until failover, so **batch density is
  preserved** (one active hub) while liveness is covered.
- **Dedup by txid** within each hub. If failover causes a migration to reach two hubs,
  both may publish; the second on-chain submission is a **harmless already-known
  duplicate**. No cross-hub state sync is needed near-term (Decision: accept harmless
  duplicates over the complexity of a shared published-set).
- The consortium's multiple orgs are the natural operators of the standby hubs, which
  also starts decentralization.

---

## 9. Boot sequence

1. **Hub key** from the keymaker quorum (reconstitute; the private key stays in-enclave).
2. **Attestation:** bind the hub public key into the Nitro attestation (Caution
   mechanism); publish `/attestation` (or over Nym) for the shim's STEVE check + auditors.
3. **Chain:** connect to the full node(s); sync `H`; verify `sendrawtransaction` works.
4. **Inbound:** start `nym-proxy-server`; begin accepting `SubmitMigration` over STEVE.
5. **Run** the flush loop against `H`.

---

## 10. Attestation, reproducible build, auditor role

Static-musl, reproducible StageX build, runs in a Nitro enclave. Publishes an attestation
carrying the hub public key + the software root hash. The shim's STEVE handshake **is**
the shim auditing the hub before trusting it with migrations; independent auditors run
the same steps (SHIM-HUB section 9). Reproducibility lets the consortium and third
parties confirm the running hub is the reviewed hub.

---

## 11. Configuration

```
listen_nym          = <hub Nym address / proxy-server config>
nodes               = ["node-a:8232", "node-b:8232"]   # tip + broadcast, >=2
network             = "testnet" | "mainnet"
flush_interval_blocks = 10                # < 20 (Brave expiry)
safety_margin_blocks  = 5                 # flush early if expiry within; covers mining time
role                = "primary" | "standby"
keymaker_quorum     = <quorum endpoints / policy>
peer_hubs           = [...]               # awareness only, no shared state near-term
```

---

## 12. Failure modes and correctness

- **Node(s) down:** cannot get tip or broadcast; with >=2 nodes this is rare, but if all
  are down the hub cannot flush. Since the shim retains + re-submits and fails over, and
  migrations carry expiry slack, brief node outages self-heal; a sustained one is what the
  shim's last-resort direct broadcast covers.
- **Hub crash:** the in-RAM queue and awaiting-confirmation set are lost (diskless). The
  shim retains each migration until it observes on-chain confirmation and re-submits, and
  fails over to a standby, so a hub crash does not lose migrations. (This makes the
  shim-side retain-until-confirmed a hard requirement, shim/DESIGN section 11.)
- **Expiry pressure:** flush early per `safety_margin`; never let a queued migration
  expire in the buffer.
- **Garbage / abuse:** re-validate + rate-limit (section 3); optionally require shim
  attestation (section 14).
- **Fee too low to mine before expiry:** the fee is in the wallet-signed tx and the hub
  cannot change it; `safety_margin` gives mining headroom, but a badly-underpaid
  migration can still fail. That is the wallet's responsibility, not the hub's.

---

## 13. Crate layout

```
zeronym/hub/
  Cargo.toml
  src/
    main.rs          # config, boot sequence, run the flush loop
    inbound.rs       # STEVE server, SubmitMigration decode, decrypt
    validate.rs      # re-parse + re-classify (zebra-chain) + expiry/well-formed checks
    queue.rs         # in-RAM dedup queue keyed by txid; expiry tracking
    flush.rs         # cadence + expiry trigger, shuffle, parallel publish, confirm-track
    chain.rs         # node connection: tip + sendrawtransaction (+ optional P2P)
    keys.rs          # keymaker quorum reconstitution; STEVE handshake; decrypt
    attest.rs        # NSM attestation binding; /attestation
    config.rs
  tests/
    flush_batch.rs   # queue N migrations, assert a single simultaneous shuffled publish
    expiry.rs        # assert early flush when expiry is within the safety margin
    dedup.rs         # duplicate txids collapse; cross-hub duplicate is harmless
```

---

## 14. STEVE and cross-party open questions (agenda for Anton + Zooko)

The shim and hub designs are settled enough to build; these are the cross-party items
to close, and the reason to get Anton, Zooko (and Nate/Taylor) in one room.

**For Anton (Caution):**
1. **STEVE wire form over Nym.** What does a STEVE session carry over the Nym tunnel:
   gRPC/h2, or a raw byte stream we frame ourselves? (Blocks the exact `SubmitMigration`
   transport, not its shape.)
2. **STEVE: mutual or one-way?** The shim must verify the hub. Should the hub also verify
   the shim's attestation (to gate abuse), or accept from anyone with rate-limiting?
3. **Keymaker quorum walkthrough.** How the **single shared hub key** is provisioned to
   multiple attested hub instances (for failover), and reconstituted across cold boots and
   upgrades.
4. **Nym server placement on managed Caution.** Can `nym-proxy-server` run parent-side
   (untrusted) alongside a managed enclave, or must it be in-enclave?
5. **Zero-ingress + attestation delivery.** Suppress the platform's public `/attestation`
   + Caddy for a true zero-inbound service, or deliver attestation inline over Nym?

**For Zooko / Nate:**
6. **Publish path.** `sendrawtransaction` to >=2 external nodes vs direct Zcash P2P to
   many peers; clearnet vs over Nym for the hub's own egress.
7. **Batch density vs failover.** Confirm primary-hub preference (converge for density,
   fail over only on outage) over spreading shims across hubs.
8. **Hub re-validation.** Confirm the hub should re-parse + re-classify (not trust the
   shim), and reject non-migrations, before batching.
9. **Flush cadence + safety margin.** Confirm ~10-block flush and the safety margin that
   covers mining time, against the real wallet expiry windows (ZIP 318).

---

## 15. Build and test

- **Unit:** `flush.rs` (N queued migrations -> one shuffled, parallel publish),
  `expiry.rs` (early flush within the safety margin), `dedup.rs` (txid collapse;
  harmless cross-hub duplicate), `validate.rs` (reject non-migrations / expired).
- **Integration:** a mock shim submitting over a local STEVE-ish channel and a mock node
  capturing `sendrawtransaction`; assert a batch of migrations is published together,
  shuffled, once per txid.
- **Enclave:** reproducible StageX build; boot in a Nitro enclave; verify the
  `/attestation` doc carries the hub key; a shim completes STEVE and submits a migration.
- **End to end:** >=1 shim in front of the live testnet Zaino, a testnet migration flows
  shim -> Nym -> hub -> batch -> testnet chain; confirm it lands and is unlinkable to the
  submitting shim.
