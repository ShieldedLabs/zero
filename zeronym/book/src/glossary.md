# Glossary and references

Component and term definitions, then the primary references. Where another chapter owns the deep dive, the entry states the meaning and links there.

## Glossary

### Anchor

A note-commitment-tree root a shielded transaction commits to, fixing which chain state it was built against. Wallets must choose *aligned* anchors within a migration epoch: a latest-anchor transaction is timestamped by its anchor, which re-links it in the revealed batch (the anchor-linkage attack, see [the problem](./problem.md)).

### Anonymity set

For a batched migration, the cross-operator batch the [hub](./components.md) publishes together. A batch of one gives no anonymity. See [honest limits](./trust.md).

### Attestation (Nitro / NSM)

A signed AWS Nitro Secure Module document binding an enclave's in-enclave-generated public key to the root hash of the software inside it, verifiable against the AWS Nitro hardware root of trust. Both shim and hub publish one. See [trust](./trust.md).

### Auditor Role

Any independent third party verifying that a public endpoint runs the attested software without trusting the operator, via its attestation plus a Certificate Transparency check for no shadow certificate. Steps in [trust](./trust.md).

### Backing lwd

The operator's own unmodified light-wallet indexer (lightwalletd or Zaino) that the [shim](./components.md) fronts as a client. Block sync and pass-through queries reach it in cleartext as today; a diverted transaction and every `GetTransaction` do not. Those go to the hub's own separate indexer, which handles transaction detail and batched broadcast.

### Batch / flush cadence

The hub accumulates migrations (a batch) and publishes them together on a strict block cadence (a flush), every twenty blocks, about twenty-five minutes. [The hub](./components.md) has the budget that fixes the interval.

### Certificate Transparency (CT)

Public append-only logs of issued TLS certificates. All Let's Encrypt certificates are CT-logged, letting the Auditor Role confirm no second, non-enclave certificate exists for a domain (no shadow cert to MITM clients). See [trust](./trust.md).

### CompactTxStreamer / SendTransaction

The light-wallet gRPC service (`cash.z.wallet.sdk.rpc.CompactTxStreamer`). The shim decodes exactly two of its methods, `SendTransaction` and `GetTransaction`, and passes every other method and stream through opaquely. See [the shim](./components.md).

### Deshield

A turnstile crossing moving value from a shielded pool to the transparent pool. Batched near-term if it spends Orchard (it is then **Orchard-touching**), passed straight through otherwise.

### Drop-in LWD

The shim looks like an ordinary light-wallet indexer to every wallet, so users need no config change and no new endpoint URL. This is why TLS must terminate inside the enclave and the shim must present a normal CA-issued certificate.

### Expiry height

The block height past which a transaction is invalid and will not mine. The hub must flush a queued migration before its expiry height, capping the flush cadence (it cannot be widened to grow a batch). Per-wallet windows are aligned in ZIP 318.

### Fail-safe (classification)

The shim's rule that any `SendTransaction` body it cannot confidently read is routed to the diverted class, never passed through. A false negative is a privacy leak; a false positive is only a wasted diversion. [The shim](./components.md) enumerates the cases and their tests.

### Key consortium

Proposed multi-org governance of the enclave and hub keys: Caution, Nym, Shielded Labs, and the Zcash Foundation. Long-term trust-distribution goal; a single trusted entity (Caution) stands up the hub at launch, consortium to follow. See [trust](./trust.md).

### Keymaker / locksmith quorum

Caution's M-of-N quorum across the consortium orgs, which would persist enclave keys across cold boots and upgrades and provision the single shared hub key to every hub instance. Designed. Separate from STEVE, which is a per-session handshake rather than key custody. See [trust](./trust.md).

### Migration

The Orchard-to-Ironwood crossing that sets the deadline: the acute, mass, non-time-sensitive event. Also the legacy name the code (`Class::Migration`, `treat_as_migration()`) still gives to the whole batched class, which is wider than a literal migration (see **Orchard-touching transaction**). See [the problem](./problem.md).

### Migration epoch

The batching window over which wallets choose identical anchors and expiry heights and the hub reveals migrations together in shuffled order. Batches are time or block-height based, never transaction-count based (else an attacker floods its own migrations to isolate a target's). See [the problem](./problem.md) and [the hub](./components.md).

### Nym

The 5-hop Sphinx mixnet with cover traffic, the near-term transport for the shim-to-hub hop: it makes that traffic unlinkable, hiding which operator or region a migration came from. Both binaries link `nym-sdk` and run a mixnet client in-process (no proxy sidecars), proven end to end over a local mixnet, but not yet deployable in an attested enclave and never run on the public mixnet. See [the architecture](./architecture.md) and the status table in [roadmap](./roadmap.md).

### Orchard-touching transaction

Formerly "Orchard exit." Any transaction carrying Orchard actions, whatever its value balance or destination: the class the shim diverts and the hub batches. The value balance is evidence, not the test. Zooko's rule; the closed-pool argument is in [the shim](./components.md).

### PCRs (Platform Configuration Registers)

The measurement values in a Nitro attestation that fix the enclave's software identity. An auditor (or, in the design, the shim's STEVE check) verifies them against expected values and the AWS Nitro root before trusting an enclave. See [trust](./trust.md).

### PIR (Private Information Retrieval)

