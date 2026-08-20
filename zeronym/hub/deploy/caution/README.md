# zero-indexer-hub on Caution (attested Nitro enclave)

The hub receives diverted migrations in plaintext and broadcasts them to the
Zcash network. That is exactly why it runs as an attested enclave.

**How that actually works, corrected 2026-08-19.** This used to say the
attestation "binds the running binary to `../EXPECTED_SHA256`". It does not, and
no attestation this platform produces contains a binary hash at all: the
Containerfile deploy path passes `None` for the manifest's `binary` field, and
`EXPECTED_SHA256` appears nowhere in the Caution platform source. There was
nothing for an auditor to compare the rebuilt hash against, so the one job the
trust model handed them could not be done.

The chain that does exist has two independent halves:

1. **`EXPECTED_SHA256` proves the BUILD is deterministic** -- that this commit
   yields this binary, on any machine. That is what `reproduce.sh` and the CI
   reproduce gate check. Nothing in the enclave ever sees this value.
2. **The ATTESTATION binds the enclave to a rebuild of the app-source repo.**
   `caution verify` clones the repository named in the manifest's `app_sources`,
   rebuilds the EIF from it, and compares PCR0, PCR1 and PCR2 against the live
   attestation, along with the TLS certificate binding.

Reproducibility is what makes the second check meaningful -- a rebuild that did
not land in the same place every time could not be compared to anything -- but
the hash itself is never compared by any tool, and an auditor should not go
looking for it.

**What an auditor should actually do**, verified against this deployment on
2026-08-18 with `Attestation verification PASSED` on both components:

```
git clone <the app_sources repo> && cd <it>
caution verify --attestation-url https://<domain>/attestation
```

Run it from a FRESH CLONE of the public repo, never from a deploying tree.
Require **all three PCRs and the TLS binding**. Do not accept a PCR2 match alone:
hub and shim, two entirely different binaries, produce byte-identical PCR2
(`21b9efbc...`), so PCR2 identifies the platform, not the code.

Sibling of `zeronym/shim/deploy/caution/`. The shim's README covers the platform
mechanics (in-enclave TLS termination on 8083, the FIDO2 login, Let's Encrypt
issuance limits, debug-mode console); this covers only what differs for the
hub.

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
  [--debug]
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

`caution verify` (from the assembled directory; or `POST /attestation`) returns
the measurement bound to the running EIF. Confirm it against a local reproduce:

```sh
git checkout <the PROVENANCE commit>
sh zeronym/hub/deploy/reproduce.sh   # must print the hash in ../EXPECTED_SHA256
```

## Cutover note (incremental mainnet)

Deploy the hub attested against our own mainnet zebrad first, then flip **our
own** shim (`zis-*`) to divert and send one real Orchard-touching transaction.
Confirm it broadcast through the hub and our indexer never saw it before pointing
any third-party shim at this hub.
