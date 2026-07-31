# Overview

The near-term Zeronym system is easy to undersell as protecting one thing, the broadcast of a migration transaction. What it really does is put an **attested, verifiable, tamper-proof front-end at every operator**, the shim running in a TEE, with attested batching hubs behind it. On that primitive it delivers two protections today (both below), and it is the foundation for the rest of the [roadmap](./roadmap.md). This chapter is the shape of the system; [threat model](./threat-model.md) is the precise statement of what is and is not protected.

## The system at a glance

It is two new pieces of attested software plus a transport:

- **zero-indexer-shim (ZIS)**: a lightweight, attested router each light-wallet operator deploys behind its existing public URL (for example `zec.rocks:443`). To every wallet it looks exactly like the indexer that was already there, so no wallet has to change anything. It forwards almost all traffic, untouched, to the operator's own unmodified backing indexer, and isolates only the migration case.
- **zero-indexer-hub (ZIH)**: a central, attested batching service, run as two or more instances with failover. It accumulates migrations from every shim on the network, holds them, and publishes them to the Zcash network together on a strict block cadence.
- **Nym** runs only between the shim and the hub.

The [architecture](./architecture.md) chapter has the full data-flow and trust diagrams and the three encryption layers. [The shim](./shim.md) and [the hub](./hub.md) are the component engineering designs. This chapter stays at the level of what the system is.

## Two paths through the shim

Traffic splits at the shim:

- **Non-crossing traffic passes straight through.** Every query, and every broadcast that does not cross a value-pool boundary (transparent-to-transparent payments, pure intra-pool shielded payments), goes to the operator's existing backend instantly. The shim does not index it and does not delay it. The operator's backend still sees the *contents*, but note it now sees them arriving from the shim, not from the wallet's IP (see the next section).
- **Migrations are isolated.** A migration (a cross-pool shielded move, for example Orchard to Ironwood) is encrypted to a key the local operator cannot access, and routed over Nym to a hub. The hub batches it with migrations from other operators and publishes the batch simultaneously after a short delay. Because the publish is delayed and co-published with others, an observer holding "IP X connected at time T" cannot time-match it to the migration when it later appears on-chain.

The shim classifies every turnstile crossing (deshields, shields, and migrations alike), but near-term it batches only migrations; deshields and shields pass through like other traffic. The classifier is general on purpose, so the batched set is a policy knob that can widen later without re-architecting.

## What the attested edge protects

Two protections hold today, on top of the deployment primitive itself.

**1. Migration broadcasts, fully.** A migration is encrypted to the hub key, routed over Nym, and published in a cross-operator batch. Its content is hidden from both the operator and the hub host, its source IP is unlinked, and its timing is broken. This is the strong, end-to-end guarantee. The one residual, that the operator can tell *that* one of its clients migrated but not the amount, is stated in [honest limits](./limits.md).

**2. The operator's indexer is blinded to requester IPs, by default and verifiably.** Because the shim proxies, every query reaches the operator's backing lwd from the shim, on the operator's own host, never from a wallet's IP. So the operator's indexer logs no longer bind a source IP to a queried address, the linkage that sits in every lwd's logs today by default. And because the shim is attested, "we do not log the IP" is a checkable property, not a promise. This removes the *passive* IP-logging surface, which is where most real-world risk lives: breaches, subpoenas, careless or sold logs.

The honest boundary on protection 2: the wallet's IP still reaches the operator's *host* at the TCP layer. On AWS Nitro the enclave has no network of its own, so the parent instance terminates the socket and proxies the bytes into the enclave; the parent sees the source IP even though it cannot read the encrypted content. Attestation covers what the shim does, not what the parent does. So a bad-faith operator could still run packet capture on the parent and timing-correlate it against the shim-sourced query stream to re-link IP to query. Protection 2 is a verifiable removal of the *default* leak, not a cryptographic guarantee against an *active* operator; closing that gap needs the wallet to arrive over Nym (Nym-aware wallets) or query-timing shaping in the shim. See [threat model](./threat-model.md) and [honest limits](./limits.md).

Beyond these two, the attested front-end is **tamper-proof and verifiable**: a wallet or auditor can confirm it is talking to exactly the attested shim code, not an operator-controlled impostor or a secretly modified front-end (see [trust](./trust.md)). And it is the **deployment vehicle for the vision**: query shaping, all-broadcast privacy, and eventually PIR can be added to the same attested edge and reach unmodified drop-in wallets, without asking them to change (see [roadmap](./roadmap.md)).

## Migrations only: Option B, the "zeroith step"

Migrations first, and only migrations, is a deliberate choice. The Orchard to Ironwood migration is the acute, mandatory, mass event, and it is not time-sensitive, which is exactly when batching helps most (a large simultaneous population to hide among) and costs the least (no urgency to broadcast). Deshields are time-sensitive commerce; shields are privacy-positive already, since the transparent side is public. So neither is batched near-term.

The design is the all-hands call's "Option B": a drop-in shim in front of each operator plus central batching hubs, chosen after a more decentralized "Option C" was set aside. ("Option A," a standalone privacy server users must point their wallets at, is deferred: past experience says getting wallets to change their endpoint URL is nearly impossible.)

The scope is intentionally minimal, the "zeroith step":

- Nym only between shim and hub, not wallet-to-shim.
- Migrations only (the classifier is general, but only migrations are batched).
- At least two attested hubs with shim failover, so a hub outage never stalls migrations.

Everything else (the attested Nym fleet, the standalone privacy server, the query-only/broadcast-only split, PIR, and full consortium key governance) is post-launch. [Roadmap](./roadmap.md) covers the deferred items; [Honest limits](./limits.md) states what this narrow scope does and does not buy.

## Why the shim, not the whole indexer in a TEE

An earlier plan put the entire indexer (a full Zebra node plus the indexer) inside the enclave, so the operator could see nothing at all. That is expensive. Until the enclave platform ships disk support, it runs entirely in RAM at roughly 400 to 500 GB, on the order of $2,000 per operator per month, with about a four-day resync on every restart. That cost wall makes operator adoption unrealistic.

The shim avoids all of it by being a thin router, not an indexer:

- **Cheap and fast to restart.** No heavy chain state lives inside the TEE, so the RAM and cost wall disappears and restarts are quick.
- **Base-agnostic.** The shim sits in front of whatever the operator already runs and passes normal traffic through, so it sidesteps the lightwalletd-versus-Zaino question entirely for the near term. Operators keep their existing backend.
- **Deployable by the people who already run the infrastructure.** The roughly five to ten existing light-wallet operators add the shim; users and wallets do not have to change their endpoint URL.

In effect the shim realizes a scoped version of the "decouple broadcast from query" idea: the crossing broadcast is split off from the operator entirely and sent to a different counterparty, the hub, while queries still go to the operator's own backend, now blinded to the wallet's IP (above). [The shim](./shim.md) and [the hub](./hub.md) document how each piece is built, and [Trust: TEE, STEVE, and the quorum](./trust.md) covers why the enclave is what makes operator-blindness real and checkable rather than merely promised.
