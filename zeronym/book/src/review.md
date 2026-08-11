# Open questions and review

The [shim and hub](./components.md) designs are settled enough to build. What remains are cross-party items to close: platform unknowns for Caution (Anton), and a security-model checklist for Taylor and Zooko. Where the [architecture](./architecture.md) or [trust](./trust.md) chapters own a mechanism, this chapter states the question and cross-links rather than restating it.

**The upstream gate (moved, not lifted).** This work initially sat behind one gate: the threat-model doc (Taylor and Zooko) was treated as the dependency that had to land before the code was written, and the proof of concept sat deliberately inside it, classifying and logging while diverting nothing. Building has since proceeded: the shim and hub are shipped and run as attested enclaves ([roadmap](./roadmap.md) has the status). The gate did not disappear, it **moved**, from *may we build this* to *may we rely on these claims*. The questions below are that second gate: the review agenda, now read against running code rather than a design.

## Open questions for Caution (Anton)

Most of these concern the boundary between our software and Caution's managed enclave platform: where TLS terminates, what STEVE carries and how it authenticates, where the Nym sidecars run, and how a service can be both attested and zero-ingress. A few block code directly; others only need Caution to confirm a default we have already chosen.

### 1. TLS termination: who owns :443?

**Resolved.** The drop-in model depends on the wallet's TLS terminating **inside the shim enclave**, so neither the operator host nor the Caution platform reads the migration in cleartext. The fear was a managed platform terminating TLS in a parent-side **Caddy** before traffic reached attested code, which breaks operator-blindness for exactly the naive TLS wallets (not STEVE- or Nym-aware) whose only protection is that the shim is a TEE and the hub batches (see [components](./components.md) and [the problem](./problem.md)).

**That was the platform default (measured 2026-08-02).** Declaring `ingress { port = 443 }` failed Caution's own provisioning while port 8443 deployed cleanly, and a running app answered on 443 with a self-signed instance-IP cert and a `Server: Caddy` header, so 443 was decrypted on the parent. It also blocked in-enclave ACME, since Let's Encrypt validates over ports 80/443 and the platform owned both, so the enclave could not prove control of its own domain.

**Caution then shipped the fix (2026-08-03/05):** `e2e_encryption { mode = "tls" }` runs a Caddy **inside** the enclave that obtains the Let's Encrypt cert itself and terminates the wallet's TLS there, so the private key is enclave-born and the operator never holds it; `upstream_protocol = "h2c"` carries the gRPC. Verified end to end on deployed enclaves (`GetLightdInfo` clean, trailers intact). The shim's own rustls/ACME stack stays dormant on Caution as the vendor-independent path.

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

- **Publish path:** the hub broadcasts through an **indexer's `CompactTxStreamer` over TLS** (not node JSON-RPC). Open: the fan-out breadth (one indexer versus several, or direct Zcash P2P `tx` to many peers for a larger relay set) and clearnet versus over Nym for the hub's own egress (see [components](./components.md)).
- **Batch density versus failover:** confirm primary-hub preference (converge for density, fail over only on outage) over spreading shims across hubs (see [trust](./trust.md)).
- **Hub re-validation (resolved to telemetry-only).** The hub re-parses and re-classifies each tx so any disagreement with the shim is *visible*, but never drops on that basis: the earlier design (reject anything that is not an Orchard-touching transaction before batching) was reversed, since the txs most likely to fail the hub's parse are exactly the ones the shim fail-safed into the batch, so rejecting them would turn the shim's fail-safe into a leak. Refusals are narrow and structural only (auth, malformed frame, byte budget, expiry admission); final validity is decided at broadcast, not by the hub (see [components](./components.md)).
- **Flush cadence and safety margin:** confirm the roughly ten-block flush and the safety margin against real wallet expiry windows aligned in ZIP 318 (see [trust](./trust.md) and [glossary](./glossary.md)).
- **JSON-RPC front-ends:** the shim intercepts only gRPC `SendTransaction`, so a migration submitted through a JSON-RPC `sendrawtransaction` front-end would bypass classification and leak. Wallets use gRPC, so this is out of near-term scope; confirm no operator front-ends migrations via JSON-RPC (see [components](./components.md)).

### Hosting and funding