Cryptographic query privacy: a client retrieves a record without the server learning which. The hardware-independent, math-based trust root planned for long-term V3, complementary to the TEE (distinct failure modes). Candidate schemes: SimplePIR/DoublePIR, FrodoPIR, YPIR, ChalametPIR. See [the roadmap](./roadmap.md).

### RA-TLS

Remote-attestation TLS, a fallback transport binding the enclave's attestation into the TLS handshake. An alternative to STEVE for the attested channel. See [open questions](./review.md).

### Shield

A turnstile crossing moving value from the transparent pool into a shielded pool (including coinbase or mining-reward shielding). Passed through near-term unless the same transaction also spends Orchard; privacy-positive already, since the transparent side is public.

### StageX

The reproducible, deterministic build system (`SOURCE_DATE_EPOCH=1`, static-musl) used to build the shim and hub binaries, so an auditor can rebuild from source and match the software root hash bound into the attestation. See [trust](./trust.md).

### STEVE

"Secure Transport Encryption Via Enclave," a Distrust protocol in Caution: a second encryption layer terminating inside the enclave, used only shim-to-hub, one-way (the client verifies the enclave). Designed, not yet integrated: the shipped shim-to-hub hop is plain TLS with no separate encrypt-to-hub-key layer. Handshake and primitives in [trust](./trust.md). Separate from the keymaker quorum.

### SubmitV1 / AckV1

The shim-to-hub wire frames. `SubmitV1` is magic `ZNS1`, a 16-byte correlation nonce, and the transaction, zero-padded to exactly 64 KiB so every submission is the same size; `AckV1` is magic `ZNA1`, the echoed nonce, and a disposition, exactly 64 bytes. `LookupV1` / `LookupReplyV1` carry `GetTransaction`. **No txid and no expiry travel on the wire**: the hub derives both, because a txid would otherwise be a correlation handle. Over the clearnet transport the shim instead POSTs raw bytes. The encrypt-to-hub-key inner layer is designed, not built. See [the shim](./components.md).

### TEE / AWS Nitro enclave

Trusted Execution Environment. Both shim and hub run as attested, diskless AWS Nitro enclaves, making operator-blindness and hub-blindness checkable rather than merely trusted (the application layer reproduces today; the framework measurement does not yet, see [trust](./trust.md)).

### Trusted Organization (TO)

The party that operates the hub and, for detection, verifies the shim's setup attestation, makes anonymous requests to confirm the attested key is served, monitors Certificate Transparency, and publicly announces detected attacks. The design is detection-based, not prevention. See [the problem](./problem.md).

### Turnstile crossing

Any transaction moving value across a value-pool boundary: a deshield, a shield, or a cross-pool migration. The classifier detects every crossing; near-term the system batches every crossing that touches Orchard (see **Orchard-touching transaction**). See [the shim](./components.md).

### Value balance

The signed net value leaving a shielded pool (positive when value leaves that pool). It is **evidence, not the predicate**: the shim diverts on the mere presence of Orchard actions, so the Orchard, Ironwood and Sapling balances are all logged to show where value went while gating nothing. See [the shim](./components.md).

### Zeronym

The Shielded Labs privacy product for Zcash light wallets (name: zero + nym, a play on "pseudonym"). Two pillars: zero-leak indexing and the Nym mixnet. See [the introduction](./introduction.md).

### zero-indexer-hub (ZIH)

The attested-TEE batcher (earlier `zero-broadcaster`) that accumulates diverted transactions from many shims, holds them in-enclave, dedups identical bytes, and publishes them together on a strict block cadence through its own hub indexer over `CompactTxStreamer`. It also answers a wallet's `GetTransaction` for a queued or flushed migration. Running two or more instances with failover, and the encrypt-to-hub-key layer it decrypts in-enclave, are designed; on the deployed hop the payload arrives as raw bytes inside TLS, and over the mixnet build as fixed 64 KiB frames. See [the hub](./components.md).

### zero-indexer-shim (ZIS)

The lightweight, attested-TEE, stateless reverse proxy an operator deploys behind their existing public URL. It passes traffic through to the backing lwd, except that it classifies every turnstile crossing and diverts Orchard-touching transactions to the hub, and routes every `GetTransaction` to the hub as well (so a migration's follow-up lookup never reaches the operator). Encrypting each diverted transaction to the hub key is designed; the Nym route is built but not yet deployed, so the deployed hop is TLS. See [the shim](./components.md).

## References

**STEVE.**
- STEVE blog post: https://distrust.co/blog/steve.html
- STEVE source repository: https://git.distrust.co/public/steve

**Zcash light-client protocol and the leak.**
- ZIP 307, Light Client Protocol for Payment Detection: https://zips.z.cash/zip-0307
- ECC, Zcash reference wallet light-client protocol: https://electriccoin.co/blog/zcash-reference-wallet-light-client-protocol/
- ZecSec (Taylor Hornby), Making Zcash light wallets faster and more private: https://defuse.ca/zecsec/making-zcash-light-wallets-faster-and-more-private.htm

**Migration timing.**
- ZIP 318 (migration expiry alignment): https://zips.z.cash/zip-0318

**Nym.**
- Nym mixnet: https://nymtech.net

**Distrust and Caution.**
- Distrust (Caution platform, StageX, STEVE): https://distrust.co

**PIR (private information retrieval).** Candidate schemes for the deferred query-privacy layer are named in the [roadmap](./roadmap.md); they are a reading list to assemble, not yet a set of chosen citations.
