# Architecture

The [zero-indexer-shim](./shim.md) + [zero-indexer-hub](./hub.md) system that stops
Orchard to Ironwood **migration** transactions from leaking a user's IP. See the
[overview](./overview.md) and [trust](./trust.md) chapters for the surrounding context.
Two diagrams below: the data flow, then the trust / verification plane (kept separate
for readability).

---

## 1. Data flow and trust boundaries

```mermaid
flowchart TB
  subgraph WAL["Wallet (user device)"]
    W["Light wallet (drop-in; not STEVE or Nym aware)"]
  end

  subgraph OP["Operator host  (UNTRUSTED)"]
    subgraph SHIM["zero-indexer-shim enclave  (attested TCB)"]
      STLS["1 TLS terminate (enclave-born key)"]
      SROUTE["2 HTTP/2 path router"]
      SCLASS["3 is_migration classifier"]
      SPROXY["4 pass-through proxy"]
      SHUB["5 hub-channel client"]
    end
    LWD["backing lwd (operator's, unmodified)"]
    NYMC["nym-proxy-client (untrusted)"]
  end

  OTHER["shims at other operators"]

  subgraph NYMNET["Nym 5-hop mixnet  (untrusted)"]
    NYM["Nym mixnet (Sphinx + cover traffic)"]
  end

  subgraph HH["Hub host  (Caution, UNTRUSTED; 2+ with failover)"]
    NYMS["nym-proxy-server (untrusted)"]
    subgraph HUBENC["zero-indexer-hub enclave  (attested TCB; no validator inside)"]
      HDEC["decrypt (STEVE server)"]
      HVAL["re-validate (stateless)"]
      HQ["dedup queue (txid, in RAM)"]
      HFLUSH["batch + publish"]
    end
  end

  HUB2["standby hub enclave (shared key)"]

  subgraph NET["Full nodes / Zcash network"]
    FN["full node(s) (2+)"]
    ZNET["Zcash P2P network"]
  end

  NOTE_OP["Residual: the operator learns THAT a client migrated (it is the one request not forwarded to its lwd), not the amount"]
  NOTE_BATCH["Anonymity set = the cross-operator batch; a batch of 1 = no anonymity"]

  %% pass-through path (thin): queries + non-migration txs
  W -->|"TLS (ends in enclave, not STEVE)"| STLS
  STLS -->|"decrypted h2 (in TCB)"| SROUTE
  SROUTE -->|"other methods + streams"| SPROXY
  SROUTE -->|"SendTransaction"| SCLASS
  SCLASS -->|"non-migration"| SPROXY
  SPROXY -->|"queries + non-migration txs (plaintext to operator)"| LWD
  LWD -->|"relay (clearnet)"| ZNET

  %% migration path (thick): encrypted end to end, bypasses the lwd
  SCLASS ==>|"migration (or fail-safe): encrypt to hub key"| SHUB
  SHUB -.->|"accepted (not yet on-chain)"| W
  SHUB ==>|"SubmitMigration over STEVE"| NYMC
  NYMC ==>|"Sphinx"| NYM
  OTHER ==> NYM
  NYM ==>|"5-hop (hides shim + region)"| NYMS
  NYMS ==>|"STEVE ciphertext (host blind)"| HDEC
  HDEC ==> HVAL
  HVAL ==>|"valid, unexpired"| HQ
  HQ ==>|"flush ~10 blocks or on expiry"| HFLUSH
  HFLUSH ==>|"sendrawtransaction (batched, shuffled)"| FN
  FN -->|"P2P relay"| ZNET
  HFLUSH -.->|"tip (getblockchaininfo)"| FN
  HDEC -.->|"Ack (Nym SURB return)"| SHUB
  SHUB -.->|"failover (dedup by txid)"| HUB2
  SHUB -.->|"last resort near expiry: direct broadcast over Nym"| NYM
  HUB2 -.-> FN

  NOTE_OP -.- LWD
  NOTE_BATCH -.- HFLUSH

  classDef enclave fill:#1b7f4d,color:#fff,stroke:#0d5233;
  classDef untrusted fill:#c0392b,color:#fff,stroke:#7f261c;
  classDef external fill:#6b7280,color:#fff,stroke:#4b5563;
  classDef client fill:#2563eb,color:#fff,stroke:#1e40af;
  classDef note fill:#fef9c3,color:#000,stroke:#ca8a04,stroke-dasharray:4 3;
  class STLS,SROUTE,SCLASS,SPROXY,SHUB,HDEC,HVAL,HQ,HFLUSH,HUB2 enclave;
  class LWD,NYMC,NYMS untrusted;
  class NYM,FN,ZNET,OTHER external;
  class W client;
  class NOTE_OP,NOTE_BATCH note;
  style OP fill:#fbeae7,stroke:#c0392b;
  style HH fill:#fbeae7,stroke:#c0392b;
  style SHIM fill:#e7f4ee,stroke:#1b7f4d;
  style HUBENC fill:#e7f4ee,stroke:#1b7f4d;
  style NYMNET fill:#eef0f2,stroke:#6b7280;
  style NET fill:#eef0f2,stroke:#6b7280;
  style WAL fill:#e8eefc,stroke:#2563eb;
```

