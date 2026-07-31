# Zeronym

**Privacy for Zcash light wallets.** A Shielded Labs product (named by Jason McGee
Stramaglia: a play on "pseudonym", zero + nym).

> **Near-term (this document): the turnstile-privacy system.** An urgent, narrowly
> scoped deliverable that stops turnstile-crossing transactions (starting with the
> Orchard to Ironwood migration) from leaking a user's IP address. Target: a soft
> **~Aug 10** deadline (a joint Nym + Shielded Labs blog post promised a mechanism by
> then).
>
> **Long-term (section 9, deferred): the vision.** A fuller indexer + Nym + TEE +
> PIR product. The near-term system is a deliberate 80% first step toward it, not
> the whole thing.

This document is the strategy spine, written after the 2026-07-30 all-hands. It is
honest about what the near-term system does and, just as importantly, what it does
NOT do. It holds until the threat-model doc (Taylor + Zooko) lands; then we scope
the build.

---

## 1. The urgent problem: turnstile-crossing IP leakage

The acute driver is the Orchard to Ironwood migration. To migrate, a wallet
broadcasts a transaction that moves value out of the Orchard pool. Today that
broadcast goes to a light-wallet indexer over clearnet, so the operator sees the
wallet's **source IP** and the **timing**. The transaction later appears in cleartext
on the public chain, so an operator holding the source IP can retrospectively link
**IP to on-chain transaction to balance**, by timing. Zooko framed this on the call as
the worst privacy-loss event in Zcash history: users linking their IP address to their
Zcash balance, hourly, for the migration window.

**The same leak applies to every turnstile crossing**, not just migrations. Any
transaction that moves value across a pool boundary (deshielding shielded to
transparent, shielding transparent to shielded, or a cross-pool migration) is revealed
on-chain and, linked to a source IP, deanonymizes the user. So the shim's classifier
covers every crossing, but near-term the system **batches (protects) only the Orchard
migration**, the urgent, mass, non-time-sensitive first case that sets the deadline;
deshields and shields pass through for now (section 2).

**Scope discipline:** this is about the migration **broadcast**, not about queries.
Which addresses a wallet looks up is a separate leak, and it is NOT in near-term scope
(section 4 says so plainly). The near-term win is IP protection for the migration, which
the call judged to be the bulk of the practical privacy at stake.

---

## 2. The near-term system: `zero-indexer-shim` + `zero-broadcaster` hub

The chosen design (the call's "Option B", after a more decentralized "Option C" was
set aside): a lightweight attested **shim** in front of each operator's existing
backend, plus attested **hubs** (>=2, with failover) that batch migration broadcasts.
Nym runs only between shim and hubs.

```
   Wallet  (naive TLS today, or Nym-aware later)
     |  gRPC / API
     v
 +------------------------- operator host -------------------------+
 |  zero-indexer-shim   (attested TEE, lightweight router)         |
 |    |                                                            |
 |    |-- query / non-crossing broadcast --> operator's EXISTING   |
 |    |     (t->t, intra-pool shielded):     legacy backend --> Zcash net
 |    |      passes through instantly                              |
 |    |                                                            |
 |    '-- TURNSTILE CROSSING tx (encrypted; operator stays blind) -+--.
 +-------------------------------------------------------------------|-+
                                                                     |  over Nym
                                                            (cover traffic hides
                                                             which region sent it)
                                                                     v
                              zero-broadcaster    (attested TEE, central, Caution-run at launch)
                                 accumulate  ->  batch  ->  publish simultaneously
                                                              (flush every <= ~15 blocks)
                                                                     |
                                                                     v
                                                          Zcash network (P2P relay)
```

**How it works:**

- **Non-crossing traffic passes straight through.** All queries, and broadcasts that
  do not cross a pool turnstile (transparent-to-transparent, and pure intra-pool
  shielded payments), pass through the shim **instantly** to the operator's existing,
  unchanged backend. The shim does not index them and does not delay them.
- **Only migrations are isolated (the classifier is general).** The shim classifies
  every turnstile crossing, but near-term it isolates only the **migration** case (a
  cross-pool shielded move, e.g. Orchard to Ironwood). Deshields and shields pass
  through instantly like other traffic. An isolated migration is **encrypted to a key
  the local operator cannot access** and routed over **Nym** to a hub.
