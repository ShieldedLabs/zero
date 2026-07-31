# Roadmap

The near-term shim + hub system is the first step toward a fuller privacy product, not the whole of it. This chapter places that step on the long-term map, gives the current status of the next version, explains why the endgame keeps both a hardware root and a mathematical one, and lists the pieces deliberately deferred off the critical path.

Read the [overview](./overview.md) first for what ships now. Everything below is the horizon behind it, deferred until the urgent migration fix lands.

## The three-version vision

The long-term product is a full wallet-facing private indexer: not just broadcasts, but the queries a light wallet makes (which addresses it looks up) served privately. That product is planned in three versions, each adding one trust-reducing layer to the last.

| Version | Adds | What it gives | Trust root |
|---|---|---|---|
| **V1** | indexer + Nym | Queries and broadcasts served over the Nym mixnet, so the indexer never sees a client's source IP or timing. | The indexer operator (who still sees query contents in cleartext). |
| **V2** | + TEE (RA-TLS) | The indexer runs inside an attested enclave; the transport terminates inside it (remote-attestation TLS), so the operator is blind to contents and the wallet can verify this cryptographically. | AWS Nitro and the hardware manufacturer. |
| **V3** | + PIR | Private information retrieval serves queries without the server learning which record was fetched, closing the access-pattern leaks a TEE structurally has. | Cryptography (hardware-independent). |

Each version is strictly additive. V2 keeps Nym and adds the enclave; V3 keeps both and adds PIR as a defense-in-depth layer rather than a replacement. The point of the ladder is that it walks the trust root down: from trusting the operator (V1), to trusting a hardware manufacturer (V2), to trusting only math (V3). The reasoning for why V3 is not redundant with V2 is below.

## Where the near-term system sits: the zeroith step

The [shim + hub system](./overview.md) is not V1. The V1/V2/V3 ladder is about query privacy for the full indexer; the near-term system is about one thing the ladder does not urgently address: the turnstile-crossing **broadcast** leak, driven by the Orchard to Ironwood migration and its ~Aug 10 deadline. So it sits alongside and ahead of the ladder, which is why the strategy calls it the **zeroith step**.

What makes it a step toward the vision rather than a detour:

- **It ships the same machinery on a smaller surface.** The near-term system already uses Nym (only between shim and hub) and an attested TEE (both the [shim](./shim.md) and the [hub](./hub.md) are Nitro enclaves). It applies those exact building blocks, Nym transport plus enclave attestation, to a narrow, urgent, auditable problem first. Building them here de-risks building them for the full indexer later, and vice versa (the V2 transport rehearsal directly de-risked the shim-to-hub tunnel and the shim/hub attestation).
- **It is a deliberate 80% first step.** IP unlinking for the migration is the bulk of the practical privacy at stake in the acute window; query privacy and the deshield/shield cases are the remaining margin, which costs exponentially more to close. The near-term system takes the Pareto win and labels it as such.
- **It already realizes a scoped version of one deferred idea.** Splitting the crossing broadcast off from the operator and sending it to a different counterparty (the hub) is a narrow instance of the query-only / broadcast-only decoupling discussed below.
- **It sidesteps a decision the full product cannot avoid.** By sitting in front of the operator's existing backend, the shim does not have to choose an indexer base (lightwalletd vs Zaino) near-term. That decision is deferred with the query-privacy product, below.

## V2 status: designed, platform-unblocked, transport-validated

As of the 2026-07-30 V2 sync, the full indexer + Nym + TEE version is not just sketched. Its three hardest gates are answered.

**Designed.** The shape is settled: the full wallet-facing private indexer, serving queries (not just broadcasts) over Nym, terminated inside an attested enclave. The remaining engineering is the in-enclave Nym integration; the TEE substrate itself, an attested Zebra + Zaino testnet enclave built reproducibly with StageX, is already live and synced to tip on testnet.

**Platform-unblocked.** Caution (via Anton) answered every platform question the design depended on:

- **Attestation binding is achievable**, three ways: the STEVE handshake, or injecting the enclave pubkey via `metadata.json` -> `user_data`, or a new runtime `arbitrary_data` field Caution would add. The mechanics of attestation binding and the STEVE handshake are covered in [trust](./trust.md).
- **Key persistence** across cold boots and software upgrades is solved by a keymaker/locksmith **M-of-N quorum across 3-4 orgs**, which survives upgrades (unlike KMS-seal-to-PCR, which breaks on upgrade) and gives the service a stable address. This quorum, and how it differs from STEVE, is detailed in [trust](./trust.md).
- **Egress just works** (broad NAT), so an enclave reaching the outside network is not a blocker.

