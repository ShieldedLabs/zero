# Threat model

This chapter states, honestly, what the near-term Zeronym system (the
[zero-indexer-shim](./shim.md) plus the [zero-indexer-hub](./hub.md), with Nym between
them) does and does not protect. It is deliberately narrow. It covers the **migration
broadcast path**, the one thing the near-term system is built to defend, and it is
explicit about everything that passes through unprotected.

The model reflects Zooko's revisions from the 2026-07-30 all-hands. Earlier drafts
distinguished "query hidden from operator" from "query hidden from indexer"; those rows
were redundant and were replaced with explicit broadcast rows. Throughout,
**verifiable** means the wallet can check the property cryptographically via
attestation, not merely trust that it holds. See [architecture](./architecture.md) for
the encryption layers that back these claims and [trust](./trust.md) for how
attestation works.

## The migration-broadcast threat table

The table compares the migration broadcast as it works today (clearnet, straight to the
operator) against the near-term Zeronym system.

| Property (migration broadcast) | Today (clearnet) | Zeronym (shim + hub) |
|---|---|---|
| Migration tx contents hidden from the **operator** | No | Yes (encrypted; the TEE shim keeps the operator blind) |
| Migration broadcast **linkable to source IP** | Yes, linkable | No (Nym shim-to-hub, plus batching) |
| **Timing** of the broadcast correlatable to an exposed IP | Yes | No (batched, simultaneous publish breaks the link) |
| Migration tx contents hidden from the **hub** | n/a | Yes (the hub is an attested TEE; Caution stays blind) |
| Guarantee is **verifiable** by the wallet | No | Yes (attested shim + hub) |
| **Query** *content* privacy (which addresses you look up) | No | **No** (content passes through; but requester IPs are blinded, see below) |

Row by row:

- **Contents hidden from the operator.** Today the operator terminates the wallet's TLS
  and reads the raw migration transaction. Under Zeronym the wallet's connection
  terminates inside the shim's enclave (a TEE), and the migration is re-encrypted to
  the hub's key before it leaves the operator host, so the operator never sees the
  cleartext. See [architecture](./architecture.md) for the three encryption layers.
- **Linkable to source IP.** Today the operator (and any on-path observer) sees the
  source IP that submitted the broadcast. Under Zeronym the migration travels from shim
  to hub over the Nym mixnet, and is published by the hub, so the source IP is unlinked
  from the on-chain transaction.
- **Timing correlatable.** Today the broadcast arrives at a moment the operator
  records, and that moment lines up with the on-chain appearance. Under Zeronym the hub
  accumulates migrations from every shim and publishes them together on a block
  cadence, so the on-chain publish time no longer matches any one wallet's submission
  time.
- **Contents hidden from the hub.** The hub is a new counterparty, so this row has no
  "today" analogue. The hub is itself an attested TEE: only the attested hub software
  decrypts migrations, to batch and publish them, and the hub's host operator (Caution
  at launch) stays blind. See [the hub](./hub.md) and [trust](./trust.md).
- **Verifiable by the wallet.** Today the wallet has no cryptographic way to check any
  of this. Under Zeronym both the shim and the hub publish attestations, so the
  properties can be verified rather than trusted. See [trust](./trust.md) for the
  attestation and reproducible-build mechanics and
  [open questions](./open-questions.md) for what is still being confirmed.
- **Query privacy.** Unchanged. The system does not protect which addresses a wallet
  looks up. This is the honesty anchor; the section below covers it.

## Naive vs Nym-aware wallets

Most wallets today do not speak Nym. They reach the shim over ordinary TLS, so the
model has to hold even for a wallet that does nothing special.

- For a **naive TLS wallet**, TLS terminates inside the shim's enclave, so the operator
  cannot read the migration transaction. But the operator's own network still observed
  that "IP X connected at time T." What protects that wallet is the **batching at the
  hub**: by holding the migration and co-publishing it with others after a delay, the
  hub ensures the operator cannot time-match "IP X active at T" against the on-chain
  migration. This is precisely why the shim must be a TEE (to blind the operator to the
  contents) and why the hub must batch (to break the timing link for the majority of
  wallets that cannot hide their own IP).
- A future **Nym-aware wallet** could encrypt the migration end-to-end to the hub key
  and route it itself, so the shim would only forward, not decrypt. The near-term
  design is built so that path drops in later; near-term wallets are assumed naive.

The dependence on batching is also the model's soft spot: batch-timing anonymity is
only as strong as how many migrations land in a single flush window. The robust,
volume-independent win is the IP unlinking from Nym; the batching is an additional
layer whose strength varies with migration density. [Honest limits](./limits.md)
treats this in full.

## The query path: content passes through, but requester IPs are blinded

The near-term system does not hide query *content*. The following reach the operator's
existing backend in the clear, exactly as today:

- **All queries.** Which addresses or block ranges a wallet looks up still goes straight
  to the operator's backend. The ZIP 307 query-content leak (see
  [the problem](./problem.md)) is not closed near-term; it is the deferred vision (see
  [the roadmap](./roadmap.md)).
- **Deshields and shields.** These are turnstile crossings the classifier detects but
  does not batch near-term (a deshield is time-sensitive commerce; a shield is already
  privacy-positive because the transparent side is public), so their broadcast metadata
  leaks as today.
- **All other broadcasts.** Transparent-to-transparent and pure intra-pool shielded
  payments are not crossings at all; they pass through instantly.

The classifier is general (it detects every crossing), so the batched set is a policy
knob that can widen later; near-term only the migration case is isolated and batched.

There is, however, one query-path protection, and it is worth stating precisely.
**The operator's indexer is blinded to requester IPs.** Because the shim proxies, every
query reaches the operator's backing lwd from the shim, on the operator's own host, not
from the wallet, so the operator's indexer logs no longer bind a source IP to a queried
address. Because the shim is attested, "we do not log the IP" is verifiable rather than
promised. This removes the passive, default IP-logging surface present in every lwd today.

The honest limit on that: the wallet's IP still reaches the operator's *host* at the TCP
layer (on Nitro the parent proxies all network into the enclave), and attestation covers
the shim, not the parent. So a bad-faith operator can still capture IPs at the network
layer and timing-correlate them against the shim-sourced query stream to re-link IP to
query. It is a verifiable removal of the *default* leak, not a guarantee against an
*active* operator; closing that gap needs the wallet over Nym (Nym-aware wallets) or
query-timing shaping. [Honest limits](./limits.md) owns the residual discussion; see also
[the overview](./overview.md) for the attested-edge framing and [the shim](./shim.md) for
the classifier.

## The residual: the operator learns *that* a client migrated

One leak survives by construction, and the threat model names it rather than hide it.
Because the shim is a drop-in in front of the operator's own backend, a migration is the
single request the shim does **not** forward to the backing lwd, so an operator watching
its own traffic can infer *that* a given source IP submitted a migration, and roughly
when. It does **not** learn *which* on-chain transaction or *what amount*: the hub's
batch mixes that client's migration with others' from operators it never sees. So the
residual is "IP X migrated something," not "IP X migrated amount Y," which is why the
table's source-IP row reads No for the amount even though the bare fact leaks.

This residual is inherent to the drop-in model, and it is why shim-side batching and
shim-to-hub cover traffic are rejected as mitigations. [Honest limits](./limits.md) is
the single owner of that argument, along with the anonymity-set dependence, delayed
broadcast, and the AWS-not-math trust root.