- **The hub batches and publishes.** The `zero-broadcaster` accumulates migrations from
  every shim on the network, holds them, and **publishes them simultaneously** after a
  delay, flushing on a strict block cadence (section 7).
- **"Zeroith step" scope:** Nym only between shim and hub, migrations only, and **at
  least two attested hubs with shim failover** so a hub outage never stalls migrations
  (section 5). Everything else is post-launch (section 8).

Why migrations specifically: the Orchard to Ironwood migration is the acute, mandatory,
mass event, and it is **not** time-sensitive, which is exactly when batching helps most
(a large simultaneous population to hide among) and costs the least (no urgency).
Deshields are time-sensitive commerce and shields are privacy-positive (the transparent
side is already public), so neither is batched near-term. The classifier is general (it
detects every crossing), so the batched set is a policy knob we can widen later without
re-architecting.

---

## 3. Why the shim, not the whole indexer in a TEE

The earlier plan put the entire indexer (Zebra + the indexer) inside the enclave.
That is expensive: until Caution ships enclave disk support, it runs entirely in RAM
at ~400 to 500 GB, roughly **$2,000 per operator per month**, with a ~**4-day
resync** on every restart.

Nate's shim breakthrough avoids all of that. The shim is a thin **router**, not an
indexer:

- **Cheap and fast to restart:** no heavy chain state inside the TEE, so restarts are
  quick and the RAM/cost wall disappears.
- **Base-agnostic:** the shim sits in front of the operator's existing backend and
  passes normal traffic through, so it **sidesteps the lightwalletd-vs-Zaino
  decision entirely** for the near term. Operators keep whatever they run.
- **Deployable by the people who already run the infra:** the ~5 to 10 existing
  light-wallet operators add the shim; users and wallets do not have to change their
  endpoint URL (which past experience says is nearly impossible to get adopted).

The shim is, in effect, a pragmatic realization of Taylor's "decouple broadcast from
query" idea, scoped to crossings: the crossing broadcast is split off from the
operator entirely and sent to a different counterparty (the hub).

---

## 4. Threat model (migration broadcast path)

Focused on the migration broadcast, per Zooko's chart revisions on the call (the old
"query hidden from operator" vs "query hidden from indexer" rows were redundant and
were replaced with explicit broadcast rows). "Verifiable" means the wallet can check
the property cryptographically via attestation, not merely trust it.

| Property (migration broadcast) | Today (clearnet) | Zeronym (shim + hub) |
|---|---|---|
| Migration tx contents hidden from the **operator** | No | Yes (encrypted; the TEE shim keeps the operator blind) |
| Migration broadcast **linkable to source IP** | Yes, linkable | No (Nym shim-to-hub, plus batching) |
| **Timing** of the broadcast correlatable to an exposed IP | Yes | No (batched, simultaneous publish breaks the link) |
| Migration tx contents hidden from the **hub** | n/a | Yes (the hub is an attested TEE; Caution stays blind) |
| Guarantee is **verifiable** by the wallet | No | Yes (attested shim + hub) |
| **Query** privacy (which addresses you look up) | No | **No, out of near-term scope** (queries pass through) |

The last row is the honesty anchor: this system protects the **migration** broadcast,
not general queries, and not deshields or shields (which pass through the shim
untouched near-term). Query privacy is the deferred vision (section 9).

**Naive vs Nym-aware wallets.** Most wallets do not speak Nym, so they reach the shim
over plain TLS: TLS terminates inside the shim-TEE, so the operator cannot read the
migration tx, but the operator's network still saw "IP X connected at time T." The
**batching at the hub is what protects those naive wallets**: by delaying and
co-publishing, the operator cannot time-match "IP X active at T" to the on-chain
migration. This is why the shim must be a TEE (to blind the operator to the contents)
and why the hub must batch (to break the timing link for the majority of wallets).

**One residual, from the drop-in model (Zooko):** the operator running the shim can tell
*that* one of its clients submitted a migration (it is the one request the shim does not
forward to the backing lwd), just not *which* on-chain tx or amount, because the batch
mixes it with migrations from other operators' clients. So "linkable to source IP" is No
for the amount, but the operator does learn the fact of a migration. This is inherent to
the drop-in model and is why shim-side batching / cover traffic are rejected (SHIM-HUB
section 4).

