# Open questions for Caution

The [shim](./shim.md) and [hub](./hub.md) designs are settled enough to build. What remains are the cross-party items to close, and the reason to get Anton (Caution), Zooko, and Nate or Taylor in one room. This chapter is that agenda. It is the load-bearing set of platform unknowns the near-term system depends on, drawn from the shim and hub engineering designs, which are meant to be reviewed together.

Most of these questions are about the boundary between our software and Caution's managed enclave platform: where TLS terminates, what STEVE carries and how it authenticates, where the Nym sidecars run, and how a service can be both attested and zero-ingress. A few block code directly; others only need Caution to confirm a default we have already chosen. Where the [architecture](./architecture.md) or [trust](./trust.md) chapters own a mechanism referenced here, this chapter states the question and cross-links rather than restating the mechanism.

The first question is load-bearing: if TLS does not terminate inside the enclave, the drop-in model fails for the majority of wallets, and the rest of the design does not matter.

---

## 1. TLS termination: who owns :443?

**This is the load-bearing question.** The entire drop-in model depends on the wallet's TLS terminating **inside the shim enclave**, so that neither the operator host nor the Caution platform can read the migration in cleartext. The shim generates its TLS key in-enclave and runs an ACME client to obtain a normal CA-issued certificate keyed to that enclave-born key, so a naive wallet gets an ordinary valid certificate while the private key never leaves the enclave (see [the shim](./shim.md)).

The risk is a managed platform that puts a **Caddy** (or any reverse proxy) in front of the enclave to terminate TLS. If it does, that proxy sees the plaintext of every request, including migrations, before the traffic reaches the attested code. That breaks operator-blindness for exactly the wallets that matter most: the majority speak plain TLS and are not STEVE- or Nym-aware, so the batching at the hub is their only protection, and the shim being a TEE is what keeps the operator blind to the contents (see [the threat model](./threat-model.md)).

**The question for Anton:** does the enclave own `:443` directly, or does a platform Caddy terminate TLS in front of it? If Caddy terminates, we need a supported path to move termination into the enclave, or the drop-in model fails for non-STEVE wallets. Everything downstream (the three encryption layers in [the architecture](./architecture.md)) assumes the answer is "the enclave."

---

## 2. STEVE wire form over Nym

[STEVE](./trust.md) is used only on the shim-to-hub channel. The `SubmitMigration { ciphertext, txid, expiry_height }` request and `Ack { txid }` reply are a fixed shape (see [the hub](./hub.md)). What is open is the **transport** that carries that shape over the Nym TCP tunnel.

Two forms are on the table:

- **gRPC / HTTP/2 over the tunnel** (RA-TLS or STEVE terminating an h2 session), which reuses familiar framing.
- **A raw framed byte stream** we frame ourselves over the Nym TCP tunnel, with the STEVE-derived session key applied to our own records.

The README flags this as the open dependency: does STEVE-over-Nym carry h2? It blocks the exact `SubmitMigration` transport, but not its message shape, so the rest of the shim and hub can be built against the settled shape while this is resolved.

**The question for Anton:** what does a STEVE session carry over Nym, gRPC/h2 or a raw byte stream we frame ourselves?

---

## 3. STEVE: mutual or one-way?

STEVE is **one-way** by default: the client (the shim) verifies the enclave (the hub), extracts its key, and derives a session key, as documented in [trust](./trust.md). One-way is enough for privacy: the shim confirms it is talking to the genuine attested hub before handing over any migration, which is all that operator-blindness and hub-blindness require.

**Mutual** STEVE would additionally have the hub verify the shim's attestation. That gates abuse: only attested shims could submit, rather than the hub accepting a `SubmitMigration` from anyone with rate-limiting. The trade-off is real. Mutual raises the abuse bar but couples every shim to attestation provisioning and complicates onboarding a new operator; one-way, plus per-channel rate-limiting and the hub's own re-validation of every incoming tx (see [the hub](./hub.md)), keeps the submit path open and simple. Hub-side re-validation and rate-limiting already bound garbage regardless of which is chosen.

**The question for Anton:** should the hub also verify the shim (mutual STEVE, to gate abuse), or accept from anyone with rate-limiting (one-way)?

---

## 4. nym-proxy-server placement on managed Caution

The hub's inbound side is fronted by `nym-proxy-server`; the shim's outbound side uses `nym-proxy-client`. Because the migration is already encrypted to the hub key before it ever reaches the Nym client (the inner encryption layer in [the architecture](./architecture.md)), these sidecars only ever move ciphertext, so on confidentiality grounds they can run **parent-side, untrusted**.

