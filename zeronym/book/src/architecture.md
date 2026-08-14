# Architecture

The [zero-indexer-shim and zero-indexer-hub](./components.md) system that stops **Orchard-touching transactions**, any transaction that carries Orchard actions (typically but not only the Orchard to Ironwood migration), from leaking a user's IP. See [introduction](./introduction.md) for the actors around it and [trust](./trust.md) for the trust chain. This chapter is the pieces you deploy and how they converse, then two diagrams: the data flow, then the trust / verification plane (kept separate for readability).

## The deployable pieces

Two new pieces of attested software plus a transport put an **attested, verifiable, tamper-proof front-end at every operator**, on which the [protections](./problem.md) rest and the whole [roadmap](./roadmap.md) builds. Five things run:

- **zero-indexer-shim (ZIS)**: a lightweight, attested router each operator deploys behind its existing public URL (for example `zec.rocks:443`). To every wallet it looks exactly like the indexer already there, so wallets need no reconfiguration. It forwards almost all traffic untouched to the operator's backing indexer, and isolates two things: transactions that **touch Orchard** (diverted to the hub) and `GetTransaction` (answered by the hub, so a wallet's lookup for its own migration never reaches the operator). Everything else (all other queries, and every broadcast that leaves Orchard untouched: transparent payments, intra-pool shielded payments, shields, deshields from other pools) passes straight through instantly; the backend still sees those contents, but arriving from the shim, not the wallet's IP. The shim is **stateless**, holding nothing about what it diverted, which is exactly why every `GetTransaction` must go to the hub.
- **zero-indexer-hub (ZIH)**: a central, attested service, designed to run as two or more instances with failover. It does two jobs. It **batches**: an Orchard-touching transaction is encrypted to a key the local operator cannot access, routed to a hub, batched with those from every other shim, and co-published on a strict block cadence after a short delay, so an observer holding "IP X connected at time T" cannot time-match it to the transaction when it appears on-chain. And it **answers lookups**: a `GetTransaction` is served from the hub's queue while the migration is unflushed (height 0, mempool), otherwise from the hub's own indexer.
- **Nym, embedded in both binaries** (built, not deployed): each side links `nym-sdk` and runs its own mixnet client **in-process**, inside the enclave, so there are no proxy sidecars and no untrusted process on the path. It runs only between shim and hub, never wallet-to-shim. The transport is proven end to end over a local mixnet but is not yet deployable in an attested enclave, so the deployed hop is still plain TLS to a pinned address ([roadmap](./roadmap.md) has the status and the two blockers).
- **The operator's backing indexer**: the unmodified lightwalletd or Zaino the operator already runs, on its internal address. To it the shim is a single ordinary gRPC client. It serves block sync, address queries, and pass-through broadcasts in cleartext, exactly as today; a diverted Orchard-touching transaction and a wallet's `GetTransaction` never reach it.
- **The hub's indexer**: a CompactTxStreamer (lightwalletd or Zaino), distinct from any operator's, that the hub connects out to over TLS to read the chain tip, publish each flushed batch, and answer a `GetTransaction` its queue does not hold. Neither enclave runs a validator of its own. (In a single-operator deployment the two indexer roles can collapse onto one instance, which removes the lookup privacy but not the batching.)

## Why this shape: Orchard-touching only, and Option B

Orchard first, and only Orchard, is deliberate. The Orchard to Ironwood migration is the acute, mandatory, mass event, and it is not time-sensitive, exactly when batching helps most (a large simultaneous population to hide among) and costs the least (no urgency to broadcast). But the batched class is drawn wider than "migration": **every** transaction that touches Orchard is batched, whatever its value balance or destination. That is Zooko's rule, and the reason is that NU6.3 closes Orchard to new value, so anyone still spending Orchard is spending legacy funds and the spend itself is the identifying event, whatever its destination ([the shim](./components.md) has the predicate and the argument). Shields and deshields from other pools still pass straight through: a shield is privacy-positive already, since the transparent side is public, and a deshield out of Ironwood or Sapling says nothing about legacy Orchard holdings.

Widening the class widens what is delayed, and the honest accounting is that it costs little. An Orchard deshield to transparent is now held for a flush window like a migration, and deshields are ordinarily time-sensitive commerce. But Orchard is closed to new value, so ordinary commerce lives in Ironwood and passes through untouched; what is left in Orchard is legacy balance, and moving legacy balance is not an urgent errand. The batched set stays a policy knob that can widen or narrow later without re-architecting.

The topology is the all-hands call's "Option B": a drop-in shim in front of each operator plus central batching hubs, chosen after a more decentralized "Option C" was set aside. ("Option A," a standalone privacy server users must point their wallets at, is deferred: past experience says getting wallets to change their endpoint URL is nearly impossible.) The scope is intentionally minimal:

- Nym only between shim and hub, not wallet-to-shim.
- Orchard-touching only (the classifier detects every turnstile crossing; only Orchard-touching transactions are batched).
- At least two attested hubs with shim failover, so a hub outage never stalls migrations (designed; whether shims prefer a primary or submit to every hub is open, see [review](./review.md)).

