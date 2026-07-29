# Nym-only hidden-service transport for the Zaino endpoint

**Status: SPEC. Not implemented.** No build code lands until the Caution platform
questions at the end are answered. This document is the design; the implementation
section is written so it can be executed in one pass once unblocked.

## Goal

Make the wallet-facing Zaino gRPC endpoint of the combined Zebra+Zaino Caution
enclave reachable **only over the Nym mixnet**, in the manner of a Tor onion
(hidden) service: addressed by a cryptographic Nym address with no inbound IP, so
both the light-wallet user's query metadata and the server's location are hidden.

Block sync and Zebra P2P stay on **clearnet** (public chain data; hiding them
protects no user, and Zebra has no proxy transport). **Only the wallet to Zaino
gRPC path moves onto Nym.**

## Threat model (what this fixes)

A light-wallet server sees which addresses a wallet asks about
(`GetTaddressTxids`, `GetAddressUtxos`), when it broadcasts (`SendTransaction`),
and can correlate by IP and timing. That metadata rides the wallet to Zaino gRPC
channel. Two adversaries matter, and they need two different defenses:

- **The network / a passive observer** learns who is querying and where the server
  is. Fix: the **Nym mixnet** (metadata privacy, location hiding).
- **The host operator (the untrusted parent EC2 instance)** could read the plaintext
  queries. Fix: an **attested end-to-end channel** that terminates *inside* the
  enclave, so confidentiality is cryptographic and provable to the wallet, not a
  matter of where a process happens to run.

The second point is the correction from Nym's engineering feedback (2026-07-29):
whoever unwraps to plaintext must be the attested TEE, and the wallet must be able
to *verify* that. See "The trust anchor" below.

## Architecture

Three enclave **outbound** clearnet paths, **zero inbound**:

```
                 AWS Nitro Enclave  (attested, diskless)
        +------------------------------------------------------+
        |   zebrad  --loopback 127.0.0.1:8232-->  zainod        |
        |  (validator)                          (indexer)       |
        |     |                          serves gRPC over       |
        |     |                          ATTESTED TLS on        |
        |     |                          127.0.0.1:8137         |
        |     |                                   ^  (TLS key   |
        |     |                                   |   born in   |
        |     |                                   |   the TEE)  |
        |     |                            nym-proxy-server      |
        |     |                            (UNTRUSTED byte mover;|
        |     |                             sees only ciphertext)|
        +-----|-------------------------------------|-----------+
              | egress                              | egress
        Zcash P2P :18233     DNS :53        Nym gateway :443 (WSS) + nym-api :443
        [CLEARNET]           [CLEARNET]     [CLEARNET carrier for Sphinx packets]
                                                     |
                                             Nym mixnet (mixnodes)
                                                     |
                                        nym-proxy-client (WALLET host)
                                        exposes 127.0.0.1:8080
                                                     |
                          wallet --server https://127.0.0.1:8080  (+ RA-TLS verifier)
```

The wallet reaches the service only by its Nym address
(`<identity>.<encryption>@<gateway>`). There is no inbound IP; inbound requests
arrive back over the same outbound gateway WebSocket. The gRPC stream is
**attested-TLS end to end from the wallet to inside the enclave**; every Nym hop,
the gateway, and `nym-proxy-server` itself see only ciphertext.

**The Nym client is untrusted.** Because TLS terminates inside the enclave, the
whole Nym mixnet stack is a ciphertext byte mover. It can run inside the enclave
(over Caution's transparent vsock proxy, verified feasible, our zebrad already does
raw outbound TCP the same way) **or on the parent**. Running it parent-side shrinks
the attested TCB to `zaino + rustls` and sidesteps the long-lived-WSS-egress and
gateway-idle-timeout questions. Either way, compromising it yields only ciphertext
plus coarse traffic metadata, never wallet queries.

## The trust anchor: attested TLS (RA-TLS) inside the tunnel

This is the load-bearing mechanism. It is the well-known RA-TLS / attested-TLS
pattern (Gramine, Knauth et al. 2018; AWS "verify enclave counterparties"),
transposed to Nitro:

1. **At enclave boot**, generate a TLS keypair *inside* the enclave; the private
   key never leaves tmpfs.
2. **Bind the public key into the attestation.** Call the Nitro Security Module:
   `Request::Attestation { user_data, nonce, public_key }`
   (`aws-nitro-enclaves-nsm-api`), with the DER TLS pubkey in `public_key` (or
   `SHA-256(cert)` in `user_data`). The NSM returns a COSE_Sign1 document signed by
   the AWS Nitro root, carrying PCR0/1/2 **plus the embedded key**.
