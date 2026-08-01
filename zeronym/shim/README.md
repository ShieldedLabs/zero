# zero-indexer-shim (ZIS)

Proof of concept for the Zeronym shim: a transparent reverse proxy an operator
puts in front of their existing light-wallet indexer (lightwalletd or Zaino).

It forwards every `CompactTxStreamer` request to the backing indexer unchanged,
including streaming responses and gRPC trailers. The one exception is
`SendTransaction`, whose body it decodes, classifies with the real `zebra-chain`
parser, and **logs**. Nothing else about the call changes.

The design lives in The Zeronym Book, which is reviewed separately on the
`claude/zeronym-book` branch: see its `components.md` chapter for the shim and
`problem.md` for the threat model. This crate is one afternoon of it.

## What this PoC deliberately does NOT do

* **It does not divert.** A detected Orchard-touching transaction is logged and
  then forwarded to the backing indexer exactly like any other. The PoC is
  non-destructive by design, so the only visible effect of classification is a
  log line. The one exception, and the only request the shim refuses to
  forward, is a `SendTransaction` body it could not buffer: over 4 MiB, or a
  client body stream that broke mid-upload. Those bytes cannot be replayed
  byte-for-byte, and forwarding a body that could be neither read nor reproduced
  is the leak this component exists to prevent
  (`an_oversized_send_transaction_is_refused_and_never_forwarded`).
* No hub, no Nym, no STEVE.
* No TLS and no ACME. Transport is plaintext h2c with prior knowledge on both
  legs. `curl http://...` will look broken even when the shim is healthy;
  `grpcurl -plaintext` and tonic channels over `http://` both work, because both
  use prior knowledge.
* No enclave and no attestation.
* No upstream connection pooling across clients. One HTTP/2 connection to the
  backing indexer is opened per inbound client connection, lazily, on the first
  request that needs it. It IS redialled when the indexer restarts
  (`the_shim_redials_after_the_backing_indexer_restarts` pins that), because
  without a redial the shim answers UNAVAILABLE forever on a healthy connection
  to the wallet, and a clean application-level status is exactly what a wallet's
  reconnect logic does not react to.

## The classifier

`src/classify.rs` is the highest-stakes file and is a pure, total function of the
raw transaction bytes: no I/O, no state, no config.

```text
is_orchard_touching(tx) := tx has at least one Orchard action
```

One conjunct. No version guard, no destination check, no amount, no sign. What
the shim detects is **Orchard activity**: presence. Not an exit, not a direction,
not a quantity.

This is Zooko's ruling on the classifier's scope, in his words: any transaction
that has any Orchard actions in it is (a) potentially security-sensitive, because
it could leak information the user did not want to disclose, and (b) probably
time-insensitive, because people and their tools are already used to the idea
that doing anything with Orchard funds might take longer than normal. So the safe
default is to divert every one of them to the batching system, regardless of
whether `orchard_value_balance` is greater than the fee, equal to the fee, or
zero.

Both halves of that rationale matter, and together they are what keeps the rule
from growing:

* **Security-sensitive.** NU6.3 closes Orchard to new *value*: a
  transaction-level rule forbids value entering, so the chain predicate is
  Orchard pool value non-increasing and `orchard_vb >= 0` holds for every
  post-activation transaction. Anyone still holding Orchard notes has therefore
  held them since before activation, which makes touching Orchard *at all* the
  identifying event: it reveals "this IP controls legacy Orchard funds" against a
  finite, shrinking set of holders. Spending publishes nullifiers whatever the
  balance nets to, and where the value lands afterwards changes nothing about
  that inference, so an Orchard withdrawal to transparent or to Sapling is
  diverted on exactly the same footing as one into Ironwood.
* **Time-insensitive.** Batching costs latency, and Orchard users already expect
  legacy-fund movement to be slow. That is what makes the safe default
  affordable here, and it is why the rule stops where it does.

