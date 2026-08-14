# Open questions and review

The [shim and hub](./components.md) designs are settled enough to build. What remains are cross-party items to close: platform unknowns for Caution (Anton), and a security-model checklist for Taylor and Zooko. Where the [architecture](./architecture.md) or [trust](./trust.md) chapters own a mechanism, this chapter states the question and cross-links rather than restating it.

**The gate moved.** The threat-model doc was once the dependency that had to land before code was written. The shim and hub now ship as attested enclaves ([roadmap](./roadmap.md)), so the question is no longer *may we build this* but *may we rely on these claims*. The list below is that second gate, read against running code.

## Open questions for Caution (Anton)

### 1. TLS termination (resolved)

The wallet's TLS terminates inside the shim enclave. The platform default terminated it parent-side, which would have broken operator-blindness for exactly the naive TLS wallets the shim exists to protect; Caution shipped in-enclave termination on 2026-08-05 and it is verified end to end (see [components](./components.md)).

### 2. STEVE wire form over Nym

[STEVE](./trust.md) is used only on the shim-to-hub channel. The transport question this section used to ask is now answered by the shipped code: there is no TCP tunnel and no h2 session over the mixnet. Each side's linked `nym-sdk` client sends fixed-size `SubmitV1` / `AckV1` frames as anonymous messages with reply SURBs (see [components](./components.md)). What remains open is only whether STEVE wraps those frames, and how.

**The question for Anton:** does a STEVE session wrap our fixed-size frames as records under the session key, or does STEVE expect to own the transport itself?

### 3. STEVE: mutual or one-way?

STEVE is **one-way** by default: the shim verifies the hub, extracts its key, and derives a session key ([trust](./trust.md)). That is enough for privacy. Mutual STEVE would raise the abuse bar but couple every shim to attestation provisioning and complicate onboarding a new operator, and one-way plus rate-limiting plus the hub's own re-validation already bounds garbage.

**The question for Anton:** should the hub also verify the shim (mutual STEVE, to gate abuse), or accept from anyone with rate-limiting (one-way)?

### 4. Deploying the mixnet transport in an attested enclave

The sidecar-placement question this section used to ask is moot: there are no sidecars to place, because both sides link `nym-sdk` and run it in-process. Two concrete platform blockers replaced it, and together they are why the mixnet transport is built but not deployed.

- **Address publication.** The hub's Nym address is minted per client build and written only to a log. An attested enclave does not expose that log, so a shim has no supported way to learn the address of the hub it is meant to reach.
- **Gateway pinning.** Egress from an enclave is locked to a `/32` allowlist, but a Nym client chooses its gateway dynamically and that choice cannot currently be pinned to the allowlisted address.

**The question for Anton:** what is the supported way for an attested enclave to publish a value minted at boot (here, its Nym address), and can gateway selection be pinned so a mixnet client works under a locked egress allowlist?

### 5. Zero-ingress and attestation delivery

Today the `/attestation` endpoint is public on both shim and hub. On the shim it is now shim-served: the router claims the path and relays to the platform's `bootproofd` rather than forwarding it to the operator's indexer. That is what the [Auditor Role](./trust.md) fetches over HTTPS to verify an endpoint. But a hub ideally wants to be **zero-ingress**: nothing listening on the public internet, reachable only over Nym, presenting no public attack surface and leaking nothing about its location. Those goals collide: a true zero-inbound service cannot also serve a public `/attestation` behind a platform Caddy. So either the platform suppresses its public `/attestation` and Caddy, or the attestation is **delivered inline over Nym** (as part of, or alongside, the STEVE handshake). If it moves to Nym, the auditor and the shim's own STEVE check need that alternate delivery path defined.

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

- **Publish path:** the hub broadcasts through an **indexer's `CompactTxStreamer` over TLS** (not node JSON-RPC). Open: the fan-out breadth (one indexer versus several, or direct Zcash P2P `tx` to many peers for a larger relay set) and clearnet versus over Nym for the hub's own egress (see [components](./components.md)).
- **Batch density versus failover:** confirm primary-hub preference (converge for density, fail over only on outage) over spreading shims across hubs (see [trust](./trust.md)).
- **Hub re-validation (resolved to telemetry-only):** the hub re-parses so a disagreement with the shim is visible, and never drops on that basis. Reasoning in [components](./components.md).
- **Flush cadence and safety margin:** confirm the twenty-block flush and the four-block mining margin against real wallet expiry windows aligned in ZIP 318 (see [the hub](./components.md)).
- **JSON-RPC front-ends:** the shim intercepts only gRPC `SendTransaction`, so a migration submitted through a JSON-RPC `sendrawtransaction` front-end would bypass classification and leak. Wallets use gRPC, so this is out of near-term scope; confirm no operator front-ends migrations via JSON-RPC (see [components](./components.md)).

### Hosting and funding

