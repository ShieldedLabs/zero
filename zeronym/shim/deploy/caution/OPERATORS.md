# Running the zero-indexer-shim (operator guide)

Run the shim as an attested Nitro enclave in front of your own lightwalletd or
Zaino. It forwards every gRPC call to your indexer unchanged; the one exception is
that it classifies `SendTransaction` (does it touch Orchard?) and logs the verdict.
The two backends are equals: the shim routes purely on the request path
(`src/proxy.rs`), so it never cares which indexer answers. lightwalletd is what
Shielded Labs run themselves.

**Phase 1 is forward-only, so it adds no privacy yet.** It classifies and logs
but forwards everything. What it buys you is the integration, the TLS, and the
attestation, all in place. **Phase 2, diversion, works and is what you should
deploy**: pass `--hub-nym` and Orchard-touching transactions go to Shielded Labs'
hub over the **Nym mixnet** instead of to your indexer, which is where privacy
begins. For you that is a redeploy, not a new integration. See "Diversion" below
for what is proven and what is not.

> **If you have read an older copy of this guide:** `--hub`/`--hub-tls` (the
> transitional clearnet hop) is **legacy and no longer works against the current
> hub**, which refuses clearnet submissions by default. Use `--hub-nym`. An
> otherwise healthy shim built with `--hub` will fail every divert closed.

> **Read "What this does and does not hide" before you tell anyone what this
> gives them.** At today's volumes the anonymity set is one transaction. Content
> privacy and IP unlinking are real; batching anonymity is not yet.

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

**DNS — ordering is load-bearing.** On fully-managed Caution the record is a
**CNAME to `<app-id>.apps.caution.sh`**, not an A record, and the app id does not
exist until `caution apps create` has run. So the sequence is:

```
caution apps create      # prints the app id
  -> create the CNAME:   <tls-domain>  CNAME  <app-id>.apps.caution.sh
git push caution main    # boots the enclave AND orders the certificate
```

Create the record **before** the push: the push is what orders the certificate,
and ACME can only validate a name that already resolves. Every push spends one of
that hostname's **5 weekly production issuances** (there is no staging on this
path), so a push into missing DNS burns one. `zeronym/deploy.sh` automates this
in the right order.

Expect the first ACME attempt to fail with `NXDOMAIN` and **recover on its own in
about a minute**: Caution publishes the A record on `…apps.caution.sh` only after
the health check passes, so the chain cannot resolve when Caddy first tries.
Measured on the attested hub: deploy completed 15:03:33Z, TLS still failing at
15:04, valid production Let's Encrypt certificate serving by 15:05. Only escalate
if it is still failing after ~5 minutes.

The record must be **DNS-only**: a Cloudflare-proxied (orange cloud) record
terminates TLS at Cloudflare, which destroys the in-enclave-key property the
whole attestation argument rests on, and blocks the ACME challenge so no
certificate ever issues. Both failures are silent.

**Choose the wallet-facing name carefully.** It must be a name your *indexer's*
operator holds no certificate for. A shim served on a hostname they can obtain a
certificate for can be transparently impersonated by them, which defeats the
guarantee for exactly the naive-TLS wallets it protects.

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

**On fully-managed apps, pass the attestation URL explicitly.** `caution apps
create` does not write a `.caution/deployment.json` (that is `caution init`, the
BYOC path), so verify has nothing to infer from and fails with *"No deployment
found. Either run 'caution init' first or provide --url"*. That message is
misleading here — do **not** run `caution init`, which would provision a second
AWS stack. Just name the endpoint:

```bash
caution verify --attestation-url https://<tls-domain>/attestation
```

The same command is what anyone else uses, with no Caution account and no
checkout of yours.

Caveats:

- Without `app_sources` in the manifest (the `--app-source` flag), verify cannot
  reproduce the deployment, and the attestation then proves only that *some*
  image runs in a genuine enclave.