**Orchard only, not Ironwood, and that boundary is deliberate.** Ironwood is the
NEW pool, where ordinary time-sensitive commerce will live, so the
time-insensitivity half of the rationale does not hold for it. A transaction
carrying only Ironwood actions reads `orchard_shielded_data() == None` and passes
through. There is no Ironwood arm in the predicate and none should be added
(`an_ironwood_only_transaction_is_a_pass_through` pins it, whichever way the
Ironwood balance points).

**What the widening closed.** A transaction must pay a fee, and unless the fee
comes from another pool it comes out of Orchard, so most internal shuffling
already showed `orchard_value_balance > 0` and the old exit predicate caught it.
The gap was the shuffle whose fee is paid from a **different** pool, which leaves
`orchard_value_balance == 0` with Orchard actions still present, legacy notes
still spent and their nullifiers still published. That transaction used to be
handed to the operator's indexer in the clear
(`zero_orchard_value_balance_with_orchard_actions_is_a_migration`).

Three precisions to keep straight:

* Orchard is closed to new value, **not** to activity. Same-receiver change still
  lands in the pool and the note commitment tree keeps growing. It is not
  "exit-only", and that is exactly why a presence test catches activity an exit
  test does not.
* V5 transactions carry Orchard bundles too, so a V5 Orchard spend leaks the same
  fact as a V6 one and is caught by the same line. Dropping the version conjunct
  needs no replacement guard: `zebra-chain`'s `orchard_shielded_data()` is
  version-agnostic and returns `None` for V1..V4, where there is no Orchard
  bundle at all, so a transparent transaction passes by the predicate itself
  rather than by a special case.
* The predicate is written as "an Orchard bundle is present" rather than "the
  action count is non-zero" because the two are *exactly* equivalent and the
  first form cannot be fooled by an empty bundle:
  `orchard::ShieldedData.actions` is an `AtLeastOne<AuthorizedAction>`
  (`zebra/zebra-chain/src/orchard/shielded_data.rs`), so a bundle that exists
  carries at least one action. `presence_and_action_count_agree` pins the
  equivalence, and a `debug_assert` in `classify_with_evidence` fires in debug
  and in CI if it ever stops holding.

`orchard_value_balance` is still parsed and still logged, and it gates nothing.
It is **evidence**: `orchard_actions` says whether to divert, `orchard_vb` says
how much moved. `orchard_vb=+0` on a line reading `MIGRATION` is precisely the
case the widening added. `ironwood_value_balance` and `sapling_value_balance` are
evidence on the same footing, showing an operator where an Orchard withdrawal
went; neither may become part of the predicate.

The diverted class is still spelled `Class::Migration`, and the routing helper is
still `treat_as_migration()`. That name is imprecise now, and kept deliberately:
an Orchard-to-transparent deshield is not literally a migration into Ironwood,
and a net-zero internal shuffle moves nothing anywhere. Post-NU6.3 all Orchard
activity is legacy-fund activity, so batching all of it is the right behaviour
and only the label lags. Read "migration" in the log lines and the operator docs
as the legacy name for the class; `is_orchard_touching` is the accurate name for
the predicate behind it.

A false negative is a privacy leak, so anything the shim cannot read cleanly
(unparseable bytes, a compressed gRPC frame, a truncated message, a protobuf
that does not decode) is treated as a migration, logged as `MIGRATION-FAILSAFE`.
The rule is written once, in `Class::treat_as_migration()`.

### Which requests reach the classifier

> **The interception set must be a superset of every routing predicate any
> supported backend uses, never a subset.**

A predicate narrower than the backend's fails *open*: the backend acts on a
request the classifier never saw. The vendored tonic server Zaino is built from
dispatches on `req.uri().path()` alone, with no HTTP-method guard, so a `GET` to
the `SendTransaction` path reaches its `send_transaction` handler. `route_for()`
in `src/proxy.rs` is therefore a pure function of the path only, and cannot see
the HTTP method even if someone wants it to. Paths whose final segment spells
`sendtransaction` in another case, or with a trailing slash, are classified too:
no backend we have checked routes those, but the two mistakes are not
symmetric.

