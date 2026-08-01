# Introduction

Zeronym is privacy for Zcash light wallets, a Shielded Labs product named by Jason McGee Stramaglia: a play on "pseudonym," zero + nym. The name encodes two pillars:

- **zero**: zero-leak indexing. A light wallet should sync and transact without handing an indexer the raw material to deanonymize it.
- **nym**: the [Nym](./glossary.md) mixnet, the transport that unlinks a wallet's traffic from its source IP and region.

Today a Zcash light wallet leaks. Under the ZIP 307 light-client protocol it talks to an indexer over clearnet, so the operator sees the wallet's source IP and the timing of everything it does. [The problem and threat model](./problem.md) lays out that leak and its worst instance, the Orchard to Ironwood migration, which Zooko called the worst privacy-loss event in Zcash history.

## Near-term and long-term

Two efforts share a name and a direction but not a scope.

**The near-term system** is urgent and narrowly scoped: stop turnstile-crossing transactions, starting with the Orchard to Ironwood migration, from leaking a user's IP. It targets a soft ~Aug 10 deadline (a joint Nym and Shielded Labs blog post promised a mechanism by then). It is two small attested binaries, a drop-in shim at each operator and a central batching hub with Nym between them, deliberately an 80% first step, honest about the 20% it does not cover. The first of those binaries now exists as a proof of concept: the shim proxies transparently and classifies migrations with the real `zebra-chain` parser, but non-destructively, it logs a detected migration and still forwards it. Diversion, the hub, Nym, TLS, the reproducible build, the enclave and its attestation are not built yet (see [the shim and the hub](./components.md)).

**The long-term vision** is the fuller product the name promises: a wallet-facing private indexer serving queries, not just broadcasts, over Nym, terminated inside an attested enclave, with PIR added later as a hardware-independent layer. That arc is deferred until the migration fix ships. [Roadmap](./roadmap.md) traces the three versions (V1 Nym, V2 +TEE, V3 +PIR). Most of this book is the near-term system, because that is what is being built now.

## The system at a glance

Two new pieces of attested software plus a transport put an **attested, verifiable, tamper-proof front-end at every operator**, on which the two protections below rest and the whole [roadmap](./roadmap.md) builds:

- **zero-indexer-shim (ZIS)**: a lightweight, attested router each operator deploys behind its existing public URL (for example `zec.rocks:443`). To every wallet it looks exactly like the indexer already there, so wallets need no reconfiguration and no new endpoint URL (they do need one thing, aligned anchors and expiry within a migration epoch, see [the problem](./problem.md)). It forwards almost all traffic, untouched, to the operator's own unmodified backing indexer, and isolates only transactions that move value **out of the Orchard pool**. Everything else (every query, and every broadcast that leaves Orchard untouched: transparent payments, intra-pool shielded payments, shields, deshields from other pools) passes straight through instantly, not indexed and not delayed; the backend still sees the *contents*, but arriving from the shim, not the wallet's IP.
- **zero-indexer-hub (ZIH)**: a central, attested batching service, run as two or more instances with failover. An Orchard exit (typically the Orchard to Ironwood migration) is encrypted to a key the local operator cannot access, routed over Nym to a hub, batched with migrations from every other shim, and co-published to the Zcash network on a strict block cadence after a short delay, so an observer holding "IP X connected at time T" cannot time-match it to the migration when it later appears on-chain.
- **Nym** runs only between the shim and the hub.

[Architecture](./architecture.md) has the full data-flow and trust diagrams and the three encryption layers; [The shim and the hub](./components.md) is the component engineering design.

## What the attested edge protects

Two protections hold today, on top of the deployment primitive itself.

**1. Migration broadcasts, fully** (and every other Orchard exit, which gets the same treatment). As above, a migration's content is hidden from both the operator and the hub host, its source IP is unlinked, and its timing is broken: the strong, end-to-end guarantee. The one residual, that the operator can tell *that* one of its clients migrated but not the amount, is stated in [honest limits](./trust.md).

**2. The operator's indexer is blinded to requester IPs, by default and verifiably.** Because the shim proxies, every query reaches the operator's backing lwd from the shim, on the operator's own host, never from a wallet's IP. So the operator's indexer logs no longer bind a source IP to a queried address, the linkage that sits in every lwd's logs today by default. And because the shim is attested, "we do not log the IP" is a checkable property, not a promise. This removes the *passive* IP-logging surface, where most real-world risk lives: breaches, subpoenas, careless or sold logs.

