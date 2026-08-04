# The shim and the hub

The concrete engineering designs for both TEE services. The shim and the hub are meant to be reviewed together by Anton (Caution) and Zooko; the cross-party open forks are collected in [review](./review.md). **Decision:** marks a committed choice. See [trust](./trust.md) for the STEVE and attestation deep-dive and for [honest limits](./trust.md); see [problem](./problem.md) for threat context.

## The zero-indexer-shim (ZIS)

The ZIS is an attested-TEE proxy an operator deploys behind their **existing public URL** (e.g. `zec.rocks:443`). It is a drop-in LWD to every wallet (no reconfiguration or endpoint change; wallets do need aligned anchors and expiry within a migration epoch, see [the problem](./problem.md)), forwards all traffic to the operator's unmodified backing lwd, and isolates only **Orchard-exit** `SendTransaction`s (transactions moving value out of the Orchard pool, the class the code and the hub protocol still call a *migration*), which it routes over Nym to the hub.

### Why a shim, not the whole indexer in a TEE

An earlier plan put the entire indexer (a full Zebra node plus the indexer) inside the enclave, so the operator could see nothing at all. That is expensive: until the enclave platform ships disk support, it runs entirely in RAM at roughly 400 to 500 GB, on the order of $2,000 per operator per month, with about a four-day resync on every restart. That cost wall makes operator adoption unrealistic.

The shim avoids it by being a thin router, not an indexer:

- **Cheap and fast to restart.** No heavy chain state lives inside the TEE, so the RAM and cost wall disappears and restarts are quick.
- **Base-agnostic.** It sits in front of whatever the operator already runs, sidestepping the lightwalletd-versus-Zaino question entirely for the near term.
- **Deployable by the people who already run the infrastructure.** The roughly five to ten existing operators add the shim; users and wallets do not change their endpoint URL.

In effect the shim realizes a scoped "decouple broadcast from query": the crossing broadcast is split off from the operator entirely and sent to a different counterparty, the hub, while queries still go to the operator's own backend, now blinded to the wallet's IP. [Trust and honest limits](./trust.md) covers why the enclave makes operator-blindness real and checkable rather than merely promised.

### PoC status

A working proof of concept exists at `zeronym/shim` (commit `56394a1a54`): a transparent h2c gRPC reverse proxy that fronts an operator's existing indexer, forwards every method, stream and trailer verbatim, and decodes exactly one path (`SendTransaction`) to classify it with the real vendored `zebra-chain` parser and **log** the verdict. It is **non-destructive**: it still forwards migrations, it does not divert. Not built: diversion, the hub, Nym, STEVE, TLS/ACME, the enclave, attestation, the reproducible build. Everything in this chapter is the production **design** unless marked **(built)**.

**What it proves, tested rather than asserted** (59 passing tests, plus one ignored by design, the fixture regenerator; the indexer behind them is a hand-rolled h2c mock and a real tonic `CompactTxStreamer` server, never yet a live lightwalletd or Zaino, which for these properties is the stronger evidence: the mock records the exact bytes it received and can stall a stream on command): unknown method paths pass through carrying the *backend's* own status, so the proxy is path-agnostic and forward-compatible with methods it has never heard of; a `SendTransaction` reaches the indexer byte-for-byte with `te`, `grpc-timeout` and custom metadata intact and only the origin retargeted; gRPC trailers survive in both response shapes (a real trailers frame, and trailers-only responses where `grpc-status` rides in the headers); the streaming tests fail by *timeout* if a body is ever buffered, in both directions; an unreachable indexer answers `grpc-status 14 UNAVAILABLE` rather than dropping the connection; and the shim redials after the backing indexer restarts.

The PoC is fully transparent only *because* it is non-destructive. The production shim is transparent to the wallet by design and deliberately **not** transparent to the operator: a diverted migration never reaches their indexer, and that asymmetry is the documented residual (the operator learns *that* a client migrated, see [honest limits](./trust.md)).

### Topology and process model

```
                         AWS Nitro enclave (attested, diskless)
  wallet --TLS/h2-->   +---------------------------------------------------+
  https://<puburl>:443 |  zero-indexer-shim                                 |
                       |   1. TLS terminate (enclave-born key + cert)       |
                       |   2. route by HTTP/2 :path                         |
                       |        SendTransaction -> classify                 |
                       |          migration -> encrypt -> hub channel ------+--.
                       |          non-migration --------------------.       |  |
                       |        any other method / stream ----------|       |  |
                       |                                            vv       |  | Nym
                       |                              proxy to backing lwd   |  | tunnel
                       +---------------------|-------------------------------+--|--------+
                                             | internal cleartext gRPC          |
                                             v                                  v
                                   operator's existing lwd            local nym-proxy-client
                                   (unmodified)                               -> hub (ZIH)
```

