---
applyTo: "zeronym/**"
---

# Reviewing zeronym

First-party code: a shim and a hub that carry Zcash traffic over the Nym mixnet,
deployed as attested enclaves. Unlike the vendored trees, we own all of this and
normal review applies.

## The properties worth protecting

- **Privacy is the product.** The point of routing over a mixnet is that an
  observer cannot link a transaction to its submitter. Treat anything that could
  deanonymize a user as high severity: a clearnet fallback path, an error branch
  that bypasses the mixnet, a log line carrying an address or transaction id, a
  timing or ordering signal that correlates submission with origin, or a
  distinguishing retry pattern.
- **Reproducibility is a published claim.** `deploy/EXPECTED_SHA256` and the
  reproduce scripts back the auditor-facing claim that a binary matches its
  source. Flag any change that could make the build non-deterministic —
  timestamps, embedded paths, network fetches during build, unpinned
  dependencies — and any change to build inputs that does not also re-baseline
  the published hash.
- **Enclave and deploy integrity.** Attestation, TLS termination, and the
  control-plane paths are trust boundaries. Check that a failure in one stage
  cannot leave a later stage running unprotected, and that a partial deploy
  fails closed rather than pointing users at something unverified.

## Failure handling

Deploy scripts run once, by hand, against live infrastructure, and a mid-script
failure can strand resources that cost real money and rate-limited quota
(certificate issuance, enclave creation) to recreate. When reviewing a script,
check what happens if each step fails partway: whether the operator can retry
the failed step alone, or is forced to redo everything from the start.

## Secrets

Nothing in a committed file should carry a key, seed, wallet, or credential.
Flag any example, test fixture, or default that looks like a real one.