---

## 5. Honest limits and open risks

State these plainly; the blog post should not overclaim.

1. **Batch anonymity depends on migration density, and cannot be widened past expiry.**
   The robust, volume-independent win is IP unlinking via Nym. The *additional*
   batch-timing anonymity is only as strong as how many migrations land in a single
   flush window; a batch of one is not anonymous. During the acute migration window this
   is naturally dense; outside it, the backstop is hub-generated cover traffic. The
   flush cadence is capped by expiry (section 7), so we cannot widen the window to
   compensate. Report the achieved batch size honestly.
2. **A hub outage must not stall migrations (and the hub is concentrated trust).** The
   hub must hold every migration to publish it, and a single hub is a single point of
   failure. The design: (a) run **at least two attested hubs with shim failover**, each
   deduping by txid, so an outage on one does not stop migrations and a rare
   double-publish is a harmless on-chain duplicate; (b) the shim **holds and retries**,
   using each migration's expiry slack (~25 min to ~2 hr) to ride out transient blips;
   (c) as a last resort, if every hub is unreachable and a queued migration nears
   expiry, the shim **broadcasts it directly over Nym** (IP still hidden, only the
   batching lost) rather than let it expire. An outage thus degrades privacy at the
   margin, never liveness. Because the hubs are attested TEEs, no hub operator sees
   cleartext; the multi-org consortium (section 6) governs the keys.
3. **Expiry margin.** Flushing "every <= 20 blocks" cuts it fine for a wallet that
   mints a tx with a 20-block expiry. Flush **well under 20** (aim ~10-15)
   for headroom (section 7).
4. **The 80% framing.** Mark's read, which the room accepted: IP protection is the
   bulk of the necessary privacy here, and public-chain visibility only shows that
   *an entity* holds a balance, not *who*. Nate's caution stands: privacy is an arms
   race, and gains past the first ~70 to 80% cost exponentially more. This is a
   deliberate Pareto first step, labeled as such.

---

## 6. Trust and attestation

- **TEE is mandatory** for baseline operator blindness. A non-enclave shim would let
  the operator read crossing contents; a hacked adjacent node could peer at
  outbound txs. The enclave is what makes operator-blindness real and checkable.
- **The hub is an attested TEE.** Even the hub's operator (Caution at launch) cannot
  read cleartext crossing txs; only the attested hub software can, and only to
  batch and publish.
- **"Steve"** is Caution's enclave-to-enclave encrypted key-sharing protocol, used to
  move the keys the shim encrypts to. It is under review (Zooko, Nate, Taylor).