- **Enclave contents (the TCB):** the ZIS binary + rustls. The **Nym client is untrusted** (payload is already encrypted to the hub key), so `nym-proxy-client` runs as a sidecar (in-enclave on managed Caution since we do not control the parent; parent-side on BYOC).
- **The backing lwd is untrusted for migrations** (it never sees them) and trusted for everything else (it already serves those today). From its perspective the ZIS is a single gRPC client.
- **Supervisor:** a small PID-1 script starts the Nym client then the ZIS, ties their lifecycles, mirrors `deploy/caution-zaino/combined/run-both.sh`.

### Request pipeline (the core)

**Decision: an HTTP/2 reverse proxy, not a full `tonic` server.** After TLS termination the ZIS routes by the `:path` pseudo-header:

- **Every path except `SendTransaction`** (all queries, all streams, unknown/new methods, other services): **proxy verbatim** to the backing lwd. Forward request headers + streaming body, stream the response body and **trailers** (`grpc-status`) back. No decode. This is a generic h2 reverse proxy and is base-agnostic (works for Zaino or lightwalletd; unknown methods pass through).
- **`/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction`** (unary): buffer the one request message (small), strip the 5-byte gRPC length prefix, `prost`-decode `RawTransaction { data }`, and classify:
  - **pass-through** (no value left the Orchard pool) -> proxy to the backing lwd exactly like the fallback, return the backing lwd's real `SendResponse` (so the operator's node actually relays it and the client gets the true result).
  - **Orchard exit** (or a fail-safe verdict) -> encrypt to the hub key, hand to the hub channel, and **synthesize** a gRPC response: `SendResponse { errorCode: 0 }`, framed with the 5-byte prefix + `grpc-status: 0` trailer, so the client sees "accepted."

Rationale over a `tonic` server re-exporting all ~20 methods: this touches exactly one message type (smallest auditable TEE surface), and hyper handles h2 framing / flow control / trailers for the pass-through. "Proxy to backing lwd" is a shared helper used by both the fallback and the non-migration `SendTransaction` case.

**Decision: route on PATH ALONE, and keep the interception set a SUPERSET of every routing predicate any supported backend uses (built).** The tonic server Zaino is built from dispatches on `req.uri().path()` with no HTTP-method guard, so a `GET` to the `SendTransaction` path reaches its `send_transaction` handler. A routing predicate *narrower* than the backend's fails **open**: the backend acts on a request the classifier never saw, which is the false-negative direction, a migration broadcast in the clear. The PoC had exactly that bug (interception gated on `method == POST`, caught by adversarial review). It now routes through a pure `route_for(path)` that structurally cannot see the HTTP method, plus a near-miss arm that hands any path whose final segment case-insensitively spells `sendtransaction` to the classifier with a distinct warning. The two mistakes are not symmetric: classifying a request the backend would have rejected costs a log line; failing to classify one it accepts is the leak. Being stricter than the thing behind you is not conservative.

**Production requirement: classify before you connect.** A wallet whose migration is about to be diverted must not cause the operator's indexer to see a request, or even a connection. The PoC surfaced this and has not solved it: it originally dialled the backing indexer on TCP accept, and now dials on the first request instead, but still before classification (`handle()` obtains the upstream before it routes). In a diverting shim that hands the operator a connection-level trace of a wallet that was never going to talk to them. Production order is buffer, classify, then connect only if the verdict is pass-through.

**Decision (parse-fail / uncertain classification): fail safe for privacy.** If a `SendTransaction` body cannot be parsed or classified, treat it as a **migration** (route to the hub) rather than forward it in the clear. A false positive only delays a normal tx (the hub still broadcasts it); a false negative would leak a real migration. The hub validates and broadcasts, so an unparseable/invalid tx is caught there. (Bounded exception if this proves to break a common well-formed shape; its soundness is a [review](./review.md) item.)

**Compression must be controlled, not relayed.** gRPC compression is not supported on the intercept path: a compressed `SendTransaction` cannot be parsed, so it fails safe to migration. That alone is not enough, because compression is *negotiated*. The backing indexer advertises `grpc-accept-encoding` on responses, so an operator who switches compression on in their own indexer makes wallets compress every `SendTransaction` and pushes every migration into the fail-safe arm instead of genuine classification: an operator-controlled lever on the classifier, in a component whose threat model is that the operator is the adversary. The shim therefore rewrites the indexer's advertised `grpc-accept-encoding` to `identity` on every response it relays, in one place, and leaves the request direction alone, where that header is the wallet's own statement **(built)**. Response compression (`grpc-encoding`) is relayed untouched, and the pass-through path stays opaque, so client compression there is fine. A wallet that compresses unprompted still lands in the fail-safe; whether that stays the shipped policy is a [review](./review.md) item.

### The classifier: detecting an Orchard-touching transaction

**Decision (Zooko, second ruling): the predicate is the mere PRESENCE of Orchard actions, and the value balance decides nothing (built).**

