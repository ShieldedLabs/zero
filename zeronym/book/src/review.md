# Open questions and review

The [shim and hub](./components.md) designs are settled enough to build. What remains are cross-party items to close: platform unknowns for Caution (Anton), and a security-model checklist for Taylor and Zooko. Where the [architecture](./architecture.md) or [trust](./trust.md) chapters own a mechanism, this chapter states the question and cross-links rather than restating it.

**The upstream gate.** All of this sits behind one gate: the threat-model doc (Taylor and Zooko) is the upstream dependency, and the build is held until it lands and is signed off as safe to run. These questions, closed with Anton and reviewed by Taylor and Zooko, are what that review needs before code lands.

## Open questions for Caution (Anton)

Most of these concern the boundary between our software and Caution's managed enclave platform: where TLS terminates, what STEVE carries and how it authenticates, where the Nym sidecars run, and how a service can be both attested and zero-ingress. A few block code directly; others only need Caution to confirm a default we have already chosen.

### 1. TLS termination: who owns :443?

**The load-bearing question.** The entire drop-in model depends on the wallet's TLS terminating **inside the shim enclave**, so neither the operator host nor the Caution platform can read the migration in cleartext. The shim generates its TLS key in-enclave and runs an ACME client to obtain a normal CA-issued cert keyed to that enclave-born key, so a naive wallet gets an ordinary valid certificate while the private key never leaves the enclave (see [components](./components.md)).

The risk is a managed platform that puts a **Caddy** (or any reverse proxy) in front of the enclave to terminate TLS. If it does, that proxy sees the plaintext of every request, including migrations, before it reaches attested code. That breaks operator-blindness for exactly the wallets that matter most: the majority speak plain TLS and are not STEVE- or Nym-aware, so hub batching is their only protection and the shim being a TEE is what keeps the operator blind to contents (see [the problem](./problem.md)).

**The question for Anton:** does the enclave own `:443` directly, or does a platform Caddy terminate TLS in front of it? If Caddy terminates, we need a supported path to move termination into the enclave, or the drop-in model fails for non-STEVE wallets. Everything downstream (the three encryption layers in [architecture](./architecture.md)) assumes the answer is "the enclave."

### 2. STEVE wire form over Nym

[STEVE](./trust.md) is used only on the shim-to-hub channel. The `SubmitMigration { ciphertext, txid, expiry_height }` request and `Ack { txid }` reply are a fixed shape (see [components](./components.md)). Open is the **transport** that carries that shape over the Nym TCP tunnel:

- **gRPC / HTTP/2 over the tunnel** (RA-TLS or STEVE terminating an h2 session), reusing familiar framing.
- **A raw framed byte stream** we frame ourselves over the Nym TCP tunnel, with the STEVE-derived session key applied to our own records.

This blocks the exact `SubmitMigration` transport but not its message shape, so the rest of the shim and hub can be built against the settled shape while it resolves.

**The question for Anton:** what does a STEVE session carry over Nym, gRPC/h2 or a raw byte stream we frame ourselves?

### 3. STEVE: mutual or one-way?

STEVE is **one-way** by default: the shim verifies the enclave (the hub), extracts its key, and derives a session key ([trust](./trust.md)). One-way is enough for privacy: the shim confirms it is talking to the genuine attested hub before handing over any migration, which is all operator- and hub-blindness require. **Mutual** STEVE would additionally have the hub verify the shim's attestation, gating abuse (only attested shims could submit, rather than the hub accepting from anyone with rate-limiting). The trade-off is real: mutual raises the abuse bar but couples every shim to attestation provisioning and complicates onboarding a new operator. One-way plus per-channel rate-limiting and the hub's own re-validation of every incoming tx (see [components](./components.md)) keeps the submit path open and simple, and already bounds garbage regardless of choice.

**The question for Anton:** should the hub also verify the shim (mutual STEVE, to gate abuse), or accept from anyone with rate-limiting (one-way)?

### 4. nym-proxy-server placement on managed Caution