The classifier can also be blinded from the other end, so
`proxy::normalize_response_encoding` rewrites the backing indexer's advertised
`grpc-accept-encoding` to `identity` on the way back. Without it, an operator
could turn on compression negotiation in their own indexer, wallets would start
compressing `SendTransaction` bodies, and every send would land in the
compression fail-safe: an operator-controlled lever on the classifier, in a
component whose threat model is that the operator is the adversary. Response
compression itself (`grpc-encoding`) is relayed untouched.

## Layout

| Path | What it is |
| --- | --- |
| `src/classify.rs` | The turnstile predicate. Pure. Audit this first. |
| `src/intercept.rs` | `SendTransaction` only: unframe, decode, classify, log, replay the original bytes. |
| `src/proxy.rs` | The h2c reverse proxy. Everything else is opaque. |
| `src/config.rs` | Two socket addresses. |
| `tests/` | Transparency and classifier vectors. See below. |
| `deploy/` | The StageX reproducible build. See `deploy/README.md`. |

The `cargo build` above is for development. The **audited** artifact is the
static-musl binary produced by `deploy/`, whose whole purpose is that two
independent builds of a commit yield the same hash, so an auditor can match it
against the hash bound into an enclave attestation. Without that, an attestation
proves only that some binary runs in a genuine enclave, not that it is the
binary anyone reviewed.

That coupling runs both ways, and it is the reason the predicate and the deploy
directory move together. Widening the predicate changed `src/classify.rs`,
`src/intercept.rs` and `src/lib.rs`, so the published binary hash moved with
them. The single machine-readable copy is `deploy/EXPECTED_SHA256`, and
`deploy/README.md` records what each value was built from. A hash is only ever
measured from *committed* source: `deploy/assemble.sh` builds its context from
`git archive HEAD` and cannot see a working tree, so a classifier edit that has
not been committed is absent from the build while every script cheerfully
confirms the old hash.

## Running it

```sh
cargo build --release
./target/release/zero-indexer-shim --listen 127.0.0.1:9068 --backend 127.0.0.1:9067
```

`ZIS_LISTEN` and `ZIS_BACKEND` work too. The defaults are exactly the pair above:
9067 is the conventional lightwalletd and Zaino gRPC port, so the operator's
existing indexer keeps its usual address and the shim takes the new one. Point a
wallet at the shim's address. `RUST_LOG=info` (the default) shows the verdicts.

The per-request `zis::proxy` line is at `debug`, below the default, on purpose:
a line naming the method each wallet called is exactly the metadata this
component exists to deny the operator, and by default the shim does not write an
access log on the operator's box. `RUST_LOG=zis::proxy=debug,info` turns it on
for a demo or a debugging session.

## Reproducing the demo

```sh
./demo.sh                    # offline: needs nothing but cargo
./demo.sh HOST:PORT          # live: in front of a real lightwalletd or Zaino
```

The offline demo starts a stub indexer, puts a real shim in front of it, and
sends eight calls through: Orchard actions into Ironwood, Orchard actions with no
Ironwood bundle at all, Orchard actions whose value balance is exactly zero, an
Ironwood-only transaction, a real mainnet V4 transparent transaction, garbage, a
compressed body, and one ordinary proxied method. It then runs the test suite.
The live demo drives the same shim with `grpcurl` against your own indexer; it
falls back to the offline demo if grpcurl is missing or the backing indexer is
unreachable.

Real output from the offline demo, with timestamps and the stub indexer's own
lines dropped and the long lines wrapped:

```text
INFO zis::classify: MIGRATION detected: the transaction carries Orchard actions, so
  it is diverted whatever its Orchard value balance (this PoC still forwards it;
  production diverts it to the hub) version=V6 orchard_actions=1
  orchard_vb=+250000 ironwood_vb=-240000 sapling_vb=+0 expiry=None inputs=0
  outputs=0 tx_len=11994 diverted_in_production=true
INFO zis::classify: MIGRATION detected: the transaction carries Orchard actions, so
  it is diverted whatever its Orchard value balance (this PoC still forwards it;
  production diverts it to the hub) version=V6 orchard_actions=1
  orchard_vb=+250000 ironwood_vb=+0 sapling_vb=+0 expiry=None inputs=0 outputs=0
  tx_len=6010 diverted_in_production=true
INFO zis::classify: MIGRATION detected: the transaction carries Orchard actions, so
  it is diverted whatever its Orchard value balance (this PoC still forwards it;
  production diverts it to the hub) version=V6 orchard_actions=1 orchard_vb=+0
  ironwood_vb=-240000 sapling_vb=+0 expiry=None inputs=0 outputs=0 tx_len=11994
  diverted_in_production=true
INFO zis::classify: passthrough: SendTransaction carries no Orchard actions
  version=V6 orchard_actions=0 orchard_vb=+0 ironwood_vb=-240000 sapling_vb=+0
  expiry=None inputs=0 outputs=0 tx_len=6010 diverted_in_production=false
INFO zis::classify: passthrough: SendTransaction carries no Orchard actions
  version=V4 orchard_actions=0 orchard_vb=+0 ironwood_vb=+0 sapling_vb=+0
  expiry=Some(2222000) inputs=1 outputs=4 tx_len=205
  diverted_in_production=false
WARN zis::classify: MIGRATION-FAILSAFE: unparseable SendTransaction body, treating
  as migration error="parse error: bad tx header" tx_len=64 frame_len=71
  body_prefix=00000000420a40ffffffffffff diverted_in_production=true
WARN zis::classify: MIGRATION-FAILSAFE: SendTransaction body could not be
  classified, treating as migration reason="grpc-encoding is not identity"
  detail="gzip" frame_len=12002 body_prefix=0100002edd0ada5d0600008098
  diverted_in_production=true
DEBUG zis::proxy: proxied method=POST
  path=/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo status=200
  grpc_status="(in trailers)"
```

The **third** verdict is the whole of the widening in one line of output:
`orchard_vb=+0` on a transaction that still says `MIGRATION`, because
`orchard_actions=1`. No Orchard value moved at all, so under the old exit
predicate that line read `passthrough` and handed the transaction to the
operator's indexer in the clear, nullifiers and all. The second verdict is the
previous round's exhibit, still valid: `ironwood_vb=+0`, so the value went to
transparent or Sapling rather than into Ironwood, and the destination does not
enter the rule.

The **fourth** verdict is the boundary in the other direction, and the reason the
rule stops at Orchard: `orchard_actions=0` on a V6 that carries a real Ironwood
bundle (`ironwood_vb=-240000`). Ironwood is the new pool where ordinary
time-sensitive commerce lives, so it passes through. The fifth is the other
realistic pass-through: real mainnet transparent bytes with no Orchard bundle.
(Those bytes are a coinbase, because that is the mainnet transaction committed in
this crate; what the classifier reads is the absent Orchard bundle, which any
ordinary transparent or Sapling payment shares.)

(The demo turns `zis::proxy=debug` on explicitly. The shipped binary does not.)

The parts alone:

```sh
cargo run --example shim_demo    # the log output above
cargo test                       # the assertions
```

## Tests

```sh
cargo test
```

* `tests/grpc_transparency.rs`: a real tonic `CompactTxStreamer` server standing
  in for the indexer and the generated tonic client standing in for a wallet.
  Every call is made twice, directly and through the shim, and the two results
  must be identical.
* `tests/proxy_transparency.rs`: the same properties at the raw HTTP/2 level,
  where a tonic client would hide them. Byte-exact request frames, trailers as
  frames (both directions), trailers-only responses, unknown method paths, and
  two gated streaming tests, one per direction, that fail by timeout if the shim
  ever buffers. Also the three failure paths: an oversized `SendTransaction` is
  refused and never forwarded, a non-POST or near-miss `SendTransaction` is
  still intercepted, and a restarted backing indexer is redialled on the wallet's
  existing connection.
* `tests/classify_logging.rs`: captures the shim's own `tracing` output and
  asserts the verdicts. Since the PoC is non-destructive, this is the only
  evidence that the classifier ran at all.
