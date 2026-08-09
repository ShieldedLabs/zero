# zero-indexer-hub on Caution (attested Nitro enclave)

The hub receives diverted migrations in plaintext and broadcasts them to the
Zcash network. That is exactly why it runs as an attested enclave: the
attestation binds the running binary to `../EXPECTED_SHA256`, so an auditor who
reproduces the build knows the code holding the plaintext is the code they read.

Sibling of `zeronym/shim/deploy/caution/`. The shim's README covers the platform
mechanics (in-enclave TLS termination on 8083, the FIDO2 login, Let's Encrypt
staging-vs-production limits, debug-mode console); this covers only what differs
for the hub.

## What differs from the shim deploy

- **Egress is to a set of full nodes, not one indexer.** `--nodes` takes a
  comma-separated list of literal `IPv4:port`, and one `/32` egress block is
  emitted per node. Every submission is broadcast to every node.
- **Inbound is HTTP/1.1, not gRPC.** The hub's endpoint is a plain `POST` of raw
  transaction bytes, so the enclave config sets **no** `upstream_protocol`
  (Caddy's default HTTP/1.1 is correct; the shim needed `h2c` only because it is
  an HTTP/2-only gRPC server).
- **No DNS egress** (no port 53), same as the shim: nodes are dialled by literal
  IP, so a poisoned DNS answer has nothing to poison.
- **Optional node basic-auth** via `--node-user` / `--node-password`, applied to
  every node. A zebrad with `enable_cookie_auth = false` ignores it.

## Assemble and deploy

```sh
sh zeronym/hub/deploy/caution/assemble-caution.sh \
  --name zeronym-hub-1 \
  --nodes 203.0.113.10:8232,203.0.113.11:8232 \
  --tls-domain hub.example.org \
  [--node-user zcashrpc --node-password <secret>] \
  [--production] [--debug]
```

Then, from the assembled directory:

```sh
caution login --username <name> --qr
caution apps create --name zeronym-hub-1
caution init <app-id>
git remote add caution ssh://git@dashboard.caution.co:2222/<app-id>.git
git push caution main
```

Point the hub's DNS name at the enclave IP the deploy prints, and set the shim's
`ZIS_HUB` to that address and `ZIS_HUB_TLS` to `--tls-domain` so the shim
verifies the enclave's in-enclave certificate.

## Verify the attestation

`caution verify <app-id>` (or `POST /attestation`) returns the measurement bound
to the running EIF. Confirm it against a local reproduce:

```sh
git checkout <the PROVENANCE commit>
sh zeronym/hub/deploy/reproduce.sh   # must print the hash in ../EXPECTED_SHA256
```

## Cutover note (incremental mainnet)

Deploy the hub attested against our own mainnet zebrad first, then flip **our
own** shim (`zis-*`) to divert and send one real Orchard-touching transaction.
Confirm it broadcast through the hub and our indexer never saw it before pointing
any third-party shim at this hub.