- **Key governance = a multi-sig consortium** (Anton's proposal): Caution, Nym,
  Shielded Labs, and the Zcash Foundation collectively attest to the key state and
  software integrity. That is the long-term trust-distribution goal; for launch, a
  single trusted entity (Caution) stands up the hub, with the consortium to follow.
- **Verification UX.** A user (or, in practice, a wallet developer acting as trust
  proxy) downloads the source, runs a reproducible build to get a checksum hash, and
  matches it against the signed hashes published at the attestation URL. Wallets
  verify once and pass the assurance to end users. Any independent third party can also audit an endpoint
via its attestation plus a **Certificate Transparency** check that no shadow cert exists
(the Auditor Role; steps in `SHIM-HUB.md` section 9).

---

## 7. Timing and expiry constraints

Blocks target ~75 seconds. A batched crossing tx must publish before its expiry
height, or it fails on-chain. Because all crossings are batched, queued txs carry a
range of expiry windows, so the hub must flush before the tightest. Per the call, the
windows are:

| Wallet family | Expiry window (migration case) |
|---|---|
| librustzcash | current height + 40 blocks |
| Zingo | 100 blocks |
| Brave | 20 blocks (the tight one) |

Brave binds the constraint: the hub must **flush every <= 20 blocks**, and in
practice a bit under that for margin (~10 to 15). This aligns with the migration
expiry choices in **ZIP 318**. See the ZIP-318 migration-timing work for the
per-wallet expiry-decay detail.

Batches will typically hold transactions with one to two months of validity from
their scheduled broadcast; if txs in a batch carry differing expiries correlated to
their submission times, that can erode the batching benefit, so the hub should avoid
leaking per-tx expiry ordering.

---

## 8. Scope for the ~Aug 10 deadline

**Components to ship:**
- `zero-indexer-shim`: the attested TEE router, deployed at operators.
- `zero-broadcaster` (the hub, aka `zero-indexer-hub`): the attested TEE batcher.
- **Nym** between shim and hub (the zeroith step).

**Critical path:** shim + hub + Nym-between-them + **one operator** running the shim
(Caution counts). Guard this line ruthlessly; descope, simplify, and shorten anything
off it.

**Explicitly post-launch (do NOT let these onto the critical path):**
- The **attested Nym fleet** (deprioritized on the call: the near-term system does
  not require users or wallets to touch Nym directly, so a better public Nym network
  is no longer the first thing to build).
- **Option A** (a standalone privacy server users must point their wallets at).
- The **query-only / broadcast-only binary split** (elegant, but wallets assume a
  single endpoint today).
- **PIR** (a later defense-in-depth layer, section 9).
- Full **consortium** key governance (single trusted launch first).

**Open dependencies:** the threat-model doc (Taylor + Zooko) is the upstream gate;
Caution's Steve handshake (confirmed feasible; open: does STEVE-over-Nym carry h2);
hub hosting and funding (Caution may cover a
demo window; Shielded Labs may subsidize operators or run a donation drive); possibly
Nym / Nym's Coastline running the hub component.

---

## 9. The long-term vision (deferred)

The near-term system is the first step toward a fuller privacy product, not the whole
of it. The horizon, deferred until the urgent migration fix ships:

- **Indexer + Nym + TEE (V2).** The full wallet-facing private indexer: queries (not
  just broadcasts) served over Nym, terminated inside an attested enclave. **Status (V2
  sync, 2026-07-30): designed, platform-unblocked, and transport-validated.** Caution
  (Anton) answered every platform gate: attestation-binding is achievable (Steve
  handshake, or the pubkey via `metadata/user_data`, or a new runtime `arbitrary_data`
  field); key persistence via a keymaker/locksmith **M-of-N quorum across 3-4 orgs**
  surviving cold boots and upgrades; egress just works (broad NAT). The transport was
  **rehearsed for real**: nym-proxy (from nymtech/nym) ran our actual CompactTxStreamer
  gRPC over the live Nym **mainnet** mixnet against the live testnet enclave, end to end
  (~10x slower than clearnet, latency-bound; fine for migrations). The TEE substrate
  (attested Zebra+Zaino testnet enclave) is live; the remaining build is the in-enclave
  Nym integration. This directly de-risks the near-term shim-to-hub tunnel and the
  shim/hub attestation. Spec: `deploy/caution-zaino/NYM.md`.
- **V3 (PIR) is not redundant with the TEE** (rationale sharpened). V2 is private only
  *if you trust AWS and the hardware*. V3/PIR is private *via math*, hardware-independent,
  and closes the access-pattern / IO-pattern leaks a Nitro enclave structurally has when
  state is parent-mediated at mainnet scale. Nate framed TEE and PIR as **complementary,
  not equivalent** (distinct failure modes: TEE = hardware-manufacturer or
  physical-boundary compromise; PIR = cryptographic or software flaws), so the endgame
  uses **both**, defense in depth, with PIR as the trust-root-removal step. (Anton's "a
  TEE the hypervisor cannot inspect is functionally PIR" was the view Nate corrected.)
- **The decoupled query-only / broadcast-only split.** Taylor's proposal: one attested
  instance proves it only answers queries and refuses broadcasts, a separate flavor
  only accepts broadcasts and refuses queries. The shim already realizes a scoped
  version of this for turnstile crossings.
- **The attested Nym fleet.** Caution's planned global network of TEE-enabled Nym
  nodes (South Africa, Chicago, Brazil, Singapore, mirroring their DNS Cedar). Useful
  for a healthier public mixnet and broader adoption; deprioritized because the
  near-term system routes Nym only shim-to-hub.
- **The indexer-base decision (lightwalletd vs Zaino).** Sidestepped near-term (the
  shim keeps the operator's existing backend). It only re-emerges if and when we
  build a first-party indexer for the deferred query-privacy product; at that point
  the standalone analysis leaning lightwalletd, and the repo's existing PIR-platform
  designation, come back into play.

---

## 10. Status and next steps

**Status:** a live, queryable Zaino instance built with StageX (reproducible) is synced
to tip on **testnet**, in an attested enclave (64 GB holds testnet at tip for 24h). The
enclave itself does not yet run Nym, but the **Nym transport is validated end to end**
via the V2 rehearsal (real gRPC over the live Nym mainnet mixnet), which de-risks the
shim-to-hub tunnel too. The Caution platform gates for the attested channel are now
**answered** (section 9). Mainnet is gated on Caution enclave memory / disk support.

**Now**
- [x] This strategy doc (refocused on the turnstile-privacy system).

**Next (upstream dependency first)**
- [ ] Threat-model doc (Taylor + Zooko), reviewed by Mark and Nate. **This gates the
      build.**
- [ ] Then scope the shim + hub build (`zero-indexer-shim`, `zero-broadcaster`, Nym
      shim-to-hub, the flush cadence, attestation and reproducible build).

**Holding**
- [ ] The build itself, until the threat model lands and is signed off as safe to run.

---

## 11. Where things live, and commit convention

| Location | Contents | Ownership / commit style |
|---|---|---|
| `zeronym/` | This doc, and the `zero-indexer-shim` + `zero-broadcaster` code and configs | Zero-owned. Plain conventional commits. |
| `deploy/caution-zaino/` | The existing testnet enclave engineering (reproducible image, supervisor, Caution policy) | A separate ongoing session. Do not duplicate. |
| operator backends | The operator's existing indexer, unchanged | Not ours. The shim sits in front of it. |
| vendored subtrees (`zaino/`, etc.) | Only if a near-term change needs one | Upstream-first; patches tagged `[zero]`. |

Nym is not in the repo; a pinned version is vendored where the shim/hub need it. The
shim and hub are new Zero-owned software, not forks of the indexer.

---

## 12. Glossary and references

**Glossary**
- **Turnstile crossing:** a transaction that moves value across a value-pool boundary
  (transparent to/from shielded, or between shielded pools): a deshield, a shield, or a
  cross-pool migration. The class of txs the shim isolates.
- **`zero-indexer-shim`** (shim): the lightweight, attested-TEE router deployed at
  operators. Passes everything except migrations to the operator's existing backend;
  classifies every crossing but isolates only migrations, routing them over Nym to a hub.
- **`zero-broadcaster`** (the hub, aka `zero-indexer-hub`): the attested-TEE batcher
  (run as >=2 with failover) that accumulates migrations and publishes them
  simultaneously on a strict block cadence.
- **Nym:** the mixnet, used near-term only between shim and hub, for cover traffic and
  region unlinkability.
- **"Steve":** Caution's enclave-to-enclave encrypted key-sharing protocol (under
  review).
- **Key consortium:** the proposed multi-sig governance of the enclave/Nym keys
  (Caution, Nym, Shielded Labs, Zcash Foundation), long-term.

**References**
- Source: the 2026-07-30 Shielded Labs all-hands (Mark, Zooko, Nate, Anton via Mark,
  Jason, and others).
- ZIP 318 (migration expiry alignment); the ZIP-318 migration-timing work for
  per-wallet expiry detail.
- Taylor's threat-model / security-model files (the decouple-broadcast proposal),
  pending the new threat-model doc.
- Existing enclave engineering: `deploy/caution-zaino/` and its `NYM.md` transport
  spec.
- Longer-term background (deferred vision): [ZIP 307](https://zips.z.cash/zip-0307),
  the light-client leak ([ECC](https://electriccoin.co/blog/zcash-reference-wallet-light-client-protocol/),
  [ZecSec](https://defuse.ca/zecsec/making-zcash-light-wallets-faster-and-more-private.htm)),
  and PIR (SimplePIR/DoublePIR, FrodoPIR, YPIR, ChalametPIR) for the eventual
  query-privacy layer.
