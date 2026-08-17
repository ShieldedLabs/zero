# The shim and the hub

The concrete engineering designs for both TEE services. **Decision:** marks a committed choice. [Trust](./trust.md) has the STEVE and attestation deep-dive and the honest limits.

## The zero-indexer-shim (ZIS)

The ZIS is an attested-TEE proxy an operator deploys behind their **existing public URL** (e.g. `zec.rocks:443`). It is a drop-in LWD to every wallet (no reconfiguration or endpoint change; wallets do need aligned anchors and expiry within a migration epoch, see [the problem](./problem.md)). It forwards most traffic to the operator's unmodified backing indexer, but intercepts two methods and routes them to the hub: an **Orchard-touching** `SendTransaction` (a transaction that carries Orchard actions, the class the code and hub protocol still call a *migration*), and every `GetTransaction`, so a wallet's follow-up on a diverted migration also bypasses the operator. The shim-to-hub hop runs over the Nym mixnet in the deployed pair ([roadmap](./roadmap.md) has the status table).

### Why a shim, not the whole indexer in a TEE

An earlier plan put the entire indexer (a full Zebra node plus the indexer) inside the enclave, so the operator could see nothing at all. That is expensive: until the enclave platform ships disk support, it runs entirely in RAM at roughly 400 to 500 GB, on the order of $2,000 per operator per month, with about a four-day resync on every restart. That cost wall makes operator adoption unrealistic.

The shim avoids it by being a thin router, not an indexer:

- **Cheap and fast to restart.** No chain state inside the TEE, so the RAM and cost wall disappears.
- **Base-agnostic.** It fronts whatever the operator already runs, sidestepping lightwalletd versus Zaino.
- **Deployable by the people already running the infrastructure.** The roughly five to ten existing operators add the shim.

### Status

The shim began as a proof of concept (commit `56394a1a54`) that classified `SendTransaction` with the real vendored `zebra-chain` parser and only **logged** the verdict. It is now the shipped component: an Orchard-touching transaction stops at the shim and goes to the hub, `GetTransaction` is answered by the hub rather than the operator, and the shim holds **no per-migration state**, so a restart or a second instance loses nothing. The hub is its own crate (`zeronym/hub/`: queue, batcher, chain connection, its own reproducible build). Both run as attested Nitro enclaves with in-enclave TLS and reproducible StageX builds, and a third party has run the operator runbook (`zeronym/shim/deploy/caution/OPERATORS.md`) end to end. Per-mechanism status, including which builds currently reproduce, is the table in [the roadmap](./roadmap.md).

**What it proves, tested rather than asserted** (tested against a hand-rolled h2c mock and a real tonic `CompactTxStreamer` server rather than a live indexer, which for these properties is stronger evidence: the mock records the exact bytes it received and can stall a stream on command): unknown method paths pass through carrying the *backend's* own status, so the proxy is path-agnostic and forward-compatible with methods it has never heard of; a `SendTransaction` reaches the indexer byte-for-byte with `te`, `grpc-timeout` and custom metadata intact and only the origin retargeted; gRPC trailers survive in both response shapes (a real trailers frame, and trailers-only responses where `grpc-status` rides in the headers); the streaming tests fail by *timeout* if a body is ever buffered, in both directions; an unreachable indexer answers `grpc-status 14 UNAVAILABLE` rather than dropping the connection; and the shim redials after the backing indexer restarts.

The shipped shim is transparent to the wallet by design and deliberately **not** transparent to the operator: a diverted migration and its follow-up `GetTransaction` never reach their indexer, and that asymmetry is the documented residual (the operator still learns *that* a client migrated, see [honest limits](./trust.md)).

### Topology and process model

The shim terminates the wallet's TLS on an enclave-born key, routes by HTTP/2 `:path`, and sends an Orchard-touching `SendTransaction` and every `GetTransaction` to the hub while everything else proxies to the operator's unmodified indexer. [The architecture](./architecture.md) has the data-flow diagram.

- **Enclave contents (the TCB):** the ZIS binary + rustls, plus `nym-sdk` in the mixnet build, which is linked in-process rather than run as a sidecar and is therefore inside the TCB ([trust](./trust.md) states what that costs).
- **The backing lwd is untrusted for migrations** (it never sees them) and for `GetTransaction` (also hub-served), and trusted for the rest, which it already serves today. From its perspective the ZIS is a single gRPC client.
- **Supervisor:** in-binary. The driver, correlator and client-lifecycle supervisor are spawned as tasks by the ZIS itself, so there is no PID-1 wrapper script and no second process to keep alive.

### Request pipeline (the core)

**Decision: an HTTP/2 reverse proxy, not a full `tonic` server.** After TLS termination the ZIS routes by the `:path` pseudo-header:

- **Every path except `SendTransaction`, `GetTransaction`, and two control-plane paths** (all other queries, all streams, unknown/new methods, other services): **proxy verbatim** to the backing lwd. The exceptions the shim answers itself are `/.well-known/caution/health`, served locally, and `/attestation`, relayed to the platform's `bootproofd`. It owns these because on managed Caution under h2c the platform routes them to the app, so a shim that forwarded them would send the attestation health check to the Zcash indexer and fail to boot. The relay is behind `ZIS_CAUTION_ATTESTATION` (default on) with the dialled address in `ZIS_CAUTION_BOOTPROOFD_ADDR`; turn it off for BYOC, for non-h2c, or once Caution serves the paths itself, and the shim is a pure proxy again. Forward request headers and streaming body, stream the response body and **trailers** (`grpc-status`) back. No decode.
- **`/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction`** (unary): buffer the one request message (small), strip the 5-byte gRPC length prefix, `prost`-decode `RawTransaction { data }`, and classify:
  - **pass-through** (no value left the Orchard pool) -> proxy to the backing lwd exactly like the fallback, return the backing lwd's real `SendResponse` (so the operator's node actually relays it and the client gets the true result).
  - **Orchard-touching** (or a fail-safe verdict) -> hand to the hub, and **synthesize** a gRPC response: `SendResponse { errorCode: 0 }`, framed with the 5-byte prefix + `grpc-status: 0` trailer, so the client sees "accepted."
- **`/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTransaction`** (unary): decode the `TxFilter`, and when a hub is configured, answer from the **hub**, never the operator. The hub checks its batch queue first (a diverted-but-unflushed migration comes back with height 0, marking it still in the mempool) and otherwise asks the hub's own indexer. Address-level queries (`GetTaddressTxids`, `GetTaddressBalance`, `GetAddressUtxos`) are **not** intercepted and still reach the operator, an open gap (see [honest limits](./trust.md)).

Rationale over a `tonic` server re-exporting all ~20 methods: this decodes only two message types (`RawTransaction` for `SendTransaction`, `TxFilter` for `GetTransaction`), still the smallest auditable TEE surface, and hyper handles h2 framing / flow control / trailers for the pass-through. "Proxy to backing lwd" is a shared helper used by both the fallback and the non-Orchard `SendTransaction` case.

**Decision: route on PATH ALONE, and keep the interception set a SUPERSET of every routing predicate any supported backend uses (built).** The tonic server Zaino is built from dispatches on `req.uri().path()` with no HTTP-method guard, so a `GET` to the `SendTransaction` path reaches its `send_transaction` handler. A routing predicate *narrower* than the backend's fails **open**: the backend acts on a request the classifier never saw, which is the false-negative direction, a migration broadcast in the clear. The PoC had exactly that bug (interception gated on `method == POST`, caught by adversarial review). It now routes through a pure `route_for(path)` that structurally cannot see the HTTP method, plus a near-miss arm that hands any path whose final segment case-insensitively spells `sendtransaction` to the classifier with a distinct warning. Being stricter than the thing behind you is not conservative.

**Classify before you connect (built).** A wallet whose migration is about to be diverted must not cause the operator's indexer to see a request, or even a connection. The PoC surfaced this and dialled too early (`handle()` obtained the upstream before it routed), which in a diverting shim would hand the operator a connection-level trace of a wallet that was never going to talk to them. The shipped shim implements the production order: buffer, classify, then connect only if the verdict is pass-through. The upstream pool is lazy and the dial happens after routing, so a diverted migration never opens even a TCP connection to the operator's indexer (`src/proxy.rs` states the property at the dial site).

**Decision (parse-fail / uncertain classification): fail safe for privacy.** If a `SendTransaction` body cannot be parsed or classified, treat it as a **migration** (route to the hub) rather than forward it in the clear. A false positive only delays a normal tx (the hub still broadcasts it); a false negative would leak a real migration. The hub validates and broadcasts, so an unparseable/invalid tx is caught there. (Bounded exception if this proves to break a common well-formed shape; its soundness is an open item.)

**Compression must be controlled, not relayed.** gRPC compression is not supported on the intercept path: a compressed `SendTransaction` cannot be parsed, so it fails safe to migration. But compression is *negotiated*: the backing indexer advertises `grpc-accept-encoding` on responses, so an operator who enables compression pushes every migration into the fail-safe arm instead of genuine classification, an operator-controlled lever on the classifier in a component whose threat model is that the operator is the adversary. The shim therefore rewrites that header to `identity` on every relayed response, in one place, and leaves the request direction alone, where the header is the wallet's own statement **(built)**. Response compression (`grpc-encoding`) is relayed untouched, and the pass-through path stays opaque, so client compression there is fine. A wallet that compresses unprompted still lands in the fail-safe; whether that stays the shipped policy is an open question.

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

Its reasoning still explains why Orchard: NU6.3 closes the pool to new *value*, so anyone still holding Orchard notes has held them since before activation, and **spending Orchard at all** is the identifying event, revealing that this IP controls legacy funds against a finite and shrinking set. Where the value lands changes nothing. Keep the precision: closed to new value, **not to activity**, since same-receiver change still lands in the pool.

