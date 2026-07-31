# Glossary and references

Component and term definitions, then the primary references. Where another chapter owns the deep dive, the entry states the meaning and links there.

## Glossary

**Anchor.** A note-commitment-tree root a shielded transaction commits to, fixing which chain state it was built against. Wallets must choose *aligned* anchors within a migration epoch: a latest-anchor transaction is timestamped by its anchor, which re-links it in the revealed batch (the anchor-linkage attack, see [the problem](./problem.md)).

**Anonymity set.** For a batched migration, the cross-operator batch the [hub](./components.md) publishes together. A batch of one gives no anonymity. See [honest limits](./trust.md).

**Attestation (Nitro / NSM).** A signed AWS Nitro Secure Module document binding an enclave's in-enclave-generated public key to the root hash of the software inside it, verifiable against the AWS Nitro hardware root of trust. Both shim and hub publish one. See [trust](./trust.md).

**Auditor Role.** Any independent third party verifying that a public endpoint runs the attested software without trusting the operator, via its attestation plus a Certificate Transparency check for no shadow certificate. Steps in [trust](./trust.md).

**Backing lwd.** The operator's own unmodified light-wallet indexer (lightwalletd or Zaino) that the [shim](./components.md) fronts as a client. Queries and non-migration broadcasts pass through in cleartext as today; migrations never reach it.

**Batch / flush cadence.** The hub accumulates migrations (a batch) and publishes them together on a strict block cadence (a flush), targeting roughly every ten blocks, under Brave's tight twenty-block migration expiry. See [the hub](./components.md) and [honest limits](./trust.md).

**Certificate Transparency (CT).** Public append-only logs of issued TLS certificates. All Let's Encrypt certificates are CT-logged, letting the Auditor Role confirm no second, non-enclave certificate exists for a domain (no shadow cert to MITM clients). See [trust](./trust.md).

**CompactTxStreamer / SendTransaction.** The light-wallet gRPC service (`cash.z.wallet.sdk.rpc.CompactTxStreamer`) and the one method the shim decodes, `SendTransaction`, carrying a `RawTransaction`. Every other method and stream passes through opaquely. See [the shim](./components.md).

**Deshield.** A turnstile crossing moving value from a shielded pool to the transparent pool. Detected by the classifier but passed straight through near-term, not batched.

**Drop-in LWD.** The shim looks like an ordinary light-wallet indexer to every wallet, so users need no config change and no new endpoint URL (wallets do need aligned anchors and expiry within a migration epoch, see [the problem](./problem.md)). Why TLS must terminate inside the enclave and the shim must present a normal CA-issued certificate.

**Expiry height.** The block height past which a transaction is invalid and will not mine. The hub must flush a queued migration before its expiry height, capping the flush cadence (it cannot be widened to grow a batch). Per-wallet windows are aligned in ZIP 318.

**Fail-safe (classification).** The shim's rule that any `SendTransaction` body it cannot confidently read (unparseable bytes, a truncated or compressed gRPC frame, trailing bytes after the transaction) is treated as a migration, never passed through as ordinary traffic. A false negative is a privacy leak; a false positive is only a wasted diversion. See [the shim](./components.md).

**Interception superset rule.** The shim routes on the request path alone, and its interception set must be a superset of every routing predicate any supported backend uses. Being stricter than the backend fails *open*: the backend acts on a request the classifier never saw. See [the shim](./components.md).

**Key consortium.** Proposed multi-org governance of the enclave and hub keys: Caution, Nym, Shielded Labs, and the Zcash Foundation. Long-term trust-distribution goal; a single trusted entity (Caution) stands up the hub at launch, consortium to follow. See [trust](./trust.md).

**Keymaker / locksmith quorum.** Caution's M-of-N quorum mechanism (across three to four consortium orgs) that persists enclave keys across cold boots and upgrades and provisions the single shared hub key to every hub instance. Separate from STEVE. See [trust](./trust.md).

**Migration.** The turnstile crossing the near-term system batches: a cross-pool shielded move, the Orchard-to-Ironwood migration (value leaving Orchard and entering Ironwood in a V6 transaction). The acute, mass, non-time-sensitive event setting the deadline. See [the problem](./problem.md).

