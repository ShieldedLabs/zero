# zero-indexer

Privacy for Zcash light wallets: an attested front-end that stops pool-crossing transactions from leaking your IP.

Under the [ZIP 307](https://zips.z.cash/zip-0307) light-client protocol a wallet talks to an indexer over clearnet, so the operator sees its source IP and the timing of everything it does. `zero-indexer` is a Shielded Labs product that stops transactions that contain Orchard actions from leaking a user's IP.

**The long-term vision** is a wallet-facing private indexer serving queries, not just broadcasts, over Nym, terminated inside an attested enclave, with PIR added later as a hardware-independent layer.

## Table of Contents

- [Security](#security)
- [Background](#background)
- [Install](#install)
- [Usage](#usage)
- [How it works](#how-it-works)
- [Status](#status)
- [Maintainers](#maintainers)
- [Thanks](#thanks)
- [Contributing](#contributing)
- [License](#license)

## Security

**Protected.** An Orchard-touching broadcast is hidden from the operator, because the wallet's TLS terminates inside an attested enclave, and the on-chain transaction carries no link to the wallet's IP, because the hub publishes it. That second property is volume-independent.

**Not protected.**

- **Query content.** Which addresses a wallet looks up still reaches the operator. Only `GetTransaction` is hub-served.
- **The operator learns *that* a client migrated**, though not the amount or which transaction. A diverted request is the one thing it does not see, and that asymmetry survives padding, so shim-side batching and cover traffic were rejected rather than attempted.
- **Batch-timing anonymity is conditional, and the condition is not met.** Mainnet carries **0.77 Orchard-touching transactions per block** (144 blocks at tip 3,433,105), so the modal batch is zero or one, where the anonymity set is the transaction itself. The lever is adoption, not code.
- **The trust root is AWS and the hardware, not mathematics.** No mathematical fallback today; PIR removes it and is deferred.
- **The broadcast is delayed** up to ~25 minutes, and submit is dispatch-only, so a wallet is told "accepted" before the shim knows the hub received the frame. A failed migration can fail silently.

**Not yet verifiable.** The enclaves are attested and running, but an auditor cannot yet tie either back to a public commit. PCR2 (the application binary) reproduces; PCR0 and PCR1 do not, so `caution verify` reports FAILED on healthy enclaves. The reproduce jobs run on pull requests and manual dispatch, **not on every push**; they last ran 2026-08-17 on `e91170ed` and both reported DOES NOT REPRODUCE, with re-baselines landed since and no run against them. The live pair's provenance also fails: the shim's cites a non-public commit, the hub's a hash its own cited commit does not produce.

**Scope.** zero-indexer targets the server-side and network-metadata adversaries in Taylor Hornby's [wallet app threat model](https://zcash.readthedocs.io/en/latest/rtd_pages/wallet_threat_model.html), not the wallet-local concerns that model assigns to the wallet.

Report security issues to Shielded Labs privately.

## Background

A light wallet delegates chain validation to an indexer and speaks to it over [ZIP 307](https://zips.z.cash/zip-0307). The protocol gets note privacy right and metadata privacy wrong: trial decryption is client-side, so the indexer never learns which notes are yours, but it terminates your connection, so it sees your source IP, the timing of every request, and the addresses you query.

![Current Zcash transaction publication: wallets connect over TLS directly to one of a handful of indexers, each of which publishes to the mempool](./images/zcash_current_publication.svg)

The TLS in that picture is doing less than it looks. It protects the transaction from everyone except the party that matters, because it terminates *at* the indexer, which is exactly the party positioned to log the source IP and join it to the public chain.

That join is the attack. A transaction crossing a value-pool boundary reveals the movement in cleartext on-chain, so an operator holds two halves of a linkage: *source IP X submitted a broadcast at time T*, and *a pool-crossing transaction moving amount Y appeared at roughly time T*. Joining them links **IP address to on-chain transaction to balance**. Nothing in the shielded cryptography prevents it, because the leak is in the transport, not the transaction. It is a retrospective attack as much as a live one, since the chain is permanent and logs can be joined later.

The acute case is the mandatory Orchard to Ironwood migration, which Zooko called the worst privacy-loss event in Zcash history. It is **mandatory** (users cannot opt out, the value has to move), **mass** (a large population migrates), and **concentrated** (the window is bounded, so many correlatable broadcasts land close together). Those same three properties are what make migrations the ideal thing to protect first: a mandatory, non-urgent mass of transactions can be batched and published together.

## Install

For **indexer operators**, who are the people who deploy this. The shim is a drop-in: it sits behind your existing public URL, in front of your existing unmodified indexer, and wallets need no reconfiguration.

The operator runbook is [`shim/deploy/caution/OPERATORS.md`](./shim/deploy/caution/OPERATORS.md), which covers prerequisites, deploy, verify and the config reference end to end. A third-party operator has run it start to finish.

## Usage

Four audiences, four different answers.

- **Wallet users** install nothing and change no setting. Point your wallet at the same endpoint URL as before.
- **Wallet developers** have exactly one requirement, and it is a hard one: choose **aligned anchors and expiry heights** within a migration epoch, the [ZIP 318](https://zips.z.cash/zip-0318) behavior. A latest-anchor wallet is timestamped by its anchor, which re-links it inside the revealed batch and undoes the protection.
- **Operators** run the shim in front of their indexer, and optionally a hub. Orchard-touching transactions and `GetTransaction` lookups stop being yours to see; everything else passes through as today.
- **Auditors** verify an endpoint without trusting its operator: fetch its attestation, check the PCRs against the AWS Nitro root, reproduce the build and compare hashes, and check Certificate Transparency for a shadow certificate. Read [Security](#security) first for what that currently does and does not establish.

## How it works

![Zero-indexer transaction publication: wallets connect over TLS to a zero-indexer-shim inside a TEE at each organisation, the shims route Orchard-touching transactions over the Nym mixnet to a zero-indexer-hub inside its own TEE, and the hub publishes to the mempool](./images/zcash_zero_indexer_publication.svg)

Green boxes are attested enclaves, the only things that ever see a migration in cleartext. Each shim links its mixnet client **in-process**, so there is no separate Nym node inside the TEE to draw: the shim itself emits Sphinx traffic. The mixnet is outside both enclaves because the mix nodes are untrusted. Wallets never speak Nym.

The grey edge from each shim to its own indexer is the **pass-through path**: ordinary queries and non-Orchard broadcasts, plaintext the operator reads exactly as today. The green edge is the **diverted path** that bypasses the operator entirely. Both organisations' migrations enter the mixnet and one edge leaves it, which is the anonymity property: the hub cannot tell which shim a migration came from.

Two components:

- **`zero-indexer-shim`** is a lightweight attested router each operator deploys behind its existing URL. It classifies every `SendTransaction`, and any transaction carrying **Orchard actions** goes to the hub instead of the operator. The predicate is presence, not value: `is_orchard_touching(tx) := tx.orchard_shielded_data().is_some()`. NU6.3 closes Orchard to new value, so anyone still spending Orchard has held those notes since before activation, and the spend itself is the identifying event whatever its destination. Classification happens *before* the upstream dial, so a diverted transaction never opens even a TCP connection to the operator's indexer. Anything the shim cannot confidently parse fails safe toward diversion: a false negative is a privacy leak, a false positive is only a wasted diversion. The shim holds no per-migration state.
- **`zero-indexer-hub`** accumulates diverted transactions from every shim in an in-RAM queue inside its own enclave and publishes them together on a strict 20-block cadence, shuffled and in parallel, so an on-chain observer sees them appear together and unordered. Flushes fire on block height only, never on transaction count, since a count-based trigger lets an attacker isolate a target by flooding. It admits a migration only if it provably survives the next scheduled flush, which makes urgency unreachable rather than handling it.

## Status

**Deployed:** classify and divert; the stateless shim; hub queue, batch and flush; `GetTransaction` served by the hub; attested Nitro enclaves (since 2026-08-01, first third-party operator 2026-08-10); in-enclave TLS termination; and the **Nym transport**, running on the public mixnet since 2026-08-14, with the hub publishing its address at `GET /nym-address`.

**Partly built:** multi-hub failover. The shim rotates which hub address each submit targets; holding a migration across requests does not exist.

**Designed, no code yet:** the STEVE handshake, the encrypt-to-hub-key layer, the keymaker quorum and consortium governance, and confirmation tracking.

On **2026-08-11** a real Orchard to Ironwood migration traversed the full stack on mainnet: held at the shim, batched at the hub, published on the cadence, with the operator's indexer never seeing it. That run predates the mixnet deployment and used the clearnet hop, and at today's adoption the batch was size one, so it proved the mechanics and content privacy rather than batching anonymity. No migration has yet been observed crossing Nym in production.

The reproducibility gaps are in [Security](#security), and they are the near-term priority.

## Maintainers

[Shielded Labs](https://shieldedlabs.net), in [`ShieldedLabs/zero`](https://github.com/ShieldedLabs/zero).

## Thanks

Diagrams by [@zooko](https://github.com/zookoatshieldedlabs/zero-indexer-diagrams), regenerated here with the Nym hop. Thanks to Caution and to Nym.

## Contributing

Issues and pull requests go to [`ShieldedLabs/zero`](https://github.com/ShieldedLabs/zero). Cross-party questions still open with Caution are tracked in [`OPEN-QUESTIONS.md`](./OPEN-QUESTIONS.md).

Review of the architecture itself is the contribution most wanted right now. [Security](#security) lists what to attack first.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](./LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

This covers `zeronym/`. The vendored upstreams elsewhere in this repository (`zebra/`, `zaino/`, `zcashd/` and the rest) keep their own licenses.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