- Older copies of this guide warned that verify reports PCR0/1 FAILED on a
  healthy enclave, because Caution's builder fetched its framework from a
  floating `main.tar.gz`. **That is fixed**: on the attested pair deployed
  2026-08-14 the manifest pinned both the enclave and framework sources to
  commits, and **all three PCRs reproduced** on both, with the TLS certificate
  binding verified. Expect a clean `✓ Attestation verification PASSED`.
- **Do not fall back to "PCR2 is the one that matters".** That advice circulated
  while PCR0/1 were failing, and it is wrong on this platform. Measured
  2026-08-14 across the attested shim and hub — two entirely different binaries:

  | | shim | hub |
  |---|---|---|
  | PCR0 | `accb679a…` | `218d1f64…` |
  | PCR1 | `accb679a…` | `218d1f64…` |
  | PCR2 | `21b9efbc…` | `21b9efbc…` **(identical)** |

  **PCR2 does not distinguish the application.** PCR0/PCR1 are what change with
  it. So an attestation accepted on a PCR2 match alone would prove only that
  *some* Caution enclave is running, not that it is running your reviewed code —
  which is the entire claim. Require **all three** to reproduce, and treat a
  PCR0/1 mismatch as a real finding about the application until proven otherwise.
  (The observation is empirical; we have not confirmed with Caution which layer
  each index measures on their EIF layout.)

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

## Diversion (Phase 2) — over the Nym mixnet

The hub is live and diversion works end to end **over Nym**. Add to
`assemble-caution.sh`:

```
  --hub-nym    <hub-nym-address> \
  --nym-egress 92.39.63.14/32:443:tcp \
  --nym-egress 0.0.0.0/0:9000:tcp \
  --nym-egress 1.1.1.1/32:53:udp
```

Shielded Labs supply the hub address; it is also readable from the hub itself at
`https://<hub-domain>/nym-address`, which is the authoritative copy (see
"Operating" for why it can change). The `--nym-egress` rules are the enclave's
entire allowlist for reaching the mixnet: the nym-api, a gateway, and a DNS
resolver. Forward-only stays the default: no `--hub-nym`, no diversion.

`--hub`/`--hub-tls` is the **legacy** clearnet hop. The current hub refuses
clearnet submissions by default, so do not use it.

**Do NOT use `--nym-gateway` yet.** The flag pins the entry gateway (and would let
you narrow the `0.0.0.0/0:9000` rule to a `/32`), but it has not been exercised
against the public Nym network. It takes the gateway's IDENTITY key while the
egress rule takes its IP ADDRESS, and a mismatch fails closed with no console on
an attested enclave.

What changes for you: an Orchard-touching `SendTransaction` never reaches your
indexer (proven in CI by a connection-counting backend, `tests/divert.rs`), and
the wallet gets the usual lightwalletd reply, `errorCode 0` with the txid in
`errorMessage`. Every `GetTransaction` is served by the hub too, since a shim
that held no state could not tell a migration's txid from any other. If the hub
is unreachable the shim answers gRPC `UNAVAILABLE` and still never falls back to
your indexer: it fails closed, by design.

Proven on mainnet 2026-08-11 over the clearnet hop: a real Orchard-to-Ironwood
migration from an unmodified wallet was held on submission and published on the
hub's 20-block cadence. Proven again over **Nym** on 2026-08-13: a migration went
wallet → shim → mixnet → hub → broadcast, and since the hub's clearnet submit
path is now closed, its arrival is proof the mixnet carried it rather than
inference. **Batch size was one in both**, so these runs prove the mechanics and
content privacy, not batching anonymity.

### What this does and does not hide

State this plainly to anyone relying on a shim you run. It is the claim the whole
system stands on, and overclaiming is worse than the limit itself:

> Given a verified attestation, the code holding your migration in plaintext is
> the published code: it broadcasts the transaction and retains nothing
> identifying. It does **not** hide from the hub's host that a migration was
> submitted at a given time, and at current volumes that timing is linkable to
> the resulting on-chain transaction. **The anonymity set is the batch, and the
> batch is currently size one.** For an Orchard **deshield**, the amount remains
> recoverable by the operator through address-level queries the shim does not
> intercept.

