# zero-indexer-shim on Caution

Deploy the shim as a standalone attested Nitro enclave, in front of a backing
indexer.

> **Running your own indexer?** See [OPERATORS.md](OPERATORS.md) for the
> third-party operator runbook. This document is the design rationale for the
> enclave deploy plus Shielded Labs' own deployment notes.

## Why this deploy exists

`deploy/README.md` proves one half of the shim's trust argument: the published
binary hash `3e9e1cec…` is reproducible from source. That half, on its own,
proves nothing about what is *running*. An operator can publish an auditable
recipe and run something else.

This deploy is the other half. A Nitro attestation binds a measurement of the
loaded image into a signed document, so an auditor can check that the thing
answering their queries is built from the source they read. The two claims are
only worth something together:

| | proves | does not prove |
|---|---|---|
| reproducible build | source and published hash agree | that hash is what runs |
| attestation alone | *some* image runs in a real enclave | which source produced it |
| both | the code you read is the code serving you | |

The attestation half is now demonstrated: the shim has been deployed as an
attested enclave on Caution, `POST /attestation` returns a document binding the
loaded image, and `caution verify` rebuilds from source and compares.

## What this is not, yet

Two limits, stated plainly because a reader could otherwise assume this is a
finished privacy product. Neither is a bug; both are scope.

**The shim does not divert anything.** It classifies `SendTransaction` bodies,
logs the verdict, and forwards every request unchanged. There is no hub, no Nym
transport, no batching. What is deployed here is the *interception point* and its
classifier, proven correct and proven attested, which is the prerequisite for
diversion rather than a substitute for it.

**The backend is a literal IP.** `ZIS_BACKEND` parses as a `SocketAddr`, so a
hostname will not even parse. This costs flexibility and buys something: the
enclave never resolves DNS, its egress rule is a single `/32`, and it therefore
cannot be pointed at a third party by a poisoned DNS answer. When the backend
moves, both `ZIS_BACKEND` and the egress CIDR in `caution.hcl` must change
together. The backend cert is authenticated by name (`ZIS_BACKEND_TLS`) even
though the address is an IP.

Wallet-facing TLS is handled by Caution's in-enclave Caddy, which obtains a Let's
Encrypt certificate for the declared domain and terminates inside the enclave, so
the private key never leaves it. The shim's own rustls+ACME stack (`src/tls.rs`)
is the vendor-independent path and stays dormant on Caution.

## Deploy (Shielded Labs' own)

Assemble the deploy repository. This refuses a dirty `zeronym/shim` (it builds
from `git archive HEAD`) and rejects a non-literal-IPv4 backend. One enclave
fronts exactly one indexer, so each backend gets its own app.

```bash
sh zeronym/shim/deploy/caution/assemble-caution.sh \
  --name zeronym-shim-lwd \
  --backend 66.42.124.202:443 \
  --backend-tls lwd.shieldedinfra.net \
  --tls-domain zis-lwd.shieldedinfra.net
```

All four flags are required; add `--app-source <public-git-url>` so `caution
verify` can rebuild what was deployed (OPERATORS.md covers publishing the
assembled context). `66.42.124.202` is Shielded Labs' own load balancer
(Traefik, terminating TLS in front of lightwalletd); a third-party operator points
these at their own indexer instead (see [OPERATORS.md](OPERATORS.md)).

Then, from the assembled directory. The first command is needed more often than
you expect; the CLI session expires quietly and every other command then fails
with a confusing error:

```bash
caution login --username <name> --qr
caution apps create      # run from the assembled dir; adds the `caution` remote
git push caution main
```

`--qr` is not optional in practice: without it the CLI blocks on a local
authenticator and gives no hint that another flow exists (`--qr` prints a URL
any browser can open, including on the same machine). `caution apps create`
takes no `--name`; it reads the repo, assigns a generated name, and adds the
`caution` git remote for you, so there is no separate `git remote add` step.
Create a **new** app per shim; pushing into another app's repo replaces that
enclave. To redeploy: re-assemble and push again. The assembler preserves
`.caution/` and the git history, so the push fast-forwards onto the app the
directory is already bound to. If the push is refused (an unrelated history
from before the preservation fix, or an app stuck in a failed state), fall back
to the destroy cycle: destroy the app, `git remote remove caution`, `apps
create` a fresh one, push, repoint DNS. `apps destroy` prompts; pipe `echo y`
when scripting.

The enclave IP is stable across successful redeploys: Caution allocates an
Elastic IP per app and re-associates it on each deploy, and it survives
instance replacement. It is released only by teardown or by a failed deploy's
rollback, so a changed IP means the previous deploy failed and rolled back.
When the IP does change, on Shielded Labs' infra the enclave's egress IP must
be re-allowed on the backend's ingress `ipAllowList` (in shielded-infra) and
its DNS record updated together; a stale allowlist entry fails closed, and the
symptom is the shim hanging on every upstream dial with no console to see why.

## Verify

Point a lightwallet client at the wallet-facing domain on `:443` (TLS). The shim
is meant to be indistinguishable from the indexer behind it, so the check is that
a normal query returns a normal answer:

```bash
grpcurl -import-path lightwalletd/walletrpc -proto service.proto \
  <tls-domain>:443 cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo
```

The reply should be byte-identical to querying the backend directly. If it is not,
the shim is not transparent and that is a bug in the shim, not in the deployment.

Then the part that makes it more than a proxy:

```bash
caution verify        # from the assembled directory; it infers the app
```

This takes a nonce, fetches a fresh attestation, rebuilds the image from the
published `app_sources` repo, and compares measurements. It is what turns "they
say this is the code" into something checkable. Third parties can run
`caution verify --attestation-url https://<domain>/attestation` with no Caution
account and no checkout. Expect a clean `PASSED` with all three PCRs
reproducing: the Caution-side unpinned-framework bug that once made verify
report PCR0/1 FAILED on healthy enclaves is fixed. Require all three. See
OPERATORS.md for why a PCR2 match alone proves nothing about which application
is running.

## When it boots but does not serve

Every previous enclave failure here presented identically: TCP accepts, nothing
answers. It has never once been diagnosable from the outside, because the Caution
CLI has no logs or console command.

The fix is always the same. In `caution.hcl` set `debug.enabled = true` (the SSH
key is already listed), push, then:

```bash
ssh ec2-user@<enclave-ip>
```

and read `/var/log/nitro_enclaves/enclave-console.log`, which holds the enclave's
stdout. Note that debug mode disables attestation, so this tells you why it is
broken but cannot itself be the deployed configuration.

Known causes, all previously hit and all worth checking first:

- `unit.command` naming a path that does not exist in the image. The enclave
  panics with nothing useful on the outside.
- Passing a binary an environment variable inside its own config namespace.
  `ZEBRA_CONF` was read by zebrad as an unknown config field, which killed PID 1
  and put the enclave in a reboot loop. `ZIS_LISTEN` and `ZIS_BACKEND` are safe
  precisely because the shim defines them.
- The stagex busybox base leaving `/lib` and `/lib64` as dangling symlinks, which
  breaks Caution's EIF assembly. The runtime stage already materialises
  `/usr/lib` to prevent it.
