# Overview

The near-term Zeronym system protects one thing: the broadcast of a turnstile-crossing transaction, starting with the Orchard to Ironwood migration. It leaves everything else, including all queries, exactly as it is today. [Threat model](./threat-model.md) is the precise statement of what is and is not protected; this chapter is the shape of the system and why it is shaped this way.

## The system at a glance

It is two new pieces of attested software plus a transport:

- **zero-indexer-shim (ZIS)**: a lightweight, attested router each light-wallet operator deploys behind its existing public URL (for example `zec.rocks:443`). To every wallet it looks exactly like the indexer that was already there, so no wallet has to change anything. It forwards almost all traffic, untouched, to the operator's own unmodified backing indexer, and isolates only the migration case.
- **zero-indexer-hub (ZIH)**: a central, attested batching service, run as two or more instances with failover. It accumulates migrations from every shim on the network, holds them, and publishes them to the Zcash network together on a strict block cadence.
- **Nym** runs only between the shim and the hub.

The [architecture](./architecture.md) chapter has the full data-flow and trust diagrams and the three encryption layers. [The shim](./shim.md) and [the hub](./hub.md) are the component engineering designs. This chapter stays at the level of what the system is.

## Two paths through the shim

Traffic splits at the shim:

- **Non-crossing traffic passes straight through.** Every query, and every broadcast that does not cross a value-pool boundary (transparent-to-transparent payments, pure intra-pool shielded payments), goes to the operator's existing backend instantly, visible to the operator in the clear, exactly as today. The shim does not index it and does not delay it.
- **Migrations are isolated.** A migration (a cross-pool shielded move, for example Orchard to Ironwood) is encrypted to a key the local operator cannot access, and routed over Nym to a hub. The hub batches it with migrations from other operators and publishes the batch simultaneously after a short delay. Because the publish is delayed and co-published with others, an observer holding "IP X connected at time T" cannot time-match it to the migration when it later appears on-chain.

The shim classifies every turnstile crossing (deshields, shields, and migrations alike), but near-term it batches only migrations; deshields and shields pass through like other traffic. The classifier is general on purpose, so the batched set is a policy knob that can widen later without re-architecting.

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

In effect the shim realizes a scoped version of the "decouple broadcast from query" idea: the crossing broadcast is split off from the operator entirely and sent to a different counterparty, the hub, while queries stay with the operator as before. [The shim](./shim.md) and [the hub](./hub.md) document how each piece is built, and [Trust: TEE, STEVE, and the quorum](./trust.md) covers why the enclave is what makes operator-blindness real and checkable rather than merely promised.
