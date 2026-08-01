# zero-indexer-shim (ZIS)

Proof of concept for the Zeronym shim: a transparent reverse proxy an operator
puts in front of their existing light-wallet indexer (lightwalletd or Zaino).

It forwards every `CompactTxStreamer` request to the backing indexer unchanged,
including streaming responses and gRPC trailers. The one exception is
`SendTransaction`, whose body it decodes, classifies with the real `zebra-chain`
parser, and **logs**. Nothing else about the call changes.

The design lives in the book at `../book/src/` (`components.md` for the shim,
`problem.md` for the threat model). This crate is one afternoon of it.

## What this PoC deliberately does NOT do

* **It does not divert.** A detected Orchard exit is logged and then forwarded to
  the backing indexer exactly like any other transaction. The PoC is
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
is_orchard_exit(tx) := orchard_value_balance > 0   (value LEAVING Orchard)
```

One conjunct. No version guard, no destination check. What the shim detects is an
**Orchard exit**: a transaction that moves value out of the Orchard pool,
whatever pool the value lands in afterwards.

This is Zooko's ruling on the classifier's scope, and the argument behind it is
the closed pool. NU6.3 closes Orchard to new *value*: a transaction-level rule
forbids value entering, so the chain predicate is Orchard pool value
non-increasing, and `orchard_vb >= 0` holds for every post-activation
transaction. Anyone still holding Orchard notes has therefore held them since
before activation, which makes spending Orchard *at all* the identifying event:
it reveals "this IP controls legacy Orchard funds" against a finite, shrinking
set of holders. Where the value lands afterwards changes nothing about that
inference, so an Orchard withdrawal to transparent or to Sapling is diverted on
exactly the same footing as one into Ironwood.

Two precisions to keep straight:

* Orchard is closed to new value, **not** to activity. Same-receiver change still
  lands in the pool and the note commitment tree keeps growing. It is not
  "exit-only".
* V5 transactions carry Orchard bundles too, so a V5 Orchard spend is as real an
  exit as a V6 one. Dropping the version conjunct needs no replacement guard:
  `zebra-chain`'s `orchard_value_balance()` reads `orchard_shielded_data()`,
  which is version-agnostic and returns zero when there is no Orchard bundle, so
  a V1..V4 transparent transaction reads `orchard_vb == 0` and passes by the
  predicate itself rather than by a special case.

`ironwood_value_balance` is still parsed and still logged, and it now gates
nothing. It is **evidence**: the field that shows an operator where an Orchard
exit went, which is exactly how you see the classifier catching the destinations
the old Orchard-to-Ironwood predicate missed.

The diverted class is still spelled `Class::Migration`, and the routing helper is
still `treat_as_migration()`. That name is imprecise now, and kept deliberately:
an Orchard-to-transparent deshield is not literally a migration into Ironwood.
Post-NU6.3 every Orchard exit is legacy-fund movement, so batching all of them is
the right behaviour and only the label lags. Read "migration" in the log lines
and the operator docs as the legacy name for the class; `is_orchard_exit` is the
accurate name for the predicate behind it.

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
sends six calls through: an Orchard exit into Ironwood, an Orchard exit with no
Ironwood bundle at all, a real mainnet V4 transparent transaction, garbage, a
compressed body, and one ordinary proxied method. It then runs the test suite.
The live demo drives the same shim with `grpcurl` against your own indexer; it
falls back to the offline demo if grpcurl is missing or the backing indexer is
unreachable.

Real output from the offline demo, with timestamps and the stub indexer's own
lines dropped and the long lines wrapped:

```text
INFO zis::classify: MIGRATION detected: an Orchard exit, value LEAVING the Orchard
  pool for any destination (this PoC still forwards it; production diverts it to
  the hub) version=V6 orchard_vb=+250000 ironwood_vb=-240000 sapling_vb=+0
  expiry=None inputs=0 outputs=0 tx_len=11994 diverted_in_production=true
INFO zis::classify: MIGRATION detected: an Orchard exit, value LEAVING the Orchard
  pool for any destination (this PoC still forwards it; production diverts it to
  the hub) version=V6 orchard_vb=+250000 ironwood_vb=+0 sapling_vb=+0
  expiry=None inputs=0 outputs=0 tx_len=6010 diverted_in_production=true
INFO zis::classify: passthrough: SendTransaction moved no value out of Orchard
  version=V4 orchard_vb=+0 ironwood_vb=+0 sapling_vb=+0 expiry=Some(2222000)
  inputs=1 outputs=4 tx_len=205 diverted_in_production=false
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

The second verdict is the whole of the scope change in one line of output:
`ironwood_vb=+0` on a transaction that still says `MIGRATION`. Value left Orchard
for transparent or Sapling, and under the old Orchard-to-Ironwood predicate that
line read `passthrough`, which handed the transaction to the operator's indexer
in the clear. The third verdict is the realistic pass-through: real mainnet
transparent bytes whose Orchard balance is zero. (Those bytes are a coinbase,
because that is the mainnet transaction committed in this crate; what the
classifier reads is `orchard_vb == 0`, which any ordinary transparent or Sapling
payment shares.)

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
  `an_orchard_exit_without_an_ironwood_bundle_is_a_migration` (a withdrawal with
  no Ironwood bundle at all, which the old predicate passed through in the
  clear), `every_orchard_exit_is_a_migration_whatever_the_destination` (Ironwood
  balance negative, positive and absent, all `Migration`),
  `a_v5_orchard_spend_is_a_migration` (no version guard),
  `the_predicate_is_directional_not_symmetric` and
  `value_entering_orchard_is_pass_through` (the sign of the Orchard balance alone
  decides), and `zero_orchard_balance_is_correctly_a_pass_through`.