The hub's inbound side is fronted by `nym-proxy-server`; the shim's outbound side uses `nym-proxy-client`. Because the migration is already encrypted to the hub key before it reaches the Nym client (the inner encryption layer in [architecture](./architecture.md)), these sidecars only ever move ciphertext, so on confidentiality grounds they can run **parent-side, untrusted**. The open item is a platform constraint, not a security one: on managed Caution we do not control the parent, so whether a parent-side sidecar is permitted next to a managed enclave is Caution's call. This mirrors on the shim side, where `nym-proxy-client` runs in-enclave on managed Caution but parent-side on bring-your-own-cloud.

**The question for Anton:** can `nym-proxy-server` (and the shim's `nym-proxy-client`) run parent-side alongside a managed enclave, or must the Nym proxy run in-enclave?

### 5. Zero-ingress and attestation delivery

Today the `/attestation` endpoint is public and platform-served on both shim and hub. That is what the [Auditor Role](./trust.md) fetches over HTTPS to verify an endpoint. But a hub ideally wants to be **zero-ingress**: nothing listening on the public internet, reachable only over Nym, presenting no public attack surface and leaking nothing about its location. Those goals collide: a true zero-inbound service cannot also serve a public `/attestation` behind a platform Caddy. So either the platform suppresses its public `/attestation` and Caddy, or the attestation is **delivered inline over Nym** (as part of, or alongside, the STEVE handshake). If it moves to Nym, the auditor and the shim's own STEVE check need that alternate delivery path defined.

**The question for Anton:** can the platform suppress the public `/attestation` and Caddy for a true zero-inbound service, and if so, what is the supported way to deliver the attestation (inline over Nym) to the shim and to independent auditors?

### 6. The Rust STEVE SDK timeline

STEVE's JS SDK ships today; the **Rust SDK is still in development** (see [trust](./trust.md)). Both endpoints we are building need the handshake in Rust: the shim's hub-channel client and the hub's STEVE server. Three paths:

- **Wait for the Rust SDK** and integrate it directly.
- **Implement the handshake ourselves** from the standard primitives documented in [trust](./trust.md).
- **An RA-TLS fallback** for the attested channel if STEVE itself is not ready in time.

**The question for Anton:** what is the Rust STEVE SDK timeline, and do you recommend we wait for it, implement the handshake directly now, or use an RA-TLS fallback?

### 7. Keymaker quorum walkthrough

The single shared hub key that all shims encrypt to, and that makes hub failover clean, is persisted and reconstituted by the **keymaker / locksmith M-of-N quorum** across the consortium orgs (Caution, Nym, Shielded Labs, ZF). The mechanism, and its separation from STEVE, is documented in [trust](./trust.md). The open item is operational, not conceptual: a concrete walkthrough of how the single shared key is **provisioned to multiple attested hub instances** (so any hub can decrypt for failover) and **reconstituted across cold boots and upgrades** on the managed platform, which beats KMS-seal-to-PCR precisely because it survives an upgrade.

**The question for Anton:** walk us through provisioning the shared hub key to N attested instances and reconstituting it across boots and upgrades via the quorum.

### Companion questions for Zooko and Nate

Design confirmations rather than platform unknowns; they live with the component that owns them, so they are cross-linked, not restated:

- **Publish path:** `sendrawtransaction` to two or more external nodes versus direct Zcash P2P `tx` to many peers, and clearnet versus over Nym for the hub's own egress (see [components](./components.md)).
- **Batch density versus failover:** confirm primary-hub preference (converge for density, fail over only on outage) over spreading shims across hubs (see [trust](./trust.md)).
- **Hub re-validation:** confirm the hub should re-parse and re-classify each tx, not trust the shim, and reject non-migrations before batching (see [components](./components.md)).
- **Flush cadence and safety margin:** confirm the roughly ten-block flush and the safety margin against real wallet expiry windows aligned in ZIP 318 (see [trust](./trust.md) and [glossary](./glossary.md)).
- **JSON-RPC front-ends:** the shim intercepts only gRPC `SendTransaction`, so a migration submitted through a JSON-RPC `sendrawtransaction` front-end would bypass classification and leak. Wallets use gRPC, so this is out of near-term scope; confirm no operator front-ends migrations via JSON-RPC (see [components](./components.md)).

### Hosting and funding

A practical open item, separate from the technical unknowns: who hosts and funds the hubs at launch. Caution may cover a demo window; Shielded Labs may subsidize operators or run a donation drive; Nym (or Nym's Coastline) may run the hub component. Launch logistics, not a design blocker.

## For review by Taylor and Zooko

The security-relevant claims and assumptions that need expert review before we rely on them. A living checklist, not a finished argument, aimed at the **security model** (platform questions above are Caution's).

### The attested edge

- **Verifiable no-IP-logging (protection 2).** We claim the operator's indexer is blinded to requester IPs by default: the shim proxies, so queries reach the backing lwd from the shim, not the wallet, and attested no-logging makes that checkable. Is the framing ("removes the passive, default leak, not a guarantee against an active operator") fair and correctly bounded? See [introduction](./introduction.md) and [the problem](./problem.md).
- **The network-layer residual.** On Nitro the parent host still sees the wallet's source IP at the TCP layer, so a bad-faith operator can packet-capture and timing-correlate to re-link IP to query. Is that the correct and complete residual, or are there other cross-layer re-linking paths we are missing?
- **Tamper-proof front-end.** We claim attestation plus Certificate Transparency lets a wallet or auditor verify it is talking to the real attested shim, not an impostor or a modified front-end. Does the CT check fully close cert substitution for the drop-in URL?

### The migration path

- **The honest residual.** The operator learns *that* a client migrated (the one request not forwarded to its lwd), not the amount. Is that the complete residual, and are shim-side batching and shim-to-hub cover traffic correctly rejected as mitigations?
- **Anonymity set = the cross-operator batch.** At low migration volume a batch can be size 1 (no anonymity). Is hub-generated cover traffic the right (and only) backstop, and what batch size is "enough"? See [trust](./trust.md).
- **The classifier.** `is_migration` is a V6 transaction with value leaving Orchard and entering Ironwood. A false negative broadcasts a migration in the clear (a privacy failure), so the fee-aware turnstile predicate has to be correct and complete. Is it? Is fail-safe-for-privacy (treat an unparseable `SendTransaction` as a migration) sound? See [components](./components.md).
- **Expiry.** Flushing under about ten blocks against the wallet expiry windows (Brave 20, Zingo 100, librustzcash +40; ZIP 318). Is the safety margin sound against real mining latency?

### Trust and transport

- **STEVE.** One-way (the shim verifies the hub), X25519 ECDH plus an Ed25519 signature, HKDF-SHA256, CBOR and AES-256-GCM. Is one-way sufficient, or is mutual attestation needed to gate abuse at the hub? Is the corrected STEVE understanding right? See [trust](./trust.md).
- **The trust root.** V2 privacy trusts AWS and the hardware, not math. Is the TEE-now-PIR-later (defense-in-depth, distinct failure modes) posture the right long-term answer? See [trust](./trust.md) and [roadmap](./roadmap.md).
- **Nym.** A 5-hop mixnet for the shim-to-hub path, with STEVE only shim-to-hub and wallet-to-shim being plain TLS terminating in the enclave. Any transport assumptions to challenge?

### Coverage against the wallet threat model

We claim Zeronym targets the **server-side and network-metadata** concerns in Taylor's [wallet app threat model](https://zcash.readthedocs.io/en/latest/rtd_pages/wallet_threat_model.html), specifically the surveilling-lightwalletd and compromised-lightwalletd adversaries, and not the wallet-app-local concerns (key and seed storage, memo integrity, dust resilience, wallet fingerprinting, supply chain), which the model itself lists as the wallet's to address. Near-term the system eliminates one item on that list (migration-broadcast IP linkage) and blinds the operator's indexer to requester IPs; the full vision (indexer + Nym + TEE + PIR) is meant to close the rest of the metadata list. Is that boundary drawn correctly? A full concern-by-concern coverage matrix is planned as a follow-up.