A practical open item, separate from the technical unknowns: who hosts and funds the hubs at launch. Caution may cover a demo window; Shielded Labs may subsidize operators or run a donation drive; Nym (or Nym's Coastline) may run the hub component. Launch logistics, not a design blocker.

## For review by Taylor and Zooko

The security-relevant claims and assumptions that need expert review before we rely on them. A living checklist, not a finished argument, aimed at the **security model** (platform questions above are Caution's). Some items below were written against the shim proof of concept, a non-destructive classifier that logged and still forwarded; the shipped shim now diverts, the hub exists, and both run as attested enclaves, so read each item against running code: where it settles a question the item says what is settled and what is left.

### The attested edge

- **Verifiable no-IP-logging (protection 2).** We claim the operator's indexer is blinded to requester IPs by default: the shim proxies, so the queries that still go to the operator reach its backing lwd from the shim, not the wallet, and attested no-logging makes that checkable. (Transaction-detail lookups, `GetTransaction`, no longer reach the operator at all: the hub's indexer serves them now. Address-level queries still do.) Is the framing ("removes the passive, default leak, not a guarantee against an active operator") fair and correctly bounded? See [the problem](./problem.md).
- **The network-layer residual.** On Nitro the parent host still sees the wallet's source IP at the TCP layer, so a bad-faith operator can packet-capture and timing-correlate to re-link IP to query. Is that the correct and complete residual, or are there other cross-layer re-linking paths we are missing?
- **Tamper-proof front-end.** We claim attestation plus Certificate Transparency lets a wallet or auditor verify it is talking to the real attested shim, not an impostor or a modified front-end. Does the CT check fully close cert substitution for the drop-in URL?
- **Reproducible-attestation gap (PCR0/PCR1).** The application binary reproduces (its measurement is the attestation's PCR2), but the EnclaveOS base image and kernel (PCR0, PCR1) are not yet reproducible end to end, and `caution verify` cannot currently confirm them (a known Caution limitation, so PCR2 is the measurement that carries weight today). Is PCR2-only reproducibility an acceptable interim, and what closes PCR0/PCR1? See [roadmap](./roadmap.md).

### The migration path

- **The honest residual.** The operator learns *that* a client migrated (the one request not forwarded to its lwd), not the amount. Is that the complete residual, and are shim-side batching and shim-to-hub cover traffic correctly rejected as mitigations?
- **Anonymity set = the cross-operator batch.** At low migration volume a batch can be size 1 (no anonymity). Is hub-generated cover traffic the right (and only) backstop, and what batch size is "enough"? See [trust](./trust.md).
- **The classifier, and what the proof of concept settled.** `is_orchard_touching` is now a pure function over the vendored `zebra-chain` parser, and fail-safe for privacy is implemented rather than proposed: every body the shim cannot read cleanly routes to the migration arm, each case pinned by a test ([components](./components.md) has the set). **Left open:** is that set complete, and does treating every unreadable body as a migration stay right now that diversion is destructive rather than a log line?
- **The classifier predicate: RESOLVED by Zooko (presence).** The shipped predicate is one conjunct on the presence of Orchard actions, `is_orchard_touching(tx) := tx.orchard_shielded_data().is_some()`, with the value balance logged as evidence and gating nothing. The earlier `V6` and `ironwood_value_balance < 0` conjuncts are gone and the code matches ([components](./components.md)). The reasoning is the closed pool: NU6.3 forbids new value entering Orchard, so everyone still holding Orchard notes has held them since before activation, and **spending Orchard at all** is the identifying event, against a finite, shrinking set. Destination is irrelevant, so an Orchard deshield to transparent is batched exactly like an Orchard-to-Ironwood migration. (History kept visible because the rest of this list reads against it. A first ruling batched on value out of Orchard alone, `orchard_value_balance > 0`; a **gross** alternative, an Orchard bundle with at least one spend, was floated and criticised for sweeping in ordinary same-receiver change. The presence rule **dissolves** the net-versus-gross question rather than answering it: any Orchard bundle diverts, so no threshold has to be drawn. Measured cost nil, since at a recent mainnet tip every Orchard-touching transaction already had value out, so the wider rule diverted not one extra transaction; [components](./components.md) has the count.) The consequence to accept: the batched class is wider than *migrations*, so an Orchard deshield is delayed by a flush window too (why that costs little: [architecture](./architecture.md), [honest limits](./trust.md)).
- **The net-zero shuffle: now closed, and it was the deciding shape.** The first ruling's criterion was **value** (`orchard_value_balance > 0`); its rationale was **spending** (post-NU6.3, spending Orchard at all is identifying). They diverge in one shape: a transaction that spends legacy Orchard notes but nets to exactly zero (fee from transparent or Sapling, change to the same receiver) moves no value out, yet still publishes those notes' nullifiers. Under `> 0` it classified `PassThrough` and would broadcast in the clear. The presence rule closes it: the shuffle carries Orchard actions, so it diverts. This was open question 4 in `zeronym/shim/README.md`; Zooko's presence ruling resolves it by making the rationale (spending) the criterion, at the accepted cost of also diverting ordinary same-receiver change, which the mainnet count shows is not extra load today.
- **Compressed `SendTransaction`: policy conflict.** A compressed body never reaches the parser and lands in the fail-safe arm; because compression is negotiated, the shim normalizes the indexer's advertised `grpc-accept-encoding` back to `identity` (see [components](./components.md) for the lever this denies the operator). **Is normalizing right, or should the shim strip the header and refuse a compressed `SendTransaction` outright?** The cost of a fail-safe is only a delay: the hub still broadcasts a false-positive rather than rejecting it, since hub re-validation is telemetry-only (resolved above).
- **Wallet-produced migration: now demonstrated on mainnet.** The unit vectors are still built with `zebra-chain`'s own serializer, but on 2026-08-11 a **real** Orchard to Ironwood migration traversed the full stack on mainnet: classified, held, batched, and published on the cadence, with the operator's indexer never seeing the direct submit. That closes the old "nothing a real wallet built has reached the classifier" gap. What it does **not** yet prove is batching *anonymity*: at current adoption the batch was **size one**, so the run validates the mechanics and content privacy, not the anonymity set (see the batch-size item above and [honest limits](./trust.md)).
- **The interception set: is the backend survey complete?** The shim intercepts on path alone and must stay a superset of every backend's routing predicate; a fail-open bug of exactly that shape was found in the PoC and fixed (see [components](./components.md)). The known residual is percent-decoding: the shim compares the path as received, so a backend that decoded before matching would route a request the shim passes through. Neither tonic nor lightwalletd does, but the survey covers only the backends we checked.
- **Expiry.** Flushing every twenty blocks against the wallet expiry windows (Brave 20, librustzcash 40, Zingo 100; ZIP 318), with Brave out of scope for v1 so the binding value is 40. Is `20 + 4 + 6 = 30` sound against real mining latency, and is the delivery-lag allowance right given Nym's measured 9 to 10 second unary round trip?

### Trust and transport

- **STEVE.** One-way (the shim verifies the hub), X25519 ECDH plus an Ed25519 signature, HKDF-SHA256, CBOR and AES-256-GCM. Is one-way sufficient, or is mutual attestation needed to gate abuse at the hub? Is the corrected STEVE understanding right? See [trust](./trust.md).
- **The trust root.** V2 privacy trusts AWS and the hardware, not math. Is the TEE-now-PIR-later (defense-in-depth, distinct failure modes) posture the right long-term answer? See [trust](./trust.md) and [roadmap](./roadmap.md).
- **Nym.** A 5-hop mixnet for the shim-to-hub path, with STEVE only shim-to-hub and wallet-to-shim being plain TLS terminating in the enclave. Any transport assumptions to challenge?

### Detection design and the wallet requirement

- **Wallet anchor/expiry alignment.** The protection requires ZIP-318-like wallets to choose identical anchors and expiry heights within a migration epoch; a latest-anchor wallet is re-linkable via the anchor (see [the problem](./problem.md)). Confirm with wallet authors, and align the hub's batch granularity to how wallets pick anchors and expiries.
- **TEE hardening.** The design assumes the enclave resists state rollback (rewind/replay) and memory-access-pattern observation. Both are open hardening items: are they achievable on the target platform, and what is the residual exposure if not?
- **The `zec.rocks` certificate.** Its existing TLS cert is valid through October, so the scheme is ineffective for that domain until then without a fresh domain or key revocation (and wallets that check revocation). Decide the domain / revocation path.
- **Accepted non-defenses.** Active wallet-tagging (a false chain or hold-back forcing identifiable anchors) and the transaction-size side channel (a distinctive migration size re-links via TLS ciphertext length) are out of scope by design. Confirm these are acceptable, or scope mitigations.
- **Operator-error alarms.** Losing TEE state or an accidental certificate renewal is indistinguishable from an attack and will be announced as one. Confirm operators accept this and can run carefully (auto-renewal disabled, enclave state guarded).

### Coverage against the wallet threat model

We claim Zeronym targets the **server-side and network-metadata** concerns in Taylor's [wallet app threat model](https://zcash.readthedocs.io/en/latest/rtd_pages/wallet_threat_model.html), specifically the surveilling-lightwalletd and compromised-lightwalletd adversaries, and not the wallet-app-local concerns (key and seed storage, memo integrity, dust resilience, wallet fingerprinting, supply chain), which the model itself lists as the wallet's to address. Near-term the system eliminates one item on that list (migration-broadcast IP linkage) and blinds the operator's indexer to requester IPs; the full vision (indexer + Nym + TEE + PIR) is meant to close the rest of the metadata list. Is that boundary drawn correctly? A full concern-by-concern coverage matrix is planned as a follow-up.