**Migration epoch.** The batching window over which wallets choose identical anchors and expiry heights and the hub reveals migrations together in shuffled order. Batches are time or block-height based, never transaction-count based (else an attacker floods its own migrations to isolate a target's). See [the problem](./problem.md) and [the hub](./components.md).

**Nym.** The 5-hop Sphinx mixnet with cover traffic, used near-term only between shim and hub. It makes shim-to-hub traffic unlinkable, hiding which operator or region a migration came from. See [the architecture](./architecture.md).

**nym-proxy-client / nym-proxy-server.** The Nym SDK TcpProxy sidecars: the client runs shim-side and tunnels to the server fronting the hub. Both move only ciphertext (the migration is already encrypted to the hub key), so they can be untrusted. Their exact placement on managed Caution is an [open question](./review.md).

**PCRs (Platform Configuration Registers).** The measurement values in a Nitro attestation that fix the enclave's software identity. An auditor or the shim's STEVE check verifies them against expected values and the AWS Nitro root before trusting an enclave. See [trust](./trust.md).

**PIR (Private Information Retrieval).** Cryptographic query privacy: a client retrieves a record without the server learning which. The hardware-independent, math-based trust root planned for long-term V3, complementary to the TEE (distinct failure modes). Candidate schemes: SimplePIR/DoublePIR, FrodoPIR, YPIR, ChalametPIR. See [the roadmap](./roadmap.md).

**RA-TLS.** Remote-attestation TLS, a fallback transport binding the enclave's attestation into the TLS handshake. An alternative to STEVE for the attested channel. See [open questions](./review.md).

**Shield.** A turnstile crossing moving value from the transparent pool into a shielded pool (including coinbase or mining-reward shielding). Detected but passed through near-term; privacy-positive already, since the transparent side is public.

**StageX.** The reproducible, deterministic build system (`SOURCE_DATE_EPOCH=1`, static-musl) used to build the shim and hub binaries, so an auditor can rebuild from source and match the software root hash bound into the attestation. See [trust](./trust.md).

**STEVE.** "Secure Transport Encryption Via Enclave," a Distrust protocol in Caution: a second encryption layer terminating inside the enclave, used only shim-to-hub, one-way (the client verifies the enclave). Handshake and primitives in [trust](./trust.md). Separate from the keymaker quorum.

**SubmitMigration.** The shim-to-hub request `SubmitMigration { ciphertext, txid, expiry_height }`, answered by `Ack { txid }`. The tx body is encrypted to the hub key; txid and expiry are in the clear to the hub for dedup and flush scheduling. See [the shim](./components.md) and [the hub](./components.md).

**TEE / AWS Nitro enclave.** Trusted Execution Environment. Both shim and hub run as attested, diskless AWS Nitro enclaves, making operator-blindness and hub-blindness cryptographically checkable rather than merely trusted. See [trust](./trust.md).

**Trusted Organization (TO).** The party that operates the hub and, for detection, verifies the shim's setup attestation, makes anonymous requests to confirm the attested key is served, monitors Certificate Transparency, and publicly announces detected attacks. The design is detection-based, not prevention. See [the problem](./problem.md).

**Turnstile crossing.** Any transaction moving value across a value-pool boundary: a deshield, a shield, or a cross-pool migration. The classifier detects every crossing; near-term the system batches only the migration case. See [the shim](./components.md).

**Value balance.** The signed net value leaving a shielded pool (positive when value leaves that pool). With transparent input/output presence, it is how the classifier distinguishes a deshield, shield, and migration from an ordinary fee-only shielded payment, without a magic threshold. See [the shim](./components.md).

**Zeronym.** The Shielded Labs privacy product for Zcash light wallets (name: zero + nym, a play on "pseudonym"). Two pillars: zero-leak indexing and the Nym mixnet. See [the introduction](./introduction.md).

**zero-indexer-hub (ZIH).** The attested-TEE batcher (run as two or more instances with failover, earlier `zero-broadcaster`) that accumulates migrations from many shims over Nym, decrypts them in-enclave, dedups by txid, and publishes them together on a strict block cadence. See [the hub](./components.md).

**zero-indexer-shim (ZIS).** The lightweight, attested-TEE reverse proxy an operator deploys behind their existing public URL. It passes all traffic through to the backing lwd, except it classifies every turnstile crossing and isolates migrations, encrypting each to the hub key and routing it over Nym. See [the shim](./components.md).

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

**PIR (private information retrieval), candidate schemes for the deferred query-privacy layer.**
- SimplePIR and DoublePIR (Henzinger et al.)
- FrodoPIR
- YPIR
- ChalametPIR