The honest boundary on protection 2: the wallet's IP still reaches the operator's *host* at the TCP layer (on Nitro the parent proxies all traffic into the enclave, and attestation covers the shim, not the parent), so it removes the *default* leak, not an *active* operator who packet-captures and timing-correlates. [The problem and threat model](./problem.md) develops this in full.

Beyond the two, the front-end is **tamper-proof and verifiable**: a wallet or auditor can confirm it is talking to exactly the attested shim code, not an operator-controlled impostor (see [trust](./trust.md)). And it is the **deployment vehicle for the vision**: query shaping, all-broadcast privacy, and eventually PIR can be added to the same attested edge and reach the same drop-in wallets.

## Orchard exits only: Option B, the "zeroith step"

Orchard first, and only Orchard, is deliberate. The Orchard to Ironwood migration is the acute, mandatory, mass event, and it is not time-sensitive, exactly when batching helps most (a large simultaneous population to hide among) and costs the least (no urgency to broadcast). But the batched class is drawn one step wider than "migration": **every** transaction moving value out of Orchard is batched, whatever pool the value lands in. That is Zooko's rule, and the reason is that NU6.3 closes Orchard to new value, so anyone still spending Orchard is spending legacy funds and the spend itself is the identifying event, whatever its destination ([the shim](./components.md) has the predicate and the argument). Shields and deshields from other pools still pass straight through: a shield is privacy-positive already, since the transparent side is public, and a deshield out of Ironwood or Sapling says nothing about legacy Orchard holdings.

Widening the class widens what is delayed, and the honest accounting is that it costs little. An Orchard deshield to transparent is now held for a flush window like a migration, and deshields are ordinarily time-sensitive commerce. But Orchard is closed to new value, so ordinary commerce lives in Ironwood and passes through untouched; what is left in Orchard is legacy balance, and moving legacy balance is not an urgent errand. The batched set stays a policy knob that can widen or narrow later without re-architecting.

The design is the all-hands call's "Option B": a drop-in shim in front of each operator plus central batching hubs, chosen after a more decentralized "Option C" was set aside. ("Option A," a standalone privacy server users must point their wallets at, is deferred: past experience says getting wallets to change their endpoint URL is nearly impossible.) The scope is intentionally minimal:

- Nym only between shim and hub, not wallet-to-shim.
- Orchard exits only (the classifier detects every turnstile crossing; only exits from Orchard are batched).
- At least two attested hubs with shim failover, so a hub outage never stalls migrations.

Everything else (the attested Nym fleet, the standalone privacy server, the query-only/broadcast-only split, PIR, and full consortium key governance) is post-launch. [Roadmap](./roadmap.md) covers the deferred items; [honest limits](./trust.md) states what this narrow scope does and does not buy.

## Why the shim, not the whole indexer in a TEE

An earlier plan put the entire indexer (a full Zebra node plus the indexer) inside the enclave, so the operator could see nothing at all. That is expensive: until the enclave platform ships disk support, it runs entirely in RAM at roughly 400 to 500 GB, on the order of $2,000 per operator per month, with about a four-day resync on every restart. That cost wall makes operator adoption unrealistic.

The shim avoids it by being a thin router, not an indexer:

- **Cheap and fast to restart.** No heavy chain state lives inside the TEE, so the RAM and cost wall disappears and restarts are quick.
- **Base-agnostic.** It sits in front of whatever the operator already runs, sidestepping the lightwalletd-versus-Zaino question entirely for the near term.
- **Deployable by the people who already run the infrastructure.** The roughly five to ten existing operators add the shim; users and wallets do not change their endpoint URL.

In effect the shim realizes a scoped "decouple broadcast from query": the crossing broadcast is split off from the operator entirely and sent to a different counterparty, the hub, while queries still go to the operator's own backend, now blinded to the wallet's IP. [Trust and honest limits](./trust.md) covers why the enclave makes operator-blindness real and checkable rather than merely promised.

## How to read this book

- [The problem and threat model](./problem.md): the leak, the migration event, the adversaries, and the precise statement of what the migration-broadcast path protects and what stays out of scope.
- [Architecture](./architecture.md): the data-flow and trust diagrams and the three encryption layers.
- [The shim and the hub](./components.md): the engineering design of both components.
- [Trust and honest limits](./trust.md): the deep dive on Nitro attestation, the STEVE handshake, and the key quorum, and what the system does not do.
- [Roadmap](./roadmap.md) and [Open questions and review](./review.md) look forward; the [Glossary and references](./glossary.md) collects the terms and sources.

Written in the Zcash design-book tradition: clean, direct, and honest about its limits. Where the near-term system stops short of the vision, it says so.