* `tests/classify_vectors.rs` and `tests/classify_generated.rs`: the predicate,
  against committed V6 wire-byte fixtures and against freshly generated ones.
  The ones that pin the scope specifically are
  `zero_orchard_value_balance_with_orchard_actions_is_a_migration` (the gap the
  widening closes: `orchard_vb == 0` with Orchard actions present, which the old
  exit predicate passed through in the clear),
  `an_ironwood_only_transaction_is_a_pass_through` (the boundary the rule must
  not cross, asserted whichever way the Ironwood balance points),
  `orchard_actions_without_an_ironwood_bundle_are_a_migration` and
  `every_orchard_touching_transaction_is_a_migration_whatever_the_destination`
  (the destination is not part of the rule),
  `the_direction_of_the_orchard_balance_does_not_change_the_verdict` and
  `the_magnitude_of_the_orchard_balance_does_not_change_the_verdict` (neither
  sign nor size is read), and `a_v5_orchard_spend_is_a_migration` (no version
  guard). The equivalence the predicate is written on is pinned by
  `presence_and_action_count_agree` and
  `the_predicate_is_the_presence_of_orchard_actions`, both unit tests in
  `src/classify.rs`.

The five V6 fixtures in `tests/fixtures/` are built in memory by `zebra-chain`'s
own `transaction::arbitrary` helpers and serialized to real wire bytes. They
round-trip through zebra's V6 codec and through the `librustzcash` re-parse that
zebra's deserializer performs internally, but they are not transactions any
wallet broadcast. Three carry Orchard bundles and are all `Migration`
(`v6_migration.bin` into Ironwood, `v6_orchard_only.bin` with no Ironwood bundle,
`v6_reverse.bin` with the balance negated, which is consensus-invalid post-NU6.3
and kept only as a directionality probe). `v6_orchard_zero.bin` is the net-zero
Orchard shuffle the widening added, and `v6_ironwood_only.bin` is the
Ironwood-only pass-through that keeps the rule from swallowing ordinary commerce.
The only real mainnet bytes in the crate are the V4 transparent vector, which is
the realistic pass-through. `regenerate_fixtures` in `classify_generated.rs`
rewrites all five; it is `#[ignore]`d because it writes into the source tree.

**This is the largest outstanding gap in the evidence**, and the vector to close
it already exists: the regtest end-to-end test at
`zaino/live-tests/e2e/tests/ironwood_activation.rs` builds a consensus-valid
Orchard to Ironwood migration (the `orchard_note_spends_to_ironwood_across_boundary`
case). Capturing that transaction's raw bytes as
`tests/fixtures/v6_migration_real.bin` and asserting it classifies as `Migration`
with `orchard_actions > 0` turns the crate's central claim from "our own
generator round-trips" into "a transaction a wallet actually produced is
detected". Its `orchard_vb` and `ironwood_vb` are worth recording alongside as
evidence, but neither is what the verdict rests on. It needs a running regtest
node, which is why it is not here.

## Notes for whoever picks this up

* `zebra-chain` and `zaino-proto` are **path dependencies on the vendored
  subtrees**. `zaino-proto` must stay `default-features = false`: its `heavy`
  feature lets its `build.rs` find `protoc` and regenerate its committed protos
  inside the vendored tree, and it also pulls a second `zebra-chain` from
  crates.io. `git status --porcelain zaino/ zebra/` must stay empty after a
  build; that is the tripwire.
* Containerizing this will break on those path deps unless the image uses a
  repo-root build context. That is the same failure that reverted the orchard
  vendored-path pilot (e9e8c15d91).
* Diversion plugs into `intercept::send_transaction`, as one branch on
  `inspection.treat_as_migration()` right after the log.

## Open questions

1. A compressed `SendTransaction` is currently logged as `MIGRATION-FAILSAFE`
   and still forwarded. The locked scope said log and forward; the book says
   treat as a migration. In a non-destructive PoC both answers forward, so only
   the label differs, but the label is what the production routing decision gets
   read off. The shim no longer lets the operator *cause* this case at will (it
   normalizes the advertised `grpc-accept-encoding` to `identity`), but a wallet
   that compresses unprompted still lands here.
2. Depending on `zaino-proto` pulls tonic into the shipped dependency graph for
   two protobuf messages. Hand-writing those two structs (about 20 lines) would
   shrink the enclave's trusted surface before the enclave build.
