# Roadmap

The near-term shim + hub system is the first step toward a fuller private indexer, not the whole of it. This chapter is the horizon behind it: layers that come after, not blockers on it.

## The three-version vision

The long-term product is a full wallet-facing private indexer: not just broadcasts, but the queries a light wallet makes (which addresses it looks up) served privately. Three strictly additive versions, each adding one trust-reducing layer:

| Version | Adds | What it gives | Trust root |
|---|---|---|---|
| **V1** | indexer + Nym | Queries and broadcasts served over the Nym mixnet, so the indexer never sees a client's source IP or timing. | The indexer operator (who still sees query contents in cleartext). |
| **V2** | + TEE (RA-TLS) | The indexer runs inside an attested enclave; the transport terminates inside it (remote-attestation TLS), so the operator is blind to contents and the wallet can verify this cryptographically. | AWS Nitro and the hardware manufacturer. |
| **V3** | + PIR | Private information retrieval serves queries without the server learning which record was fetched, closing the access-pattern leaks a TEE structurally has. | Cryptography (hardware-independent). |

The ladder walks the trust root down: operator (V1), hardware manufacturer (V2), only math (V3). V2 keeps Nym and adds the enclave; V3 keeps both and adds PIR as defense in depth, not a replacement (why it is not redundant is below).

## Where the near-term system sits: the zeroith step

The [shim + hub system](./architecture.md) is not V1. The ladder is full query privacy; the near-term system covers what the ladder does not urgently reach, the turnstile-crossing **broadcast** leak, driven by the mandatory Orchard to Ironwood migration. So it sits ahead of the ladder rather than on it: the **zeroith step**.

It is already a partial down-payment. Transaction-detail lookups (`GetTransaction`) are served by the hub's own indexer, so the query leak is no longer entirely future, though address-level queries still reach the operator. It uses the same machinery on a narrower surface (both components are Nitro enclaves, and Nym is staged on the shim-to-hub hop), so building it de-risks the full indexer and vice versa. And it is a deliberate 80% first step: IP unlinking for the migration is the bulk of the practical privacy at stake in the acute window, while the rest of query privacy and the shield and non-Orchard deshield cases are the remaining margin, which costs far more to close.

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

**Platform-unblocked.** Caution answered every platform question the design depended on. Attestation binding is achievable three ways (the STEVE handshake, the enclave pubkey injected via `metadata.json` into `user_data`, or a new runtime `arbitrary_data` field). Key persistence across cold boots and upgrades is solved by a keymaker/locksmith **M-of-N quorum across 3-4 orgs**, which survives upgrades where KMS-seal-to-PCR does not, and gives the service a stable address. Egress just works (broad NAT). Mechanics for all three are in [trust](./trust.md).

**Transport-validated.** Rehearsed, not modeled: nym-proxy carried our actual `CompactTxStreamer` gRPC over the **live Nym mainnet mixnet** against the live testnet enclave, end to end, at roughly 10x slower than clearnet (unary calls ~9-10s, `GetBlockRange` ~19 blocks/s), which is fine for non-time-sensitive migrations. Nym mainnet uses ticketbook ecash credentials, so the client needs Nyx-RPC egress (`rpc.nymtech.net:443`). That rehearsal used a standalone proxy pair, so its numbers bound the mixnet's cost rather than the shipped code path, which links `nym-sdk` into each binary.

The residual platform questions are the Caution agenda in [review](./review.md).

## V3 (PIR): not redundant with the TEE

PIR is sometimes assumed redundant once queries run inside an attested enclave the hypervisor cannot inspect. It is not, for three reasons:

- **Different trust roots.** V2 is private only *if you trust AWS and the hardware*; V3/PIR is private *via math*, hardware-independent, removing the manufacturer and platform from the trust base. PIR is the trust-root-removal step.
- **Different leaks.** A Nitro enclave still has structural access-pattern and IO-pattern leaks when its state is parent-mediated at mainnet scale: the pattern of which records it touches is observable even when the contents are not. PIR closes exactly those, the server does not learn which record was fetched.
- **Complementary failure modes.** A TEE fails on hardware-manufacturer or physical-boundary compromise; PIR fails on cryptographic or software flaws. Disjoint reasons, so the endgame uses **both**, PIR layered on top of the TEE rather than instead of it.

The PIR building blocks under consideration (SimplePIR/DoublePIR, FrodoPIR, YPIR, ChalametPIR) are in the [glossary](./glossary.md#pir-private-information-retrieval). PIR is a later layer, off the near-term critical path.

## Deferred items

Real parts of the vision, held back so they never landed on the launch critical path. One documentation item belongs here too: a concern-by-concern coverage matrix against Taylor's wallet app threat model, showing which entries Zeronym closes and which stay the wallet's ([review](./review.md) states the claimed boundary).

**The query-only / broadcast-only binary split.** Taylor's proposal: one attested instance proves it only answers queries and refuses broadcasts, a separate flavor only accepts broadcasts and refuses queries, so neither can correlate a client's reads with its writes. The near-term shim already realizes a scoped version for turnstile crossings. The general split is deferred because wallets today assume a single endpoint, so requiring two is an adoption cost with no near-term payoff.

**The attested Nym fleet.** Caution's planned global network of TEE-enabled Nym nodes (South Africa, Chicago, Brazil, Singapore, mirroring their DNS Cedar deployment), for a healthier public mixnet and broader adoption. Deprioritized because the near-term system routes Nym only between shim and hub, so users and wallets never touch Nym directly, and a better public Nym network is no longer the first thing to build.

**The indexer-base decision (lightwalletd vs Zaino).** The shim sits in front of whatever backend the operator already runs, so no base has to be chosen now. It re-emerges only when we build a first-party indexer for the deferred query-privacy product.

**Two more, from launch-scope discipline.** Full **consortium key governance** is the long-term trust-distribution goal, but launch stands the hub up under a single trusted entity ([trust](./trust.md) treats that concentration honestly). And **Option A**, the standalone privacy server, was set aside for the drop-in shim ([architecture](./architecture.md) has the reasoning).
