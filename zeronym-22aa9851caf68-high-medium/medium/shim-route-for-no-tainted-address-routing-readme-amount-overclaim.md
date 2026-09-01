# `README.md:33`'s "not the amount or which transaction" is false: the reference light-client SDK hands the operator's own indexer the wallet's whole transparent address set on every sync, and the operator's indexer then serves back the txid and the value of any diverted Orchard-to-transparent spend

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: The claim at `audit-target/zeronym/README.md:33` (and the disclosure it contradicts at `:32`); the routing table that lets it happen at `audit-target/zeronym/shim/src/proxy.rs:744-783` (`route_for`) and `:702-733` (`Route`); the relay at `:787-832` (`pass_through`); the project's own statement of the leak at `audit-target/zeronym/shim/ENDPOINTS.md:130-132`; the stateless-shim commitment that makes the designed fix unimplementable as written at `audit-target/zeronym/shim/src/intercept.rs:56-63`
**Found by agent:** Local (file audit of `shim/src/proxy.rs`); mechanism corrected and re-based on primary sources during validation
**In scope of audit?** Yes — `audit-context/AUDIT-INSTRUCTIONS.md` puts `README.md` in scope "as security claims" and asks that an overclaim be treated as an ICTM finding. Extra focus area 11 is exactly this.

> **FILENAME NOTE.** The filename is retained because `PROGRESS.md`,
> `audit-state/globals/G11-G33-…` (row **N2**, chain **C**) and
> `issues/confirmed/readme-volume-independent-source-ip-claim-overstated.md`
> all reference it. **Use the title, not the filename.** The original title framed
> this as "`route_for` implements none of the `ENDPOINTS.md` INTERCEPT set"; that
> framing was demoted during validation because `ENDPOINTS.md` disclaims being
> implemented and is separately owned (see *Ownership* below). The finding is the
> README sentence and the concrete leak behind it.

## Description

`README.md`'s Security section carries two bullets, four lines apart, on opposite
sides of the *Protected* / *Not protected* boundary. The first states a
mechanism; the second denies that mechanism's consequence.

`README.md:32` (accurate, and the sharpest disclosure in the document):

> - **Query content.** Address-level queries (`GetTaddressTxids`, `GetTaddressBalance`,
>   `GetAddressUtxos`) are not intercepted and still reach the operator.

`README.md:33` (false for any diverted transaction with a transparent leg the
wallet owns):

> - **The operator learns *that* a client migrated**, though not the amount or which
>   transaction.

The project already states the join in its own words. `shim/ENDPOINTS.md:132`, in
the `GetTaddressBalance` row of the INTERCEPT table:

> **The sharpest amount leak:** a balance poll bracketing the flush yields
> post-minus-pre = the exact deshielded amount, turning "operator learns *that* a
> client migrated" into "amount Y".

and `:130`, for `GetTaddressTransactions` / `GetTaddressTxids`:

> Names the migration's transparent leg; the operator joins IP C to the on-chain
> batched tx once the hub publishes.

None of that routing exists. `route_for` (`proxy.rs:744-783`) is the shim's entire
method-routing table and can distinguish exactly two Zcash methods,
`SendTransaction` and `GetTransaction`. Every transparent-address method returns
`Route::PassThrough` and is relayed verbatim to the operator's indexer.

**The real mechanism is sharper than the "bracket the flush and subtract" the
issue was originally filed with, and it was established during validation from
the reference SDK rather than inferred.** `zcash_client_backend`'s sync loop calls
`refresh_utxos` on **every sync pass** for every account
(`librustzcash/zcash_client_backend/src/sync.rs:117-126`), and that function sends
**the wallet's complete set of transparent receivers in a single
`GetAddressUtxos` request** (`sync.rs:501-516`). The reply carries, per UTXO,
`txid`, `index`, `valueZat` and `height`
(`lightwalletd/walletrpc/service.proto:212-219`). So the operator does not have to
difference two balances: their own indexer *serves the wallet the txid and the
value*, and the request that asked for it is a durable per-wallet fingerprint.

## Attack Scenario and Steps

