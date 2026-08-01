# Roadmap

The near-term shim + hub system is the first step toward a fuller private indexer, not the whole of it. See the [introduction](./introduction.md) for what ships now; this chapter is the horizon behind it, deferred until the migration fix lands.

## The three-version vision

The long-term product is a full wallet-facing private indexer: not just broadcasts, but the queries a light wallet makes (which addresses it looks up) served privately. Three strictly additive versions, each adding one trust-reducing layer:

| Version | Adds | What it gives | Trust root |
|---|---|---|---|
| **V1** | indexer + Nym | Queries and broadcasts served over the Nym mixnet, so the indexer never sees a client's source IP or timing. | The indexer operator (who still sees query contents in cleartext). |
| **V2** | + TEE (RA-TLS) | The indexer runs inside an attested enclave; the transport terminates inside it (remote-attestation TLS), so the operator is blind to contents and the wallet can verify this cryptographically. | AWS Nitro and the hardware manufacturer. |
| **V3** | + PIR | Private information retrieval serves queries without the server learning which record was fetched, closing the access-pattern leaks a TEE structurally has. | Cryptography (hardware-independent). |

The ladder walks the trust root down: operator (V1), hardware manufacturer (V2), only math (V3). V2 keeps Nym and adds the enclave; V3 keeps both and adds PIR as defense in depth, not a replacement (why it is not redundant is below).

## Where the near-term system sits: the zeroith step

The [shim + hub system](./introduction.md) is not V1. The ladder is query privacy for the full indexer; the near-term system addresses what the ladder does not urgently cover, the turnstile-crossing **broadcast** leak, driven by the Orchard to Ironwood migration and its ~Aug 10 deadline. So it sits alongside and ahead of the ladder: the **zeroith step**. Why it is a step toward the vision, not a detour:

- **Same machinery, smaller surface.** It already uses Nym (only shim to hub) and an attested TEE (both the [shim and hub](./components.md) are Nitro enclaves), on a narrow, urgent, auditable problem first. Building them here de-risks the full indexer and vice versa: the V2 transport rehearsal directly de-risked the shim-to-hub tunnel and the shim/hub attestation.
- **A deliberate 80% first step.** IP unlinking for the migration is the bulk of the practical privacy at stake in the acute window; query privacy, and the shield and non-Orchard deshield cases, are the remaining margin, which costs exponentially more to close.
- **A scoped version of a deferred idea.** Splitting the crossing broadcast off from the operator to a different counterparty (the hub) is a narrow instance of the query-only / broadcast-only decoupling below.
- **It sidesteps a decision the full product cannot avoid.** Sitting in front of the operator's existing backend, the shim need not choose an indexer base (lightwalletd vs Zaino) near-term. That decision is deferred, below.

## Near-term status: the first component is built

The [shim](./components.md) now exists as a proof of concept (commit `56394a1a54`, `zeronym/shim/`): a transparent h2c gRPC reverse proxy in front of an operator's existing indexer that forwards every method, stream and trailer verbatim, and decodes exactly one path, `SendTransaction`, classifying it with the real vendored `zebra-chain` parser. Transparency is tested rather than asserted, and the classifier runs on V6 wire bytes serialized by zebra's own codec, though no transaction a wallet produced has been classified yet (see [review](./review.md)). What it feeds is not yet a routing decision: the PoC is **non-destructive**, it logs a detected migration and then still forwards it. None of diversion, the hub, Nym, STEVE, TLS, the enclave, attestation or a reproducible build is in it.

Three milestones turn it into the component this book describes:

1. **Diversion to the hub**, the branch that makes a migration stop at the shim. It carries an ordering constraint the PoC surfaced: classify first, connect second, because a wallet whose transaction is about to be diverted must not cause the operator's indexer to see even a TCP connection.
2. **The reproducible StageX build**, a prerequisite rather than polish, because the [Auditor Role](./trust.md) rests on rebuild-to-the-same-hash. The PoC is a plain `cargo build`; its three concrete blockers (a repo-root build context, `zaino-proto` feature pinning, a parser not yet identical to the node's) are identified in [trust](./trust.md).
3. **The enclave and attestation**, the point at which [trust](./trust.md) applies to the shim at all.

## V2 status: designed, platform-unblocked, transport-validated

As of the 2026-07-30 V2 sync, the three hardest gates of the indexer + Nym + TEE version are answered.

**Designed.** The full wallet-facing private indexer, serving queries (not just broadcasts) over Nym, terminated inside an attested enclave. The remaining engineering is the in-enclave Nym integration; the TEE substrate, an attested Zebra + Zaino testnet enclave built reproducibly with StageX, is already live and synced to tip on testnet.

**Platform-unblocked.** Caution (via Anton) answered every platform question the design depended on:

- **Attestation binding is achievable**, three ways: the STEVE handshake, injecting the enclave pubkey via `metadata.json` -> `user_data`, or a new runtime `arbitrary_data` field Caution would add. Mechanics are in [trust](./trust.md).
- **Key persistence** across cold boots and software upgrades is solved by a keymaker/locksmith **M-of-N quorum across 3-4 orgs**, which survives upgrades (unlike KMS-seal-to-PCR, which breaks on upgrade) and gives the service a stable address. How it differs from STEVE is in [trust](./trust.md).
- **Egress just works** (broad NAT), so an enclave reaching the outside network is not a blocker.

**Transport-validated.** Rehearsed, not modeled: nym-proxy, built from `nymtech/nym`, carried our actual `CompactTxStreamer` gRPC over the **live Nym mainnet mixnet** against the live testnet enclave, end to end. Performance was roughly 10x slower than clearnet (unary calls ~9-10s, `GetBlockRange` ~19 blocks/s), latency-bound and warming over the first one or two calls, fine for non-time-sensitive migrations. Nym mainnet uses ticketbook ecash credentials, so the Nym client needs Nyx-RPC egress (`rpc.nymtech.net:443`). Spec: `deploy/caution-zaino/NYM.md`.

The residual questions this does not close (who terminates the wallet's TLS, the STEVE wire form over Nym, mutual vs one-way STEVE, `nym-proxy-server` placement, zero-ingress attestation delivery, and the Rust STEVE SDK timeline) are the agenda for Caution in [review](./review.md).

## V3 (PIR): not redundant with the TEE

The objection that PIR adds nothing once queries run inside an attested enclave the hypervisor cannot inspect ("a TEE the hypervisor cannot inspect is functionally PIR") was raised and corrected on the call. PIR is not redundant, for three reasons:

- **Different trust roots.** V2 is private only *if you trust AWS and the hardware*; V3/PIR is private *via math*, hardware-independent, removing the manufacturer and platform from the trust base. PIR is the trust-root-removal step.
- **Different leaks.** A Nitro enclave still has structural access-pattern and IO-pattern leaks when its state is parent-mediated at mainnet scale: the pattern of which records it touches is observable even when the contents are not. PIR closes exactly those, the server does not learn which record was fetched.
- **Complementary failure modes.** A TEE fails on hardware-manufacturer or physical-boundary compromise; PIR fails on cryptographic or software flaws. Disjoint reasons, so the endgame uses **both**, PIR layered on top of the TEE rather than instead of it.

The PIR building blocks under consideration (SimplePIR/DoublePIR, FrodoPIR, YPIR, ChalametPIR) are in the [glossary and references](./glossary.md). PIR is a later layer, off the near-term critical path.

## Deferred items

Real parts of the vision, held back so they never land on the ~Aug 10 critical path.

**The query-only / broadcast-only binary split.** Taylor's proposal: one attested instance proves it only answers queries and refuses broadcasts, a separate flavor only accepts broadcasts and refuses queries, so neither can correlate a client's reads with its writes. The near-term shim already realizes a scoped version for turnstile crossings. The general split is deferred because wallets today assume a single endpoint, so requiring two is an adoption cost with no near-term payoff.

**The attested Nym fleet.** Caution's planned global network of TEE-enabled Nym nodes (South Africa, Chicago, Brazil, Singapore, mirroring their DNS Cedar deployment), for a healthier public mixnet and broader adoption. Deprioritized because the near-term system routes Nym only between shim and hub, so users and wallets never touch Nym directly, and a better public Nym network is no longer the first thing to build.

**The indexer-base decision (lightwalletd vs Zaino).** The shim sits in front of whatever backend the operator already runs, so no base has to be chosen now. It re-emerges only when we build a first-party indexer for the deferred query-privacy product; then the standalone analysis (which leans toward lightwalletd) and the repo's existing PIR-platform designation come back into play.

**Two more, from launch-scope discipline.** Full **consortium key governance** (Caution, Nym, Shielded Labs, and the Zcash Foundation collectively attesting) is the long-term trust-distribution goal, but launch stands up the hub under a single trusted entity first, consortium to follow. And **Option A**, a standalone privacy server users must point their wallets at, was set aside for the drop-in shim, because changing wallet endpoint URLs has historically been nearly impossible to get adopted.
