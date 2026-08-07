# Running the zero-indexer-shim: operator guide

For an indexer operator who wants to run the `zero-indexer-shim` in front of
their own lightwalletd or Zaino. It walks the whole path: prerequisites, deploy,
verify, operate.

## Read this first: where this is in the roadmap

The shim ships in two phases, and this guide is **Phase 1**.

- **Phase 1 (this guide): forward-only.** The shim sits in front of your indexer
  and is transparent: it forwards every `CompactTxStreamer` request to your
  indexer unchanged. It decodes `SendTransaction`, classifies whether the
  transaction touches Orchard, and **logs** that, but it still forwards it. So
  **Phase 1 adds no privacy.** What it does buy you: your integration, your
  wallet-facing TLS, and the enclave attestation are all in place, and the code
  running is provably the code an auditor reviewed.
- **Phase 2 (next): diversion.** Orchard-touching transactions are routed to
  Shielded Labs' attested hub instead of your indexer, so your indexer stops
  seeing migration content. For you that is a **redeploy** of the same enclave
  with a hub endpoint added, not a new integration.

If forward-only-with-no-privacy-yet is not worth your time, that is a reasonable
call: the value of doing Phase 1 now is that Phase 2 becomes a one-line redeploy.

## Why an attested enclave (the trust model)

In Phase 2 the shim's whole job is that **you, the operator, cannot see the
migration traffic**. If you ran the shim as an ordinary process on your box you
could read that traffic, which would defeat the point. So the shim runs inside an
**AWS Nitro enclave** (via Caution): the host you control cannot read enclave
memory, the wallet-facing TLS key is generated *inside* the enclave, egress is
locked to your indexer's address, and an attested build has no console.

That is only trustworthy if two things hold together:

| | proves | does not prove |
|---|---|---|
| reproducible build | source and the published hash agree | that hash is what runs |
| attestation alone | *some* image runs in a real enclave | which source produced it |
| both | the code you read is the code serving wallets | |

Auditors close the loop with `caution verify` (rebuild from source, compare to a
fresh attestation) and Certificate Transparency (a cert for your name that isn't
accounted for is a red flag). This path is not theoretical: Shielded Labs has
deployed and attested a shim exactly this way.

## What you need

- A **Caution account** and the `caution` CLI, with FIDO2 login. Third-party
  access may need to be arranged with us first, so ask before you start.
- **Your own mainnet indexer** (lightwalletd or Zaino), reachable from the
  internet at a literal `IPv4:port` and presenting a **TLS certificate**. If your
  indexer serves plaintext gRPC today (the default), see "TLS in front of your
  indexer" below.
- A **DNS name you control** for wallets to connect to (for example
  `lwd.yourdomain.net`).
- A checkout of `github.com/ShieldedLabs/zero` at the shim commit you are
  auditing (the one whose `zeronym/shim/deploy/EXPECTED_SHA256` you will match).

## Deploy

1. **Get the source and pin it.**

   ```bash
   git clone https://github.com/ShieldedLabs/zero && cd zero
   git checkout <commit>          # the commit this guide ships in
   ```

2. **Assemble a deploy repo for your enclave.** One enclave fronts exactly one
   indexer.

   ```bash
   sh zeronym/shim/deploy/caution/assemble-caution.sh \
     --name       <your-enclave-name> \
     --backend    <your-indexer-ipv4>:<port> \
     --backend-tls <name-on-your-indexer-cert> \
     --tls-domain <your-wallet-facing-domain>
   ```

   - `--backend` is a **literal IPv4 and port**, never a hostname: the enclave
     never resolves DNS (its egress is a single `/32` with no port 53), so a
     poisoned DNS answer has nothing to poison.
   - `--backend-tls` is the DNS name on your indexer's certificate. The enclave
     dials the IP above but authenticates the connection against this name.
   - `--tls-domain` is what wallets connect to. Caution's in-enclave Caddy obtains
     a Let's Encrypt certificate for it and terminates wallet TLS inside the
     enclave.

   This writes a sibling directory with the build context, a rendered
   `caution.hcl`, the commit-pinned `Containerfile`, and a `PROVENANCE` file.

3. **Create the app and push** (from the assembled directory):

   ```bash
   caution login --username <name> --qr     # --qr matters: without it the CLI
                                            # blocks on a local authenticator
   caution apps create                      # reads the repo, creates the app,
                                            # and adds the `caution` git remote
   git push caution main                    # builds the enclave image and boots it
   ```

   `caution apps create` takes no name and adds the remote for you, so there is no
   separate `init` or `git remote add` step. Create a **new** app per enclave;
   pushing into another app's repo replaces that enclave. The push prints the
   enclave's public IP on success.

