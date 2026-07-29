# Nym-only hidden-service transport for the Zaino endpoint

**Status: SPEC. Not implemented.** No build code lands until the Caution platform
questions at the end of this doc are answered. This document is the design; the
implementation section is written so it can be executed in one pass once unblocked.

## Goal

Make the wallet-facing Zaino gRPC endpoint of the combined Zebra+Zaino Caution
enclave reachable **only over the Nym mixnet**, in the manner of a Tor onion
(hidden) service: the service is addressed by a cryptographic Nym address with no
inbound IP, so both the light-wallet user's query metadata and the server's
location are hidden.

Block sync and Zebra P2P stay on **clearnet**. They carry only public chain data,
hiding them protects no wallet user, they are high-volume, and Zebra has no proxy
transport anyway. **Only the wallet to Zaino gRPC path moves onto Nym.**

## Threat model (what this fixes)

A light-wallet server sees which addresses a wallet asks about (`GetTaddressTxids`,
`GetAddressUtxos`), when it broadcasts (`SendTransaction`), and can correlate by
IP and timing. Today that metadata rides the wallet to Zaino gRPC channel, which
is a public TCP bind. Moving that one channel onto Nym hides the querying user and,
because the service is addressed by key rather than IP, the server's location too.
The enclave already blinds the operator to memory; Nym extends the privacy boundary
out to the wallet.

## Architecture (ingress-only over Nym)

Three enclave **outbound** clearnet paths, **zero inbound**:

```
                 AWS Nitro Enclave  (attested, diskless)
        +------------------------------------------------------+
        |   zebrad  --loopback 127.0.0.1:8232-->  zainod        |
        |  (validator)                          (indexer)       |
        |     |                                 gRPC on         |
        |     |                                 127.0.0.1:8137  |
        |     |                                      ^          |
        |     |                                      | loopback |
        |     |                              nym-proxy-server    |
        +-----|--------------------------------------|----------+
              | egress                               | egress
        Zcash P2P :18233     DNS :53         Nym gateway :443 (WSS) + nym-api :443
        [CLEARNET]           [CLEARNET]      [CLEARNET carrier for Sphinx packets]
                                                      |
                                              Nym mixnet (mixnodes)
                                                      |
                                        nym-proxy-client (on the WALLET host)
                                        exposes 127.0.0.1:8080
                                                      |
                                        wallet --server http://127.0.0.1:8080
```

The wallet reaches the service **only** by its Nym address
(`<identity>.<encryption>@<gateway>`). There is no inbound IP. `nym-proxy-server`
receives requests over the mixnet and forwards them to Zaino on loopback; replies
return over the mixnet via SURBs. Because the destination is a mixnet participant
(not a clearnet host), traffic stays end-to-end inside the mixnet, no exit node
ever sees the plaintext gRPC.

## Wallet UX

The wallet is used normally. The user runs one sidecar and points the wallet at a
local port, the same shape as `tor` + a `.onion`:

```
nym-proxy-client -s <server Nym address> --listen_port 8080
zingo-cli --server http://127.0.0.1:8080
```

Supported today by desktop/CLI wallets that take a `--server` flag (zingo-cli,
ywallet). **Zashi (mobile) is out of scope**: it cannot host a local Nym client and
its Tor is Arti-embedded, not a generic proxy setting. A future native "wallet
embeds Nym" is upstream wallet work; Zaino lists a planned `NymService` backend but
it is unimplemented (`zaino/packages/zaino-serve/src/lib.rs:3`,
`zaino/docs/use_cases.md:44`).

## Component choice