The V6 fixtures in `tests/fixtures/` are built in memory by `zebra-chain`'s own
`transaction::arbitrary` helpers and serialized to real wire bytes. They
round-trip through zebra's V6 codec and through the `librustzcash` re-parse that
zebra's deserializer performs internally, but they are not transactions any
wallet broadcast. (`v6_orchard_only.bin` is unchanged bytes with a changed
verdict: an Orchard exit with no Ironwood bundle, so it is a `Migration` now
where it used to be a `PassThrough`. The only real mainnet bytes in the crate are
the V4 transparent vector, which is the realistic pass-through.)

**This is the largest outstanding gap in the evidence**, and the vector to close
it already exists: the regtest end-to-end test at
`zaino/live-tests/e2e/tests/ironwood_activation.rs` builds a consensus-valid
Orchard to Ironwood migration (the `orchard_note_spends_to_ironwood_across_boundary`
case). Capturing that transaction's raw bytes as
`tests/fixtures/v6_migration_real.bin` and asserting it classifies as `Migration`
with `orchard_vb > 0` turns the crate's central claim from "our own generator
round-trips" into "a transaction a wallet actually produced is detected". Its
`ironwood_vb < 0` is worth recording alongside as evidence, but it is not what
the verdict rests on. It needs a running regtest node, which is why it is not
here.

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
4. **For Zooko and Taylor: a net-zero Orchard bundle that still spends legacy
   notes.** The ruling's criterion is `orchard_value_balance > 0`, value leaving
   the pool, and the predicate implements exactly that. But a transaction can
   spend legacy Orchard notes and net to zero (fee paid from transparent or
   Sapling, change back to the same receiver), and spending them publishes those
   notes' nullifiers on the wire. The ruling's own rationale is that *spending
   Orchard at all* is the identifying event, so that transaction is an
   identifying event the criterion does not catch: it classifies `PassThrough`
   and is broadcast in the clear through the operator's indexer. This is not a
   proposal to change the predicate, and the code has not been changed to
   pre-empt it; it is the question of whether the criterion should widen to "an
   Orchard bundle is present with at least one spend". That is a design owner's
   call, not the classifier's, and widening has a real cost (it sweeps in
   ordinary same-receiver-change activity, which is why the old *gross*
   alternative was rejected on its own terms, below). Flagged so it stays open
   rather than being closed by a comment.

## Settled: the predicate's scope

**Zooko has ruled.** *Any tx with Orchard value balance > 0 is a privacy risk to
the user, regardless of the destination pool.* The predicate is that one
conjunct, and the other two (`tx.version == V6`, `ironwood_value_balance < 0`)
are gone. See [The classifier](#the-classifier) for the closed-pool argument; the
short form is that NU6.3 closed Orchard to new value, so spending Orchard at all
is the identifying event and where the value lands does not change what was
revealed.

**The old net-versus-gross question is dissolved, not answered.** It sat in the
Open questions list above until now, and its history is worth keeping legible
rather than deleting:

* This note first claimed the strict `orchard_value_balance > 0` left a
  false-negative window at net-zero *or net-negative* Orchard, and floated a
  gross alternative ("an Orchard bundle with at least one spend AND
  `ironwood_value_balance < 0`"). **That was wrong and was retracted**: it ignored
  NU6.3's cross-address restriction, under which `orchard_vb >= 0` always, so the
  net-negative case is consensus-invalid and cannot appear on chain. The gross
  alternative is also worse on its own terms, since Orchard stays open to
  *activity* and it would sweep in ordinary same-receiver-change transactions.
  Do not reintroduce it.
* What survived the retraction was a narrower judgement call: batch the
  `orchard_vb == 0` case anyway, as cheap insurance?
* Under Zooko's criterion that *judgement call* answers itself. The criterion is
  that value **left** a closed pool. An Orchard bundle netting to exactly zero
  moved none out (pure same-receiver change, which is still possible because
  Orchard is closed to new value, not to activity), and whatever entered Ironwood
  alongside it came from transparent or Sapling. So passing it through is what
  the ruling specifies, not a conservative reading of it. Pinned by
  `zero_orchard_balance_is_correctly_a_pass_through`.
* What is **not** settled by that, and is open question 4 above: the same
  net-zero bundle can still spend legacy notes and publish their nullifiers, so
  the criterion and the rationale behind it do not coincide at exactly this
  point. That is a scope question for Zooko, not a defect in the predicate.

Three cases exhaust the space:

| `orchard_vb` | Verdict | Why |
| --- | --- | --- |
| `> 0` | `Migration` | Legacy value left a pool closed to new value. Batch it. |
| `== 0` | `PassThrough` | No Orchard value left the pool, which is the ruling's criterion. (A net-zero bundle can still spend legacy notes: open question 4.) |
| `< 0` | `PassThrough` | Value entering Orchard: consensus-invalid post-NU6.3. Kept only as a directionality probe in the tests. |

So the batched set is no longer "migrations" in the literal sense. It is every
Orchard exit, which post-NU6.3 is legacy-fund movement whatever its destination.