This replaces a three-conjunct predicate (`tx.version == V6 && orchard_value_balance > 0 && ironwood_value_balance < 0`). - **The Ironwood conjunct** handed an Orchard withdrawal to transparent or to Sapling straight to the operator's indexer in the clear, though it leaks exactly the same fact. Demonstrated, not theorized: the PoC demo used to print `passthrough ... orchard_vb=+250000 ironwood_vb=+0` for precisely that shape, and the same fixture now lands in the diverted class.
- **The V6 conjunct** passed V5 Orchard spends, which are equally real Orchard activity. Nothing replaces it: `orchard_shielded_data()` is version-agnostic and absent on a V1-V4 transparent transaction (`transaction.rs`), which therefore passes by the predicate itself.

All three value-balance sub-cases carry Orchard actions, so all three divert:

| `orchard_value_balance` | Shape | Verdict |
|---|---|---|
| `> 0` | value leaving Orchard (an exit) | divert |
| `== 0` | same-receiver shuffle, fee from another pool: nullifiers published, nets to zero | divert |
| `< 0` | value entering Orchard, consensus-invalid post-NU6.3 | divert (but unmineable; kept only as a directionality probe in tests) |

Only a transaction with **no Orchard bundle** passes through. Each shape has its own test vector (including an Orchard withdrawal with no Ironwood bundle, and a V5 Orchard spend), so a regression toward the narrower predicate fails the suite rather than passing quietly.

**A naming note.** The diverted class is `Class::Migration` in the code and the routing helper is `treat_as_migration()`, but an Orchard deshield or a same-receiver shuffle is not literally a migration. The code's own accurate name is `is_orchard_touching`, so this book says **Orchard-touching transaction** for the class and keeps *migration* as the legacy label for the same thing.

**Decision: parse with `zebra-chain`** (`orchard_value_balance()` `transaction.rs:1503`, `ironwood_value_balance()` `:1520`, `expiry_height()` `:510`), not a hand-rolled parser. A misclassification is a privacy failure, so correctness outweighs the extra dependency weight; a hand-rolled parser would have to walk most of the tx anyway to reach the bundle value balances. Fixture: `zaino/live-tests/e2e/tests/ironwood_activation.rs`.

The classifier is a pure function `fn classify(raw: &[u8]) -> Class` (Class = Migration | PassThrough | Unparseable) with unit tests over real vectors. No I/O, no state, no clock, no config, so it is the easy part to audit. Built as `classify.rs`, which returns the evidence a verdict rests on (version, the Orchard, Ironwood and Sapling value balances, expiry, lengths) alongside the verdict itself, so a log line can never disagree with the routing decision. The Ironwood balance is now **evidence only**: it gates nothing, and it is logged because it is what shows an operator where an Orchard-touching transaction sent its value, which is how you see the classifier catching the destinations it used to miss.

**Full consumption is part of the parse (built).** `Transaction::zcash_deserialize` stops at the end of the transaction and ignores trailing bytes, so a valid tx followed by junk parses `Ok`. Without a cursor-position check the shim would classify a *prefix* of what the backing node acts on, Trailing bytes are therefore `Unparseable`, which fails safe toward migration.

**The fail-safe taxonomy (built).** Every body the shim cannot confidently classify fails safe *toward* migration, never toward pass-through, and that rule is written exactly once (`Class::treat_as_migration`). Cases now covered by tests: unparseable protobuf, a truncated or over-long gRPC frame, trailing bytes after the unary message, an empty transaction, the gRPC compression flag set, a `grpc-encoding` that is not `identity` (`identity` itself is correctly not treated as compression), and a declared message length that would overflow the frame bounds. A body the shim could neither read nor reproduce (over the 4 MiB buffer cap, or a client stream that broke mid-upload) is refused outright rather than forwarded unclassified.

### Language and crates

**Decision: Rust**, static-musl, reproducible under StageX. It matches the ecosystem, reuses `zebra-chain` for the classifier and `rustls` for TLS, and yields a small enclave image. One dependency is worth naming because it changes the TCB: `nym-sdk` is pinned to a git tag and **linked into the binary** behind the `mixnet-driver` feature, which both deploy Containerfiles enable by default. The rest are of record in `zeronym/shim/Cargo.toml`.

### The hub channel (ZIS -> ZIH)

The mixnet is the primary transport. Each side runs a linked `nym-sdk` client in-process and exchanges fixed-size frames. The clearnet path (an HTTP `POST /` to submit, `POST /transaction` to look up) still exists but is **off unless `ZIH_HTTP_SUBMIT` asks for it**, so a hub is mixnet-only by default. STEVE and the encrypt-to-hub-key layer are still **designed**.

**Decision: submit is dispatch-only.** The shim answers the wallet's `SendTransaction` the moment the frame is dispatched to the mixnet, rather than blocking on the hub's ack. The ack is a full mixnet round trip, ~10 seconds even healthy and minutes under gateway backpressure, and because neither side runs a validator it only ever confirmed that the hub *queued* the frame, never that the transaction was valid or in a mempool. The diverted path already relies on the wallet's own confirmation-via-sync for both, so waiting bought latency rather than safety, and reported a queued transaction as failed at today's throughput. `submit` returns as soon as the dispatch succeeds and fails closed when there is no hub address to send to. The asymmetry is deliberate: a **lookup** still awaits its `LookupReplyV1`, because a `GetTransaction` has no answer without one.

