# Glossary and references

Definitions of the components and terms used across this book, followed by the primary references. Each entry is deliberately short: where another chapter owns the deep dive, this glossary states what the term means and links there.

## Glossary

**Anonymity set.** For a batched migration, the set of transactions it hides among: the cross-operator batch the [hub](./hub.md) publishes together. A batch of one gives no anonymity. See [honest limits](./limits.md).

**Attestation (Nitro / NSM).** A signed document from the AWS Nitro Secure Module (NSM) that binds an enclave's in-enclave-generated public key to the root hash of the software running inside it, verifiable against the AWS Nitro hardware root of trust. Both the shim and the hub publish one. See [trust](./trust.md).

**Auditor Role.** Any independent third party who verifies that a public endpoint really runs the attested software, without trusting the operator: fetch the TLS certificate, load the enclave's attestation (a COSE_Sign1 document proving the key was enclave-born and carrying the software root hash), and use Certificate Transparency to confirm no shadow certificate exists for the domain. See [trust](./trust.md).

**Backing lwd.** The operator's own, unmodified light-wallet indexer (lightwalletd or Zaino), which the [shim](./shim.md) sits in front of as a client. All queries and all non-migration broadcasts pass through to it in cleartext, exactly as today; migrations never reach it.

**Batch / flush cadence.** The hub accumulates migrations and publishes them together (a batch) on a strict block cadence (a flush), targeting roughly every ten blocks, well under Brave's tight twenty-block migration expiry. See [the hub](./hub.md) and [honest limits](./limits.md).

**Certificate Transparency (CT).** The public, append-only logs of issued TLS certificates. All Let's Encrypt certificates are CT-logged, which is what lets the Auditor Role confirm no second, non-enclave certificate exists for an operator's domain (no shadow cert to MITM clients with). See [trust](./trust.md).

**CompactTxStreamer / SendTransaction.** The light-wallet gRPC service (`cash.z.wallet.sdk.rpc.CompactTxStreamer`) and the one method the shim decodes, `SendTransaction`, which carries a `RawTransaction`. Every other method and stream passes through opaquely. See [the shim](./shim.md).

**Deshield.** A turnstile crossing that moves value from a shielded pool to the transparent pool. Detected by the shim's classifier but passed straight through near-term, not batched.

**Drop-in LWD.** The property that the shim looks like an ordinary light-wallet indexer to every wallet, so users and wallets need no configuration change (no new endpoint URL). This is why TLS must terminate inside the enclave and why the shim must present a normal CA-issued certificate.

**Expiry height.** The block height past which a transaction is invalid and will not mine. The hub must flush a queued migration before its expiry height, which is why the flush cadence is capped and cannot be widened to grow a batch. Per-wallet windows are aligned in ZIP 318.

**Key consortium.** The proposed multi-org governance of the enclave and hub keys: Caution, Nym, Shielded Labs, and the Zcash Foundation. Long-term trust-distribution goal; a single trusted entity (Caution) stands up the hub at launch, with the consortium to follow. See [trust](./trust.md).

**Keymaker / locksmith quorum.** Caution's M-of-N quorum mechanism (across three to four consortium orgs) that persists enclave keys across cold boots and upgrades and provisions the single shared hub key to every hub instance. It is separate from STEVE. See [trust](./trust.md).

**Migration.** The specific turnstile crossing the near-term system batches: a cross-pool shielded move, the Orchard-to-Ironwood migration (value leaving Orchard and entering Ironwood in a V6 transaction). The acute, mass, non-time-sensitive event that sets the deadline. See [the problem](./problem.md).

**Nym.** The 5-hop Sphinx mixnet with cover traffic, used near-term only between the shim and the hub. It makes shim-to-hub traffic unlinkable, hiding which operator or region a migration came from. See [the architecture](./architecture.md).

