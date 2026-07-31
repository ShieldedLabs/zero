# zero-indexer-shim (ZIS): design

The concrete engineering design for the shim. Context and threat model are in
[../SHIM-HUB.md](../SHIM-HUB.md) and [../README.md](../README.md); this doc is the
"how it is built." **Decision:** marks a committed choice; genuinely open forks are in
section 13.

The ZIS is an attested-TEE proxy an operator deploys behind their **existing public
URL** (e.g. `zec.rocks:443`). It is a drop-in LWD to every wallet (no wallet change),
forwards all traffic to the operator's unmodified backing lwd, and isolates only
**migration** `SendTransaction`s, which it routes over Nym to the hub.

---

## 1. Topology and process model

```
                         AWS Nitro enclave (attested, diskless)
  wallet --TLS/h2-->   +---------------------------------------------------+
  https://<puburl>:443 |  zero-indexer-shim                                 |
                       |   1. TLS terminate (enclave-born key + cert)       |
                       |   2. route by HTTP/2 :path                         |
                       |        SendTransaction -> classify                 |
                       |          migration -> encrypt -> hub channel ------+--.
                       |          non-migration --------------------.       |  |
                       |        any other method / stream ----------|       |  |
                       |                                            vv       |  | Nym
                       |                              proxy to backing lwd   |  | tunnel
                       +---------------------|-------------------------------+--|--------+
                                             | internal cleartext gRPC          |
                                             v                                  v
                                   operator's existing lwd            local nym-proxy-client
                                   (unmodified)                               -> hub (ZIH)
```

- **Enclave contents (the TCB):** the ZIS binary + rustls. **Nym client is untrusted**
  (payload is already encrypted to the hub key), so `nym-proxy-client` runs as a sidecar
  (in-enclave on managed Caution since we do not control the parent; parent-side on BYOC).
- **The backing lwd is untrusted for migrations** (it never sees them) and trusted for
  everything else (it already serves those today).
- **Supervisor:** a small PID-1 script starts the Nym client then the ZIS, ties their
  lifecycles, mirrors `deploy/caution-zaino/combined/run-both.sh`.

---

## 2. Language and dependencies

**Decision: Rust.** Matches the ecosystem, lets us reuse `zebra-chain` for the
classifier and `rustls` for TLS, and produces a static-musl binary for a small
reproducible enclave image.

Key crates:
- `hyper` + `hyper-util` (HTTP/2 server + client), `tower`, optionally `axum` (its
  router) for path routing. `rustls` + `tokio-rustls` for TLS. See section 3 for why
  not the full `tonic` server.
- `prost` + the generated `RawTransaction` / `SendResponse` types (depend on
  `zaino-proto`, or a tiny local `build.rs` over `service.proto`) for the one decoded
  method.
- `zebra-chain` for transaction parsing / value balances (section 5).
- `aws-nitro-enclaves-nsm-api` for attestation; an ACME client (`instant-acme` or
  `rustls-acme`) for the cert (section 8); the Nym client is a separate process, not a
  linked crate.

---

## 3. Request pipeline (the core)

**Decision: an HTTP/2 reverse proxy, not a full `tonic` server.** After TLS
termination the ZIS routes by the `:path` pseudo-header:

- **Every path except `SendTransaction`** (all queries, all streams, unknown/new
  methods, other services): **proxy verbatim** to the backing lwd. Forward request
  headers + streaming body, stream the response body and **trailers** (`grpc-status`)
  back. No decode. This is a generic h2 reverse proxy and is base-agnostic (works for
  Zaino or lightwalletd; unknown methods just pass through).
- **`/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction`** (unary): buffer the one
  request message (small), strip the 5-byte gRPC length prefix, `prost`-decode
  `RawTransaction { data }`, and classify (section 5):
  - **non-migration** -> proxy to the backing lwd exactly like the fallback, return the
    backing lwd's real `SendResponse` (so the operator's node actually relays it and the
    client gets the true result).
  - **migration** -> encrypt to the hub key, hand to the hub channel (section 6), and
    **synthesize** a gRPC response: `SendResponse { errorCode: 0 }`, framed with the
    5-byte prefix + `grpc-status: 0` trailer, so the client sees "accepted."

Rationale over a `tonic` server re-exporting all ~20 methods: this touches exactly one
message type (smallest auditable TEE surface), and hyper handles h2 framing / flow
control / trailers for the pass-through. The "proxy to backing lwd" step is a shared
helper used by both the fallback and the non-migration `SendTransaction` case.

**Decision (parse-fail / uncertain classification): fail safe for privacy.** If a
`SendTransaction` body cannot be parsed or classified, treat it as a **migration**
(route to the hub) rather than forward it in the clear. A false positive only delays a
normal tx (the hub still broadcasts it); a false negative would leak a real migration.
The hub validates and broadcasts, so an unparseable/invalid tx is caught there. (Bounded
exception if this proves to break a common well-formed shape, section 13.)