So: content privacy and IP unlinking are real today. **Batching anonymity is
not** — it only becomes true above an adoption threshold, because the anonymity
set is the cross-operator batch and is worth exactly as many operators as are
running diversion.

### What it costs to run

The shim holds a **persistent, shaped Nym client**. It emits cover traffic
continuously — **order of gigabytes per day, per shim** — whether or not anyone
diverts anything. That is not waste: uniform traffic is what hides *that* a
divert happened. The knobs that would reduce it
(`disable_loop_cover_traffic_stream`, `--no-cover`, `--fastmode`) are
**forbidden**, because each one forfeits the shaping.

Budget for it, and know that Nym's free tier meters **volume** (currently
250 GB / 30 days). See "Known failure modes" for what happens when it runs out.

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
| `ZIS_HUB_NYM` | hub Nym address(es) to divert to — a comma-separated LIST | the address from `https://<hub-domain>/nym-address` |
| `ZIS_NYM_GATEWAY` | pin the entry gateway by IDENTITY key (repeatable; rotates across rebuilds) | **leave unset for now** — untested against public Nym |
| `ZIS_NYM_ROTATION_SECS` | rotate the shim's mixnet identity every N seconds, bounding how long the hub can link your submissions | a deployment decision; unset = never |
| `ZIS_CAUTION_ATTESTATION` | let the shim answer Caution's own `/attestation` and health paths | **`true` on managed Caution** (the default). Under h2c the platform routes these to the app, and a shim that proxied them to your indexer would fail its health check and never boot |
| `ZIS_LOOKUP_TIMEOUT_SECS` | how long a `GetTransaction` waits for the hub before failing closed | default **90 s** (raised from 25 s against measurement). Tuning it changes the enclave config, not the binary, so your `EXPECTED_SHA256` and reproducibility trail stay put. It **multiplies** by the number of `--hub-nym` addresses |
| `ZIS_HUB` / `ZIS_HUB_TLS` | **legacy** clearnet hop | unset — the current hub refuses clearnet submissions |

`ZIS_BACKEND_TLS` does double duty: the name the backend's certificate must
present, and the request `:authority` your ingress routes on. Hence the one
confusing symptom: a bare `grpcurl <ip>:443` with no name fails with
"certificate signed by unknown authority", because the terminator serves its
default certificate to a client that sent no SNI. That is expected, not a
backend fault; the shim always sends the name.

When diversion is configured (`--hub-nym`), the diverted path is baked into the
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
- **Session expiry**: FIDO2 sessions expire often, and the resulting errors point
  the wrong way. "No deployment found. Either run 'caution init' first" has two
  causes — an expired session, and a fully-managed app (which never writes
  `.caution/deployment.json`). Re-run `caution login --qr --username <name>`, and
  for verify pass `--attestation-url`. **Never** run the suggested `caution init`:
  it would provision a second AWS stack.
- **Reading state from an attested enclave**: there is no SSH. Use
  `https://<tls-domain>/attestation` and, on the hub, `/healthz` and
  `/nym-address`. Deliberately, no endpoint exposes queue depth or counts —
  that would be an oracle for the anonymity-set size.
- Boots but will not serve? Set `debug.enabled = true`, push, and read
  `/var/log/nitro_enclaves/*.log` over SSH. Debug **disables attestation**, so it
  is a diagnostic only, never the deployed config — and note the SSH key is
  whatever you passed to `--ssh-key` at assemble time, so the person who deploys
  is the one who can read the console.

## Known failure modes

All three of these fail **quietly**. None throws an error you would see without
looking, which is what makes them worth reading before you deploy.

**1. The hub's Nym address changes, and your shim is baked to the old one.**
The hub's identity lives in RAM (an enclave is diskless), so a hub **process
restart** mints a new address. Your shim carries the old one in its immutable
enclave config, so every divert then fails closed until you re-assemble and
redeploy against the new address. A client *reconnect* is fine — the address
survives that — but a restart is not.