**nym-proxy-client / nym-proxy-server.** The Nym SDK TcpProxy sidecars: the client runs on the shim side and opens a tunnel to the server fronting the hub. Both move only ciphertext (the migration is already encrypted to the hub key), so they can be untrusted. Their exact placement on managed Caution is an [open question](./open-questions.md).

**PCRs (Platform Configuration Registers).** The measurement values in a Nitro attestation that fix the enclave's software identity. An auditor or the shim's STEVE check verifies the PCRs against the expected values and the AWS Nitro root before trusting an enclave. See [trust](./trust.md).

**PIR (Private Information Retrieval).** Cryptographic query privacy: a client retrieves a record without the server learning which record. The hardware-independent, math-based trust root planned for the long-term V3, complementary to the TEE (distinct failure modes). See [the roadmap](./roadmap.md). Candidate schemes: SimplePIR/DoublePIR, FrodoPIR, YPIR, ChalametPIR.

**RA-TLS.** Remote-attestation TLS, a fallback transport in which the enclave's attestation is bound into the TLS handshake. Listed as an alternative to STEVE for the attested channel. See [open questions](./open-questions.md).

**Shield.** A turnstile crossing that moves value from the transparent pool into a shielded pool (including coinbase or mining-reward shielding). Detected but passed through near-term; privacy-positive already, since the transparent side is public.

**StageX.** The reproducible, deterministic build system (`SOURCE_DATE_EPOCH=1`, static-musl) used to build the shim and hub binaries, so an auditor can rebuild from source and match the software root hash bound into the attestation. See [trust](./trust.md).

**STEVE.** "Secure Transport Encryption Via Enclave," a Distrust protocol integrated into Caution: a second encryption layer that terminates inside the enclave. Used only on the shim-to-hub channel. One-way by default (the client verifies the enclave), it verifies attestation and PCRs against the Nitro root, does an X25519 ECDH, verifies an Ed25519 signature, derives a session key via HKDF-SHA256, and encrypts CBOR payloads with AES-256-GCM. Separate from the keymaker quorum. See [trust](./trust.md).

**SubmitMigration.** The shim-to-hub request `SubmitMigration { ciphertext, txid, expiry_height }`, answered by `Ack { txid }`. The tx body is encrypted to the hub key; txid and expiry are in the clear to the hub for dedup and flush scheduling. See [the shim](./shim.md) and [the hub](./hub.md).

**TEE / AWS Nitro enclave.** Trusted Execution Environment. Both the shim and the hub run as attested, diskless AWS Nitro enclaves, which is what makes operator-blindness and hub-blindness real and cryptographically checkable rather than merely trusted. See [trust](./trust.md).

**Turnstile crossing.** Any transaction that moves value across a value-pool boundary: a deshield, a shield, or a cross-pool migration. The shim's classifier detects every crossing; near-term the system batches only the migration case. See [the shim](./shim.md).

**Value balance.** The signed net value leaving a shielded pool in a transaction (positive when value leaves that pool). Combined with transparent input/output presence, it is how the classifier distinguishes a deshield, a shield, and a migration from an ordinary fee-only shielded payment, without a magic threshold. See [the shim](./shim.md).

**Zeronym.** The Shielded Labs privacy product for Zcash light wallets (name: zero + nym, a play on "pseudonym"). Two pillars: zero-leak indexing and the Nym mixnet. See [the introduction](./introduction.md).

**zero-indexer-hub (ZIH).** The attested-TEE batcher (run as two or more instances with failover, earlier called `zero-broadcaster`) that accumulates migrations from many shims over Nym, decrypts them in-enclave, dedups by txid, and publishes them together on a strict block cadence. See [the hub](./hub.md).

**zero-indexer-shim (ZIS).** The lightweight, attested-TEE reverse proxy an operator deploys behind their existing public URL. It passes all traffic through to the backing lwd, except it classifies every turnstile crossing and isolates migrations, encrypting each to the hub key and routing it over Nym. See [the shim](./shim.md).

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