A practical open item, separate from the technical unknowns: who hosts and funds the hubs at launch. Caution may cover a demo window; Shielded Labs may subsidize operators or run a donation drive; Nym (or Nym's Coastline) may run the hub component. Launch logistics, not a design blocker.

## For review by Taylor and Zooko

The security-relevant claims that need expert review before we rely on them. A living checklist, not a finished argument, aimed at the **security model**; the platform questions above are Caution's.

### The attested edge

- **Verifiable no-IP-logging (protection 2).** Is the framing ("removes the passive, default leak, not a guarantee against an active operator") fair and correctly bounded? See [the problem](./problem.md).
- **The network-layer residual.** On Nitro the parent host still sees the wallet's source IP at the TCP layer, so a bad-faith operator can packet-capture and timing-correlate to re-link IP to query. Is that the correct and complete residual, or are there other cross-layer re-linking paths we are missing?
- **Tamper-proof front-end.** We claim attestation plus Certificate Transparency lets a wallet or auditor verify it is talking to the real attested shim, not an impostor or a modified front-end. Does the CT check fully close cert substitution for the drop-in URL?
- **Reproducible-attestation gap (PCR0/PCR1).** The application binary reproduces (its measurement is the attestation's PCR2), but the EnclaveOS base image and kernel (PCR0, PCR1) are not yet reproducible end to end, and `caution verify` cannot currently confirm them (a known Caution limitation, so PCR2 is the measurement that carries weight today). Is PCR2-only reproducibility an acceptable interim, and what closes PCR0/PCR1? See [roadmap](./roadmap.md).

### The migration path

- **The honest residual.** The operator learns *that* a client migrated (the one request not forwarded to its lwd), not the amount. Is that the complete residual, and are shim-side batching and shim-to-hub cover traffic correctly rejected as mitigations?
- **Anonymity set = the cross-operator batch.** At low migration volume a batch can be size 1 (no anonymity), and the 2026-08-11 mainnet run classified and published a real wallet-built migration but at batch size one, so it validated the mechanics and content privacy rather than the anonymity set. Is hub-generated cover traffic the right (and only) backstop, and what batch size is "enough"? See [trust](./trust.md).
- **The classifier, and what the proof of concept settled.** `is_orchard_touching` is now a pure function over the vendored `zebra-chain` parser, and fail-safe for privacy is implemented rather than proposed: every body the shim cannot read cleanly routes to the migration arm, each case pinned by a test ([components](./components.md) has the set). **Left open:** is that set complete, and does treating every unreadable body as a migration stay right now that diversion is destructive rather than a log line?
- **The classifier predicate (resolved).** Presence only: `is_orchard_touching(tx) := tx.orchard_shielded_data().is_some()`, with the value balance logged as evidence and gating nothing. Zooko's ruling; the closed-pool argument, the superseded `> 0` predicate it replaced, and the measured cost of the widening are all in [components](./components.md). The consequence to accept: the batched class is wider than *migrations*, so an Orchard deshield is delayed by a flush window too.
- **Compressed `SendTransaction`: policy conflict.** A compressed body never reaches the parser and lands in the fail-safe arm; because compression is negotiated, the shim normalizes the indexer's advertised `grpc-accept-encoding` back to `identity` (see [components](./components.md) for the lever this denies the operator). **Is normalizing right, or should the shim strip the header and refuse a compressed `SendTransaction` outright?** The cost of a fail-safe is only a delay: the hub still broadcasts a false-positive rather than rejecting it, since hub re-validation is telemetry-only (resolved above).
- **The interception set: is the backend survey complete?** The shim intercepts on path alone and must stay a superset of every backend's routing predicate; a fail-open bug of exactly that shape was found in the PoC and fixed (see [components](./components.md)). The known residual is percent-decoding: the shim compares the path as received, so a backend that decoded before matching would route a request the shim passes through. Neither tonic nor lightwalletd does, but the survey covers only the backends we checked.
- **Expiry.** Is the flush budget ([components](./components.md) has the arithmetic and the wallet expiry values) sound against real mining latency, and is the delivery-lag allowance right given the mixnet's measured round trip?

### Trust and transport

- **STEVE.** One-way, X25519 ECDH plus an Ed25519 signature, HKDF-SHA256, CBOR and AES-256-GCM. Is that understanding right? See [trust](./trust.md). (Mutual versus one-way is question 3 above.)
- **The trust root.** V2 privacy trusts AWS and the hardware, not math. Is the TEE-now-PIR-later (defense-in-depth, distinct failure modes) posture the right long-term answer? See [trust](./trust.md) and [roadmap](./roadmap.md).
- **Nym.** A 5-hop mixnet for the shim-to-hub path, with STEVE only shim-to-hub and wallet-to-shim being plain TLS terminating in the enclave. Any transport assumptions to challenge?

### Detection design and the wallet requirement

- **Wallet anchor/expiry alignment.** The protection requires ZIP-318-like wallets to choose identical anchors and expiry heights within a migration epoch; a latest-anchor wallet is re-linkable via the anchor (see [the problem](./problem.md)). Confirm with wallet authors, and align the hub's batch granularity to how wallets pick anchors and expiries.
- **TEE hardening.** The design assumes the enclave resists state rollback (rewind/replay) and memory-access-pattern observation. Both are open hardening items: are they achievable on the target platform, and what is the residual exposure if not?
- **The `zec.rocks` certificate.** Its existing TLS cert is valid through October, so the scheme is ineffective for that domain until then without a fresh domain or key revocation (and wallets that check revocation). Decide the domain / revocation path.
- **Accepted non-defenses.** Active wallet-tagging (a false chain or hold-back forcing identifiable anchors) and the transaction-size side channel (a distinctive migration size re-links via TLS ciphertext length) are out of scope by design. Confirm these are acceptable, or scope mitigations.
- **Operator-error alarms.** Losing TEE state or an accidental certificate renewal is indistinguishable from an attack and will be announced as one. Confirm operators accept this and can run carefully (auto-renewal disabled, enclave state guarded).

### Coverage against the wallet threat model

We claim Zeronym targets the **server-side and network-metadata** concerns in Taylor's [wallet app threat model](https://zcash.readthedocs.io/en/latest/rtd_pages/wallet_threat_model.html), specifically the surveilling-lightwalletd and compromised-lightwalletd adversaries, and not the wallet-app-local concerns (key and seed storage, memo integrity, dust resilience, wallet fingerprinting, supply chain), which the model itself lists as the wallet's to address. Is that boundary drawn correctly?