Note: gRPC compression on the intercept path is not supported; advertise `identity`
only. If a client sets `grpc-encoding: gzip` on a `SendTransaction`, either decompress
or, to stay safe, treat as migration (do not mis-parse). Everything on the pass-through
path is opaque, so client compression there is fine.

---

## 4. What the operator does

1. Deploy the ZIS enclave, point the public DNS/URL (`zec.rocks:443`) at it.
2. Configure the ZIS with the **backing lwd's internal address** (e.g.
   `10.0.0.5:9067`) and the **hub's Nym address**.
The backing lwd is unchanged and stays on its internal address. From its perspective
the ZIS is a single gRPC client.

---

## 5. The migration classifier

Reuse the fee-aware turnstile predicate from SHIM-HUB section 3, batching only the
`migrate` case:

```
is_migration(tx) := tx.version == V6
                 && orchard_value_balance  > 0    # value leaving Orchard
                 && ironwood_value_balance < 0    # value entering Ironwood
```

**Decision: parse with `zebra-chain`** (`Transaction::V6`, `orchard_value_balance()`
`transaction.rs:1503`, `ironwood_value_balance()` `:1520`, `expiry_height()` `:510`),
not a hand-rolled V6 parser. A misclassification is a privacy failure, so correctness
outweighs the extra dependency weight; a hand-rolled parser would have to walk most of
the tx anyway to reach the bundle value balances. Fast-path pre-V6 txs to pass-through
(only V6 can carry Ironwood). Fixture: `zaino/live-tests/e2e/tests/ironwood_activation.rs`.

The classifier is a pure function `fn classify(raw: &[u8]) -> Class` (Class = Migration
| PassThrough | Unparseable) with unit tests over real deshield / shield / migrate /
normal-shielded vectors. It has no I/O and no state, so it is the easy part to audit.

---

## 6. The hub channel (ZIS -> ZIH)

The encrypted migration travels ZIS -> local `nym-proxy-client` -> Nym mixnet ->
`nym-proxy-server` -> ZIH. To the ZIS this is a local TCP endpoint the Nym client
exposes.

- **Attested, encrypted channel.** The ZIS verifies the hub's attestation and
  establishes a shared key (STEVE handshake, per the V2 sync), then sends each migration
  as an encrypted message. The payload is end-to-end encrypted to the hub enclave, so
  the Nym client, the mixnet, and the parent host see only ciphertext. **Decision: the
  migration is encrypted to the hub key regardless of channel**, so a compromised Nym
  path yields nothing.
- **Message:** a small framed record `SubmitMigration { ciphertext, txid, expiry_height }`
  (txid + expiry in the clear to the hub for dedup and flush scheduling; the tx body
  encrypted). The hub replies `Ack { txid }`.
- **Delivery guarantees (section 12):** the ZIS holds the migration until it has an Ack
  from some hub, retrying and failing over across the >=2 hubs, using the migration's
  expiry slack; last-resort direct broadcast before expiry.

---

## 7. Boot sequence

1. **Key material.** Obtain the TLS keypair: reconstitute from the **keymaker M-of-N
   quorum** if one exists, else generate in-enclave and register it with the quorum
   (so it persists across cold boots and upgrades). The private key never leaves the
   enclave.
2. **Certificate.** Ensure a valid CA cert for the public domain via **ACME**
   (section 8), keyed to the enclave-born key.
3. **Attestation.** Bind the TLS public key into the Nitro attestation (Caution
   mechanism: `user_data` / `arbitrary_data` / STEVE, section 11). Serve the
   `/attestation` endpoint (or expose it over Nym, section 13).
4. **Hub session.** STEVE-handshake each configured hub; cache the shared keys.
5. **Backing lwd.** Open the upstream h2 connection(s) to the backing lwd; health-check.
6. **Listen.** Bind `:443`, serve.

---

## 8. TLS and certificate model