3. **Zaino serves gRPC over that TLS**, terminating inside the enclave. Zaino
   already has this path: `GrpcServerConfig.tls` -> `get_valid_tls()` ->
   `ServerTlsConfig::new().identity(...)` (`zaino/packages/zaino-serve/src/server/config.rs:11-54`),
   wired at `grpc.rs:75-83`. The only delta is generating the cert/key in-enclave at
   boot instead of file-provisioning them.
4. **The wallet verifies the attestation, not the PKI.** It dials
   `https://127.0.0.1:8080` (a self-signed cert, SNI `localhost`), so it replaces
   standard cert verification with a custom `rustls` verifier that: chains the
   COSE_Sign1 doc to the AWS Nitro root; checks PCR0/1/2 equal the pinned
   reproducible-build values; checks the presented cert pubkey equals the
   attestation `public_key`; checks nonce freshness. Only then does it send. Marlin
   `NitroProver` is portable prior art.

**One key, two jobs.** The attestation binds the **TLS key**, not the Nym address.
That single keypair is both the "encrypted stream only the TEE can decrypt" and the
attested identity. Consequence: the wallet trusts the key regardless of which Nym
address routes to it, so a hijacked or rebound Nym address can only **deny** service,
never decrypt or MITM. (Binding the Nym address instead would be weaker, since the
Nym client may run outside the TEE.)

## Component choice (transport)

**nym-sdk TcpProxy binaries**: `nym-proxy-server` (mixnet -> `127.0.0.1:8137`) and
`nym-proxy-client` (wallet side). Reference: `github.com/nymtech/nym-zcash-grpc-demo`
(tunnels lightwalletd gRPC over Nym, tested with zingo-cli). rustls + ring, no
OpenSSL, so static-musl if built into the image. The TcpProxy *module* is marked
deprecated in favour of the newer **Stream** module, but the proxy *binaries* still
ship and are the turnkey path; Stream is the migration target for a custom bridge.
The transport is layer-agnostic, so the attested-TLS gRPC rides inside it opaquely;
a many-block `GetBlockRange` is correct but slow and bursty (SURB replenishment +
2 KB Sphinx payloads at mixnet latency, not bandwidth).

## Wallet UX

```
nym-proxy-client -s <server Nym address> --listen_port 8080
zingo-cli --server https://127.0.0.1:8080     # with the RA-TLS verifier
```

Desktop/CLI wallets that take a `--server` flag (zingo-cli, ywallet), plus the
custom RA-TLS verifier (they already use rustls, so a bounded add). **Zashi (mobile)
out of scope**: can't host a Nym client; its Tor is Arti-embedded.

## Implementation plan (gated, to execute in one pass once unblocked)

Anchors in `deploy/caution-zaino/combined/` unless noted.

1. **In-enclave key + attestation** (new, small): at boot, generate a TLS keypair in
   the enclave and call NSM to attest it with `public_key=DER(pubkey)`; write
   cert/key to the tmpfs config dir. Wire into `run-both.sh` before zaino starts.
   Depends on Caution NSM access (open question 1).
2. **Enable zaino TLS**: point `zainod-colocated-testnet.toml` `[grpc_settings.tls]`
   at the generated cert/key, and flip `listen_address` `'0.0.0.0:8137'` ->
   `'127.0.0.1:8137'` (line 11). Uses the existing `get_valid_tls()` path; note the
   image is currently built `no_tls_use_unencrypted_traffic`, so the build feature
   must allow TLS.
3. **Nym transport**: `nym-proxy-server -u 127.0.0.1:8137`. Preferred **parent-side**
   (untrusted mover, smaller TCB). If in-enclave instead: add a third static-musl
   StageX stage in `Containerfile` after `zaino-builder` (~line 154), mirror that
   stage + the build-barrier idiom (line 147), and `COPY` the binary into runtime
   (beside lines 208-209); vendor a pinned nym (Nym is not in the zero repo) via
   `assemble-combined.sh` (lines 19, 21-26); add an `SV_NYM_*` child in `run-both.sh`
   (locals 20-26, `nym_pid` 32-33, teardown 35-40, launch after line 87, health loop
   line 90).
