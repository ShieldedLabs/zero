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

* **It does not divert.** A detected Orchard to Ironwood migration is logged and
  then forwarded to the backing indexer exactly like any other transaction. The
  PoC is non-destructive by design, so the only visible effect of classification
  is a log line. The one exception, and the only request the shim refuses to
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
is_migration(tx) := tx.version == V6
                 && orchard_value_balance  > 0   (value LEAVING Orchard)
                 && ironwood_value_balance < 0   (value ENTERING Ironwood)
```

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

The offline demo starts a stub indexer, puts a real shim in front of it, sends a
migration, a non-migration, garbage, a compressed body and one ordinary method
through, and then runs the test suite. The live demo drives the same shim with
`grpcurl` against your own indexer; it falls back to the offline demo if grpcurl
is missing or the backing indexer is unreachable.

Real output from the offline demo:

```text
INFO zis::classify: MIGRATION detected: value leaving Orchard and entering Ironwood
  version=V6 orchard_vb=+250000 ironwood_vb=-240000 sapling_vb=+0 expiry=None
  inputs=0 outputs=0 tx_len=11994 diverted_in_production=true
INFO zis::classify: passthrough: SendTransaction non-migration
  version=V6 orchard_vb=-250000 ironwood_vb=+240000 ... diverted_in_production=false
WARN zis::classify: MIGRATION-FAILSAFE: unparseable SendTransaction body, treating as migration
  error="parse error: bad tx header" tx_len=64 frame_len=71 diverted_in_production=true
DEBUG zis::proxy: proxied method=POST path=/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo status=200
```

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

The V6 fixtures in `tests/fixtures/` are built in memory by `zebra-chain`'s own
`transaction::arbitrary` helpers and serialized to real wire bytes. They
round-trip through zebra's V6 codec and through the `librustzcash` re-parse that
zebra's deserializer performs internally, but they are not transactions any
wallet broadcast. **This is the largest outstanding gap in the evidence**, and
the vector to close it already exists: the regtest end-to-end test at
`zaino/live-tests/e2e/tests/ironwood_activation.rs` builds a consensus-valid
Orchard to Ironwood migration (the `orchard_note_spends_to_ironwood_across_boundary`
case). Capturing that transaction's raw bytes as
`tests/fixtures/v6_migration_real.bin` and asserting it classifies as
`Migration` with `orchard_vb > 0` and `ironwood_vb < 0` turns the crate's central
claim from "our own generator round-trips" into "a transaction a wallet actually
produced is detected". It needs a running regtest node, which is why it is not
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
2. The predicate's strict `orchard_value_balance > 0`. An earlier version of this
   note called it a false-negative window at net-zero *or net-negative* Orchard
   and floated a gross alternative ("an Orchard bundle with at least one spend
   AND `ironwood_value_balance < 0`"). **That was wrong and is retracted**: it
   ignored NU6.3's cross-address restriction. Post-activation a
   transaction-level rule forbids new value entering the Orchard pool, so
   `orchard_vb >= 0` always and the net-negative case is consensus-invalid. The
   only excluded shape is `orchard_vb == 0`, a pure same-receiver-change Orchard
   bundle, where value entering Ironwood came from transparent or Sapling: a
   shield into Ironwood, not an Orchard migration. The gross alternative is now
   worse, since Orchard stays open to *activity* (same-receiver change) and it
   would sweep in ordinary non-migrations. Pinned by
   `zero_orchard_balance_is_the_known_predicate_boundary`. The remaining question
   for Taylor and Zooko is narrower: batch the `orchard_vb == 0` case anyway as
   cheap insurance, accepting false positives?
3. Depending on `zaino-proto` pulls tonic into the shipped dependency graph for
   two protobuf messages. Hand-writing those two structs (about 20 lines) would
   shrink the enclave's trusted surface before the enclave build.
4. The shim dials the backing indexer before it classifies anything, just later
   than it used to: the dial is now lazy, on the first request, rather than on
   TCP accept. Harmless while the PoC is non-destructive, but at the diversion
   milestone the operator's indexer must not see so much as a TCP connection for
   a wallet whose transaction is about to be diverted. Classify first, connect
   second.