**Reading it:** thin arrows are the **pass-through path** (all queries and all
non-migration txs) which go to the operator's unmodified backing lwd as **plaintext the
operator can read**, exactly as today. Thick arrows are the **migration path**, which
is encrypted end to end and **never touches the backing lwd**. Green = attested enclave
processes (the only things that ever see migration cleartext); red = untrusted host
processes / sidecars (they see only ciphertext on the migration path); gray = external
networks; blue = the drop-in wallet.

**Three nested encryption layers on the migration (shim to hub) path**, so only the two
attested enclaves ever see cleartext:
1. **Inner:** the tx is encrypted to the **hub key** at the classifier (survives a
   compromised Nym client or host).
2. **Middle:** **STEVE** (AES-256-GCM) terminates inside the hub enclave.
3. **Outer:** **Nym** Sphinx across the 5-hop mixnet.

---

## 2. Trust, attestation, and verification plane

```mermaid
flowchart LR
  W["Light wallet"]
  subgraph OPH["Operator host (untrusted)"]
    SENC["shim enclave (attested)"]
  end
  subgraph HUBH["Hub host (untrusted)"]
    HENC["hub enclave (attested)"]
  end
  ATT["AWS Nitro NSM (hardware root of trust)"]
  STAGEX["StageX reproducible build (root hash)"]
  Q["keymaker M-of-N quorum (Caution / Nym / SL / ZF)"]
  AUD["Auditor (independent)"]
  CT["Certificate Transparency logs"]
  NOTE_TRUST["V2 privacy trusts AWS + the hardware, not math; PIR (V3) removes this trust root"]

  STAGEX -->|"software root hash"| ATT
  ATT -->|"binds enclave pubkey + PCRs"| SENC
  ATT -->|"binds enclave pubkey + PCRs"| HENC
  Q -->|"TLS key persistence (cross-boot + upgrade)"| SENC
  Q -->|"single shared hub key (all hubs)"| HENC
  SENC -->|"ACME cert (Let's Encrypt, CT-logged)"| CT
  AUD -->|"fetch /attestation, verify PCRs vs Nitro root"| SENC
  AUD -->|"verify hub attestation + PCRs"| HENC
  AUD -->|"check no shadow cert"| CT
  AUD -->|"reproduce build == attested hash"| STAGEX
  AUD -->|"passes assurance to users"| W
  NOTE_TRUST -.- ATT

  classDef enclave fill:#1b7f4d,color:#fff,stroke:#0d5233;
  classDef keyinfra fill:#7c3aed,color:#fff,stroke:#5b21b6;
  classDef actor fill:#d97706,color:#fff,stroke:#92400e;
  classDef external fill:#6b7280,color:#fff,stroke:#4b5563;
  classDef client fill:#2563eb,color:#fff,stroke:#1e40af;
  classDef note fill:#fef9c3,color:#000,stroke:#ca8a04,stroke-dasharray:4 3;
  class SENC,HENC enclave;
  class Q,ATT,STAGEX keyinfra;
  class AUD actor;
  class CT external;
  class W client;
  class NOTE_TRUST note;
  style OPH fill:#fbeae7,stroke:#c0392b;
  style HUBH fill:#fbeae7,stroke:#c0392b;
```