```
is_orchard_touching(tx) := tx.orchard_shielded_data().is_some()   # ANY Orchard actions
```

Zooko's words: any transaction carrying Orchard actions is *potentially* security-sensitive, because it could leak something the user did not intend to disclose, and *probably* time-insensitive, because people and their tooling already expect Orchard to be slow. So the safe default is to divert it whatever `orchard_value_balance` says.

The gap this closes over the previous rule is the internal shuffle that pays its fee from another pool: Orchard actions present, legacy notes spent and their nullifiers published, and yet a value balance of exactly zero. The earlier predicate handed precisely that to the operator's indexer in the clear. Note the measured cost of the widening, which is nil: across 144 mainnet blocks at tip 3,433,105, **all 111 Orchard-touching transactions already had `orchard_value_balance > 0`**, so the wider rule diverted not one extra transaction. It is prospective cover, not new load.

The balance is still parsed and still logged, as *evidence* beside the deciding fact (the action count). Orchard only, never Ironwood, and that boundary is load-bearing rather than incidental: the time-insensitivity half of the rationale holds for Orchard, the closing legacy pool, and fails for Ironwood, the new pool where ordinary time-sensitive commerce lives.

The prior ruling, superseded, was the sign of the value balance alone:

```
is_orchard_exit(tx) := orchard_value_balance(tx) > 0    # value LEAVING the Orchard pool
```

Its reasoning still explains why Orchard is the pool in question: **any transaction touching Orchard is a privacy risk to the user, regardless of the destination pool.** NU6.3 closes Orchard to new *value* (a transaction-level rule forbids value entering, so the chain predicate is Orchard pool value non-increasing and `orchard_value_balance >= 0` post-activation). Anyone still holding Orchard notes has therefore held them since before activation, which makes **spending Orchard at all** the identifying event: it reveals that this IP controls legacy Orchard funds, against a finite and shrinking set. Where the value lands afterwards changes nothing about that inference. Keep the precision: Orchard is closed to new value, **not to activity**, because same-receiver change still lands in the pool and the note-commitment tree keeps growing.

This replaces a three-conjunct predicate (`tx.version == V6 && orchard_value_balance > 0 && ironwood_value_balance < 0`). Both dropped conjuncts were real gaps, not redundancy:

- **The Ironwood conjunct** handed an Orchard withdrawal to transparent or to Sapling straight to the operator's indexer in the clear, though it leaks exactly the same fact. Demonstrated, not theorized: the PoC demo used to print `passthrough ... orchard_vb=+250000 ironwood_vb=+0` for precisely that shape, and the same fixture now lands in the diverted class.
- **The V6 conjunct** passed V5 Orchard spends, which are equally real Orchard exits. Nothing replaces it: `orchard_value_balance()` reads the Orchard bundle, is version-agnostic, and returns zero when there is no bundle (`transaction.rs:1503`), so a V1-V4 transparent transaction reads `0` and passes by the predicate itself.

Three cases, exhaustively:

| `orchard_value_balance` | Verdict | Why |
|---|---|---|
| `> 0` | divert (batch) | Legacy value left a closed pool: the identifying event |
| `== 0` | pass through | No Orchard value left the pool, which is the ruling's criterion (see the caveat below the table) |
| `< 0` | pass through | Value entering Orchard, consensus-invalid post-NU6.3; kept only as a directionality probe in tests |

**One caveat on the `== 0` row, open rather than closed.** A transaction can spend legacy Orchard notes and still net to zero (fee from transparent or Sapling, change back to the same receiver), and spending them publishes those notes' nullifiers. The ruling's rationale is that *spending Orchard at all* is the identifying event, so that shape is an identifying event the ruling's criterion (`> 0`, value **leaving**) does not catch. The predicate follows the criterion as written; whether to widen it to "an Orchard bundle with at least one spend" is Zooko's call, tracked as open question 4 in `zeronym/shim/README.md` and in front of him and Taylor. Widening is not free: it would sweep in ordinary same-receiver-change activity, which is why the old *gross* alternative was rejected on its own terms.

That **dissolves** the old net-versus-gross boundary question rather than answering it; [review](./review.md) keeps the retracted analysis visible, because the history is what the rest of that checklist is read against. Each case has its own vector (including an Orchard exit with no Ironwood bundle at all, and a V5 Orchard spend), so a regression back toward the narrower predicate fails the suite rather than passing quietly.

**A naming note, rather than papering over it.** The diverted class is still `Class::Migration` in the code, the routing helper is still `treat_as_migration()`, and the wire message is still `SubmitMigration`, but an Orchard-to-transparent deshield is not literally a migration. Post-NU6.3 every Orchard exit is legacy-fund movement, so the behaviour is right and only the name is imprecise. This book says **Orchard exit** for the concept and treats *migration* as the legacy label for the same class.

