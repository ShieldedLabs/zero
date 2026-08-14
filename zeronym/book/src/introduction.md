# Introduction

Zeronym is privacy for Zcash light wallets, a Shielded Labs product named by Jason McGee Stramaglia: a play on "pseudonym," zero + nym. The name encodes two pillars:

- **zero**: zero-leak indexing. A light wallet should sync and transact without handing an indexer the raw material to deanonymize it.
- **nym**: the [Nym](./glossary.md#nym) mixnet, the transport that unlinks a wallet's traffic from its source IP and region.

Today a Zcash light wallet leaks. Under the ZIP 307 light-client protocol it talks to an indexer over clearnet, so the operator sees the wallet's source IP and the timing of everything it does. [The problem and threat model](./problem.md) lays out that leak and its worst instance, the Orchard to Ironwood migration, which Zooko called the worst privacy-loss event in Zcash history.

## Near-term and long-term

Two efforts share a name and a direction but not a scope.

**The near-term system** is urgent and narrowly scoped: stop transactions that touch the Orchard pool, starting with the Orchard to Ironwood migration, from leaking a user's IP. It is a small, attested system, deliberately an 80% first step, honest about the 20% it does not cover. The shim and the hub are built and run as attested enclaves, and a real Orchard to Ironwood migration has gone through the whole stack on mainnet: held, batched, and published on the flush cadence, with the operator's indexer never seeing it. The Nym hop is built into both binaries and proven over a local mixnet, but is not yet deployable in an attested enclave. STEVE and the key consortium remain design ([roadmap](./roadmap.md) has the status table).

**The long-term vision** is the fuller product the name promises: a wallet-facing private indexer serving queries, not just broadcasts, over Nym, terminated inside an attested enclave, with PIR added later as a hardware-independent layer. That arc is deferred until the migration fix ships. [Roadmap](./roadmap.md) traces the three versions (V1 Nym, V2 +TEE, V3 +PIR). Most of this book is the near-term system, because that is what is being built now.

## The system in its world

Seen from outside, Zeronym is one thing: an attested front-end that a wallet talks to exactly as it talks to an indexer today, and that publishes the transactions most at risk on someone else's schedule rather than the sender's. Who touches it, and what it touches:

```mermaid
flowchart TB
  USER["Wallet user (the IP at stake)"] -->|"runs"| WAL["Wallet software (zingo, Zashi, ywallet)"]
  WAL -->|"queries + broadcasts, same endpoint URL as today"| ZN["ZERONYM"]
  OPR["Light-wallet operator"] -->|"deploys the front-end; untrusted for Orchard-touching contents"| ZN
  TO["Hub operator / Trusted Organization"] -->|"operates the batcher; announces a detected attack"| ZN
  AUD["Auditor (any independent party)"] -->|"verifies attestation + CT, without trusting an operator"| ZN
  KC["Key consortium (Caution / Nym / SL / ZF)"] -->|"governs the long-lived keys"| ZN
  ZN -->|"publishes transactions, reads the chain tip"| ZEC["Zcash network"]
  ZN -->|"unlinkable internal transport"| NYM["Nym mixnet"]
  ZN -->|"attested execution, hardware root of trust"| NITRO["AWS Nitro"]
  ZN -->|"ordinary CA certificate, publicly logged"| CT["Let's Encrypt + Certificate Transparency"]
  ZN -->|"hosted on"| CAU["Caution's enclave platform"]

  classDef enclave fill:#1b7f4d,color:#fff,stroke:#0d5233;
  classDef client fill:#2563eb,color:#fff,stroke:#1e40af;
  classDef actor fill:#d97706,color:#fff,stroke:#92400e;
  classDef keyinfra fill:#7c3aed,color:#fff,stroke:#5b21b6;
  classDef external fill:#6b7280,color:#fff,stroke:#4b5563;
  class ZN enclave;
  class USER,WAL client;
  class OPR,TO,AUD actor;
  class KC keyinfra;
  class ZEC,NYM,NITRO,CT,CAU external;
```

## The cast

| Actor | Role | Trusted for |
|---|---|---|
| **Wallet user** | The person whose IP is at stake. Installs nothing, changes no setting | n/a |
| **Wallet software** | zingo, Zashi, ywallet and the rest. No reconfiguration, but must choose **aligned anchors and expiry heights** within a migration epoch ([ZIP 318](https://zips.z.cash/zip-0318)). The one hard requirement asked of wallets, and [the problem](./problem.md) explains why a latest-anchor wallet re-links itself | n/a |
| **Light-wallet operator** | One of roughly five to ten organizations running a public indexer. Deploys the front-end behind its own URL, in front of its own unmodified indexer | **Untrusted** for Orchard-touching contents. Still sees ordinary query contents |
| **Hub operator / Trusted Organization** | Runs the central batching service (Caution at launch) and performs **detection**: verifying attestation, monitoring CT, and **publicly announcing** signs of an attack | Untrusted for contents; relied on for liveness and detection |
| **Auditor** | Any independent party, in practice often a wallet developer auditing once for all its users ([trust](./trust.md) has the steps) | Verifies **without trusting the operator** |
| **Key consortium** | Caution, Nym, Shielded Labs, the Zcash Foundation | Hold the long-lived keys as an M-of-N quorum, so no single party controls them |