3. The shim dials the backing indexer before it classifies anything, just later
   than it used to: the dial is now lazy, on the first request, rather than on
   TCP accept. Harmless while the PoC is non-destructive, but at the diversion
   milestone the operator's indexer must not see so much as a TCP connection for
   a wallet whose transaction is about to be diverted. Classify first, connect
   second.
Open question 4, "a net-zero Orchard bundle that still spends legacy notes", is
**closed**. Zooko widened the predicate to answer it, the code implements the
answer, and the history is under [Settled: the predicate's
scope](#settled-the-predicates-scope).

## Settled: the predicate's scope

**Zooko has ruled, twice, and the second ruling supersedes the first.** The
predicate is now the presence of Orchard actions:

> Any transaction that has any Orchard actions in it is (a) potentially
> security-sensitive, because it could leak information the user did not want to
> disclose, and (b) probably time-insensitive, because people and their tools are
> already used to the idea that doing anything with Orchard funds might take
> longer than normal. So a nice safe default would be: if there are any Orchard
> transactions in here we divert them to the batching system for added
> security/privacy, regardless of whether `orchard_value_balance` is >= the fee,
> == the fee, or is 0.

See [The classifier](#the-classifier) for the full rationale and for the Ironwood
boundary, which is the one place the rule deliberately stops.

**The history, because the file used to argue the opposite and that is worth
keeping legible rather than deleting.**

* The first ruling was `orchard_value_balance > 0`, value leaving a closed pool,
  with the `tx.version == V6` and `ironwood_value_balance < 0` conjuncts already
  dropped. That predicate is gone. Anything in this repository or its history
  that states `is_orchard_exit(tx) := orchard_value_balance > 0` as the current
  rule is superseded.
* Before that, this note floated a *gross* alternative ("an Orchard bundle with
  at least one spend AND `ironwood_value_balance < 0`") on the theory that the
  exit test left a window at net-zero *or net-negative* Orchard. The
  net-negative half was wrong and was retracted: NU6.3's transaction-level rule
  makes `orchard_vb >= 0` always, so value entering Orchard is consensus-invalid
  and cannot appear on chain. Do not reintroduce the `ironwood_value_balance < 0`
  conjunct; the destination is not part of the rule, and adding an Ironwood arm
  would break the boundary the current rule depends on.
* The net-zero half was right, and the file argued it away. It concluded that
  passing `orchard_vb == 0` through "is what the ruling specifies, not a
  conservative reading of it", on the ground that no value left the pool. **That
  conclusion is retracted.** A transaction must pay a fee, and unless the fee
  comes from another pool it comes out of Orchard, so the case that actually
  slipped through was narrow: an internal shuffle whose fee is paid from
  transparent or Sapling. Narrow is not empty. It spends legacy Orchard notes and
  publishes their nullifiers, which is the identifying event the whole rationale
  turns on, and the shim handed it to the operator's indexer in the clear.
* Zooko's widening closes that by not reading the balance at all. It costs a
  wider diverted set (ordinary same-receiver-change activity is now batched too),
  which the earlier note treated as a reason against. Under the second ruling
  that cost is accepted on purpose: Orchard users already expect legacy-fund
  movement to be slow, and a false positive is a wasted diversion where a false
  negative is a privacy leak.

Two cases exhaust the space, and the axis is the action count, not the balance:

| `orchard_actions` | Verdict | `orchard_vb` (evidence only) |
| --- | --- | --- |
| `> 0` | `Migration` | Any value. `> 0` is an exit, `== 0` is the net-zero shuffle the widening added, `< 0` is consensus-invalid post-NU6.3 and kept only as a directionality probe in the tests. All three divert. |
| `== 0` | `PassThrough` | Always `0`: no Orchard bundle, so nothing to report. Transparent, Sapling and Ironwood-only transactions land here. |

So the batched set is not "migrations" in the literal sense. It is every
transaction that touches Orchard, which post-NU6.3 is legacy-fund activity
whatever its balance and whatever its destination.
