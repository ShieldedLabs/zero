# Light-client IP leakage

Zeronym exists to close a specific, well-understood leak in the way Zcash light
wallets talk to the network. Zcash's shielded pools hide the sender, receiver, and
amount of a transaction on-chain. They do not hide the network metadata a light wallet
exposes when it talks to the server that indexes the chain for it: the wallet's source
IP address and the timing of its requests. For most traffic that metadata is a
background privacy cost. For a transaction that crosses a value-pool boundary, and
above all for the mandatory Orchard to Ironwood migration, that metadata is enough to
link a real-world network identity to an on-chain balance.

## The ZIP 307 light-client leak

A light wallet does not run a full node. It cannot afford to download and validate the
whole chain, so it delegates that work to an indexer (a lightwalletd or Zaino instance)
and speaks to it over the light-client protocol defined in
[ZIP 307](https://zips.z.cash/zip-0307), the CompactTxStreamer gRPC service. The wallet
asks the indexer for compact blocks, trial-decrypts them locally to find its own notes,
and submits its own transactions back through the same indexer.

Two properties of that relationship matter here:

- **Note contents stay private from the server.** Trial decryption happens on the
  client, so the indexer never learns, from the block data it serves, which notes
  belong to the wallet. This is the part the light-client protocol gets right.
- **Metadata does not.** The indexer terminates the wallet's connection, so it sees the
  wallet's source IP address, the exact timing of every request, and the set of
  addresses and block ranges the wallet queries. See the ECC and ZecSec writeups of the
  light-client leak (referenced in [the glossary](./glossary.md)).

Zeronym's near-term system targets one half of that metadata leak: the **broadcast**.
When a wallet submits a transaction, it calls SendTransaction on the indexer over
clearnet TLS. The operator terminates that TLS, so it sees the raw transaction bytes,
the source IP that sent them, and the moment they arrived. (The other half, which
addresses a wallet looks up, is the query leak. It is out of near-term scope; see
[the threat model](./threat-model.md) and [the roadmap](./roadmap.md).)

## From metadata to balance: the correlation

A source IP and a timestamp are only metadata until they can be joined to something.
Turnstile-crossing transactions supply the join.

A transaction that moves value across a value-pool boundary reveals that movement in
cleartext on the public chain. The value leaving one pool (and, for a deshield, the
transparent output that receives it) is visible to anyone. So an operator, or anyone
who later obtains the operator's logs, holds two halves of a linkage:

1. From its own connection logs: source IP X submitted a broadcast at time T.
2. From the public chain: a turnstile-crossing transaction moving amount Y appeared at
   roughly time T.

Joining the two by timing links **IP address to on-chain transaction to balance**.
Nothing about the shielded cryptography prevents this: the leak is in the transport,
not the transaction. It is a retrospective attack as much as a live one, because the
public chain is permanent and connection logs can be kept and joined later.

## The acute event: the Orchard to Ironwood migration

The reason this leak is urgent right now is the Orchard to Ironwood migration. Ironwood
is the new shielded pool; to migrate, a wallet must broadcast a transaction that moves
value out of the Orchard pool and into Ironwood. This is not optional and it is not
niche. It is a mandatory, mass event that plays out across a migration window rather
than in a single instant, so a large fraction of the user base will each broadcast at
least one pool-crossing transaction over a bounded period.

Every one of those broadcasts, sent the way light wallets send transactions today,
exposes a source IP and a timestamp that can be joined to the resulting on-chain
migration. Zooko framed this on the 2026-07-30 all-hands as **the worst privacy-loss
event in Zcash history**: users linking their IP address to their Zcash balance,
hourly, for the duration of the migration window.

Migrations are the acute case for three compounding reasons:

- **Mandatory:** users cannot opt out to protect themselves; the value has to move.
- **Mass:** a large population migrates, so the leak is broad rather than isolated.
- **Concentrated:** the window is bounded, so many correlatable broadcasts land close
  together in time.

The same properties that make migrations dangerous also make them the ideal thing to
protect first. Because migrations are mandatory, mass, and not time-sensitive, a large
population of them can be batched together and published at once, which is exactly
what the near-term system does (see [the overview](./overview.md)).

## Every turnstile crossing leaks, not just migrations

The migration is the acute driver, but the underlying leak is general. Any **turnstile
crossing** (a transaction that moves value across a value-pool boundary) is revealed
on-chain and, linked to a source IP, deanonymizes the user. There are three shapes:

- **Deshield:** shielded value moving to a transparent output.
- **Shield:** transparent input moving into a shielded pool.
- **Migration:** shielded value moving from one shielded pool into a different shielded
  pool (Orchard to Ironwood).

Because the leak is structural, the shim's classifier detects **every** crossing, not
just migrations. Near-term, though, the system isolates and protects only the migration
case; deshields and shields pass straight through, because deshields are time-sensitive
commerce and shields are already privacy-positive (the transparent side is public
regardless). Treating the batched set as a policy knob, rather than hardcoding
"migration," means the protected set can widen later without re-architecting. Which
crossings are protected today, and which pass through, is spelled out in
[the threat model](./threat-model.md) and [the overview](./overview.md).

Note that deshield and shield crossings exist on mainnet today, independent of
Ironwood, so the leak is real for current mainnet traffic and not only for the future
migration.

## Scope: the broadcast, not the query

It is worth stating plainly, because it is the discipline that keeps the near-term
deliverable achievable: this problem is about the migration **broadcast**, not about
queries. Which addresses a wallet looks up is a separate leak in the same ZIP 307
relationship, and it is deliberately not in near-term scope. The judgment from the
all-hands was that IP protection for the migration broadcast is the bulk of the
practical privacy at stake right now. Query privacy is the deferred vision; see
[the threat model](./threat-model.md) for the explicit scope boundary and
[the roadmap](./roadmap.md) for where query privacy returns.

## The adversaries

Against the migration broadcast as it works today (clearnet), three adversaries can
perform the IP-to-balance linkage:

- **The light-wallet operator (indexer).** There are roughly five to ten operators
  running the light-wallet backends (lightwalletd or Zaino). Each terminates its
  clients' TLS, so today it sees the migration transaction in cleartext, the source IP
  that submitted it, and the timing. It can log all three and join them to the public
  chain at leisure. This is the primary adversary the near-term system is built to
  blind.
- **A passive network observer.** Anyone on the path between the wallet and the
  operator (an ISP, a transit provider, a hosting network) sees the connection
  metadata: which IP contacted the operator and when. Even without the transaction
  contents, the timing plus the public chain is enough to correlate.
- **The retrospective correlator.** Because the public chain is permanent and logs
  persist, any party that later obtains operator logs or network captures can perform
  the same join after the fact. The window for this attack does not close when the
  migration does.

The near-term system's job is to break the specific linkages these adversaries rely
on: it blinds the operator to the transaction contents, unlinks the broadcast from the
source IP, and breaks the timing correlation. Exactly which properties change, and
which do not, is the subject of [the threat model](./threat-model.md).