**Decision: parse with `zebra-chain`** (`orchard_value_balance()` `transaction.rs:1503`, `ironwood_value_balance()` `:1520`, `expiry_height()` `:510`), not a hand-rolled parser. A misclassification is a privacy failure, so correctness outweighs the extra dependency weight; a hand-rolled parser would have to walk most of the tx anyway to reach the bundle value balances. Fixture: `zaino/live-tests/e2e/tests/ironwood_activation.rs`.

The classifier is a pure function `fn classify(raw: &[u8]) -> Class` (Class = Migration | PassThrough | Unparseable) with unit tests over real vectors. No I/O, no state, no clock, no config, so it is the easy part to audit. Built as `classify.rs`, which returns the evidence a verdict rests on (version, the Orchard, Ironwood and Sapling value balances, expiry, lengths) alongside the verdict itself, so a log line can never disagree with the routing decision. The Ironwood balance is now **evidence only**: it gates nothing, and it is logged because it is what shows an operator where an Orchard exit went, which is how you see the classifier catching the destinations it used to miss.

**Full consumption is part of the parse (built).** `Transaction::zcash_deserialize` stops at the end of the transaction and ignores trailing bytes, so a valid tx followed by junk parses `Ok`. Without a cursor-position check the shim would classify a *prefix* of what the backing node acts on, meaning the shim and the node could disagree about what the transaction even is. Trailing bytes are therefore `Unparseable`, which fails safe toward migration.

**The fail-safe taxonomy (built).** Every body the shim cannot confidently classify fails safe *toward* migration, never toward pass-through, and that rule is written exactly once (`Class::treat_as_migration`). Cases now covered by tests: unparseable protobuf, a truncated or over-long gRPC frame, trailing bytes after the unary message, an empty transaction, the gRPC compression flag set, a `grpc-encoding` that is not `identity` (`identity` itself is correctly not treated as compression), and a declared message length that would overflow the frame bounds. A body the shim could neither read nor reproduce (over the 4 MiB buffer cap, or a client stream that broke mid-upload) is refused outright rather than forwarded unclassified.

### Language and crates

**Decision: Rust.** Matches the ecosystem, reuses `zebra-chain` for the classifier and `rustls` for TLS, and produces a static-musl binary for a small reproducible enclave image. Key crates: `hyper` + `hyper-util` (HTTP/2 server + client), `tower`, optionally `axum` (its router) for path routing; `rustls` + `tokio-rustls` for TLS; `prost` + the generated `RawTransaction` / `SendResponse` types (depend on `zaino-proto`, or a tiny local `build.rs` over `service.proto`) for the one decoded method; `zebra-chain` for tx parsing / value balances; `aws-nitro-enclaves-nsm-api` for attestation; an ACME client (`instant-acme` or `rustls-acme`) for the cert; the Nym client is a separate process, not a linked crate.

### The hub channel (ZIS -> ZIH)

The encrypted migration travels ZIS -> local `nym-proxy-client` -> Nym mixnet -> `nym-proxy-server` -> ZIH. To the ZIS this is a local TCP endpoint the Nym client exposes.

- **Attested, encrypted channel.** The ZIS verifies the hub's attestation and establishes a shared key (STEVE handshake), then sends each migration as an encrypted message. The payload is end-to-end encrypted to the hub enclave, so the Nym client, the mixnet, and the parent host see only ciphertext. **Decision: the migration is encrypted to the hub key regardless of channel**, so a compromised Nym path yields nothing.
- **Message:** a small framed record `SubmitMigration { ciphertext, txid, expiry_height }` (txid + expiry in the clear to the hub for dedup and flush scheduling; the tx body encrypted). The hub replies `Ack { txid }`.
- **Delivery guarantees:** the ZIS holds the migration until it has an Ack from some hub, retrying and failing over across the >=2 hubs using the migration's expiry slack; last-resort direct broadcast before expiry.

### TLS and certificate model

