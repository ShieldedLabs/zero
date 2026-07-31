# Introduction

Zeronym is privacy for Zcash light wallets. It is a Shielded Labs product, named by Jason McGee Stramaglia: a play on "pseudonym," zero + nym.

The name encodes the two pillars the full product stands on:

- **zero**: zero-leak indexing. A light wallet should be able to sync and transact without handing an indexer the raw material to deanonymize it.
- **nym**: the [Nym](./glossary.md) mixnet, the transport that unlinks a wallet's traffic from its source IP address and region.

Today a Zcash light wallet leaks. Under the ZIP 307 light-client protocol it talks to an indexer over clearnet, so the operator running that indexer sees the wallet's source IP and the timing of everything it does. [Light-client IP leakage](./problem.md) lays out that leak and its worst instance, the Orchard to Ironwood migration, which Zooko called the worst privacy-loss event in Zcash history.

## Near-term and long-term

This book documents two efforts that share a name and a direction but not a scope.

**The near-term system** is an urgent, narrowly scoped deliverable: stop turnstile-crossing transactions, starting with the Orchard to Ironwood migration, from leaking a user's IP. It targets a soft ~Aug 10 deadline (a joint Nym and Shielded Labs blog post promised a mechanism by then). It is two small attested binaries, a drop-in shim at each operator and a central batching hub, with Nym between them. It is deliberately an 80% first step, honest about the 20% it does not cover.

**The long-term vision** is the fuller product the name promises: a wallet-facing private indexer that serves queries, not just broadcasts, over Nym, terminated inside an attested enclave, with PIR added later as a hardware-independent layer. That arc is deferred until the urgent migration fix ships. [Roadmap](./roadmap.md) traces the three versions (V1 Nym, V2 +TEE, V3 +PIR) and what is already de-risked.

Most of this book is about the near-term system, because that is what is being built now.

## How to read this book

- Start with [Light-client IP leakage](./problem.md): the leak, the migration event, and the adversaries.
- [Overview](./overview.md) is the near-term system at a glance: the drop-in shim, the batching hub, and why a lightweight router beats putting a whole indexer in an enclave.
- [Threat model](./threat-model.md) is the precise statement of what the migration-broadcast path protects and what stays out of scope.
- [Architecture](./architecture.md) holds the data-flow and trust diagrams and the three encryption layers.
- The two component chapters, [the shim](./shim.md) and [the hub](./hub.md), are the engineering designs.
- [Trust: TEE, STEVE, and the quorum](./trust.md) is the deep dive on Nitro attestation, the STEVE handshake, and the key quorum; [Honest limits](./limits.md) states plainly what the system does not do.
- [Roadmap](./roadmap.md) and [Open questions for Caution](./open-questions.md) look forward; the [Glossary and references](./glossary.md) collects the terms and sources.

The book is written in the Zcash design-book tradition: clean, direct, and honest about its limits. Where the near-term system stops short of the vision, it says so.
