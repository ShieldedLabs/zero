# Roadmap

The near-term shim + hub system is the first step toward a fuller private indexer, not the whole of it. See [the architecture](./architecture.md) for what ships now; this chapter is the horizon behind it: the layers that come after the near-term system, not blockers on it. That system has shipped and carried a real mainnet migration, so these are next, not prerequisites.

## The three-version vision

The long-term product is a full wallet-facing private indexer: not just broadcasts, but the queries a light wallet makes (which addresses it looks up) served privately. Three strictly additive versions, each adding one trust-reducing layer:

| Version | Adds | What it gives | Trust root |
|---|---|---|---|
| **V1** | indexer + Nym | Queries and broadcasts served over the Nym mixnet, so the indexer never sees a client's source IP or timing. | The indexer operator (who still sees query contents in cleartext). |
| **V2** | + TEE (RA-TLS) | The indexer runs inside an attested enclave; the transport terminates inside it (remote-attestation TLS), so the operator is blind to contents and the wallet can verify this cryptographically. | AWS Nitro and the hardware manufacturer. |
| **V3** | + PIR | Private information retrieval serves queries without the server learning which record was fetched, closing the access-pattern leaks a TEE structurally has. | Cryptography (hardware-independent). |

The ladder walks the trust root down: operator (V1), hardware manufacturer (V2), only math (V3). V2 keeps Nym and adds the enclave; V3 keeps both and adds PIR as defense in depth, not a replacement (why it is not redundant is below).

## Where the near-term system sits: the zeroith step

The [shim + hub system](./architecture.md) is not V1. The ladder is full query privacy for the indexer; the near-term system covers what the ladder does not urgently reach, the turnstile-crossing **broadcast** leak, driven by the mandatory Orchard to Ironwood migration. So it sits alongside and ahead of the ladder: the **zeroith step**. It is already a partial down-payment on the ladder: transaction-detail lookups (`GetTransaction`) are served by the hub's own indexer today, so the query leak is no longer entirely future; the address-level queries (which addresses a wallet looks up) still reach the operator. Why it is a step toward the vision, not a detour:

- **Same machinery, smaller surface.** It already uses an attested TEE (both the [shim and hub](./components.md) run as Nitro enclaves) and stages Nym on the shim-to-hub hop, on a narrow, urgent, auditable problem first. Building them here de-risks the full indexer and vice versa: the V2 transport rehearsal directly de-risked the shim-to-hub tunnel and the shim/hub attestation.
- **A deliberate 80% first step.** IP unlinking for the migration is the bulk of the practical privacy at stake in the acute window; the rest of query privacy, and the shield and non-Orchard deshield cases, are the remaining margin, which costs far more to close.
- **A scoped version of a deferred idea.** Splitting the crossing broadcast off from the operator to a different counterparty (the hub) is a narrow instance of the query-only / broadcast-only decoupling below.
- **It sidesteps a decision the full product cannot avoid.** Sitting in front of the operator's existing backend, the shim need not choose an indexer base (lightwalletd vs Zaino) near-term. That decision is deferred, below.

## Near-term status

**This table is the one place status is maintained.** Elsewhere the book marks a mechanism *(deployed)*, *(built, not deployed)* or *(designed)* and links here. Three states, meaning: **deployed** runs in production today; **built, not deployed** exists in the binaries and passes tests but does not yet run in an attested deploy; **designed** has no code.

| Mechanism | Status | Detail |
|---|---|---|
| Classify and divert Orchard-touching transactions | Deployed | Classify before connect, so a diverted transaction never opens even a TCP connection to the operator's indexer |
| Stateless shim | Deployed | No per-migration state, so a restart or a second instance loses nothing |
| Hub queue, batch, flush on cadence | Deployed | |
| `GetTransaction` served by the hub | Deployed | Address-level queries still reach the operator |
| Reproducible StageX build, both binaries | Deployed | CI-checked on every change; **PCR2 only**, see the gap below |
| Attested Nitro enclaves | Deployed | Shielded Labs' own indexers since 2026-08-01; first third-party operator 2026-08-10 |
| In-enclave TLS termination | Deployed | Landed 2026-08-05, so the TLS key is enclave-born |
| Nym transport | Built, not deployed | Linked into both binaries, proven end to end over a local mixnet; never run on the public mixnet |
| Multi-hub failover | Partly built | Address rotation within one request exists on the mixnet transport; holding a migration across requests does not |
| STEVE handshake | Designed | |
| Encrypt-to-hub-key layer | Designed | |
| Keymaker quorum, consortium governance | Designed | Launch stands the hub up under a single trusted entity |
| Confirmation tracking and re-submit | Designed | Nothing tracks whether a flushed batch was mined |

On **2026-08-11** a real Orchard to Ironwood migration traversed the full stack on mainnet: held at the shim, batched at the hub, published on the cadence, with the operator's indexer never seeing it.

