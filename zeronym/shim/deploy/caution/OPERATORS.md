# Running the zero-indexer-shim (operator guide)

Run the shim as an attested Nitro enclave in front of your own lightwalletd or
Zaino. It forwards every gRPC call to your indexer unchanged; the one exception is
that it classifies `SendTransaction` (does it touch Orchard?) and logs the verdict.
The two backends are equals: the shim routes purely on the request path
(`src/proxy.rs`), so it never cares which indexer answers. lightwalletd is what
Shielded Labs run themselves.

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

- **A Caution account.** Creation is gated on an access code:
  `caution register --alpha-code <CODE>`, code from Shielded Labs or
  `info@caution.co`. Registration needs a FIDO2 authenticator that supports
  discoverable credentials: a platform passkey or YubiKey 5 works, a Ledger does
  not (it deliberately disables resident keys, and the failure looks like "FIDO2
  is broken"). `--qr` prints a URL any browser can open, including on the same
  machine with Touch ID.
- **The `caution` CLI**:
  `git clone https://codeberg.org/caution/platform && cd platform && make install-cli`
  (macOS needs `CAUTION_ACCEPT_HOST_BUILD_RISK=1`). The reproducible StageX build
  of the CLI is Linux/x86_64 only, and that binary is what performs attestation
  verification, so an auditor should verify from a Linux/x86_64 box.
- **A push key**: `caution ssh-keys add --from-agent` (after login) authorizes
  the git pushes.
- **For BYOC, a paid Caution subscription.** Without one, `git push` returns
  HTTP 402 after your AWS stack has already been provisioned and is billing.
- **Your indexer** (lightwalletd or Zaino) reachable at a literal `IPv4:port`
  over TLS. If it serves plaintext gRPC (the default), front it with a TLS
  terminator that proxies **h2c** to the backend; nginx, Caddy, Envoy, and
  Traefik all work as long as h2c goes upstream. On Traefik v3 use an
  `IngressRoute` with `scheme: h2c`; the `serversscheme` annotation is silently
  ignored and every call 500s.
- **A DNS name you control** for wallets.
- A checkout of `github.com/ShieldedLabs/zero` at the commit you are auditing.

## Where the enclave runs

- **Fully managed**: in Caution's AWS account. `caution apps create` and push;
  nothing to provision.
- **BYOC**, your own AWS account:
  `AWS_PROFILE=<profile> caution init --byoc --region <region>` provisions the
  VPC, S3 bucket, instance and builder roles, launch template and ASG, and wires
  the `caution` git remote. Teardown is `caution teardown --byoc`.

Pass `--region` explicitly, chosen by measured latency to your indexer; the
silent default is `us-west-2`. Never hand-allocate an Elastic IP for the app: it
is invisible to `teardown --byoc` and then blocks VPC deletion.

## Deploy

Create an empty **public** git repository first (the assembled context gets
published there; verification depends on it), then:

```bash
sh zeronym/shim/deploy/caution/assemble-caution.sh \
  --name        <enclave-name> \
  --backend     <indexer-ipv4>:<port> \
  --backend-tls <name-on-indexer-cert> \
  --tls-domain  <wallet-facing-domain> \
  --app-source  <public-git-url>
```

- `--backend` is a literal IPv4 (the enclave never resolves DNS).
- `--backend-tls` is the name on your indexer's cert: dialed by IP, authenticated by name.
- `--tls-domain` is what wallets connect to; the in-enclave Caddy gets its Let's Encrypt cert for it.
- `--app-source` is recorded in the manifest so `caution verify` can rebuild what
  you deployed. Without it verification is impossible for anyone, including you.

Re-running the script is safe: it preserves `.caution/` and the git history, so
the directory stays bound to its app. Then, from the directory it creates:

```bash
caution login --username <name> --qr
caution apps create      # fully managed; BYOC already has the remote from `caution init --byoc`
git push caution main    # builds and boots the enclave; prints its IP
```

Publish the same commit to your public repo, on `main`, and tag it:

```bash
git remote add origin <public-git-url>
git push origin main && git tag deploy-1 && git push origin deploy-1
```

The manifest pins the branch AND the commit, so push `main` itself, and tag each
deployed commit: a branch tip moves and can be garbage-collected, the tag keeps
the manifest's commit reachable. Caution's own remote is push-only, so this
published repo is the only route an auditor has to the deployed tree.

**DNS**: create the `A` record for `--tls-domain` the moment the push prints the
IP. Measured: with DNS already correct when the enclave boots, the certificate
appears in ~30 seconds; created after boot, still nothing after 5 minutes. The
record must be **DNS-only**: a Cloudflare-proxied (orange cloud) record
terminates TLS at Cloudflare, which destroys the in-enclave-key property the
whole attestation argument rests on, and blocks the ACME TLS-ALPN-01 challenge
so no certificate ever issues. Both failures are silent.

Then point wallets at `<tls-domain>:443`. One app per enclave.

## Verify

```bash
# transparency: the reply matches your backend's own
grpcurl -import-path lightwalletd/walletrpc -proto service.proto \
  <tls-domain>:443 cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo

caution verify                         # from the assembled directory
sh zeronym/shim/deploy/reproduce.sh    # source reproduces deploy/EXPECTED_SHA256
```

No proto handy? A raw gRPC probe needs neither grpcurl nor a checkout:

```bash
printf '\x00\x00\x00\x00\x00' | curl -s --http2 \
  -H 'content-type: application/grpc' -H 'te: trailers' --data-binary @- \
  https://<tls-domain>/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo | strings
```

`caution verify` takes no app id: it infers the deployment from `.caution/`.
Anyone else verifies with no Caution account and no checkout:

```bash
caution verify --attestation-url https://<tls-domain>/attestation
```

Two caveats, as of Caution platform `8e31ea7`:

- Without `app_sources` in the manifest (the `--app-source` flag), verify
  refuses outright: "Cannot reproduce private code deployment".
- Verify currently reports FAILED on a perfectly healthy enclave: Caution's
  builder fetches its framework from a floating `main.tar.gz` rather than the
  pinned commit, so PCR0/1 stop reproducing whenever that branch moves. PCR2,
  the application layer, reproduces and is the check that matters today: a
  genuine, unmodified enclave whose application is the published source. The
  one-line fix is reported to Caution.

For `reproduce.sh`, the result that counts is a match on independent hardware:
two builds on one machine share CPU, kernel, and Docker. On an arm64 Mac it runs
under emulation; a third-party operator has matched the then-published hash on
native x86_64.

## Testnet

Fully supported, no changes: the classifier is a pure function of the transaction
bytes with no network parameters, so behavior is identical. Just point `--backend`
at a testnet indexer. A worked example, courtesy of zec.rocks:
`--backend 199.170.132.107:443 --backend-tls na-jfk.testnet.metal.zec.rocks`,
and `GetLightdInfo` through the shim answers `chainName: "test"`.

## Config reference

Every option is a CLI flag and an environment variable (prefix `ZIS_`). On
Caution you set these through `assemble-caution.sh`; the table is for
understanding what they mean.

| env var | meaning | you point it at |
|---|---|---|
| `ZIS_LISTEN` | wallet-facing listen address | `0.0.0.0:8083` inside the enclave (the port Caution's Caddy forwards to) |
| `ZIS_BACKEND` | backing indexer address, a literal `SocketAddr` | **your own indexer** |
| `ZIS_BACKEND_TLS` | DNS name to authenticate the backend cert as | the name on your indexer's cert |
| `ZIS_TLS_DOMAIN` | wallet-facing ACME domain | left **unset** on Caution; the in-enclave Caddy owns the cert |

`ZIS_BACKEND_TLS` does double duty: the name the backend's certificate must
present, and the request `:authority` your ingress routes on. Hence the one
confusing symptom: a bare `grpcurl <ip>:443` with no name fails with
"certificate signed by unknown authority", because the terminator serves its
default certificate to a client that sent no SNI. That is expected, not a
backend fault; the shim always sends the name.

When diversion is configured (`--hub`), the diverted path is baked into the
audited binary and the enclave's egress rules at assemble time, not a knob you
set at runtime: an operator cannot silently repoint it.

## Operating

- **The enclave IP is stable across successful redeploys.** Caution allocates an
  Elastic IP per app and re-associates it each deploy; it survives instance
  replacement. It is released only by teardown or by a failed deploy's rollback,
  so a changed IP means the previous deploy failed and rolled back.
- **Redeploy** = re-assemble, `git push caution main`: the preserved history
  makes the push a fast-forward. If it is refused (unrelated history, or the app
  in a failed state), fall back to the cycle: `echo y | caution apps destroy
  <app-id>`, `git remote remove caution`, `caution apps create`, push, repoint
  DNS (new app id AND new IP).
- **Certificates**: the enclave is diskless, so every restart is a fresh Let's
  Encrypt order, and every push spends one of the hostname's 5 weekly production
  issuances (there is no staging on this path). Iterate on throwaway
  `--tls-domain`s; strategy in `RESTARTS.md`.
- **Watch Certificate Transparency** (crt.sh) for your `--tls-domain`: as the
  domain's operator you are best placed to notice a certificate you cannot
  account for.
- **Session expiry**: when a command claims "No deployment found. Either run
  'caution init' first", re-run `caution login --qr` before believing it. The
  stated remedy would provision a second AWS stack.
- Boots but will not serve? Set `debug.enabled = true`, push, and read
  `/var/log/nitro_enclaves/*.log` over SSH. Debug disables attestation, so it is a
  diagnostic only, never the deployed config.

## Phase 1 caveat

Say it plainly to anyone relying on this: Phase 1 forwards **every** request,
including Orchard-touching `SendTransaction`s, to your indexer. Nothing is diverted
or hidden. Privacy begins with Phase 2.