4. **Network policy** (in-repo `combined/caution.hcl`, not the deployed copy): delete
   the `8137` ingress; add egress for the pinned Nym gateway (WSS `:443`) and nym-api
   (`validator.nymtech.net:443`); keep zebra P2P (`:18233`) and DNS (`:53`).
5. **Wallet RA-TLS verifier**: a custom `rustls` `ServerCertVerifier` that pins
   PCR0/1/2 and checks cert-pubkey == attestation `public_key`. Ships with the
   `nym-proxy-client` bundle or as a small wallet-side tool.
6. **HTTP/2 window** (streaming perf over high-RTT mixnet):
   `zaino/packages/zaino-serve/src/server/grpc.rs:75` is a bare `Server::builder()`.
   Add `initial_stream_window_size` / `initial_connection_window_size` /
   `http2_adaptive_window(true)`. `[zero]` patch.
7. **Stable address**: diskless enclave -> `nym-proxy-server` keys live in tmpfs, so
   the Nym address churns per cold boot. Since the wallet trusts the **TLS key**, a
   churning Nym address only forces re-publishing the address, it does not weaken
   security. A stable address (nicer UX) still wants **KMS-seal-to-PCR** for the Nym
   identity key. Note: the same KMS-seal mechanism can persist the TLS key too, if a
   stable long-lived attested key is preferred over per-boot regeneration.

## Security model note

The plaintext-Nym design (wallet dials `http://...`, Nym client trusted to sit
in-enclave) gives confidentiality that is **topological and unverifiable** by the
wallet: the untrusted parent could run the Nym client itself and read every query,
indistinguishably. Treat it as acceptable **only for a throwaway PoC**, never for a
published service. The attested-TLS layer above is what makes it real.

## Open questions for Caution (the gate)

1. **NSM access / attestation binding (primary):** can the in-enclave workload call
   the NSM (`/dev/nsm`) to produce an attestation document carrying a workload-chosen
   `public_key` (the in-enclave TLS pubkey)? If Caution's runtime monopolizes the
   NSM, does its `/attestation` endpoint let us supply `public_key` / `user_data`?
   This is the linchpin: without it, RA-TLS has no attestation to bind to.
2. **Key sealing / persistence:** any parent-side sealed store or KMS-to-PCR path so
   the enclave can persist a key (TLS and/or Nym identity) across cold boots without
   the operator seeing it? Needed only for a stable address / long-lived key.
3. **Gateway egress:** does the allowlist permit a persistent long-lived outbound WSS
   to a pinned Nym gateway (`:443`) + HTTPS to nym-api (`:443`)? Any parent-proxy
   idle timeout that would drop a long-lived gateway WebSocket? (Moot if the Nym
   client runs parent-side.)
4. **Zero-ingress app:** is an outbound-only app (no ingress rules) supported?
5. **(If the gateway needs ecash / ticketbook credentials):** egress to Nyx chain
   RPC (`rpc.nymtech.net:443`).

## Verification (later phase, once built)

- **CI**: if the Nym client is in-enclave, extend the build to compile the third nym
  stage + double-build digest compare.
- **Local**: `nym-proxy-server` against a local TLS-enabled zaino, `nym-proxy-client`
  on another host, `zingo-cli --server https://127.0.0.1:8080` with the RA-TLS
  verifier; confirm the verifier rejects a wrong-PCR / wrong-pubkey endpoint, then
  confirm `GetLightdInfo`, a bounded `GetBlockRange`, and a birthday sync complete
  over the mixnet. Expect latency-bound and slow vs the ~328 blocks/s clearnet
  baseline.
- **On Caution**: deploy, confirm a zero-ingress app boots and attests with our
  `public_key` in the doc, dial the published Nym address, and have the wallet verify
  PCRs + pubkey before syncing.

## Critical files

- `zaino/packages/zaino-serve/src/server/config.rs:11-54` (existing `get_valid_tls`)
  and `grpc.rs:75-83` (TLS wiring + HTTP/2 window) - the in-enclave termination point
- `deploy/caution-zaino/combined/run-both.sh` (boot keygen+attest; optional nym child)
- `deploy/caution-zaino/combined/zainod-colocated-testnet.toml` (enable TLS; loopback)
- `deploy/caution-zaino/combined/caution.hcl` (drop ingress, add egress) - in-repo copy
- `deploy/caution-zaino/combined/Containerfile` + `assemble-combined.sh` (only if the
  nym client is built in-enclave)
- NEW: wallet-side RA-TLS verifier (custom rustls `ServerCertVerifier`)