**Decision: ACME-issued cert for the public domain, key born and held in the enclave.**
Wallets do standard TLS against `zec.rocks:443`, so the ZIS must present a **valid
CA-issued cert** (drop-in, no wallet change). The private key must be enclave-born (else
the operator holds it and can MITM). So the enclave generates the key and runs an **ACME
client** (Let's Encrypt) to get the cert, completing the TLS-ALPN-01 or HTTP-01 challenge
itself (it controls the endpoint). The key persists via the keymaker quorum; the cert
renews via ACME. All Let's Encrypt certs are **CT-logged**, which is exactly what the
Auditor Role's Certificate Transparency check relies on.

This is what makes the drop-in and the Auditor Role coexist: a normal wallet gets a
normal valid cert; an auditor additionally checks (a) the cert's key is attested as
enclave-born and (b) CT shows no other valid cert for the domain (no operator shadow
cert). See SHIM-HUB section 9.

---

## 9. Attestation binding

Bind the **TLS public key** (not the Nym address) into the attestation, so the auditor
(and, later, RA-TLS-aware clients) can check cert-pubkey == attested key. Three feasible
mechanisms from the V2 sync: STEVE handshake, `metadata.json -> user_data` (build-time,
implies a persisted key), or a runtime `arbitrary_data` field Caution would add. The
same in-enclave-keygen + NSM path Zaino/V2 use applies.

---

## 10. Configuration

```
public_domain      = "zec.rocks"            # for ACME + the served endpoint
backing_lwd         = "10.0.0.5:9067"        # operator's existing lwd (internal)
hubs               = ["<nym-addr-1>", "<nym-addr-2>"]   # >=2 for failover
network            = "testnet" | "mainnet"   # selects Ironwood branch id / activation
flush_safety_blocks = 5                       # broadcast direct if expiry within N blocks
acme               = { provider, contact, challenge = "tls-alpn-01" }
```

Config is loaded at boot; secrets (none needed in the clear, since keys are quorum- or
enclave-held) are not on disk.

---

## 11. Failure modes and correctness

- **Backing lwd down:** pass-through requests fail as they would today; the ZIS is
  transparent, so this is the operator's existing failure mode, not a new one.
- **Hub unreachable:** hold + retry + fail over across hubs; last-resort direct broadcast
  before expiry (SHIM-HUB section 4 Resilience). The ZIS keeps a migration until it sees
  an Ack, and ideally until it observes the tx on-chain, re-submitting if a hub crash
  lost it.
- **Restart durability:** the enclave is diskless, so in-flight migrations in RAM are
  lost on a ZIS restart. Mitigation: keep the ACK-pending set small (short retry loop),
  and rely on the wallet's own resend if needed; the hub dedups by txid.
- **Invalid migration -> false success:** the ZIS returns success before the hub
  broadcasts, so an invalid tx fails silently at flush. Do stateless sanity at the ZIS
  (parseable, not already expired); full validity is the hub's broadcast result
  (surfacing it to the client is out of near-term scope).
- **The operator learns *that* a client migrated** (SHIM-HUB section 8 item 8):
  inherent; do not attempt shim-side mitigation (rejected variants).

---

## 12. Crate layout

```
zeronym/shim/
  Cargo.toml
  build.rs                  # prost for RawTransaction/SendResponse (or dep zaino-proto)
  src/
    main.rs                 # config load, boot sequence, serve
    tls.rs                  # in-enclave keygen, ACME, keymaker-quorum persistence
    attest.rs               # NSM attestation binding; /attestation endpoint
    proxy.rs                # h2 server, :path routing, reverse-proxy to backing lwd
    intercept.rs            # SendTransaction decode + gRPC frame + response synth
    classify.rs             # is_migration over zebra-chain (pure, unit-tested)
    hub.rs                  # STEVE session, encrypt, SubmitMigration, retry/failover
    config.rs
  tests/
    classify_vectors.rs     # deshield/shield/migrate/normal fixtures
    proxy_passthrough.rs    # a mock backing lwd; assert non-migration passes through
```

---

## 13. Open decisions (for review)

1. **Cert model.** ACME-in-enclave (above) commits the enclave to running an ACME client
   and owning the `:443`/`:80` challenge. Confirm that is the intended drop-in cert story
   vs. an alternative (operator-provisioned cert would leak the key, so it is out).
2. **Hub channel wire form.** STEVE-derived key + our own framing over the Nym TCP tunnel,
   vs RA-TLS/h2 over the tunnel. Pending Anton's answer on whether STEVE-over-Nym carries
   gRPC/h2. The `SubmitMigration` message shape is channel-independent, so this does not
   block the rest.
3. **Parse-fail policy.** Fail-safe-for-privacy (treat unparseable `SendTransaction` as a
   migration) is the default above; confirm it is acceptable that a rare unparseable
   normal tx would be delayed.
4. **JSON-RPC.** Wallets use gRPC, so `sendrawtransaction` over JSON-RPC is out of
   near-term scope. Confirm no operator front-ends migrations via JSON-RPC.
5. **Attestation delivery for zero-ingress.** The `/attestation` endpoint is public today
   (platform-served). For a true zero-inbound service we either suppress the platform's
   public endpoint or serve attestation inline over Nym (SHIM-HUB / NYM.md).

---

## 14. Build and test

- **Unit:** `classify.rs` against real vectors (the `ironwood_activation.rs` migrate tx,
  plus constructed deshield/shield/normal-shielded) - this is the correctness-critical
  piece.
- **Integration:** a mock backing lwd; assert every non-`SendTransaction` method and a
  non-migration `SendTransaction` pass through unchanged, and a migration is diverted to
  a mock hub (never reaches the backing lwd).
- **Enclave:** reproducible StageX build; boot in a Nitro enclave; verify the
  `/attestation` doc carries the TLS pubkey; run the Auditor Role steps (SHIM-HUB
  section 9) against it.
- **End to end:** ZIS in front of the live testnet enclave's Zaino, a real wallet syncs
  (pass-through) and submits a testnet migration that lands in a hub batch.
