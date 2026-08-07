# Running the zero-indexer-shim (operator guide)

Run the shim as an attested Nitro enclave in front of your own lightwalletd or
Zaino. It forwards every gRPC call to your indexer unchanged; the one exception is
that it classifies `SendTransaction` (does it touch Orchard?) and logs the verdict.

**This is Phase 1: forward-only, so it adds no privacy yet.** It classifies and
logs but forwards everything. What it buys you is the integration, the TLS, and
the attestation, all in place. **Phase 2 (next): diversion** routes Orchard-touching
transactions to Shielded Labs' hub instead of your indexer, which is where privacy
begins. For you that is a redeploy, not a new integration.

## Why an attested enclave

So that once diversion lands, you (the operator) cannot see the migration traffic:
the shim runs in an AWS Nitro enclave you operate but cannot inspect. Trust comes
from two halves together, a reproducible build (source matches a published hash)
and a Nitro attestation (that hash is what runs); `caution verify` checks both.
Full rationale in `deploy/caution/README.md`.

## Prerequisites

- A Caution account and the `caution` CLI (FIDO2). Ask us to enable third-party access.
- Your indexer (lightwalletd or Zaino) reachable at a literal `IPv4:port` over TLS.
  If it serves plaintext gRPC (the default), front it with a gRPC-aware TLS
  terminator that proxies **h2c** to the backend. On Traefik v3 use an
  `IngressRoute` with `scheme: h2c`; the `serversscheme` annotation is silently
  ignored and every call 500s.
- A DNS name you control for wallets.
- A checkout of `github.com/ShieldedLabs/zero` at the commit you are auditing.

## Deploy

```bash
sh zeronym/shim/deploy/caution/assemble-caution.sh \
  --name        <enclave-name> \
  --backend     <indexer-ipv4>:<port> \
  --backend-tls <name-on-indexer-cert> \
  --tls-domain  <wallet-facing-domain>
```

- `--backend` is a literal IPv4 (the enclave never resolves DNS).
- `--backend-tls` is the name on your indexer's cert: dialed by IP, authenticated by name.
- `--tls-domain` is what wallets connect to; the in-enclave Caddy gets its Let's Encrypt cert for it.

Then, from the directory it creates:

```bash
caution login --username <name> --qr   # --qr uses phone-based FIDO2
caution apps create                    # creates the app and adds the git remote
git push caution main                  # builds and boots the enclave; prints its IP
```

Point your `--tls-domain` at that IP right after the push (so the cert can issue),
then point wallets at `<tls-domain>:443`. One app per enclave; redeploy later is
just `git push caution main`.

## Verify

```bash
# transparency: the reply matches your backend's own
grpcurl -import-path lightwalletd/walletrpc -proto service.proto \
  <tls-domain>:443 cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo

caution verify <app-id>                # attestation matches the pushed source
sh zeronym/shim/deploy/reproduce.sh    # source reproduces deploy/EXPECTED_SHA256
```

## Testnet

Fully supported, no changes: the classifier is a pure function of the transaction
bytes with no network parameters, so behavior is identical. Just point `--backend`
at a testnet indexer.

## Operating

- The enclave IP changes on each redeploy, so update your DNS each time.
- The enclave is diskless, so every restart is a fresh Let's Encrypt order (limit
  5/week per name); iterate with throwaway `--tls-domain`s.
- Boots but will not serve? Set `debug.enabled = true`, push, and read
  `/var/log/nitro_enclaves/*.log` over SSH. Debug disables attestation, so it is a
  diagnostic only, never the deployed config.

## Phase 1 caveat

Say it plainly to anyone relying on this: Phase 1 forwards **every** request,
including Orchard-touching `SendTransaction`s, to your indexer. Nothing is diverted
or hidden. Privacy begins with Phase 2.