A consequence worth stating: reply SURBs are still attached and still sized for an ack nobody reads, so the hub spends them replying into a dropped receiver. They stay non-zero because a zero count would push the driver off the anonymous-send path, which is the property that matters; trimming them toward the anonymity minimum is a throughput follow-up.

- **The wire frames (built).** `SubmitV1` is magic `ZNS1`, a 16-byte correlation nonce, a length, and the transaction, zero-padded to exactly 64 KiB; `AckV1` is magic `ZNA1`, the echoed nonce, and a disposition plus refusal code, exactly 64 bytes. `LookupV1` and `LookupReplyV1` carry `GetTransaction` the same way. **Decision: fixed-size frames**, so a passive observer learns nothing from the size of a submission, and every reply looks like every other.
- **Decision: no txid and no expiry on the wire.** The hub derives both from the bytes it receives. An earlier design put them in the clear so the hub could dedup and schedule without parsing, but a txid handed across the transport is exactly a correlation handle, which is what the mixnet hop exists to destroy. Requests are correlated by the per-request nonce instead, which means nothing outside the pending-request table.
- **Attested, encrypted channel (designed).** The ZIS would verify the hub's attestation and derive a shared key (STEVE), then encrypt each migration to the hub key regardless of channel, so a compromised path yields nothing. Neither layer exists yet: on both transports the only encryption is what carries the frame.
- **Delivery guarantees.** The shim rotates which hub address each submit targets, so load and a dead address spread across a multi-homed hub over successive sends. That is the whole of failover today: it is not primary-preference, and whether shims should prefer a primary or submit to every hub is open. What stays designed is holding a migration across requests: retrying on expiry slack, and a last-resort direct broadcast before expiry. The shim keeps no such state, so recovery rides on the wallet's own resend.

### TLS and certificate model