Each enclave generates its key in-enclave and binds the public key into the **Nitro
attestation** (root hash from the reproducible **StageX** build). The **keymaker M-of-N
quorum** persists keys across cold boots and upgrades, and hands the **single shared hub
key** to every hub instance (which is what makes failover clean). The **Auditor Role**
is any independent party: verify the attestation + PCRs against the AWS Nitro root, and
check **Certificate Transparency** that no shadow cert exists for the domain. The shim's
one-way **STEVE** handshake performs this same enclave-verification against the hub.

---

## 3. STEVE (corrected, from git.distrust.co/public/steve)

STEVE = "Secure Transport Encryption Via Enclave" (Distrust; in Caution). Used **only on
the shim to hub channel**, not on the wallet to shim path. It is **one-way**: the client
(shim) verifies the enclave (hub). Handshake: verify the hub attestation + PCRs vs the
Nitro root, extract the hub's Ed25519 key, do an **X25519 ECDH** with an ephemeral key,
verify the hub's **Ed25519 signature** over its ephemeral key, derive the session key via
**HKDF-SHA256**; payloads are **CBOR + AES-256-GCM**. It runs as a reverse proxy on
`:8080`. The Rust SDK is still in development (JS SDK ships today). The keymaker/locksmith
quorum is a **separate** Caution mechanism, not part of STEVE.

---

## 4. Honest caveats (built into the design)

- **The operator learns THAT a client migrated, not the amount.** A migration is the one
  `SendTransaction` the shim does not forward to the backing lwd, so the operator can
  infer by traffic analysis that a source IP submitted a migration and when. The
  cross-operator batch hides *which* on-chain tx / amount is that client's. This is
  inherent to the drop-in model, so shim-side batching and shim-to-hub cover traffic are
  rejected (they cannot hide the fact of a migration).
- **Anonymity set = the cross-operator batch.** The hub aggregates migrations from many
  operators' shims; the amount's anonymity is the batch size, which can be 1 at low
  volume. Hub-generated decoys cost real value and are a last resort, not the lever.
- **Delayed broadcast.** The shim returns "accepted" immediately, but the tx is not in
  any mempool until the hub flushes (up to ~25 min). Migrations are not time-sensitive,
  so this is acceptable, but the pending-state UX must be defined.
- **Retain until confirmed (hard requirement).** The hub is diskless, so a crash loses
  its in-RAM queue. Safety relies on the shim retaining each migration until it observes
  on-chain confirmation and re-submitting, plus hub failover.
- **The trust root is AWS + the hardware, not math.** V2 privacy is contingent on
  trusting the Nitro manufacturer and platform. PIR (V3) is the hardware-independent
  alternative that removes this root; TEE and PIR are complementary (distinct failure
  modes).

---

## 5. Open questions for Anton (Caution)

1. **Who terminates the wallet's TLS?** The design needs TLS to terminate **inside the
   shim enclave** (so the operator cannot read it). Does the enclave own `:443`, or does
   a platform **Caddy** terminate TLS in front of it? If Caddy terminates it, the
   operator sees plaintext migrations and the drop-in model fails for non-STEVE wallets.
   This is the load-bearing question.
2. **STEVE wire form over Nym:** a raw framed byte stream, or HTTP/gRPC? (Only affects the
   `SubmitMigration` transport, not its shape.)
3. **STEVE mutual vs one-way:** one-way is enough for privacy; mutual would additionally
   gate abuse by requiring shim attestation. Which?
4. **`nym-proxy-server` placement** on managed Caution: parent-side (untrusted) or must it
   run in-enclave?
5. **Zero-ingress + attestation delivery:** suppress the platform's public `/attestation`
   + Caddy for a true zero-inbound service, or deliver the attestation inline over Nym?
6. **Rust STEVE SDK timeline** (currently in development), vs us implementing the handshake
   directly from standard primitives, vs an RA-TLS fallback.