**The honest gaps that remain.** That mainnet run proved the mechanics and content privacy end to end, but at today's adoption the batch was **size one**, so it does not yet prove batching *anonymity*; that needs many migrations in one flush window (see [honest limits](./trust.md)). Attestation reproducibility covers the application binary but not the EnclaveOS base image and kernel (PCR0 and PCR1), so `caution verify` cannot yet establish the whole stack (see [review](./review.md)). And two things block deploying the Nym transport in an attested enclave: the hub's Nym address is minted per client build and written only to a log the enclave does not expose, and gateway selection cannot be pinned to the locked egress allowlist.

## V2 status: designed, platform-unblocked, transport-validated

As of the 2026-07-30 V2 sync, the three hardest gates of the indexer + Nym + TEE version are answered.

**Designed.** The full wallet-facing private indexer, serving queries (not just broadcasts) over Nym, terminated inside an attested enclave. The in-enclave Nym integration has since been built for the near-term system; what remains is operating it on the public mixnet and clearing the two attested-deploy blockers above. The TEE substrate, an attested Zebra + Zaino testnet enclave built reproducibly with StageX, is already live and synced to tip on testnet.

**Platform-unblocked.** Caution (via Anton) answered every platform question the design depended on:

- **Attestation binding is achievable**, three ways: the STEVE handshake, injecting the enclave pubkey via `metadata.json` -> `user_data`, or a new runtime `arbitrary_data` field Caution would add. Mechanics are in [trust](./trust.md).
- **Key persistence** across cold boots and software upgrades is solved by a keymaker/locksmith **M-of-N quorum across 3-4 orgs**, which survives upgrades (unlike KMS-seal-to-PCR, which breaks on upgrade) and gives the service a stable address. How it differs from STEVE is in [trust](./trust.md).
- **Egress just works** (broad NAT), so an enclave reaching the outside network is not a blocker.

**Transport-validated.** Rehearsed, not modeled: nym-proxy, built from `nymtech/nym`, carried our actual `CompactTxStreamer` gRPC over the **live Nym mainnet mixnet** against the live testnet enclave, end to end. Performance was roughly 10x slower than clearnet (unary calls ~9-10s, `GetBlockRange` ~19 blocks/s), latency-bound and warming over the first one or two calls, fine for non-time-sensitive migrations. Nym mainnet uses ticketbook ecash credentials, so the Nym client needs Nyx-RPC egress (`rpc.nymtech.net:443`). That rehearsal used a standalone proxy pair; the shipped transport instead links `nym-sdk` into each binary, so those numbers bound the mixnet's cost, not the current code path.

The residual platform questions are the Caution agenda in [review](./review.md).

## V3 (PIR): not redundant with the TEE

The objection that PIR adds nothing once queries run inside an attested enclave the hypervisor cannot inspect ("a TEE the hypervisor cannot inspect is functionally PIR") was raised and corrected on the call. PIR is not redundant, for three reasons:

- **Different trust roots.** V2 is private only *if you trust AWS and the hardware*; V3/PIR is private *via math*, hardware-independent, removing the manufacturer and platform from the trust base. PIR is the trust-root-removal step.
- **Different leaks.** A Nitro enclave still has structural access-pattern and IO-pattern leaks when its state is parent-mediated at mainnet scale: the pattern of which records it touches is observable even when the contents are not. PIR closes exactly those, the server does not learn which record was fetched.
- **Complementary failure modes.** A TEE fails on hardware-manufacturer or physical-boundary compromise; PIR fails on cryptographic or software flaws. Disjoint reasons, so the endgame uses **both**, PIR layered on top of the TEE rather than instead of it.

The PIR building blocks under consideration (SimplePIR/DoublePIR, FrodoPIR, YPIR, ChalametPIR) are in the [glossary and references](./glossary.md). PIR is a later layer, off the near-term critical path.

## Deferred items

Real parts of the vision, held back so they never landed on the launch critical path.

**The query-only / broadcast-only binary split.** Taylor's proposal: one attested instance proves it only answers queries and refuses broadcasts, a separate flavor only accepts broadcasts and refuses queries, so neither can correlate a client's reads with its writes. The near-term shim already realizes a scoped version for turnstile crossings. The general split is deferred because wallets today assume a single endpoint, so requiring two is an adoption cost with no near-term payoff.

**The attested Nym fleet.** Caution's planned global network of TEE-enabled Nym nodes (South Africa, Chicago, Brazil, Singapore, mirroring their DNS Cedar deployment), for a healthier public mixnet and broader adoption. Deprioritized because the near-term system routes Nym only between shim and hub, so users and wallets never touch Nym directly, and a better public Nym network is no longer the first thing to build.

**The indexer-base decision (lightwalletd vs Zaino).** The shim sits in front of whatever backend the operator already runs, so no base has to be chosen now. It re-emerges only when we build a first-party indexer for the deferred query-privacy product; then the standalone analysis (which leans toward lightwalletd) and the repo's existing PIR-platform designation come back into play.

**Two more, from launch-scope discipline.** Full **consortium key governance** (Caution, Nym, Shielded Labs, and the Zcash Foundation collectively attesting) is the long-term trust-distribution goal, but launch stands up the hub under a single trusted entity first, consortium to follow. And **Option A**, a standalone privacy server users must point their wallets at, was set aside for the drop-in shim, because changing wallet endpoint URLs has historically been nearly impossible to get adopted.