**Decision: ACME-issued cert for the public domain, key born and held in the enclave.** Wallets do standard TLS against `zec.rocks:443`, so the ZIS must present a **valid CA-issued cert** (drop-in, no wallet change). The key must be enclave-born (else the operator holds it and can MITM). So the enclave generates the key and runs an **ACME client** (Let's Encrypt) to get the cert, completing the TLS-ALPN-01 or HTTP-01 challenge itself (it controls the endpoint). The key persists via the keymaker quorum; the cert renews via ACME. Let's Encrypt certs are CT-logged, which is what the Auditor Role's shadow-cert check relies on (see [trust](./trust.md)).

On Caution the platform supplies this rather than the shim: `e2e_encryption { mode = "tls" }` runs a Caddy **inside** the enclave that obtains the Let's Encrypt certificate itself, so the private key is enclave-born and the operator never holds it, and `upstream_protocol = "h2c"` carries the gRPC in to the shim. The shim's own rustls and ACME stack stays dormant there, as the vendor-independent path.

### What the operator does

1. Deploy the ZIS enclave, point the public DNS/URL (`zec.rocks:443`) at it.
2. Configure the ZIS with the **backing lwd's internal address** (e.g. `10.0.0.5:9067`) and the **hub's address** (a pinned TLS endpoint today; a Nym address under the Nym design). The backing lwd is unchanged and stays on its internal address.

### Configuration

```
ZIS_LISTEN              # the address the shim serves wallets on
ZIS_BACKEND             # the operator's existing indexer, internal address
ZIS_BACKEND_TLS         # whether that hop is TLS
ZIS_HUB                 # hub address for the clearnet transport (repeatable)
ZIS_HUB_TLS             # the hub's expected TLS name
ZIS_HUB_NYM             # hub Nym address(es) for the mixnet transport
ZIS_NYM_GATEWAY         # entry gateway(s) to pin, repeatable; rotates on rebuild
ZIS_LOOKUP_TIMEOUT_SECS # ceiling on a GetTransaction round trip
ZIS_DIAG                # diagnostic logging
ZIS_NYM_ROTATION_SECS   # sender-tag rotation interval
ZIS_NYM_TOPOLOGY        # localnet only: the harness-written topology
ZIS_CAUTION_ATTESTATION # own Caution's control-plane paths (default on)
ZIS_CAUTION_BOOTPROOFD_ADDR  # where the /attestation relay dials
ZIS_TLS_DOMAIN          # ACME: the served domain
ZIS_TLS_EMAIL           # ACME: contact
ZIS_TLS_PRODUCTION      # ACME: staging or production directory
```

No secrets are on disk: keys are quorum- or enclave-held. There is no network selector: the classifier is network-free, so nothing about the predicate depends on mainnet versus testnet.

### Failure modes and correctness

- **Backing lwd down:** pass-through requests fail as they would today; the ZIS is transparent, so this is the operator's existing failure mode, not a new one.
- **Hub unreachable:** the shipped single-hub shim submits within the request; if the hub cannot be reached it fails that submit rather than leaking the migration to the operator. Retry, fail-over across >=2 hubs on expiry slack, and last-resort direct broadcast are the designed multi-hub behavior, not yet shipped.
- **Restart / multiple instances:** stateless, so nothing is lost. The hub dedups identical payloads by their `sha256` hash.
- **Invalid migration -> false success:** the ZIS returns success before the hub broadcasts, so an invalid tx fails silently at flush. Do stateless sanity at the ZIS (parseable, not already expired); full validity is the hub's broadcast result (surfacing it to the client is out of near-term scope).
- **The operator learns *that* a client migrated** (see [honest limits](./trust.md)): inherent; do not attempt shim-side mitigation.

### Crate layout

```
zeronym/shim/src/
  main.rs         # config load, boot sequence, serve; spawns the mixnet tasks
  lib.rs
  config.rs
  proxy.rs        # h2 server, :path routing, reverse-proxy, control-plane paths
  intercept.rs    # SendTransaction + GetTransaction decode; gRPC frame + response synth
  classify.rs     # is_orchard_touching over zebra-chain (pure, unit-tested)
  hub.rs          # hub client over the clearnet transport
  tls.rs          # in-enclave keygen, ACME, keymaker-quorum persistence
  wire.rs         # SubmitV1 / AckV1 / LookupV1 / LookupReplyV1 frames
  nym.rs          # transport correlator, client-lifecycle supervisor, address failover
  nym_driver.rs   # the linked nym-sdk client (feature `mixnet-driver`)
```

Note **no `state.rs`**: the shim holds no per-migration state. There is also no `attest.rs`; `/attestation` is relayed by `proxy.rs` to the platform's `bootproofd` rather than produced by the shim. Diversion landed as a branch in `intercept::send_transaction` on the fail-safe-folded verdict, with the upstream dial moved out of `handle()` so the connect happens only after a pass-through verdict; `intercept::get_transaction` routes lookups to the hub.

## The zero-indexer-hub (ZIH)

The ZIH (earlier called `zero-broadcaster`) is an attested-TEE service that receives **migration** transactions from many shims, holds them in an in-RAM batch queue, and publishes the batch together on a strict block cadence, so no party can link a migration to the source IP that submitted it. It broadcasts through an **indexer's `CompactTxStreamer`** (`SendTransaction`) over TLS, not a node's JSON-RPC `sendrawtransaction`. Shims reach it over TLS on the deployed hop; running **>=2 instances with failover** is designed. At launch adoption the modal batch is 0 or 1, which proves the mechanics and content privacy but not batching anonymity (see [honest limits](./trust.md)).

### Topology and process model

Many shims submit inward; the hub re-parses for telemetry, queues, and flushes every N blocks, shuffled, outward through its own indexer. It speaks three RPCs to that indexer, all over TLS: `GetLightdInfo` for the tip, `SendTransaction` to broadcast a batch, and `GetTransaction` for the detail lookups shims forward.

- **Enclave contents (TCB):** the hub binary + rustls. It is **lightweight**, like the shim: no validator in-enclave. It connects OUT to an existing **indexer** (`CompactTxStreamer` over TLS) for chain tip and for broadcasting, not a node's JSON-RPC.
- **Egress:** TLS gRPC out to the hub's indexer(s) for tip + broadcast. The mixnet build adds an in-process `nym-sdk` listener (no sidecar) and needs Nyx-RPC egress for ecash. Egress to the keymaker quorum is designed.
- **Supervisor:** small PID-1 script, mirrors the shim's.

### Language and crates

**Decision: Rust**, static-musl, reproducible, same reasons as the shim. The load-bearing choice is that it speaks `CompactTxStreamer` gRPC to the hub's indexer over TLS rather than a node's JSON-RPC.

### Inbound: receiving migrations

The hub is the server end of the shim's channel. It binds an in-process mixnet listener and decodes the same fixed-size frames the shim sends. The clearnet `POST /` submit path is closed unless `ZIH_HTTP_SUBMIT` re-opens it, and when closed it falls through to the same 404 an unknown path gets, so a scanner cannot tell whether this hub would have accepted a submission at all. The `POST /transaction` lookup path is not gated.

- **Channel + auth (STEVE).** The shim verifies the hub's attestation and derives a shared key (STEVE); migrations are encrypted to the hub. Whether the hub also authenticates the shim (mutual STEVE) is an open decision: one-way is enough for privacy, mutual would gate abuse.
- **Decrypt in-enclave.** Only the attested hub software sees cleartext; the hub host operator (Caution) and the Nym path see ciphertext.
- **Re-parse, but as telemetry only, never as a drop reason.** The hub parses with `zebra-chain` and re-runs `is_orchard_touching` so a disagreement with the shim is *visible*, and it stops there. This reverses an earlier reject-on-invalid rule, caught in `zeronym/hub/REVIEW.md`. The shim fail-safes for privacy: a body it cannot read cleanly routes to the migration arm precisely *because* it could not read it, so the transactions most likely to fail the hub's parse are exactly the ones the shim deliberately diverted, and a hub that rejects them converts the shim's fail-safe into a leak, handing an adversary who can characterise the parser skew an on-demand way to force a transaction back onto the direct-broadcast path. An unparseable payload is therefore queued and published; the indexer's `SendTransaction` (which relays to its node) is the only authority on validity, and the cost of being wrong is one wasted batch slot. The permitted refusals are narrow and structural: authentication failure, a malformed frame, byte-budget exhaustion, and the expiry admission rule. **Rate-limit** per channel to bound resource use.

### The batch queue

In-RAM (diskless enclave), keyed by the **payload hash** `sha256(tx_bytes)` for dedup, not the txid: under ZIP 244 two different byte strings can share a txid, and a submitter-chosen key would let an attacker suppress another's entry, so the hash the submitter cannot forge is the right identity. Each entry carries that key plus the derived txid (telemetry only), `expiry_height`, `tx_bytes`, and `received_at`. The hub tracks the current chain height `H` from its indexer connection to schedule flushes and check expiries. Identical resubmissions collapse; this is also what makes cross-hub failover safe.

### Flush and publish (the core)

- **Flush trigger:** at every height that is a multiple of **N (Decision: N = 20, about 25 minutes)**, and at no other time. The bound is a budget rather than a round number: `N + mining_margin + delivery_lag <= min_wallet_expiry`, or `20 + 4 + 6 = 30` against a 40-block expiry, asserted at startup so a later change to N fails loudly instead of quietly pushing traffic onto the direct-broadcast path. An earlier draft set N near 10 against Brave's 20-block default, the ecosystem's lowest (librustzcash 40, Zingo 100), which let the least generous wallet cap everyone; Brave is out of scope for v1 and the ask to them is 40. Doubling the window doubles the expected batch at no cost to any wallet, the cheapest improvement available to the batch-size problem in [honest limits](./trust.md).
- **No early flush, and this replaces an earlier design.** The trigger used to fire early if any queued migration's `expiry_height` came within a safety margin. That is an attacker-operated flush clock: the hub's re-validation is stateless, so one well-formed but consensus-invalid Orchard-touching transaction per block, at no cost, collapses every batching window network-wide and permanently. The urgency is instead made unreachable by **admission control**: accept a migration only if it provably survives the next scheduled flush (`expiry >= next_flush_height(H) + mining_margin`), and refuse it otherwise so the shim holds and retries rather than broadcasting. If nothing urgent can be admitted, nothing can ever be urgent.
- Batches are triggered by **time / block-height, never by transaction count**: a count-based flush (say every 100 txs) would let an attacker submit 99 of its own migrations the instant it sees a target submit, isolating the target's transaction in the revealed batch. Batch granularity must also line up with how wallets choose anchors and expiries (see [the problem](./problem.md)).
- **Publish "simultaneously."** On flush: take all pending migrations, **shuffle the order** (never leak arrival order), and submit them through the indexer(s) as close to simultaneously as possible (parallel `SendTransaction`), so they enter the mempool together and land in the same block window. An on-chain / mempool observer then sees N migrations appear together, unordered, from many shims. **Decision: randomize order + parallel submit**; do not drip them out.
- **Confirmation tracking (designed, not built).** Move flushed migrations to an "awaiting confirmation" set; watch the chain until each is mined; **re-submit** if a tx is not seen within a few blocks (node dropped it, or a hub crash lost it). Drop from the set once confirmed or expired. Until this exists, a batch is on the network like any other submission once flushed, and nothing on either side tracks whether it was mined.
- **The anonymity set is the batch itself** (cross-operator), so batch size is the key metric. At launch adoption (measured ~0.77 Orchard-touching tx/block, one to a few operators) the modal batch is 0 or 1, and a size-1 batch's anonymity set is the transaction itself: the shuffle, the simultaneous publish, and the enclave prove content privacy and mechanics, not batching anonymity. The property is real but conditional on adoption, with no fix at v1. The hub therefore measures and exports its achieved batch size rather than asserting the property (see [honest limits](./trust.md)); hub-generated decoys are a costly last resort, not the primary lever.

### The hub's read-only endpoints

Two, both `GET`, both there because an attested enclave has no console.

- **`GET /nym-address`** publishes the address every shim needs in order to submit at all. It is the one value in this system meant to be public, so publishing it costs nothing, and before it existed reading the address and proving the binary were mutually exclusive: the address reached only the log, and exposing the log meant debug mode, which disables attestation. It answers 503 until the driver's first connect, deliberately, so an operator never pastes an empty string into a shim's `--hub-nym`.
- **`GET /healthz`** is liveness, for the same no-console reason.

**Decision: neither endpoint reveals queue depth, batch size, or a count**, and a test asserts that absence. Those numbers are exactly the achieved-batch-size measurement [honest limits](./trust.md) calls for, but served live and unauthenticated they would be an anonymity-set oracle: an observer could watch the queue fill and know how much company a migration had. Measurement belongs in operator telemetry, not on a public endpoint.

**Decision: the hub's Nym address survives a client rebuild.** One credential store is built outside the rebuild loop and cloned into each rebuilt client, so identity key, encryption key and gateway registration are all reused. Without this every rebuild minted a fresh identity, which invalidated the address baked into every shim's config, and the shim was observed doing it thousands of times. A counter forces a genuinely new identity after a bounded number of rebuilds, so a wedged client still has an escape hatch. Both arms are tested, address-unchanged and address-rotated.

### Chain connection (tip + broadcast)

**Decision: connect OUT to an existing indexer's `CompactTxStreamer` over TLS**, not a node's JSON-RPC and not a validator in-enclave. The indexer endpoint is already published over TLS, which the enclave requires: without TLS on this hop the parent host reads every batch in the clear moments before it is public. Speaking `CompactTxStreamer` also means the hub broadcasts through exactly the interface wallets use, so nothing about a batched migration looks different from an ordinary submission at the point it enters the network.

- **Tip:** poll `GetLightdInfo` for the height, keeping `H` current for flush cadence and expiry admission.
- **Broadcast:** `SendTransaction` for each tx in the flush. Configure **>=2 indexer endpoints** for robustness. Honest cost: an indexer is a single funnel in front of a single node, so the "publish to every node" property is weaker here than direct multi-node broadcast, and a batch that entered only one mempool is one outage from never being mined. Broadcasting to many P2P peers directly (Nate's point: a bigger anonymity set for the broadcast source), and over Nym to hide the hub's own IP, are designed enhancements.
- **Detail lookups:** the same indexer answers `GetTransaction`, which is how the shim's intercepted `GetTransaction` is served without touching the operator.
- The indexer connection is a hard dependency (no tip -> cannot schedule; no indexer -> cannot broadcast), so >=2 endpoints, and indexer-down is part of the hub's failure handling.

### Key management (hub key, STEVE, keymaker quorum)

The hub key is generated in-enclave and persisted by the keymaker quorum. **Decision: a single shared hub key across all hub instances**, which is what makes failover clean: a shim encrypts to "the hub key" and any attested hub can decrypt, dedup and publish, where per-hub keys would force re-encryption on failover and could strand a migration whose hub died mid-flight. Governance of that key, and the STEVE handshake the hub answers, are in [trust](./trust.md).

### Failover and multiple hubs

- **Run >=2 hubs, shared key.** A shim prefers a **primary** hub (so batches converge there and stay dense) and **fails over** to a standby only when the primary is unreachable.
- **Dedup by payload hash** within each hub. If failover causes a migration to reach two hubs, both may publish; the second on-chain submission is a **harmless already-known duplicate**. No cross-hub state sync is needed near-term (Decision: accept harmless duplicates over the complexity of a shared published-set).
- The consortium's multiple orgs are the natural operators of the standby hubs, which also starts decentralization.

### Configuration

```
ZIH_LISTEN          # inbound TLS endpoint (default 0.0.0.0:8090)
ZIH_INDEXERS        # CompactTxStreamer endpoints over TLS: tip + broadcast (repeatable)
ZIH_INDEXER_TLS     # expected TLS name for those
ZIH_NYM             # bind the in-process mixnet listener
ZIH_NYM_GATEWAY     # entry gateway(s) to pin, repeatable
ZIH_NYM_TOPOLOGY    # localnet only
ZIH_HTTP_SUBMIT     # re-open the clearnet POST / submit path (default OFF)
```

The cadence is **not** configurable. `FLUSH_INTERVAL_BLOCKS = 20`, `MINING_MARGIN = 4`, `MAX_DELIVERY_LAG = 6` and `MIN_WALLET_EXPIRY = 40` are compile-time constants in `hub/src/batcher.rs`, and the budget inequality between them is asserted at startup, so changing one and getting it wrong fails the build or the boot rather than quietly pushing traffic onto the direct-broadcast path. There is no `role` setting either: primary-versus-standby is a design question, not a shipped one.

### Failure modes and correctness

- **Indexer(s) down:** cannot get tip or broadcast; with >=2 endpoints this is rare, but if all are down the hub cannot flush. Brief outages self-heal on the migrations' expiry slack; a sustained one is what the designed last-resort direct broadcast covers.
- **Hub crash:** the in-RAM queue and awaiting-confirmation set are lost (diskless). Recovery is designed, not shipped: standby hubs plus a shim that resubmits across failover on expiry slack. Hub-crash durability rides on the wallet's own resend.
- **Expiry pressure:** admission control refuses any migration that would not survive the next scheduled flush, so nothing urgent is ever queued. There is no early-flush escape hatch (that was retired as an attacker-operated flush clock).
- **Garbage / abuse:** re-validate + rate-limit; optionally require shim attestation.
- **Fee too low to mine before expiry:** the fee is in the wallet-signed tx and the hub cannot change it; `safety_margin` gives mining headroom, but a badly-underpaid migration can still fail. That is the wallet's responsibility, not the hub's.

### Crate layout

```
zeronym/hub/src/
  main.rs         # config, boot sequence, run the flush loop
  lib.rs
  config.rs
  server.rs       # inbound submit + lookup over the clearnet transport
  queue.rs        # in-RAM dedup queue keyed by payload hash; expiry tracking
  batcher.rs      # cadence trigger, admission control, shuffle, parallel publish
  chain.rs        # indexer connection: GetLightdInfo (tip) + SendTransaction (broadcast)
  tls.rs
  wire.rs         # the same frames as the shim, decoded on this side
  nym.rs          # the in-process mixnet listener
  nym_driver.rs   # the linked nym-sdk client (feature `mixnet-driver`)
```

There is no `keys.rs` and no `attest.rs`: the STEVE handshake, the encrypt-to-hub-key layer, and keymaker reconstitution are designed and have no code yet.

## Boot, build, and attestation

Both services share the same enclave idioms: a static-musl Rust binary, a reproducible **StageX** build, running in an AWS Nitro enclave (attested, diskless). Both generate and hold keys **in-enclave** and bind a public key into the **NSM attestation**; neither writes secrets to disk. Reproducibility lets the consortium and third parties confirm the running binary is the reviewed binary.

**Attestation binding.** Each enclave binds the relevant **public key** into its attestation (shim: the TLS public key, not the Nym address; hub: the hub public key), so an auditor can check that cert-pubkey equals attested key. The candidate mechanisms and their status are in [trust](./trust.md).

**Shim boot sequence.** (1) Key material: reconstitute the TLS keypair from the keymaker M-of-N quorum if one exists, else generate in-enclave and register with the quorum (persists across cold boots/upgrades; the private key never leaves the enclave). (2) Certificate: ensure a valid CA cert for the public domain via ACME, keyed to the enclave-born key. (3) Attestation: bind the TLS public key into the Nitro attestation; serve `/attestation` (or expose over Nym). (4) Hub session: STEVE-handshake each configured hub; cache the shared keys. (5) Backing lwd: open the upstream h2 connection(s); health-check. (6) Listen: bind `:443`, serve.

**Hub boot sequence.** (1) Hub key from the keymaker quorum (reconstitute; private key stays in-enclave). (2) Attestation: bind the hub public key into the Nitro attestation; publish `/attestation` (or over Nym) for the shim's STEVE check + auditors. (3) Chain: connect to the hub's indexer(s) over TLS; sync `H` via `GetLightdInfo`; verify `SendTransaction` works. (4) Inbound: bind the in-process `nym-sdk` listener and publish the hub's Nym address. (5) Run the flush loop against `H`.

Cross-party open forks are tracked [outside this book](https://github.com/ShieldedLabs/zero/blob/main/zeronym/OPEN-QUESTIONS.md).

### Build and test

**Shim.** Unit: `classify.rs` against real vectors (the `ironwood_activation.rs` migrate tx, an Orchard-touching tx with no Ironwood bundle, a V5 Orchard spend, value entering Orchard, and a mainnet transparent tx), the correctness-critical piece. Integration: a mock backing lwd; assert every non-intercepted method and a pass-through `SendTransaction` reach it unchanged, and an Orchard-touching transaction (and every `GetTransaction`) is diverted to a mock hub, never reaching the backing lwd. Built so far: the classifier vectors (hand-written and generated), the wire-level transparency suite against both a hand-rolled h2c mock and a real `tonic` server, the logging assertions, a connection-counting backend that asserts the operator is never even dialled for a diverted transaction, the hub client, and the mixnet driver's own channel, lifecycle and diversion tests. The fixtures were long generated with zebra's own serializer, so no wallet-produced transaction had been classified; the mainnet migration run closed that gap by classifying and diverting a transaction a wallet actually produced. Enclave: the reproducible StageX build (done, CI-checked), the Nitro boot (done, live deploys since 2026-08-01), and the nonce-bound `/attestation` document (served; the manifest binds the deploy config including `ZIS_BACKEND`, while binding the TLS pubkey specifically remains open) are real, and a third-party operator has run the Auditor Role steps against a live enclave (PCR0/1 caveat in [trust](./trust.md)). **Done end to end:** a real mainnet Orchard-to-Ironwood migration has run ZIS -> hub -> batch -> published on the cadence, with the operator's indexer never seeing the direct submit. What it does not yet prove is batching anonymity: at launch adoption the batch is size 1 (see [honest limits](./trust.md)).

**Hub.** Unit: the flush (N queued migrations -> one shuffled, parallel publish), admission control (a migration that would not survive the next scheduled flush is refused, never early-flushed), dedup (identical-payload collapse; harmless cross-hub duplicate), and re-parse as telemetry (a shim/hub disagreement is logged, never a drop reason). Integration: a mock shim submitting over a local channel and a mock indexer capturing `SendTransaction`; assert a batch is published together, shuffled, once per payload. Enclave: reproducible StageX build; boot in a Nitro enclave; verify the `/attestation` doc carries the hub key; a shim submits a migration over TLS. **Done end to end (mainnet, TLS hop):** a migration flowed shim -> hub -> batch -> published through the indexer and landed on-chain, unlinkable at the point of publish. Still unproven: unlinkability *as batching* (needs a batch larger than one, i.e. adoption), and the Nym hop against the public mixnet. The mixnet transport itself is proven end to end, shipped drivers on both sides, against a local mixnet the `zeronym/nymnet` harness starts.
