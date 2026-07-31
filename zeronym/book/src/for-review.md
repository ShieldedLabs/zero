# For review by Taylor and Zooko

This page collects the security-relevant claims and assumptions in this book that need
expert review before we rely on them. It is a living checklist, not a finished argument.
Platform and Caution questions live in [open questions](./open-questions.md); this page is
the **security-model** review, aimed at Taylor and Zooko.

The build is held until the threat-model doc (Taylor and Zooko) lands and signs off. This
is the running list of what that review needs to close.

## The attested edge

- **Verifiable no-IP-logging (protection 2).** We claim the operator's indexer is blinded
  to requester IPs by default: the shim proxies, so queries reach the backing lwd from the
  shim, not the wallet, and attested no-logging makes that checkable. Is the framing
  ("removes the passive, default leak, not a guarantee against an active operator") fair
  and correctly bounded? See [overview](./overview.md) and [threat model](./threat-model.md).
- **The network-layer residual.** We state that on Nitro the parent host still sees the
  wallet's source IP at the TCP layer, so a bad-faith operator can packet-capture and
  timing-correlate to re-link IP to query. Is that the correct and complete residual, or
  are there other cross-layer re-linking paths we are missing?
- **Tamper-proof front-end.** We claim attestation plus Certificate Transparency lets a
  wallet or auditor verify it is talking to the real attested shim, not an impostor or a
  modified front-end. Does the CT check fully close cert substitution for the drop-in URL?

## The migration path

- **The honest residual.** The operator learns *that* a client migrated (the one request
  not forwarded to its lwd), not the amount. Is that the complete residual, and are
  shim-side batching and shim-to-hub cover traffic correctly rejected as mitigations?
- **Anonymity set = the cross-operator batch.** At low migration volume a batch can be
  size 1 (no anonymity). Is hub-generated cover traffic the right (and only) backstop, and
  what batch size is "enough"? See [honest limits](./limits.md).
- **The classifier.** `is_migration` is a V6 transaction with value leaving Orchard and
  entering Ironwood. A false negative broadcasts a migration in the clear (a privacy
  failure), so the fee-aware turnstile predicate has to be correct and complete. Is it? Is
  fail-safe-for-privacy (treat an unparseable `SendTransaction` as a migration) sound? See
  [the shim](./shim.md).
- **Expiry.** Flushing under about ten blocks against the wallet expiry windows (Brave 20,
  Zingo 100, librustzcash +40; ZIP 318). Is the safety margin sound against real mining
  latency?

## Trust and transport

- **STEVE.** One-way (the shim verifies the hub), X25519 ECDH plus an Ed25519 signature,
  HKDF-SHA256, CBOR and AES-256-GCM. Is one-way sufficient, or is mutual attestation needed
  to gate abuse at the hub? Is the corrected STEVE understanding right? See [trust](./trust.md).
- **The trust root.** V2 privacy trusts AWS and the hardware, not math. Is the
  TEE-now-PIR-later (defense-in-depth, distinct failure modes) posture the right long-term
  answer? See [trust](./trust.md) and [roadmap](./roadmap.md).
- **Nym.** A 5-hop mixnet for the shim-to-hub path, with STEVE only shim-to-hub and
  wallet-to-shim being plain TLS terminating in the enclave. Any transport assumptions to
  challenge?

## Coverage against the wallet threat model

We claim Zeronym targets the **server-side and network-metadata** concerns in Taylor's
[wallet app threat model](https://zcash.readthedocs.io/en/latest/rtd_pages/wallet_threat_model.html),
specifically the surveilling-lightwalletd and compromised-lightwalletd adversaries, and
not the wallet-app-local concerns (key and seed storage, memo integrity, dust resilience,
wallet fingerprinting, supply chain), which the model itself lists as the wallet's to
address. Near-term the system eliminates one item on that list (migration-broadcast IP
linkage) and blinds the operator's indexer to requester IPs; the full vision (indexer +
Nym + TEE + PIR) is meant to close the rest of the metadata list. Is that boundary drawn
correctly? A full concern-by-concern coverage matrix is planned as a follow-up to this page.
