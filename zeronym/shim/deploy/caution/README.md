# zero-indexer-shim on Caution

Deploy the shim as a standalone attested Nitro enclave, in front of a backing
indexer.

## Why this deploy exists

`deploy/README.md` proves one half of the shim's trust argument: the published
binary hash `4143ce5f…` is reproducible from source, confirmed on two machines
of different architectures. That half, on its own, proves nothing about what is
*running*. An operator can publish an auditable recipe and run something else.

This deploy is the other half. A Nitro attestation binds a measurement of the
loaded image into a signed document, so an auditor can check that the thing
answering their queries is built from the source they read. The two claims are
only worth something together:

| | proves | does not prove |
|---|---|---|
| reproducible build | source and published hash agree | that hash is what runs |
| attestation alone | *some* image runs in a real enclave | which source produced it |
| both | the code you read is the code serving you | |

Until this deploy exists the attestation half has never been demonstrated for
the shim, which is why `deploy/README.md` lists "the enclave half of the chain"
as the one thing still untested.

## What this is not, yet

Three limits, stated plainly because a reader could otherwise assume this is a
finished privacy product. None is a bug; all are scope.

**The shim does not divert anything.** It classifies `SendTransaction` bodies,
logs the verdict, and forwards every request unchanged. There is no hub, no Nym
transport, no batching. What is deployed here is the *interception point* and
its classifier, proven correct and proven attested, which is the prerequisite
for diversion rather than a substitute for it.

**Both hops are plaintext h2c.** The shim has no TLS in its dependency graph at
all. Inside the intended topology this is not a gap, because the shim sits on
the operator's own machine beside their indexer and the hop is loopback. Here it
is stretched across the public internet to a separate enclave, so both the
wallet-to-shim and shim-to-backend links are visible to a network observer. That
is acceptable for demonstrating attestation and transparency, and it is not
acceptable for real wallet traffic. Client-facing TLS is the next piece of shim
work.

**The backend is a literal IP.** `ZIS_BACKEND` parses as a `SocketAddr`, so a
hostname will not even parse. This costs flexibility and buys something: the
enclave never resolves DNS, its egress rule is a single `/32`, and it therefore
cannot be pointed at a third party by a poisoned DNS answer. When the backend
moves, both `ZIS_BACKEND` and the egress CIDR in `caution.hcl` must change
together.

## Deploy

Assemble the deploy repository. This refuses to run against a dirty
`zeronym/shim`, because it builds from `git archive HEAD` and would otherwise
deploy something other than what you are looking at:

One enclave fronts exactly one indexer, so each backend gets its own app. Both
arguments are required rather than defaulted: a wrong backend yields an enclave
that boots, serves, and quietly proxies for something nobody intended, which is
worse than one that refuses to start.

```bash
sh zeronym/shim/deploy/caution/assemble-caution.sh \
  --name zeronym-shim-zaino --backend 66.42.124.202:8137
sh zeronym/shim/deploy/caution/assemble-caution.sh \
  --name zeronym-shim-lwd --backend 66.42.124.202:9067
```

`66.42.124.202` is the cluster load balancer; the port selects which indexer.
`--backend` must be a literal IPv4 address and port, and the script rejects
anything else, because `ZIS_BACKEND` parses as a `SocketAddr` and a hostname
would not fail at assembly time but inside an enclave with no console.

Each writes `../<name>/`: the build context, a rendered `caution.hcl`, a root
`Containerfile` copied out of the commit-pinned context, and a `PROVENANCE` file
recording the source commit, backend and expected binary hash. `caution.hcl` is
rendered from `caution.hcl.tmpl` rather than hand-copied, so the egress `/32`
and `ZIS_BACKEND` cannot drift apart; when they do, every dial fails and it
looks like a shim bug rather than a firewall one.

Then, from that directory. The first command is needed more often than you
expect; the CLI session expires quietly and every other command then fails with
a confusing error:

```bash
caution login --username <name> --qr
```

`--qr` is not optional in practice. Without it the CLI blocks on a local
authenticator and gives no hint that a phone-based FIDO2 flow exists.

```bash
caution apps create --name <name>
caution init <app-id>
git remote add caution ssh://git@dashboard.caution.co:2222/<app-id>.git
git push caution main
```

`caution apps create` takes no `--name`; it reads the repo it is run in and
assigns a generated name, so run it from inside the assembled directory. It adds
the `caution` git remote for you.

Create a **new** app per shim rather than reusing an existing one. They are one
app per enclave, so pushing into another app's repo replaces that enclave.

### Currently deployed

| shim | backend | app | enclave IP |
|---|---|---|---|
| `zeronym-shim-zaino` | `66.42.124.202:443` (zaino, TLS) | `00ee815c-9d61-4349-9792-298a4581524c` | `15.164.71.196` |
| `zeronym-shim-lwd` | `66.42.124.202:443` (lightwalletd, TLS) | (held for h2c fix) | (not deployed) |

Enclave IPs change on most redeploys, and they appear in the cluster's
`MiddlewareTCP` allowlist (`shim-plaintext-routes.yaml` in shielded-infra). A
stale entry there fails closed, and the symptom is the shim hanging on every
upstream dial with no way to see why, because attested mode has no console.
Update both together.

Redeploying later is just `git push caution main` against the same app; no
re-init.

## Verify

Point any lightwallet client at `<enclave-ip>:8137`. The shim is meant to be
indistinguishable from the indexer behind it, so the check is that a normal
query returns a normal answer:

```bash
grpcurl -plaintext -import-path lightwalletd/walletrpc -proto service.proto \
  <enclave-ip>:8137 cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo
```

The reply should be byte-identical to querying the backend directly. If it is
not, the shim is not transparent and that is a bug in the shim, not in the
deployment.

Then the part that makes it more than a proxy:

```bash
caution verify <app-id>
```

This takes a nonce, fetches a fresh attestation, rebuilds the image from the
pushed source, and compares measurements. It is the capstone: it is what turns
"they say this is the code" into something checkable.

## When it boots but does not serve

Every previous enclave failure here presented identically: TCP accepts, nothing
answers. It has never once been diagnosable from the outside, because the
Caution CLI has no logs or console command.

The fix is always the same. In `caution.hcl` set `debug.enabled = true` (the SSH
key is already listed), push, then:

```bash
ssh ec2-user@<enclave-ip>
```

and read `/var/log/nitro_enclaves/enclave-console.log`, which holds the
enclave's stdout. Note that debug mode disables attestation, so this tells you
why it is broken but cannot itself be the deployed configuration.

Known causes, all previously hit on the zebra+zaino enclave and all worth
checking first:

- `unit.command` naming a path that does not exist in the image. The enclave
  panics with nothing useful on the outside.
- Passing a binary an environment variable inside its own config namespace.
  `ZEBRA_CONF` was read by zebrad as an unknown config field, which killed PID 1
  and put the enclave in a reboot loop. `ZIS_LISTEN` and `ZIS_BACKEND` are safe
  precisely because the shim defines them.
- The stagex busybox base leaving `/lib` and `/lib64` as dangling symlinks,
  which breaks Caution's EIF assembly. The runtime stage already materialises
  `/usr/lib` to prevent it.