**Decision: ACME-issued cert for the public domain, key born and held in the enclave.** Wallets do standard TLS against `zec.rocks:443`, so the ZIS must present a **valid CA-issued cert** (drop-in, no wallet change). The key must be enclave-born (else the operator holds it and can MITM). So the enclave generates the key and runs an **ACME client** (Let's Encrypt) to get the cert, completing the TLS-ALPN-01 or HTTP-01 challenge itself (it controls the endpoint). The key persists via the keymaker quorum; the cert renews via ACME. All Let's Encrypt certs are **CT-logged**, which is exactly what the Auditor Role's Certificate Transparency check relies on. This is what makes the drop-in and the Auditor Role coexist: a normal wallet gets a normal valid cert; an auditor additionally checks (a) the cert's key is attested as enclave-born and (b) CT shows no other valid cert for the domain (no operator shadow cert). See [trust](./trust.md).

### What the operator does

1. Deploy the ZIS enclave, point the public DNS/URL (`zec.rocks:443`) at it.
2. Configure the ZIS with the **backing lwd's internal address** (e.g. `10.0.0.5:9067`) and the **hub's Nym address**. The backing lwd is unchanged and stays on its internal address.

### Configuration

```
public_domain      = "zec.rocks"            # for ACME + the served endpoint
backing_lwd         = "10.0.0.5:9067"        # operator's existing lwd (internal)
hubs               = ["<nym-addr-1>", "<nym-addr-2>"]   # >=2 for failover
network            = "testnet" | "mainnet"   # selects Ironwood branch id / activation
flush_safety_blocks = 5                       # broadcast direct if expiry within N blocks
acme               = { provider, contact, challenge = "tls-alpn-01" }
```

Config is loaded at boot; no secrets are on disk (keys are quorum- or enclave-held).

### Failure modes and correctness

- **Backing lwd down:** pass-through requests fail as they would today; the ZIS is transparent, so this is the operator's existing failure mode, not a new one.
- **Hub unreachable:** hold + retry + fail over across hubs; last-resort direct broadcast before expiry. The ZIS keeps a migration until it sees an Ack, and ideally until it observes the tx on-chain, re-submitting if a hub crash lost it.
- **Restart durability:** the enclave is diskless, so in-flight migrations in RAM are lost on a ZIS restart. Mitigation: keep the ACK-pending set small (short retry loop), and rely on the wallet's own resend if needed; the hub dedups by txid.
- **Invalid migration -> false success:** the ZIS returns success before the hub broadcasts, so an invalid tx fails silently at flush. Do stateless sanity at the ZIS (parseable, not already expired); full validity is the hub's broadcast result (surfacing it to the client is out of near-term scope).
- **The operator learns *that* a client migrated** (see [honest limits](./trust.md)): inherent; do not attempt shim-side mitigation.

### Crate layout

```
zeronym/shim/
  Cargo.toml
  build.rs                  # prost for RawTransaction/SendResponse (or dep zaino-proto)
  src/
    main.rs                 # config load, boot sequence, serve
    tls.rs                  # in-enclave keygen, ACME, keymaker-quorum persistence
    attest.rs               # NSM attestation binding; /attestation endpoint
    proxy.rs                # h2 server, :path routing, reverse-proxy to backing lwd
    intercept.rs            # SendTransaction decode + gRPC frame + response synth
    classify.rs             # is_orchard_touching over zebra-chain (pure, unit-tested)
    hub.rs                  # STEVE session, encrypt, SubmitMigration, retry/failover
    config.rs
  tests/
    classify_vectors.rs     # Orchard-exit / into-Orchard / transparent fixtures
    proxy_passthrough.rs    # a mock backing lwd; assert non-migration passes through
```

The PoC ships `lib.rs`, `main.rs`, `proxy.rs`, `intercept.rs`, `classify.rs`, `config.rs` and the test files; `tls.rs`, `attest.rs` and `hub.rs` are unwritten. Diversion plugs into `intercept::send_transaction` as one branch on the fail-safe-folded verdict, immediately after the log, plus moving the upstream dial out of `handle()` so the connect happens after the verdict.

## The zero-indexer-hub (ZIH)

The ZIH (earlier called `zero-broadcaster`) is an attested-TEE service that receives encrypted **migration** transactions from many shims over Nym, batches them, and publishes them to the Zcash network together on a strict block cadence, so no party can link a migration to the source IP that submitted it. It is run as **>=2 instances with failover**.

### Topology and process model

```
   shim A ---.                          AWS Nitro enclave (attested, diskless)
   shim B ----\  Nym mixnet   +-------------------------------------------------+
   shim C -----+------------->| nym-proxy-server -> zero-indexer-hub            |
              /   (encrypted  |    decrypt (in enclave) -> validate -> queue    |
   shim N ---'    migrations) |    flush every <N blocks -> publish batch  -----+---.
                              |    hub key from keymaker M-of-N quorum          |   |
                              +---------------------|---------------------------+   | clearnet
                                                    | clearnet (tip + broadcast)    |
                                                    v                               v
                                       full node(s) (zebrad/zcashd)         Zcash P2P network
                                       getblockchaininfo + sendrawtransaction
```

- **Enclave contents (TCB):** the hub binary + rustls. It is **lightweight**, like the shim: it does NOT run a validator in-enclave (that is the 400-500 GB problem). It connects OUT to an existing full node for chain tip and for broadcasting.
- **Sidecars / egress:** `nym-proxy-server` fronts the inbound side (from shims); clearnet egress to full node(s) for tip + broadcast; egress to the keymaker quorum and (if needed) Nyx-RPC for Nym ecash. The Nym server can be parent-side (untrusted; the payload is already encrypted to the hub key).
- **Supervisor:** small PID-1 script, mirrors the shim's.

### Language and crates

**Decision: Rust**, static-musl, reproducible (StageX), same reasons as the shim. Key crates: `hyper`/`tower` or a minimal framed-TCP server for the inbound `SubmitMigration` channel; `rustls`; `prost` for the tx bytes; `zebra-chain` to re-parse and re-classify migrations (defense in depth); a JSON-RPC client (`jsonrpsee`) for the node's `sendrawtransaction` / `getblockchaininfo`; `aws-nitro-enclaves-nsm-api` for attestation; the keymaker/quorum client.

### Inbound: receiving migrations

The hub is the server end of the shim's channel. Over the Nym tunnel it accepts `SubmitMigration { ciphertext, txid, expiry_height }` and replies `Ack { txid }`.

- **Channel + auth (STEVE).** The shim verifies the hub's attestation and derives a shared key (STEVE); migrations are encrypted to the hub. Whether the hub also authenticates the shim (mutual STEVE) is an open decision (see [review](./review.md)): one-way is enough for privacy, mutual would gate abuse.
- **Decrypt in-enclave.** Only the attested hub software sees cleartext; the hub host operator (Caution) and the Nym path see ciphertext.
- **Re-parse, but as telemetry only, never as a drop reason.** The hub parses with `zebra-chain` and re-runs `is_orchard_touching` so a disagreement with the shim is *visible*, and it stops there. An earlier version of this design said "reject anything that is not a valid-looking, unexpired Orchard exit, to keep garbage out of the batch." That is backwards, and the adversarial review of the batching design, recorded in `zeronym/hub/REVIEW.md`, is where it was caught. The shim fail-safes for privacy: a body it cannot read cleanly routes to the migration arm precisely *because* it could not read it. So the transactions most likely to fail the hub's parse are exactly the ones the shim deliberately diverted, and a hub that rejects them converts the shim's fail-safe into a leak, handing an adversary who can characterise the parser skew an on-demand way to force a transaction back onto the direct-broadcast path. An unparseable payload is therefore queued and published; `sendrawtransaction` at the node is the only authority on validity, and the cost of being wrong is one wasted batch slot. The permitted refusals are narrow and structural: authentication failure, a malformed frame, byte-budget exhaustion, and the expiry admission rule. **Rate-limit** per channel to bound resource use.

### The batch queue

In-RAM (diskless enclave), keyed by **txid** for dedup. Each entry: `{ txid, expiry_height, tx_bytes, received_at }`. The hub tracks the current chain height `H` from its node connection to schedule flushes and check expiries. Duplicate submissions (same txid) collapse; this is also what makes cross-hub failover safe.

### Flush and publish (the core)

- **Flush trigger:** at every height that is a multiple of **N (Decision: N = 20, about 25 minutes)**, and at no other time. The bound is a budget rather than a round number: `N + mining_margin + delivery_lag <= min_wallet_expiry`, or `20 + 4 + 6 = 30` against a 40-block expiry, asserted at startup so a later change to N fails loudly instead of quietly pushing traffic onto the direct-broadcast path. An earlier draft set N to about 10 against "Brave's 20-block expiry", but 20 is only Brave's default and the lowest in the ecosystem (librustzcash 40, Zingo 100), so building to it let the least generous wallet cap everyone. Brave is out of scope for v1 and the ask to them is 40. Doubling the window doubles the expected batch at no cost to any wallet, which is the cheapest improvement available to the batch-size problem in [honest limits](./trust.md).
- **No early flush, and this replaces an earlier design.** The trigger used to fire early if any queued migration's `expiry_height` came within a safety margin. That is an attacker-operated flush clock: the hub's re-validation is stateless, so one well-formed but consensus-invalid Orchard-touching transaction per block, at no cost, collapses every batching window network-wide and permanently. The urgency is instead made unreachable by **admission control**: accept a migration only if it provably survives the next scheduled flush (`expiry >= next_flush_height(H) + mining_margin`), and refuse it otherwise so the shim holds and retries rather than broadcasting. If nothing urgent can be admitted, nothing can ever be urgent.
- Batches are triggered by **time / block-height, never by transaction count**: a count-based flush (say every 100 txs) would let an attacker submit 99 of its own migrations the instant it sees a target submit, isolating the target's transaction in the revealed batch. Batch granularity must also line up with how wallets choose anchors and expiries (see [the problem](./problem.md)).
- **Publish "simultaneously."** On flush: take all pending migrations, **shuffle the order** (never leak arrival order), and submit them to the node(s) as close to simultaneously as possible (parallel `sendrawtransaction`), so they enter the mempool together and land in the same block window. An on-chain / mempool observer then sees N migrations appear together, unordered, from many shims. **Decision: randomize order + parallel submit**; do not drip them out.
- **Confirmation tracking.** Move flushed migrations to an "awaiting confirmation" set; watch the chain until each is mined; **re-submit** if a tx is not seen within a few blocks (node dropped it, or a hub crash lost it). Drop from the set once confirmed or expired.
- **The anonymity set is the batch itself** (cross-operator), so batch size is the key metric; the hub logs achieved batch size honestly (see [honest limits](./trust.md)). Hub-generated decoys are a costly last resort, not the primary lever.

### Chain connection (tip + broadcast)

**Decision: connect OUT to existing full node(s) (zebrad/zcashd) over clearnet**, do not run a validator in-enclave.

- **Tip:** poll / subscribe `getblockchaininfo` (or `getbestblockhash` + height) to keep `H` current for flush cadence and expiry checks.
- **Broadcast:** `sendrawtransaction` for each tx in the flush. Connect to **>=2 nodes** for robustness; optionally speak Zcash **P2P `tx`** to many peers directly (Nate's point: P2P relay reaches a far larger node set than one lwd endpoint, a bigger anonymity set for the broadcast source). The hub's node IP is not user-linked, so clearnet is acceptable near-term; broadcasting over Nym to hide the hub's own IP is an optional enhancement.
- The node connection is a hard dependency (no tip -> cannot schedule; no node -> cannot broadcast), so >=2 nodes, and node-down is part of the hub's failure handling.

### Key management (hub key, STEVE, keymaker quorum)

- **The hub key** (that shims encrypt migrations to) is generated in-enclave and **persisted via the keymaker/locksmith M-of-N quorum across 3-4 orgs** (Caution / Nym / Shielded Labs / ZF), reconstituted across cold boots and upgrades (better than KMS-seal-to-PCR, which breaks on upgrade). The consortium governs it.
- **Decision: a single shared hub key across all hub instances**, provisioned to each attested hub by the quorum. This is what makes failover clean: a shim encrypts to "the hub key" and any hub instance can decrypt, dedup, and publish. (Per-hub keys would force the shim to re-encrypt on failover and would strand a migration if its hub died mid-flight.)
- **STEVE** is the handshake by which a shim verifies a hub's attestation (which binds the hub's public key) and derives a session key. The hub's role: present its attestation and complete the handshake. The exact STEVE wire form over Nym, and whether it is mutual, are cross-party items in [review](./review.md).

### Failover and multiple hubs

- **Run >=2 hubs, shared key.** A shim prefers a **primary** hub (so batches converge there and stay dense) and **fails over** to a standby only when the primary is unreachable. Standbys are hot but mostly idle until failover, so **batch density is preserved** (one active hub) while liveness is covered.
- **Dedup by txid** within each hub. If failover causes a migration to reach two hubs, both may publish; the second on-chain submission is a **harmless already-known duplicate**. No cross-hub state sync is needed near-term (Decision: accept harmless duplicates over the complexity of a shared published-set).
- The consortium's multiple orgs are the natural operators of the standby hubs, which also starts decentralization.

### Configuration

```
listen_nym          = <hub Nym address / proxy-server config>
nodes               = ["node-a:8232", "node-b:8232"]   # tip + broadcast, >=2
network             = "testnet" | "mainnet"
flush_interval_blocks = 20                # N + mining_margin + delivery_lag <= min wallet expiry (40)
safety_margin_blocks  = 5                 # flush early if expiry within; covers mining time
role                = "primary" | "standby"
keymaker_quorum     = <quorum endpoints / policy>
peer_hubs           = [...]               # awareness only, no shared state near-term
```

### Failure modes and correctness

- **Node(s) down:** cannot get tip or broadcast; with >=2 nodes this is rare, but if all are down the hub cannot flush. Since the shim retains + re-submits and fails over, and migrations carry expiry slack, brief node outages self-heal; a sustained one is what the shim's last-resort direct broadcast covers.
- **Hub crash:** the in-RAM queue and awaiting-confirmation set are lost (diskless). The shim retains each migration until it observes on-chain confirmation and re-submits, and fails over to a standby, so a hub crash does not lose migrations. (This makes the shim-side retain-until-confirmed a hard requirement.)
- **Expiry pressure:** flush early per `safety_margin`; never let a queued migration expire in the buffer.
- **Garbage / abuse:** re-validate + rate-limit; optionally require shim attestation (see [review](./review.md)).
- **Fee too low to mine before expiry:** the fee is in the wallet-signed tx and the hub cannot change it; `safety_margin` gives mining headroom, but a badly-underpaid migration can still fail. That is the wallet's responsibility, not the hub's.

### Crate layout

```
zeronym/hub/
  Cargo.toml
  src/
    main.rs          # config, boot sequence, run the flush loop
    inbound.rs       # STEVE server, SubmitMigration decode, decrypt
    validate.rs      # re-parse + re-classify (zebra-chain) + expiry/well-formed checks
    queue.rs         # in-RAM dedup queue keyed by txid; expiry tracking
    flush.rs         # cadence + expiry trigger, shuffle, parallel publish, confirm-track
    chain.rs         # node connection: tip + sendrawtransaction (+ optional P2P)
    keys.rs          # keymaker quorum reconstitution; STEVE handshake; decrypt
    attest.rs        # NSM attestation binding; /attestation
    config.rs
  tests/
    flush_batch.rs   # queue N migrations, assert a single simultaneous shuffled publish
    expiry.rs        # assert early flush when expiry is within the safety margin
    dedup.rs         # duplicate txids collapse; cross-hub duplicate is harmless
```

## Boot, build, and attestation

Both services share the same enclave idioms: a static-musl Rust binary, a reproducible **StageX** build, running in an AWS Nitro enclave (attested, diskless). Both generate and hold keys **in-enclave** and bind a public key into the **NSM attestation**; neither writes secrets to disk. Reproducibility lets the consortium and third parties confirm the running binary is the reviewed binary.

**Attestation binding.** Bind the relevant **public key** into the attestation (shim: the **TLS public key**, not the Nym address; hub: the hub public key), so an auditor (and, later, RA-TLS-aware clients) can check cert-pubkey == attested key. Three feasible mechanisms from the V2 sync: the STEVE handshake, `metadata.json -> user_data` (build-time, implies a persisted key), or a runtime `arbitrary_data` field Caution would add. The same in-enclave-keygen + NSM path Zaino/V2 use applies to both. The shim's STEVE handshake **is** the shim auditing the hub before trusting it with migrations; independent auditors run the same steps. See [trust](./trust.md) for STEVE, the Auditor Role, PCRs, and Certificate Transparency.

**Shim boot sequence.** (1) Key material: reconstitute the TLS keypair from the keymaker M-of-N quorum if one exists, else generate in-enclave and register with the quorum (persists across cold boots/upgrades; the private key never leaves the enclave). (2) Certificate: ensure a valid CA cert for the public domain via ACME, keyed to the enclave-born key. (3) Attestation: bind the TLS public key into the Nitro attestation; serve `/attestation` (or expose over Nym). (4) Hub session: STEVE-handshake each configured hub; cache the shared keys. (5) Backing lwd: open the upstream h2 connection(s); health-check. (6) Listen: bind `:443`, serve.

**Hub boot sequence.** (1) Hub key from the keymaker quorum (reconstitute; private key stays in-enclave). (2) Attestation: bind the hub public key into the Nitro attestation; publish `/attestation` (or over Nym) for the shim's STEVE check + auditors. (3) Chain: connect to the full node(s); sync `H`; verify `sendrawtransaction` works. (4) Inbound: start `nym-proxy-server`; begin accepting `SubmitMigration` over STEVE. (5) Run the flush loop against `H`.

The open forks (cert model, STEVE wire form over Nym, mutual vs one-way STEVE, `nym-proxy-server` placement, zero-ingress attestation delivery, JSON-RPC front-ends, keymaker walkthrough, publish path, batch-density vs failover, flush cadence) are collected in [review](./review.md).

### Build and test

**Shim.** Unit: `classify.rs` against real vectors (the `ironwood_activation.rs` migrate tx, an Orchard exit with no Ironwood bundle, a V5 Orchard spend, value entering Orchard, and a mainnet transparent tx), the correctness-critical piece. Integration: a mock backing lwd; assert every non-`SendTransaction` method and a pass-through `SendTransaction` reach it unchanged, and an Orchard exit is diverted to a mock hub (never reaches the backing lwd). Built so far: the classifier vectors, the wire-level transparency suite, and the logging assertions that are the only evidence the classifier ran at all in a non-destructive PoC. Still open, and the largest gap in the evidence: the fixtures are generated with zebra's own serializer, so no transaction a wallet actually produced has been classified; capturing the `orchard_note_spends_to_ironwood_across_boundary` tx from `ironwood_activation.rs` closes it and needs a running regtest node. Enclave: reproducible StageX build; boot in a Nitro enclave; verify the `/attestation` doc carries the TLS pubkey; run the Auditor Role steps. End to end: ZIS in front of the live testnet enclave's Zaino, a real wallet syncs (pass-through) and submits a testnet migration that lands in a hub batch.

**Hub.** Unit: `flush.rs` (N queued migrations -> one shuffled, parallel publish), `expiry.rs` (early flush within the safety margin), `dedup.rs` (txid collapse; harmless cross-hub duplicate), `validate.rs` (reject non-migrations / expired). Integration: a mock shim submitting over a local STEVE-ish channel and a mock node capturing `sendrawtransaction`; assert a batch of migrations is published together, shuffled, once per txid. Enclave: reproducible StageX build; boot in a Nitro enclave; verify the `/attestation` doc carries the hub key; a shim completes STEVE and submits a migration. End to end: >=1 shim in front of the live testnet Zaino, a testnet migration flows shim -> Nym -> hub -> batch -> testnet chain; confirm it lands and is unlinkable to the submitting shim.
