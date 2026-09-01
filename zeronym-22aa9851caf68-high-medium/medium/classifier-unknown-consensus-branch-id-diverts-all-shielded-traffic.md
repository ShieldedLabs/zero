# The next Zcash network upgrade silently turns every un-redeployed shim into a divert-everything box: an unrecognised `nConsensusBranchId` makes the classifier fail-safe on 100% of v5/v6 traffic, and no health signal, alert field or document says so

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/shim/src/classify.rs:235-241` (the parse-failure arm), `:99-101` (`Class::treat_as_migration`), `:27-31` (the Ironwood pass-through boundary the failure erases), `:44` (the "a false positive is merely a wasted diversion" cost claim); `audit-target/zeronym/shim/src/intercept.rs:117-119`, `:137-215` (`divert`), `:616-624` (the only signal emitted); `audit-target/zeronym/shim/src/nym.rs:1363-1378` (`local_txid`), `audit-target/zeronym/shim/src/hub.rs:238-239`; `audit-target/zeronym/hub/src/queue.rs:189-197`, `:328-348` (`find_by_txid`); the branch-ID table in `audit-context/zero/zebra/zebra-chain/src/parameters/network_upgrade.rs:230-249` and its readers at `audit-context/zero/zebra/zebra-chain/src/transaction/serialize.rs:1086-1087` (v5) and `:1150-1151` (v6)
**Found by agent:** Local (file audit of `shim/src/classify.rs`); validated 2026-08-18
**In scope of audit?** Yes

## Description

The shim decides where a `SendTransaction` goes by parsing the transaction with the
vendored `zebra-chain`. Bytes that do not parse are `Class::Unparseable`, and
`Class::treat_as_migration()` folds that into "divert to the hub"
(`classify.rs:99-101`). That is the right direction for privacy, and the module
argues it is cheap: *"A false positive is merely a wasted diversion."*
(`classify.rs:44`).

`zebra-chain` rejects a v5 or v6 transaction whose `nConsensusBranchId` is not in a
**compile-time table**. `NetworkUpgrade::try_from` is a linear search of
`CONSENSUS_BRANCH_IDS` returning `InvalidConsensusBranchId` for anything absent
(`network_upgrade.rs:75-85`), which `SerializationError::from` maps to
`Parse("invalid consensus branch id")` (`serialization/error.rs:98`). Both the v5 and
the v6 deserializer call it as the **second** field they read, before any bundle:

```rust
// zebra-chain/src/transaction/serialize.rs:1150-1151 (v6; the v5 arm at :1086-1087 is identical)
                let network_upgrade =
                    NetworkUpgrade::try_from(limited_reader.read_u32::<LittleEndian>()?)?;
```

The table the production build compiles ends at NU6.3. NU7 exists in the enum and in
the table only as a **test placeholder behind a cfg gate**, with the real branch ID
still a TODO:

```rust
// zebra-chain/src/parameters/network_upgrade.rs:241-244
    (Nu6_3, ConsensusBranchId(0x37a5165b)),
    // TODO: set below to (Nu7, ConsensusBranchId(0x77190ad8)), once the same value is set in librustzcash
    #[cfg(any(test, feature = "zebra-test"))]
    (Nu7, ConsensusBranchId(0xfffffffe)),