Everything else (the attested Nym fleet, the standalone privacy server, the query-only/broadcast-only split, PIR, and full consortium key governance) is post-launch. [Roadmap](./roadmap.md) covers the deferred items; [honest limits](./trust.md) states what this narrow scope does and does not buy.

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
      SCLASS["3 is_orchard_touching classifier"]
      SPROXY["4 pass-through proxy"]
      SHUB["5 hub-channel client"]
      SNYM["6 nym-sdk client (linked, in-process)"]
    end
    LWD["backing lwd (operator's, unmodified)"]
  end

  OTHER["shims at other operators"]

  subgraph NYMNET["Nym 5-hop mixnet  (untrusted)"]
    NYM["Nym mixnet (Sphinx + cover traffic)"]
  end

  subgraph HH["Hub host  (Caution, UNTRUSTED; 2+ with failover)"]
    subgraph HUBENC["zero-indexer-hub enclave  (attested TCB; no validator inside)"]
      HNYM["nym-sdk listener (linked, in-process)"]
      HDEC["decrypt (STEVE server, designed)"]
      HVAL["re-validate (stateless)"]
      HQ["batch queue (payload-hash dedup, in RAM)"]
      HFLUSH["batch + publish"]
    end
  end

  HUB2["standby hub enclave (shared key)"]

  subgraph NET["Hub's indexer / Zcash network"]
    FN["hub's indexer -> full node(s)"]
    ZNET["Zcash P2P network"]
  end

  NOTE_OP["Residual: the operator learns THAT a client migrated (it is the one request not forwarded to its lwd), not the amount"]
  NOTE_BATCH["Anonymity set = the cross-operator batch; a batch of 1 = no anonymity"]

  %% pass-through path (thin): queries + non-migration txs
  W -->|"TLS (ends in enclave, not STEVE)"| STLS
  STLS -->|"decrypted h2 (in TCB)"| SROUTE
  SROUTE -->|"other queries + streams"| SPROXY
  SROUTE -->|"SendTransaction"| SCLASS
  SROUTE ==>|"GetTransaction (hub-served)"| SHUB
  SCLASS -->|"non-migration"| SPROXY
  SPROXY -->|"queries except GetTransaction + non-migration txs (plaintext to operator)"| LWD
  LWD -->|"relay (clearnet)"| ZNET

  %% migration path (thick): encrypted end to end, bypasses the lwd
  SCLASS ==>|"migration (or fail-safe): encrypt to hub key"| SHUB
  SHUB -.->|"accepted (not yet on-chain)"| W
  SHUB ==>|"SubmitV1 frame (padded to 64 KiB)"| SNYM
  SNYM ==>|"Sphinx (anonymous send + reply SURBs)"| NYM
  OTHER ==> NYM
  NYM ==>|"5-hop (hides shim + region)"| HNYM
  HNYM ==>|"frame (host never sees it)"| HDEC
  HDEC ==> HVAL
  HVAL ==>|"valid, unexpired"| HQ
  HQ ==>|"flush every 20 blocks (~25 min)"| HFLUSH
  HFLUSH ==>|"SendTransaction (batched, shuffled)"| FN
  FN -->|"P2P relay"| ZNET
  HFLUSH -.->|"tip (GetLightdInfo) + lookup fallthrough"| FN
  HDEC -.->|"AckV1 (64 bytes, SURB return)"| SHUB
  SHUB -.->|"failover (dedup by payload hash)"| HUB2
  SHUB -.->|"last resort near expiry: direct broadcast over Nym"| NYM
  HUB2 -.-> FN

  NOTE_OP -.- LWD
  NOTE_BATCH -.- HFLUSH

  classDef enclave fill:#1b7f4d,color:#fff,stroke:#0d5233;
  classDef untrusted fill:#c0392b,color:#fff,stroke:#7f261c;
  classDef external fill:#6b7280,color:#fff,stroke:#4b5563;
  classDef client fill:#2563eb,color:#fff,stroke:#1e40af;
  classDef note fill:#fef9c3,color:#000,stroke:#ca8a04,stroke-dasharray:4 3;
  class STLS,SROUTE,SCLASS,SPROXY,SHUB,SNYM,HNYM,HDEC,HVAL,HQ,HFLUSH,HUB2 enclave;
  class LWD untrusted;
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

**Reading it:** *migration* in both diagrams is the code's label for the diverted class, which is every **Orchard-touching transaction**, any transaction that carries Orchard actions whatever its destination ([the shim](./components.md) has the predicate). Thin arrows = the **pass-through path** (queries other than `GetTransaction`, and non-migration txs), which go to the operator's unmodified backing indexer as **plaintext the operator can read**, exactly as today. Thick arrows = the paths that **bypass the operator**: the migration broadcast, encrypted end to end, and the hub-served `GetTransaction`. Green = attested enclave processes, the only things that ever see migration cleartext, and note that this now includes each side's mixnet client, which is linked in-process rather than run as a sidecar; red = the untrusted host and the operator's own indexer, which never sees the migration path at all; gray = external networks; blue = the drop-in wallet.

**Three nested encryption layers are designed for the migration (shim to hub) path**, so that only the two attested enclaves ever see cleartext. **The deployed hop today is a single layer, plain TLS**, terminated by the platform's in-enclave proxy.
1. **Inner** (designed): the tx is encrypted to the **hub key** at the classifier, so it survives a compromised host.
2. **Middle** (designed): **STEVE** (AES-256-GCM) terminates inside the hub enclave.
3. **Outer** (built, not deployed): **Nym** Sphinx across the 5-hop mixnet.

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

Each enclave generates its key in-enclave and binds the public key into the **Nitro attestation** (root hash from the reproducible **StageX** build). The **keymaker M-of-N quorum** persists keys across cold boots and upgrades, and hands the **single shared hub key** to every hub instance (what makes failover clean). The **Auditor Role** is any independent party: verify the attestation + PCRs against the AWS Nitro root, and check **Certificate Transparency** that no shadow cert exists for the domain. The shim's one-way **STEVE** handshake performs this same enclave-verification against the hub.

STEVE mechanics, the honest limits (operator learns THAT not the amount, cross-operator anonymity set, delayed broadcast, retain-until-confirmed, AWS-not-math trust root, hub failover), and the open questions for Anton (Caution) live in [trust](./trust.md) and [review](./review.md).