4. **Point DNS at the enclave.** Create an `A` record for your `--tls-domain`
   pointing at the enclave IP the deploy printed. Do this right after the push:
   the in-enclave Caddy cannot obtain its certificate until the name resolves, and
   it retries with backoff, so the certificate usually appears within a minute or
   two once DNS is live.

5. **Point wallets at the shim.** Your `--tls-domain` on port 443 now routes
   through the shim to your indexer. Repoint your users there.

## Verify

- **Transparency** (the shim is indistinguishable from your indexer): a normal
  query returns your indexer's own answer.

  ```bash
  grpcurl -import-path lightwalletd/walletrpc -proto service.proto \
    <your-tls-domain>:443 cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo
  ```

  The reply should match querying your backend directly. If it does not, that is a
  bug in the shim, not in your deployment. (No proto handy? A raw gRPC probe works
  too: `printf '\x00\x00\x00\x00\x00' | curl -s --http2 -H 'content-type:
  application/grpc' -H 'te: trailers' --data-binary @-
  https://<your-tls-domain>/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo
  | strings` should print your indexer's vendor and version.)

- **Attestation** (the running enclave is the audited source):

  ```bash
  caution verify <app-id>
  ```

  It takes a nonce, fetches a fresh attestation, rebuilds the image from the
  pushed source, and compares measurements.

- **Reproducibility** (the source-to-binary half attestation relies on):

  ```bash
  sh zeronym/shim/deploy/reproduce.sh
  ```

  Two cold builds that agree with each other and with `deploy/EXPECTED_SHA256`.

## TLS in front of your indexer (if it serves plaintext gRPC)

lightwalletd and Zaino serve plaintext h2c by default, but the enclave
authenticates your indexer by TLS name (`--backend-tls`). So terminate TLS in
front of your indexer with a reverse proxy that forwards gRPC to the backend.

One gRPC gotcha to save you hours: the proxy must speak **HTTP/2 (h2c)** to the
backend, not HTTP/1.1, or every gRPC call returns a bare `500`. On Traefik v3 in
particular, the `traefik.ingress.kubernetes.io/service.serversscheme: h2c`
annotation on a Kubernetes Ingress is silently ignored; use an `IngressRoute` with
an explicit `scheme: h2c` on the service. Any gRPC-aware TLS terminator (nginx,
Caddy, Envoy) works as long as it proxies h2c upstream.

## Operating it

- **Certificates.** The enclave is diskless, so every restart is a fresh Let's
  Encrypt order. Let's Encrypt limits duplicate certificates to **5 per week** per
  name, so while you are iterating use a throwaway `--tls-domain` (for example
  `-test-1`, `-test-2`), and switch to your real name once it is green.
- **Redeploy** is just `git push caution main` against the same app. The enclave
  IP usually changes on redeploy, so update your DNS `A` record each time.
- **Boots but does not serve?** Set `debug.enabled = true` in `caution.hcl`, push,
  and read `/var/log/nitro_enclaves/*.log` over SSH. Debug mode **disables
  attestation**, so it is a diagnostic only, never the deployed configuration.

## Config reference

Every option is a CLI flag and an environment variable (prefix `ZIS_`). On Caution
you set these through `assemble-caution.sh`; the table is for understanding what
they mean.

| env var | meaning | you point it at |
|---|---|---|
| `ZIS_LISTEN` | wallet-facing listen address | `0.0.0.0:8083` inside the enclave (the port Caution's Caddy forwards to) |
| `ZIS_BACKEND` | backing indexer address, a literal `SocketAddr` | **your own indexer** |
| `ZIS_BACKEND_TLS` | DNS name to authenticate the backend cert as | the name on your indexer's cert |
| `ZIS_TLS_DOMAIN` | wallet-facing ACME domain | left **unset** on Caution; the in-enclave Caddy owns the cert |

There is no hub or divert option yet: that arrives with Phase 2, and when it does
the diverted path is baked into the audited enclave binary, not a knob you set.

## What Phase 1 does not give you

Say this plainly to anyone relying on it: in Phase 1 **every** request, including
Orchard-touching `SendTransaction`s, is forwarded to your indexer. The classifier
only logs. No transaction is diverted, hidden, or held. Privacy begins in Phase 2.