```

`zebra-chain` is a plain (featureless) dependency of the shim
(`shim/Cargo.toml`, `zebra-chain = { path = "../../zebra/zebra-chain" }`), so the
enclave binary carries no NU7 row at all.

**Consequence.** A Zcash transaction is only valid in a block whose network upgrade
matches its `nConsensusBranchId`, so from the moment the next upgrade activates every
wallet builds transactions carrying the new ID. On any shim whose image predates that
upgrade:

* **every v5 and every v6 transaction fails to parse** and is diverted — not only
  Orchard-touching ones. Since NU5 the v5 format is what ordinary wallets emit for
  *everything*, so `Class::PassThrough` becomes an empty class in practice: Ironwood
  payments, Sapling payments and transparent payments are all diverted;
* the diverted transaction is **still published**, by the hub, at the next flush
  (`hub/src/queue.rs:189-197` folds an unparseable payload to `expiry = None`, which
  always survives admission, and `batcher::flush` broadcasts the raw bytes), so this
  is a correctness-and-availability failure rather than a privacy leak;
* but the wallet is answered with an **empty txid**, every `GetTransaction` for a
  still-queued transaction becomes unanswerable, everything over 65,503 bytes is
  **permanently refused**, and every existing silent-loss path in the divert
  pipeline widens from "migrations" to "all of this shim's send traffic".

**Nothing detects it.** `/healthz` reports only mixnet liveness, `/nym-status`'s four
alert fields (`shim/deploy/caution/OPERATORS.md:413-418`) are all unchanged, the only
signal is a per-transaction `warn!` that `shim/src/proxy.rs:111` says nobody can read
(*"an attested enclave has no console"*), and a grep for `"network upgrade"` across
the whole of `audit-target/zeronym/` returns **zero** hits — no README, runbook or
`RESTARTS.md` mentions that the image embeds a consensus-branch table with an
expiry date.

## Attack Scenario and Steps

**No attacker is required.** The trigger is a scheduled protocol event, and the
distribution's own history shows this class of staleness has already shipped once.

1. An operator deploys the attested shim image. Attestation pins it, a redeploy moves
   PCR2 and spends a certificate issuance (`shim/deploy/caution/RESTARTS.md`;
   confirmed `restarts-ledger-budget-model-omits-hub-forced-redeploys-…`), so images
   are long-lived by design.
2. Zcash activates its next network upgrade. The mainnet cadence in the vendored
   table is *accelerating* — NU6 → NU6.1 = 420,000 blocks (~1 year), NU6.1 → NU6.2 =
   218,200 (~189 days), NU6.2 → NU6.3 = 63,543 (~55 days)
   (`zebra-chain/src/parameters/constants.rs:90-99`) — and NU7 is already carried in
   the enum with a reserved branch ID awaiting only librustzcash. The next activation
   is a matter of weeks-to-months, not years.
3. A wallet sends an ordinary Ironwood payment. `intercept::inspect` decodes the gRPC
   frame and the `RawTransaction` fine and calls `classify_with_evidence(&raw.data)`
   (`intercept.rs:563`).
4. `Transaction::zcash_deserialize` hits the unknown branch ID and returns
   `Parse("invalid consensus branch id")`; `classify.rs:238-241` returns
   `Evidence::unparseable`.
5. `Inspection::treat_as_migration()` is `true` (`intercept.rs:481-489`), so
   `divert` runs (`intercept.rs:117-119`). The operator's indexer is never dialled.
   Correct for a migration; wrong for this transaction.
6. On the mixnet transport `NymHandle::submit` is dispatch-only (`nym.rs:595-690`;
   `let (ack_tx, _drop_receiver) = oneshot::channel();` at `:652`), so the wallet is
   answered `SendResponse { error_code: 0, error_message: local_txid(bytes) }` — and
   `local_txid` returns `String::new()` for bytes it cannot parse
   (`nym.rs:1370-1377`, via `hub.rs:238-239`). **Every send returns success with an
   empty txid.**
7. The transaction sits in the hub's queue for up to one flush interval (20 blocks,
   ~25 minutes) and is then published. During that window the hub cannot answer a
   lookup for it: `admit` stored `txid: None`, and `find_by_txid` only matches
   entries whose txid is `Some` (`queue.rs:189-197`, `:328-348`). Every
   `GetTransaction` — and with a hub configured, *all* of them go to the hub
   (`intercept.rs:229-236`) — returns `NOT_FOUND` for the user's own just-sent
   transaction.

**Attack Requirements and Assumptions:**
- Requires only that an operator is running an image whose vendored `zebra-chain`
  predates the active network upgrade. That is a maintenance lapse that nothing in
  the product warns about, measures, or alerts on.
- Not reachable today: mainnet is on NU6.3 (activated at height 3,428,143, tip
  ~3,451,298 on 2026-08-17), which the vendored table does contain — the `37 a5 16 5b`
  little-endian bytes at offset 8 of every committed V6 fixture.
- **No test can catch it, and a test would give the wrong answer.** `proptest-impl`
  enables `zebra-test` (`zebra-chain/Cargo.toml:47`), and `proptest-impl` is a
  dev-dependency of the shim, so under feature unification a `cargo test` build has
  the `(Nu7, 0xfffffffe)` row the release build does not. The test build and the
  shipped build therefore have *different* consensus-branch tables. (Filed separately
  as `shim-tests-parse-with-a-zebra-chain-whose-consensus-branch-table-differs-from-the-shipped-one.md`.)
- **The distribution has already shipped this exact class of staleness.** Its own
  changelog records it: *"The z3 smoke probes assert that the deployed zebrad reports
  the NU6.3 branch ID and pins activation at 3,428,143. A stale image passed every
  other probe while silently lacking both the consensus rules and the grace window
  above; the 2026-07-17 cached-layer incident shipped that exact class of mismatch."*
  (`audit-context/zero/CHANGELOG.md:138-142`). A probe was added for **zebrad**.
  No equivalent probe exists for the shim, whose branch table is just as pinned.

## Impact on Users

Every wallet behind an affected shim, for every transaction it sends — not only
migrations:

- **Up to ~25 minutes of added latency on all traffic**, including the Ironwood
  payments the design deliberately exempted *because* they are time-sensitive
  commerce (`classify.rs:27-31`: *"A transaction with only Ironwood actions must
  still pass through"*). That boundary becomes unreachable, because an Ironwood-only
  transaction never gets as far as `is_orchard_touching`.
- **An empty txid in every `SendResponse`**, where lightwalletd and the shim's own
  non-upgraded behaviour return the real one.
- **`GetTransaction` blind for the whole flush window.** The hub's queue lookup keys
  on a txid it could not compute, so the one feature that lets a wallet see its
  diverted transaction before publication stops working for everything.
- **Hard, non-retryable refusal above 65,503 bytes.** `wire::encode_submit` refuses
  (`shim/src/wire.rs:124`, `:274-279`) and `divert` answers `RESOURCE_EXHAUSTED`
  (`intercept.rs:167-179`), never forwarding and never broadcasting another way.
  Pre-upgrade such a transaction would have passed through untouched.
- **Hub unavailability becomes a total send outage.** Today an unreachable hub fails
  closed only for Orchard-touching sends (`intercept.rs:208-214`); afterwards it
  fails closed for every send the shim sees.
- **Every silent-loss path in the divert pipeline widens to all traffic.** The
  wallet is told `error_code: 0` at hand-off, so the confirmed
  `shim-nym-driver-every-teardown-path-silently-destroys-acknowledged-submits.md`
  (Medium), `nym-rotation-deferral-cannot-protect-submits-so-rotating-destroys-acknowledged-migrations.md`
  (Low) and `junk-sendtransaction-flood-…` (High) stop being scoped to a small,
  self-selected population that knows migrations are slow, and start applying to
  ordinary payments. The shim's whole mixnet egress is ~45 Sphinx packets ≈ 5.4 s per
  submit at the crate's own throttled rate (`shim/src/nym.rs:1085-1115`), which the
  design sized against ~0.77 Orchard-touching transactions per block, not against a
  shim's entire send volume.

Privacy is **not** harmed: the failure direction is toward diversion, and nothing
reaches the operator's indexer that would not have before. This is an availability
and integrity finding.

## Technical Details / Code Analysis

**The parse-failure arm** (`shim/src/classify.rs:235-241`) — the only place a
protocol-version failure and random junk are distinguished, and they are not:

```rust
pub fn classify_with_evidence(raw: &[u8]) -> Evidence {
    let mut cursor = Cursor::new(raw);

    let tx = match Transaction::zcash_deserialize(&mut cursor) {
        Ok(tx) => tx,
        Err(err) => return Evidence::unparseable(raw.len(), err.to_string()),
    };
```

**The routing policy** (`shim/src/classify.rs:99-101`):

```rust
    pub fn treat_as_migration(self) -> bool {
        matches!(self, Class::Migration | Class::Unparseable)
    }
```

**The boundary this erases** (`shim/src/classify.rs:27-31`):

```rust
//! ORCHARD ONLY, NOT IRONWOOD, and that is deliberate. Ironwood is the NEW pool,
//! where ordinary time-sensitive commerce will live, and the time-insensitivity
//! half of the rationale does not hold for it. A transaction with only Ironwood
//! actions must still pass through, so there is no Ironwood arm in this
//! predicate and none should be added.
```

**The only signal emitted** (`shim/src/intercept.rs:616-624`). The reason string is
there — `error = "parse error: invalid consensus branch id"` — but it is the same
`warn!` line random junk produces, there is no counter, and in an attested enclave
there is no console to read it from:

```rust
            Class::Unparseable => tracing::warn!(
                target: "zis::classify",
                error = evidence.error.as_deref().unwrap_or("(none)"),
                tx_len = evidence.len,
                frame_len = frame.len(),
                body_prefix = %hex_prefix(frame, GRPC_PREFIX_LEN + PREFIX_LOG_BYTES),
                diverted_in_production,
                "MIGRATION-FAILSAFE: unparseable SendTransaction body, treating as migration"
            ),
```

**The empty txid** (`shim/src/nym.rs:1370-1377` and `shim/src/hub.rs:238-239`):

```rust
pub fn local_txid(tx_bytes: &[u8]) -> String {
    use zebra_chain::serialization::ZcashDeserialize;
    match zebra_chain::transaction::Transaction::zcash_deserialize(&mut std::io::Cursor::new(
        tx_bytes,
    )) {
        Ok(tx) => tx.hash().to_string(),
        Err(_) => String::new(),
    }
}
```

```rust
                Ok(()) => Ok(Submit::Accepted {
                    txid: crate::nym::local_txid(tx_bytes),
                }),
```

**The hub side, which is why this is not a destruction finding.** `admit`'s parse is
telemetry, and a failure folds expiry to `None` (`hub/src/queue.rs:189-197`), which
`survives_next_flush` always admits (`:380-392`). So the `ExpiryTooTight` refusal
**cannot** fire for this class, and the transaction is queued and published:

```rust
        let (txid, expiry) = match Transaction::zcash_deserialize(&mut Cursor::new(tx_bytes)) {
            Ok(tx) => (
                Some(tx.hash().to_string()),
                tx.expiry_height()
                    .map(|h| h.0)
                    .filter(|height| *height != 0),
            ),
            Err(_) => (None, None),
        };
```

The same `txid: None` is what blinds `find_by_txid` (`hub/src/queue.rs:337-345`),
because the match arm requires a txid to exist:

```rust
            .find(|entry| {
                entry
                    .txid
                    .as_deref()
                    .is_some_and(|txid| txid == forward || txid == reversed)
            })
```

**Why the rest of the version handling is sound** (recorded so this is not mistaken
for a wider claim): `orchard_shielded_data()` is version-agnostic and returns `None`
for V1–V4 (`zebra-chain/src/transaction.rs:1065-1083`); `orchard_actions()` derives
from it (`:1085-1091`); an unknown transaction *version* falls to
`(_, _) => Err(SerializationError::Parse("bad tx header"))` (`serialize.rs:1220`) and
therefore also diverts; a future format appending fields to v6 would leave
`cursor.position() != raw.len()` and be caught at `classify.rs:248-257`. Every one of
those is fail-safe. Note also that a **v4** transaction is unaffected: v4 carries no
`nConsensusBranchId` on the wire, so Sapling-only v4 traffic keeps parsing. The
finding is that the file's stated cost model for the fail-safe direction is wrong,
and that one certain, scheduled, attacker-free event routes essentially the entire
traffic mix into it.

## Recommendations

1. **Give the shim a branch-ID health signal.** Distinguish "unrecognised protocol
   version" from "unparseable garbage" in `classify.rs` (the reason string is already
   in `Evidence.error`), count it, and add it to `/nym-status` and the
   `OPERATORS.md:413-418` alert table. If the unrecognised-branch rate is non-trivial
   over a window, fail `/healthz`. The zebrad half of this distribution already has
   exactly this probe (`CHANGELOG.md:138-142`); the shim needs the equivalent.
2. **Pin the newest known branch ID as build metadata** and publish it beside
   `deploy/EXPECTED_SHA256`, so an operator can compare it against the chain's active
   branch without reading source.
3. **Document the coupling.** `shim/deploy/caution/OPERATORS.md` and
   `RESTARTS.md` must state that the enclave image embeds a consensus-branch table
   and must be redeployed before the next network upgrade activates. Today the string
   "network upgrade" appears nowhere in `audit-target/zeronym/`.
4. **Correct `classify.rs:44`.** A false positive is not "merely a wasted diversion":
   it costs up to a flush interval of latency, returns an empty txid, blinds
   `GetTransaction` for the window, is a hard `RESOURCE_EXHAUSTED` above 65,503 bytes,
   and is silently destroyed by every driver teardown because submit is
   dispatch-only. Stating the real cost is what lets a future maintainer weigh
   widening the fail-safe correctly.
5. **Test the state, not the placeholder.** A test that builds a transaction with an
   unknown branch ID must exercise the *release* table; today `proptest-impl` pulls in
   `zebra-test` and gives the test build an NU7 row production does not have.

## Validation Information

**Verdict: CONFIRMED. Severity raised from Low to Medium.**

Every mechanical claim was checked against primary sources.

**Confirmed as filed:**

1. The v5 and v6 deserializers call `NetworkUpgrade::try_from` on the branch ID
   before reading any bundle (`serialize.rs:1086-1087`, `:1150-1151`), and
   `try_from` is a lookup in the const `CONSENSUS_BRANCH_IDS` table returning
   `InvalidConsensusBranchId` on a miss (`network_upgrade.rs:75-85`). The error
   surfaces as `SerializationError::Parse("invalid consensus branch id")`
   (`serialization/error.rs:98`).
2. The production build has no NU7 row. The only `Nu7` entry is
   `#[cfg(any(test, feature = "zebra-test"))] (Nu7, ConsensusBranchId(0xfffffffe))`
   — a placeholder, not the real ID, which is still a TODO
   (`network_upgrade.rs:241-244`). Cross-checked against the distribution's
   librustzcash, where `BranchId::Nu7 => 0xffff_ffff` is likewise a placeholder
   (`librustzcash/components/zcash_protocol/src/consensus.rs:751`, `:772`). Grepping
   the whole distribution for the real value `0x77190ad8` returns exactly one hit:
   the TODO comment.
3. `Unparseable` routes to divert (`classify.rs:99-101`), `divert` never falls back
   to the operator (`intercept.rs:137-215`), and the >65,503-byte case is a
   non-retryable `RESOURCE_EXHAUSTED` (`wire.rs:124` = `65536 - 33`;
   `intercept.rs:167-179`).
4. The documentation gap is real. `grep -rni "network upgrade"` over
   `audit-target/zeronym/` returns **no hits**; `grep -i upgrade` over
   `shim/deploy/caution/OPERATORS.md`, `RESTARTS.md` and `shim/README.md` returns no
   hits. Every "NU6.3" mention in the tree is about pool semantics, never about the
   table going stale.

**Corrected — the filed issue's step 9 was wrong, and its arithmetic was inverted:**

The filed version claimed the state causes **silent destruction** via
`Refusal::ExpiryTooTight` for wallets with tight expiries. It does not, and cannot.
In this state the **hub cannot parse the transaction either** (same vendored
`zebra-chain`, same missing row), so `admit` folds expiry to `None`
(`queue.rs:189-197`) and `survives_next_flush(None, …)` returns `true`
unconditionally (`queue.rs:380-392`). `ExpiryTooTight` is unreachable for exactly
this class. The transaction is queued and published.

The filed text also inverted the expiry arithmetic it used to support that claim:
with a 20-block expiry delta the admission test `expiry >= next_flush_height(tip,20)+4`
fails when `tip mod 20 < 4` — i.e. when the tip is **fewer** than four blocks past a
flush boundary (20% of heights), not "more than four blocks past" as filed. That
branch is out of scope here in any case; it belongs to the general expiry story owned
by `a-constant-tip-offset-is-a-tunable-expiry-keyed-admission-filter-…` (Medium).

**Corrected — scope was both too narrow and too broad in places:**

- "All *shielded* traffic" understates it. The branch ID sits in the v5/v6 header, so
  **transparent-only v5 transactions are diverted too**. Post-NU5 wallets emit v5 for
  everything, so `Class::PassThrough` becomes an empty class in practice.
- "Every Sapling-only transaction" needs a qualifier: a **v4** Sapling transaction
  still parses, because v4 carries no `nConsensusBranchId` on the wire. Only v5/v6
  Sapling traffic is affected.

**Added — four consequences the filed issue missed, all verified:**

- **Empty txid to every wallet.** `local_txid` returns `String::new()` on a parse
  failure (`nym.rs:1370-1377`) and `HubTransport::submit` puts it straight into
  `Submit::Accepted` (`hub.rs:238-239`), so every `SendResponse` carries
  `error_code: 0` with an empty `error_message`.
- **The hub's queue lookup goes blind.** `admit` stores `txid: None`, and
  `find_by_txid` requires `Some` (`queue.rs:328-348`), so a wallet polling for its own
  just-sent transaction gets `NOT_FOUND` for the entire flush window — the one thing
  the queue-first lookup exists to prevent.
- **Hub downtime becomes a total send outage** rather than a migration-only one.
- **Every existing silent-loss path widens from migrations to all send traffic**,
  because the wallet is acknowledged at mixnet hand-off.

**Added — the loud-or-silent question, answered:** silent. `/healthz` tracks only
`MixnetStatus::is_healthy`; `/nym-status`'s four documented alert fields
(`OPERATORS.md:413-418`: `diversion_configured`, `mixnet_connected`, `client_deaths`,
`consecutive_rebuild_failures`) are all unchanged and all green; the per-transaction
`warn!` is the only trace, and `shim/src/proxy.rs:111` states outright that *"an
attested enclave has no console"*. In the shipped `DEBUG=1` default the console does
land in a file on the parent host (established in
`attested-enclave-console-is-reopenable-from-the-parent-…`), but nothing alerts on it
and no runbook tells anyone to look.

**Added — reachability, which is the reason for the re-grade.** This needs no
attacker at all; it is produced by an ordinary scheduled network upgrade. The
mainnet cadence in the vendored constants is accelerating (420,000 → 218,200 →
63,543 blocks between the last three activations,
`zebra-chain/src/parameters/constants.rs:90-99`, i.e. ~1 year → ~189 days → ~55
days), NU7 is already carried in the enum awaiting only its branch ID, and the
distribution's own changelog records that a stale image lacking the *current* branch
rules already shipped once and was caught only after a probe was added for zebrad
(`CHANGELOG.md:138-142`). No such probe exists for the shim.

**Severity: Medium, raised from Low.** Likelihood is effectively certain over the
lifetime of a long-lived attested image, and the failure is invisible to every signal
the product exposes. Impact stops short of High because the transactions are still
published — the direction of the failure is safe, and nothing leaks — so what users
actually suffer is universal added latency, an empty txid, a blind lookup window, a
hard refusal above 64 KiB, and a large widening of the population exposed to the
already-confirmed silent-loss paths. That places it alongside
`publish-verdict-strings-are-zcashds-vocabulary-only-…` (Medium): an environmental
change, not an attacker, converts a working component into a broken one, with no
signal.

**What this issue uniquely owns:** the coupling between the enclave image's frozen
consensus-branch table and a scheduled network upgrade, and the resulting
divert-everything state with its four downstream consequences. It does **not** own:
the release-profile question (`shim-manifest-pins-no-release-profile-…`), the
test/production table divergence
(`shim-tests-parse-with-a-zebra-chain-whose-consensus-branch-table-differs-from-the-shipped-one.md`),
dispatch-only acknowledgement (`nym-submit-acks-are-never-read-…`, Medium), or any of
the teardown/flood loss paths it composes with.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