- **Detect:** poll `https://<hub-domain>/nym-address` and alert on any change.
- **Recover:** re-assemble with the new `--hub-nym`, redeploy, repoint DNS if the
  app id changed.

**2. A hub restart drops migrations your wallets were already told succeeded.**
The shim answers `SendTransaction` as soon as the migration is **dispatched onto
the mixnet** — it does not wait for the hub's acknowledgement, because that is a
full mixnet round trip (10s at best, minutes under load) and would stall wallets.
The hub then holds submissions in a RAM-only queue until its next flush. If it
restarts in that window, those migrations are gone and **no error ever reached
the wallet**.

This is deliberate, not a bug: persisting the queue would mean writing plaintext
migrations to disk, which is the one thing the hub exists to prevent. A
`SendTransaction` success has never promised block inclusion anyway — an ordinary
send only reaches a mempool. But say it plainly to your users: **the wallet must
resend if the transaction never confirms.** Resends are safe; the hub deduplicates
on the payload hash.

**3. Mixnet bandwidth runs out, and diversion stops.** Nym's free tier meters
volume, and a shim's continuous cover traffic consumes gigabytes per day. When the
allowance is exhausted the mixnet client stops working and every divert fails
closed. There is **no ticketbook (paid credential) mechanism wired up yet**, so
today the recovery is manual.

- **Detect: poll `GET /nym-status`.** Nothing else works, and it is worth
  understanding why. The shim's reachability proves nothing (the clearnet proxy
  answers normally while the mixnet hop is dead); the wallet's reply proves
  nothing (the shim answers success as soon as a migration reaches its internal
  transport); and an end-to-end divert test proves nothing either, because the
  `GetTransaction` that would confirm arrival is itself a mixnet round trip.

  ```
  curl https://<tls-domain>/nym-status
  {"diversion_configured":true,"mixnet_connected":true,"client_deaths":0,"consecutive_rebuild_failures":0}
  ```

  | field | alert when |
  |---|---|
  | `diversion_configured` | `false` on a shim you built with `--hub-nym` — it is forward-only and hiding nothing |
  | `mixnet_connected` | `false` — **migrations are being silently dropped right now** |
  | `client_deaths` | climbing steadily: gateway churn |
  | `consecutive_rebuild_failures` | non-zero and growing: it is down and not recovering |

  `GET /healthz` is process liveness. Neither endpoint exposes send counts,
  timestamps, or txids — a "last diverted at" field would tell any passer-by
  exactly when a migration went out, which is the timing correlation this system
  exists to prevent.

**Related: `GetTransaction` for a just-diverted migration is slow, and may not
answer at all.** The mixnet is slow — a migration can take minutes to reach the
hub, which is fine, because a migration is time-insensitive by design. The lookup
path, though, *waits* for a full hub round trip: measured 2026-08-14 it did not
complete once in 14 attempts across two independently deployed pairs, against
what was then a 25-second budget. The budget is now **90 s**
(`ZIS_LOOKUP_TIMEOUT_SECS`), sized so the ~12.6 s of pure packet emission at the
throttled send rate has real headroom for queueing. If it still times out the
wallet is told `UNAVAILABLE` — failing closed, never falling back to your indexer
— and sees its transaction once it is mined and ordinary sync picks it up.

Worth knowing for diagnosis: **the timing tells you which failure you have.** A
lookup that fails after the full ~25 s means the shim sent and got no reply (the
mixnet). A lookup that fails *instantly* means the shim had no transport to send
on at all (a configuration or client-lifecycle problem). A healthy pass-through
call like `GetLightdInfo` answers in about 2 s and is your control.

## Forward-only caveat

Say it plainly to anyone relying on a shim deployed without `--hub`: it forwards
**every** request, including Orchard-touching `SendTransaction`s, to your
indexer. Nothing is diverted or hidden. Privacy begins when diversion is on.