The adversary is the indexer operator — the primary adversary and the reason the
product exists. No privileged position, no configuration change, no active step:
the data arrives in their indexer's ordinary request log.

1. A wallet holding legacy Orchard notes spends them to a transparent address it
   owns (an unshield — the ordinary "move my Orchard funds to my transparent
   receiver" flow). `classify::is_orchard_touching` is true, so the shim diverts
   the broadcast to the hub and the operator's indexer is never dialled.
2. On its next sync, the wallet calls `GetAddressUtxos` with **every transparent
   receiver of the account** in one request (`sync.rs:501-509`). `route_for`
   returns `Route::PassThrough`, and `pass_through` (`proxy.rs:787-832`) relays it
   to the operator's indexer unchanged. The operator now holds this wallet's
   transparent address set — a stable identifier that survives IP changes,
   reconnections and the enclave's connection multiplexing.
3. Once the hub publishes on its 20-block cadence and the transaction is mined,
   the same recurring request returns the new UTXO. The operator's own indexer
   answers it with `txid`, `valueZat` and `height`.
4. The operator now has, for one identifiable client of theirs: **which
   transaction** (`txid`, directly, not inferred) and **the amount** (`valueZat`,
   directly). Both are the two things `README.md:33` says they do not get. They
   can also confirm it was diverted, since an Orchard-touching transaction is by
   construction one they never saw broadcast.
5. `GetTaddressTxids` (`service.proto:285`) gives the same answer a second way,
   and `GetTaddressBalance` (`:291`) gives the `ENDPOINTS.md:132` differencing
   attack as a third.

**Attack Requirements and Assumptions:**

- **Access needed: none beyond running the indexer**, which is the deployment
  model. The queries arrive unsolicited on the pass-through path.
- **No parent-host position is required** — this is what distinguishes it from the
  other two routes to the same sentence (see *Ownership*). It works in the shipped
  fully-managed Caution deployment (`deploy.sh:156` → `caution apps create`), where
  validation of `readme-volume-independent-source-ip-claim-overstated.md`
  established that the operator does **not** hold the accepting socket and cannot
  get the wallet's source IP from their own indexer.
- **Attribution without an IP.** Because the enclave has no NIC and Caddy
  multiplexes wallets onto one shim connection, the indexer sees neither the
  client address nor a reliable per-wallet connection boundary. The address set in
  the request body supplies the missing handle by itself: it is per-wallet,
  repeated on every sync, and joinable to the public chain forever.
- **Realistic because it is default behaviour of the reference SDK**, not an
  attacker-chosen path. `refresh_utxos` is gated only on the `transparent-inputs`
  feature and runs before every shielded scan.
- **Bound, stated honestly: this route does not touch the acute use case.** A
  conforming ZIP 318 Orchard-to-Ironwood migration is shielded-to-shielded and has
  no transparent leg, so nothing here recovers its amount. The affected class is
  the *rest* of the diverted population — every Orchard spend with a transparent
  output or input the wallet holds — which the shim diverts because
  `is_orchard_touching` is a presence predicate, and which `README.md:33` covers
  without qualification.
- **A second bound: the amount is public on chain anyway.** A deshield's output
  value is cleartext. What this route supplies is the *linkage* — which of the
  operator's clients that on-chain transaction belongs to — which is precisely
  what the product exists to break and precisely what `:33` claims survives.

## Impact on Users

- A user who moves legacy Orchard funds to their own transparent address through a
  zero-indexer endpoint is told, in the Security section they are pointed at, that
  the operator does not learn the amount or which transaction. The operator learns
  both, from that user's own wallet, on the next sync, with no effort and no
  detectable action. The join is permanent, because the chain is permanent and
  indexer logs can be replayed later.
- The consequence is the linkage `README.md:54` calls "the attack" — client →
  on-chain transaction → value — minus only the IP term in the fully-managed
  deployment, and including the IP term under BYOC (`caution init --byoc`) or once
  the operator takes the single step described in the confirmed
  `core-linkage-…` finding.
- An operator deploying the shim is told at `README.md:70` that "Orchard-touching
  transactions and `GetTransaction` lookups stop being yours to see" and may
  reasonably conclude their users are protected against the amount join. For
  transparent-legged spends they are not.

## Technical Details / Code Analysis

**1. The complete routing table.** `shim/src/proxy.rs:744-783`:

```rust
pub fn route_for(path: &str) -> Route {
    if path == SEND_TRANSACTION {
        return Route::Intercept;
    }
    if path == GET_TRANSACTION {
        return Route::GetTransaction;
    }
    // Caution's own endpoints, served on our host: never hand them to the indexer.
    if path == CAUTION_HEALTH { return Route::CautionHealth; }
    if path == CAUTION_ATTESTATION { return Route::CautionAttestation; }
    // The shim's own operator endpoints.
    if path == SHIM_HEALTH { return Route::ShimHealth; }
    if path == SHIM_NYM_STATUS { return Route::ShimNymStatus; }
    if path == SHIM_NYM_DIAG { return Route::ShimNymDiag; }

    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit('/').next() {
        Some(last) if last.eq_ignore_ascii_case("sendtransaction") => Route::InterceptNearMiss,
        _ => Route::PassThrough,
    }
}
```

Of the nine `Route` variants (`:702-733`), three are Zcash methods and the rest
are control-plane or status paths. There is no address handling, no `TaintedAddrs`
structure, no body inspection for any method other than the two intercepted ones,
and no list-splitting for `GetTaddressBalance`. `GetTaddressBalance`,
`GetTaddressBalanceStream`, `GetTaddressTxids`, `GetTaddressTransactions`,
`GetAddressUtxos` and `GetAddressUtxosStream` all land in the `PassThrough` default.

**2. Where they go.** `shim/src/proxy.rs:664-666`:

```rust
        Route::PassThrough | Route::CautionHealth | Route::CautionAttestation => {
            pass_through(req, pool).await
        }
```

`pass_through` (`:787-832`) dials the operator's indexer and relays the request
head and body verbatim; `forward` (`:840-870`) retargets only the origin, so every
header and the whole body reach the operator unchanged.

**3. What the wallet actually sends.** `librustzcash/zcash_client_backend/src/sync.rs:501-516`:

```rust
    let request = service::GetAddressUtxosArg {
        addresses: db_data
            .get_transparent_receivers(account_id, true, true)
            .map_err(Error::Wallet)?
            .into_keys()
            .map(|addr| addr.encode(params))
            .collect(),
        start_height: start_height.into(),
        max_entries: 0,
    };
    …
        client.get_address_utxos_stream(request)
```

called unconditionally per account from the sync entry point
(`sync.rs:117-126`), *before* shielded scanning. The reply type
(`lightwalletd/walletrpc/service.proto:212-219`) is:

```protobuf
message GetAddressUtxosReply {
    string address = 6;
    bytes txid = 1;
    int32 index = 2;
    bytes script = 3;
    int64 valueZat = 4;
    uint64 height = 5;
}
```

so `txid` and `valueZat` are served by the operator's own indexer, for a
transaction the shim went to some lengths never to show them.

**4. Why the designed fix is not merely "not built yet".** `ENDPOINTS.md`'s
INTERCEPT set is defined over recognition state — `DivertedMigrations`,
`TaintedAddrs`, `DivertedHeights`, `PendingMigration` (`ENDPOINTS.md:142-157`) —
seeded at divert time. The shipped shim forecloses that by design
(`shim/src/intercept.rs:56-63`):

```rust
/// Deliberately holds NO state about what it diverted. A stateless shim survives
/// a restart and can run as more than one instance without a follow-up query
/// leaking to the operator, because it recognises nothing: every
/// `GetTransaction` goes to the hub regardless.
pub struct Diversion {
    pub hub: HubTransport,
}
```

That statelessness is a defensible choice and is why `GetTransaction` is routed
unconditionally rather than recognised. But it means the conditional,
`TaintedAddrs`-keyed design in `ENDPOINTS.md` cannot be implemented as written, so
this leak is not on a path to closing by itself. The two options that remain are
unconditional ("broad") routing of the address methods — which `ENDPOINTS.md:194-206`
names as the load-bearing open decision — or an honest README.

## Recommendations

1. **Correct `README.md:33` — this is the cheap fix and it removes a
   contradiction that sits inside a single six-line list.** Replace "though not
   the amount or which transaction" with something the code supports, e.g.:

   > **The operator learns *that* a client migrated.** For a migration that is
   > shielded-to-shielded they learn no more than that. For an Orchard spend with
   > a transparent leg they can learn more: address-level queries are not
   > intercepted (see above), so their indexer serves the wallet the txid and
   > value of the new transparent output and receives the wallet's transparent
   > address set with the request.

2. **Say which claims are adversary-scoped.** Every bullet under *Protected* and
   *Not protected* is adversary-dependent and none says so. This one is different
   against a chain observer, against the operator in the fully-managed deployment,
   and against the operator under BYOC.

3. **If the leak is to be closed rather than disclosed, pick "broad" routing.**
   Route the transparent-address methods away unconditionally, the same shape as
   the `GetTransaction` handling that already ships. It is the only branch of
   `ENDPOINTS.md:194-206` compatible with the stateless-shim commitment at
   `intercept.rs:56-63`. **Do not implement the conditional/`TaintedAddrs` branch**
   as written: `endpoints-conditional-intercept-leaks-by-omission-…` shows a
   conditional route is itself a signal to an adversary who sees the request
   sequence.

4. **Make the routing table state the policy.** Give the address-level methods
   explicit `Route` variants rather than leaving them in the `PassThrough`
   default, so a future method is a compile-time decision rather than a silent
   omission.

## Validation Information

**VERDICT: CONFIRMED, Medium** (filed Medium; held). Validated 2026-08-18 against
the target at HEAD plus two primary sources the filing did not use: the reference
light-client SDK (`audit-context/zero/librustzcash/zcash_client_backend/src/sync.rs`)
and the `CompactTxStreamer` proto (`audit-context/zero/lightwalletd/walletrpc/service.proto`).

**Every mechanical claim re-verified.**

| claim | verified |
|---|---|
| `route_for` distinguishes only `SendTransaction` and `GetTransaction` among Zcash methods | yes — `proxy.rs:744-783`, read end to end |
| no `TaintedAddrs` / address state anywhere in the shim | yes — no occurrence in `shim/src/`; `intercept.rs:56-63` forecloses it by design |
| address methods land in `PassThrough` and are relayed verbatim | yes — `proxy.rs:664-666`, `:787-832`, `:840-870` |
| `README.md:32` and `:33` read as quoted | yes, at those exact lines |
| `ENDPOINTS.md:130`, `:131`, `:132` read as quoted | yes, at those exact lines |
| the six address methods exist on the wire surface | yes — `service.proto:285`, `:291-292`, `:323-324` |

**Three corrections applied against the filing.**

1. **The mechanism was upgraded and the filed attack step was wrong in its
   central detail.** The filing had the wallet "polling the deshield destination
   address", which is only the wallet's own address in the self-unshield case and
   is *never* the wallet's address when paying a third party — as written it would
   have overstated the reach. The real and stronger mechanism is
   `refresh_utxos` (`sync.rs:485-540`), which sends the account's **entire**
   transparent receiver set on **every** sync and gets back `txid` + `valueZat`
   per UTXO. No differencing is needed and no attacker timing is needed.
2. **The headline was re-based.** "`route_for` implements none of the
   `ENDPOINTS.md` INTERCEPT set" is a weak lead: `ENDPOINTS.md:5-6` explicitly
   disclaims being implemented ("This is the design for the FULL shim; the current
   PoC only classifies + logs `SendTransaction`"), and `README.md:32` concedes the
   gap. A disclosed gap is not the finding; the sentence that denies its
   consequence is.
3. **The IP term was struck from the primary claim.** The filing asserted the
   operator obtains "IP address → on-chain transaction → amount". In the shipped
   fully-managed deployment they do not obtain the IP from this route — the enclave
   has no NIC, Caddy's peer is `127.0.0.1`, and `proxy::forward` adds no
   forwarding header (verified: no `X-Forwarded-For` handling anywhere in
   `shim/src/`). The claim that survives, and that is sufficient to falsify
   `README.md:33`, is **amount + txid attributed to an identifiable client**,
   where the identifier is the transparent address set rather than the IP. Under
   BYOC, or after the confirmed `core-linkage-…` step 0, the IP term returns.

**Ownership — read this before the report allocates severity.**

`README.md:33` is falsified three independent ways, and the report must present
the *sentence* once with three routes rather than three findings that each
re-argue it:

| route | owner | precondition | closed by |
|---|---|---|---|
| the `log_verdict` INFO line carries `orchard_vb` and `tx_len` | `log-verdict-logs-migration-value-balance-at-info.md` (confirmed **High**) | `DEBUG=1`, i.e. an open enclave console on the parent host | remove the fields **and** default `DEBUG=0` |
| exact `\|tx\|` off the unpadded wallet leg, then select out of the batch and read `valueBalanceOrchard` | `core-linkage-…-self-timestamping.md` (confirmed **High**) | a wallet-leg observation post ("step 0") | wallet-side padding plus ZIP 318 uniformity |
| **unintercepted address queries** | **this issue** | **none** | broad routing, or an honest README |

**What this issue uniquely owns, and why it is not a triple count.** It is the
only one of the three that needs **no privileged position at all** — no parent
host, no debug deployment, no traffic capture — and therefore the only one that is
live for an operator who holds nothing but their own indexer, which is exactly the
shipped fully-managed posture. It is also the only one that **survives the complete
remediation of both Highs**: fixing the log fields and padding the wallet leg
leaves it untouched, because it runs on ordinary pass-through traffic. A prior
validator already recorded this allocation inside
`readme-volume-independent-source-ip-claim-overstated.md`, naming the
"unintercepted `GetTaddressBalance` bracketing" as one of only two routes to the
amount in an attested deployment and pointing at this file.

**Why Medium and not Low**, against the item-7n precedent that deflated three
README ICTM issues: those three were deflated because a confirmed code-side
finding fully owned their harm. **Nothing owns this one.** Likelihood is not
merely high but certain — the disclosure is passive, arrives by default from the
reference SDK, and needs no attacker action — and the impact is the product's own
headline harm minus the IP term.

**Why not High.** Three deliberate limits. (a) The acute use case is untouched: a
conforming ZIP 318 migration has no transparent leg. (b) The amount and the txid
are already public chain data; what leaks is the linkage to a client. (c) In the
shipped fully-managed deployment the linkage terminates in a pseudonymous address
set, not the IP the product exists to protect — the IP term requires BYOC or the
separately-owned step 0.

**Not covered here, and correctly filed elsewhere — do not merge:**
- `ENDPOINTS.md`'s residual section declaring these leaks *closed* by a "Zeronym
  indexer" that exists nowhere in the tree is owned by
  `endpoints-residual-section-declares-three-leaks-closed-by-a-component-that-exists-nowhere.md`
  (plausible, Low). The coordinator asked whether a confirmed issue already owns
  it: none does, but that file does, and it should be validated on its own terms.
- The conditional-route-is-itself-a-signal analysis is owned by
  `endpoints-conditional-intercept-leaks-by-omission-and-this-residual-is-never-stated.md`
  (plausible, Info). Recommendation 3 above depends on its conclusion.
- `GetLatestTreeState` / per-wallet anchor service is **not** claimed here. The
  filing raised it as "a second, independent instance"; it is a different harm
  (anchor correlation), it is the wallet-side requirement `README.md:69` does
  state, and it is owned by the `core-linkage-…` High and by
  `readme-tells-wallet-developers-there-is-exactly-one-requirement-…`. Struck from
  this issue to keep the boundary clean.

**One filed claim checked and left standing but re-scoped:** the near-miss gap on
`GetTransaction` (exact-string match, no case/slash normalisation) is referenced in
the filing's table as "see the separate near-miss finding". It is not part of this
issue's harm and is owned by `shim-route-for-gettransaction-no-nearmiss-arm.md`.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