**nym-sdk TcpProxy binaries**: `nym-proxy-server` (enclave side, mixnet ->
`127.0.0.1:8137`) and `nym-proxy-client` (wallet side). This is the architecture of
the reference `github.com/nymtech/nym-zcash-grpc-demo`, which tunnels lightwalletd
gRPC over Nym and was tested with zingo-cli, so it transfers to Zaino directly (same
`CompactTxStreamer` proto). Its TLS stack is **rustls + ring, no
OpenSSL/native-tls/aws-lc-rs** (confirmed in the demo's `Cargo.lock`), so it builds
as a fully static musl binary in a StageX stage.

The TcpProxy *module* is marked deprecated in favour of the newer **Stream** module,
but the proxy *binaries* still ship and are the only turnkey, no-code path. Stream is
the forward migration target if we later write a custom in-process bridge. TcpProxy
wraps each TCP stream as `ProxiedMessage { session_id, message_id }` so HTTP/2 frame
order survives the mixnet's unordered delivery; a many-block `GetBlockRange` is
correct but slow and bursty (throughput is gated by SURB replenishment round-trips
and ~2 KB Sphinx payloads at mixnet latency, not by bandwidth).

## Implementation plan (gated, to execute in one pass once unblocked)

Anchors are in `deploy/caution-zaino/combined/` unless noted.

1. **Build stage** in `Containerfile`: add a third static-musl StageX stage after
   the `zaino-builder` stage (~line 154), mirroring `zaino-builder` (own
   pallet-rust/pallet-clang bases, `RUSTFLAGS` `+crt-static`,
   `cargo build --release --bin nym-proxy-server`), chained off the build-barrier
   idiom (line 147) so all three heavy links serialize. Runtime stage gains one
   `COPY --from=<nym-stage> /usr/local/bin/nym-proxy-server` beside lines 208-209.
   Pin a specific nym commit/tag and vendor its `Cargo.lock`; the sealed build runs
   `--network=none`, so Nym sources must arrive during a network-allowed
   `cargo fetch`/vendor step.
2. **Vendor/assemble**: Nym is not in the zero repo. Either vendor a pinned nym as a
   top-level prefix (mirror how `orchard` is handled) and add `mkdir`/`git archive`
   lines in `assemble-combined.sh` (lines 19, 21-26), or add a pinned fetch step.
3. **Supervisor** `run-both.sh`: add `SV_NYM_*` locals (block at lines 20-26),
   `nym_pid` (32-33), a `kill -TERM "$nym_pid"` in `shutdown()` (35-40), launch
   `nym-proxy-server` **after** zaino (after line 87, since it forwards to
   `127.0.0.1:8137`), and extend the `kill -0` health loop (line 90). Flags:
   `-u 127.0.0.1:8137`, `-c <tmpfs config dir>`, `--gateway <pinned>`, `-e <env>`.
4. **Loopback bind**: `zainod-colocated-testnet.toml:11`, `listen_address`
   `'0.0.0.0:8137'` -> `'127.0.0.1:8137'` (maps to
   `zaino/packages/zaino-serve/src/server/config.rs:23`, bound at `grpc.rs:40`). No
   zebra change: its RPC is already `127.0.0.1:8232`.
5. **Network policy**: edit the **in-repo** `combined/caution.hcl` (not the deployed
   `z3-enclave/caution.hcl`, which assemble overwrites): delete the `8137` ingress;
   add egress for the pinned Nym gateway (WSS `:443`) and nym-api
   (`validator.nymtech.net:443`); keep zebra P2P (`:18233`) and DNS (`:53`).
   Reconcile the assemble `cp` so the testnet HCL survives re-assembly.
6. **HTTP/2 window** (streaming perf over high-RTT mixnet):
   `zaino/packages/zaino-serve/src/server/grpc.rs:75` is a bare `Server::builder()`.
   Add `initial_stream_window_size` / `initial_connection_window_size` /
   `http2_adaptive_window(true)`. Without this a single `GetBlockRange` stream is
   throttled to ~64 KB per round-trip over the mixnet. Small `[zero]` zaino patch.

## Stable address and attestation binding (the headline, gated)

The enclave is diskless, so `nym-proxy-server` writes its identity keys to tmpfs and
the Nym address **churns on every cold boot** unless we intervene.

- **PoC fallback**: per-boot ephemeral address (re-point the wallet each boot). Fine
  for a demo, unacceptable for a published service.
- **Real service**: **KMS-seal-to-PCR**. Generate the key once inside the enclave,
  seal it with AWS KMS bound to the enclave PCRs, store only the ciphertext on the
  parent; each boot the enclave attests and asks KMS to decrypt, materialises the key
  into the tmpfs config dir, and pins `--gateway` so the gateway component stays
  constant. The operator never sees the key and the address is stable.

**Attestation binding** is the value a Tor onion service cannot offer: publish the
freshly-generated Nym public key in the attestation document's `user_data`, so a
wallet can verify "this Nym address is served by exactly this attested, reproducible
image." Today attestation is produced entirely by Caution's platform runtime; there
is no workload `user_data` hook anywhere in the tree. Whether this is achievable is
the primary open question below.

## Open questions for Caution (the gate)

1. **Attestation `user_data`**: can an in-enclave workload inject custom `user_data`
   (a freshly generated Nym public key) into the Nitro attestation document that the
   `/attestation` endpoint returns to a nonce challenge? Without this, the
   Nym-address-to-attestation binding is impossible and we would publish an
   unverified address.
2. **Key sealing / persistence**: is there a parent-side sealed store or a
   KMS-to-PCR path so the enclave can persist its Nym identity key across cold boots
   without the operator ever seeing it? Required for a stable hidden-service address.
3. **Gateway egress**: does the egress allowlist permit a persistent, long-lived
   outbound WSS to a pinned Nym gateway (`:443`) plus HTTPS to nym-api (`:443`)? Is
   there a parent-proxy idle timeout that would drop a long-lived gateway WebSocket?
4. **Zero-ingress app**: is an outbound-only app (no ingress rules at all)
   supported? The hidden service needs no inbound.
5. **(If the gateway requires ecash / ticketbook bandwidth credentials)**: egress to
   Nyx chain RPC (`rpc.nymtech.net:443`).

## Verification (later phase, once built)

- **CI**: extend the caution-zaino build to compile the third nym stage; double-build
  digest compare for reproducibility.
- **Local**: `nym-proxy-server` against a local zaino, `nym-proxy-client` on another
  host, `zingo-cli --server http://127.0.0.1:8080`; confirm `GetLightdInfo`, a bounded
  `GetBlockRange`, and a wallet birthday sync complete over the mixnet. Expect
  latency-bound and slow versus the measured ~328 blocks/s clearnet baseline.
- **On Caution**: deploy, confirm a zero-ingress app boots and attests, dial the
  published Nym address from a wallet host, run the paces harness over the mixnet.
- **If binding lands**: verify a wallet reads the Nym address from the attestation
  document and matches PCRs before connecting.

## Critical files

- `deploy/caution-zaino/combined/Containerfile` (new nym stage + runtime COPY)
- `deploy/caution-zaino/combined/run-both.sh` (third supervised child)
- `deploy/caution-zaino/combined/zainod-colocated-testnet.toml` (loopback bind)
- `deploy/caution-zaino/combined/caution.hcl` (network: drop ingress, add egress)
- `deploy/caution-zaino/combined/assemble-combined.sh` (vendor/fetch nym)
- `zaino/packages/zaino-serve/src/server/grpc.rs:75` (HTTP/2 window, `[zero]` patch)
