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

- **Egress is to a set of indexer endpoints.** `--indexers` takes a
  comma-separated list of literal `IPv4:port`, and one `/32` egress block is
  emitted per endpoint. Every batch member is broadcast to every endpoint.
- **`--indexer-tls` is required**, naming the certificate each endpoint must
  present. Without TLS the enclave's parent host reads every batch in the clear
  moments before it is public, which removes most of the reason to run the hub in
  an enclave.
- **Inbound is HTTP/1.1, not gRPC**, even though the hub's OUTBOUND hop is gRPC.
  The shim submits a plain `POST` of raw transaction bytes, so the enclave config
  sets **no** `upstream_protocol` (Caddy's default HTTP/1.1 is correct; the shim
  needed `h2c` only because it is an HTTP/2-only gRPC server).
- **No DNS egress** (no port 53), same as the shim: endpoints are dialled by
  literal IP while the certificate is verified against `--indexer-tls`, so a
  poisoned DNS answer has nothing to poison and a hijacked address cannot
  present a valid certificate for the name.


## Assemble and deploy

```sh
sh zeronym/hub/deploy/caution/assemble-caution.sh \
  --name zeronym-hub-1 \
  --indexers 66.42.124.202:443 \
  --indexer-tls lwd.shieldedinfra.net \
  --tls-domain hub.example.org \
  [--production] [--debug]
```

Then, from the assembled directory:

```sh
caution login --username <name> --qr
caution apps create      # no --name: auto-names the app, adds the 'caution' remote
git push caution main    # builds and boots the enclave; prints its IP
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