The open item is a platform constraint, not a security one: on managed Caution we do not control the parent, so whether a parent-side sidecar is even permitted next to a managed enclave is Caution's call. The same question mirrors on the shim side, where the design notes `nym-proxy-client` runs in-enclave on managed Caution (since we do not control the parent) but parent-side on bring-your-own-cloud.

**The question for Anton:** can `nym-proxy-server` (and the shim's `nym-proxy-client`) run parent-side alongside a managed enclave, or must the Nym proxy run in-enclave?

---

## 5. Zero-ingress and attestation delivery

Today the `/attestation` endpoint is public and platform-served on both the shim and the hub. That is what the [Auditor Role](./trust.md) fetches over HTTPS to verify an endpoint. But a hub ideally wants to be **zero-ingress**: nothing listening on the public internet, reachable only over Nym, so it presents no public attack surface and leaks nothing about its own location.

Those two goals collide. A true zero-inbound service cannot also serve a public `/attestation` behind a platform Caddy. So either the platform suppresses its public `/attestation` and Caddy, or the attestation is **delivered inline over Nym** (as part of, or alongside, the STEVE handshake). If attestation moves to Nym, the auditor and the shim's own STEVE check need that alternate delivery path defined.

**The question for Anton:** can the platform suppress the public `/attestation` and Caddy for a true zero-inbound service, and if so, what is the supported way to deliver the attestation (inline over Nym) to the shim and to independent auditors?

---

## 6. The Rust STEVE SDK timeline

STEVE's JS SDK ships today; the **Rust SDK is still in development** (see [trust](./trust.md), per the STEVE blog). Both endpoints we are building need the handshake in Rust: the shim's hub-channel client and the hub's STEVE server.

Three paths, depending on the timeline:

- **Wait for the Rust SDK** and integrate it directly.
- **Implement the handshake ourselves** from standard primitives: attestation plus PCR verification against the AWS Nitro root, X25519 ECDH with ephemeral keys, Ed25519 signature verification, HKDF-SHA256 for the session key, and CBOR plus AES-256-GCM for payloads, exactly as [trust](./trust.md) documents the protocol.
- **An RA-TLS fallback** for the attested channel if STEVE itself is not ready in time.

**The question for Anton:** what is the Rust STEVE SDK timeline, and do you recommend we wait for it, implement the handshake directly from the standard primitives now, or use an RA-TLS fallback?

---

## 7. Keymaker quorum walkthrough

The single shared hub key that all shims encrypt to, and that makes hub failover clean, is persisted and reconstituted by the **keymaker / locksmith M-of-N quorum** across the consortium orgs (Caution, Nym, Shielded Labs, ZF). The mechanism, and its separation from STEVE, is documented in [trust](./trust.md).

The open item for Anton is operational, not conceptual: a concrete walkthrough of how the single shared key is **provisioned to multiple attested hub instances** (so any hub can decrypt for failover) and **reconstituted across cold boots and upgrades** on the managed platform, which is preferable to KMS-seal-to-PCR precisely because it survives an upgrade.

**The question for Anton:** walk us through provisioning the shared hub key to N attested instances and reconstituting it across boots and upgrades via the quorum.

---

## Companion questions for Zooko and Nate

The hub design also carries a set of items that are design confirmations rather than platform unknowns, aimed at Zooko and Nate. They live with the component that owns them, so they are cross-linked, not restated here:

- **Publish path:** `sendrawtransaction` to two or more external nodes versus direct Zcash P2P `tx` to many peers, and clearnet versus over Nym for the hub's own egress (see [the hub](./hub.md)).
- **Batch density versus failover:** confirm primary-hub preference (converge for density, fail over only on outage) over spreading shims across hubs (see [honest limits](./limits.md)).
- **Hub re-validation:** confirm the hub should re-parse and re-classify each tx, not trust the shim, and reject non-migrations before batching (see [the hub](./hub.md)).
- **Flush cadence and safety margin:** confirm the roughly ten-block flush and the safety margin against the real wallet expiry windows aligned in ZIP 318 (see [honest limits](./limits.md) and [the glossary](./glossary.md)).

---

## Hosting and funding

A practical open item, separate from the technical unknowns: who hosts and funds the
hubs at launch. Caution may cover a demo window; Shielded Labs may subsidize the
operators or run a donation drive; Nym (or Nym's Coastline) may run the hub component.
This is launch logistics, not a design blocker.

---

## The upstream gate

All of the above sits behind one gate: the threat-model doc (Taylor and Zooko) is the upstream dependency, and the build is held until it lands and is signed off as safe to run. These questions, closed in a room with Anton, are what that review needs before code lands.
