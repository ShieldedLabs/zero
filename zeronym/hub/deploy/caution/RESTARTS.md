# ACME issuance ledger (zero-indexer-hub)

Every enclave restart is a fresh certificate order. A Nitro enclave has no
persistent storage, so a redeploy is indistinguishable from a first deploy as far
as Let's Encrypt is concerned. Same discipline as the shim's `RESTARTS.md`; read
that one for the full reasoning.

**The limit that matters: 5 duplicate certificates per week**, per identical set
of names, on a rolling 7-day window. Exceeding it does not degrade gracefully:
issuance fails, the enclave comes up with no certificate, and every handshake
fails until the oldest order ages out. There is no console in an attested enclave
to explain it.

Two habits keep this from biting:

- **There is no staging on this path.** The in-enclave Caddy picks the ACME
  directory itself and always uses production, so every push spends an
  issuance; the throwaway-name habit below is the whole budget control.
- **Use throwaway names while iterating** (`hub-test-1`, `hub-test-2`, ...): the
  budget is keyed on the hostname set, so each distinct name has its own 5/week.
  Promote to the real name only once green, and stop redeploying it.

## Production issuances

Each row is one certificate actually issued by the **production** directory. A
restart that failed to obtain one still consumed an order, so record it too and
say so. None yet: the hub has not been deployed to a production endpoint.

**&lt;hub production domain&gt;**

| # | date | commit | note |
|---|---|---|---|
| - | - | - | (no production issuances yet) |

## Audit property

A certificate for the hub's names that does not appear in this file is either an
unrecorded deploy or someone else's certificate for our domain. Cross-check the
Certificate Transparency logs before any production redeploy:

```bash
curl -s "https://crt.sh/?q=<hub-domain>&output=json" \
  | python3 -c "import sys,json;[print(e['not_before'], e['issuer_name'][:40]) for e in json.load(sys.stdin)[:10]]"
```