**Transport-validated.** The Nym transport was not modeled, it was rehearsed for real. nym-proxy, built from `nymtech/nym`, carried our actual `CompactTxStreamer` gRPC over the **live Nym mainnet mixnet** against the live testnet enclave, end to end. Measured performance was roughly 10x slower than clearnet (unary calls ~9-10s, `GetBlockRange` ~19 blocks/s), latency-bound and warming up over the first one or two calls, which is fine for non-time-sensitive migrations. Nym mainnet uses ticketbook ecash credentials, so the Nym client needs Nyx-RPC egress (`rpc.nymtech.net:443`). The transport spec is `deploy/caution-zaino/NYM.md`.

The residual platform questions that this status does not close (who terminates the wallet's TLS, the STEVE wire form over Nym, mutual vs one-way STEVE, `nym-proxy-server` placement, zero-ingress attestation delivery, and the Rust STEVE SDK timeline) are the agenda for Caution in [open questions](./open-questions.md).

## V3 (PIR): not redundant with the TEE

A natural objection is that once queries run inside an attested enclave the hypervisor cannot inspect, PIR adds nothing. That view ("a TEE the hypervisor cannot inspect is functionally PIR") was raised and corrected on the call. PIR is not redundant with the TEE, for three reasons.

- **Different trust roots.** V2 is private only *if you trust AWS and the hardware*. V3/PIR is private *via math*: it is hardware-independent and removes the manufacturer and platform from the trust base entirely. PIR is the trust-root-removal step.
- **Different leaks.** A Nitro enclave still has structural access-pattern and IO-pattern leaks when its state is parent-mediated at mainnet scale, because the pattern of which records it touches can be observed even when the contents cannot. PIR closes exactly those access-pattern leaks by design: the server does not learn which record was fetched.
- **Complementary failure modes.** TEE and PIR fail for disjoint reasons. A TEE fails on hardware-manufacturer or physical-boundary compromise; PIR fails on cryptographic or software flaws. They are complementary, not equivalent, so the endgame uses **both**, as defense in depth, with PIR layered on top of the TEE rather than instead of it.

The PIR building blocks under consideration (SimplePIR/DoublePIR, FrodoPIR, YPIR, ChalametPIR) are listed in the [glossary and references](./glossary.md). PIR is explicitly a later layer and is off the near-term critical path.

## Deferred items

These are real parts of the vision, held back so they never land on the ~Aug 10 critical path.

**The query-only / broadcast-only binary split.** Taylor's proposal: one attested instance proves it only answers queries and refuses broadcasts, while a separate flavor only accepts broadcasts and refuses queries, so neither can correlate a client's reads with its writes. The near-term shim already realizes a scoped version of this for turnstile crossings (the crossing broadcast is split off from the operator entirely). The general split is deferred because wallets today assume a single endpoint, so requiring two would be an adoption cost with no near-term payoff.

**The attested Nym fleet.** Caution's planned global network of TEE-enabled Nym nodes (South Africa, Chicago, Brazil, Singapore, mirroring their DNS Cedar deployment). It would make for a healthier public mixnet and broader adoption. It is deprioritized precisely because the near-term system routes Nym only between shim and hub, so it does not require users or wallets to touch Nym directly, and a better public Nym network is therefore no longer the first thing to build.

**The indexer-base decision (lightwalletd vs Zaino).** The near-term shim sidesteps this cleanly: it sits in front of whatever backend the operator already runs, so no base has to be chosen now. The decision only re-emerges if and when we build a first-party indexer for the deferred query-privacy product. At that point the standalone analysis (which leans toward lightwalletd) and the repo's existing PIR-platform designation come back into play.

**Two more, from the launch-scope discipline.** Full **consortium key governance** (Caution, Nym, Shielded Labs, and the Zcash Foundation collectively attesting) is the long-term trust-distribution goal, but launch stands up the hub under a single trusted entity first, with the consortium to follow. And **Option A**, a standalone privacy server users must point their wallets at, was set aside in favor of the drop-in shim, because changing wallet endpoint URLs has historically been nearly impossible to get adopted.
