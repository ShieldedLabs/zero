# ACME issuance ledger

Every enclave restart is a fresh certificate order. A Nitro enclave has no
persistent storage, so there is nowhere to cache one: `NoCache` is the only
honest choice, and a redeploy is indistinguishable from a first deploy as far as
Let's Encrypt is concerned.

**The limit that matters: 5 duplicate certificates per week**, per identical set
of names, on a rolling 7-day window. Exceeding it does not degrade gracefully.
Issuance simply fails, the enclave comes up with no certificate, and every
handshake fails until the oldest order ages out of the window. There is no
console in an attested enclave to explain it, so the symptom is a shim that
accepts TCP and completes no TLS.

## Resolution (2026-08-05): the gRPC path is proven end to end

Two things had to land, and the second was misdiagnosed for a while below.

1. Caution shipped `upstream_protocol = "h2c"`, so the in-enclave Caddy speaks
   cleartext HTTP/2 to the shim instead of HTTP/1.1. Wired into the template the
   same day.
2. The 500 that survived (1) was NOT a Caddy-to-shim fault, which is what the
   dated rows further down guessed. It was our own backend ingress: Traefik
   v3.7.1 silently ignores the Kubernetes Ingress `serversscheme: h2c` annotation
   and forwards HTTP/1.1 to the h2c-only indexer, so every gRPC call returns a
   bare 500. Fixed in shielded-infra with an IngressRoute carrying an explicit
   `scheme: h2c`.

With both in place the lwd shim was deployed to `zis-lwd-test-1.shieldedinfra.net`
(attested build, in-enclave Caddy, trusted Let's Encrypt cert): a live
`GetLightdInfo` returns HTTP 200 with the grpc-status trailer intact, through the
shim, over TLS. The "held until the h2c fix" notes below are kept as the record
of how the diagnosis actually went, not as current status.

## Iteration strategy: throwaway test hostnames (Mark, 2026-08-05)

The limit is keyed on the hostname set, so **each distinct name has its own
independent 5/week budget**. While iterating on a deploy that is not yet green,
use throwaway names: `zis-zaino-test-1.shieldedinfra.net`,
`zis-zaino-test-2.shieldedinfra.net`, and so on. Redeploy against `-test-N` as
many times as needed; when one budget is spent, move to `-test-(N+1)`. The real
name is never touched, so its budget is preserved for launch.

When the config is proven green, promote it to the production endpoint
**`zis.shieldedinfra.net`** (its own fresh budget), and stop redeploying it.

Mechanics per test name: pass `--tls-domain zis-zaino-test-N.shieldedinfra.net`
to `assemble-caution.sh` (already parameterised), and add an ExternalDNS-driven
Service so the record follows the enclave IP (mirror `zis-zaino-dns` in
shielded-infra's `zis-enclave-dns.yaml`). Note this only sidesteps the LE budget;
a hostname change does not fix a hostname-independent failure like the backend
gRPC-ingress 500 that actually blocked this (see Resolution above).

Two habits keep this from biting:

* **Prove a change against Let's Encrypt staging first.** Staging has no
  meaningful ceiling. `ZIS_TLS_PRODUCTION` is off by default precisely so that
  reaching production is a deliberate act.
* **Log every production issuance below, on the day it happens.** A count kept
  only in memory is a count nobody has.

Other Let's Encrypt limits are not close to binding here and are noted only so
nobody re-derives them: 300 new orders per account per 3 hours, and 50
certificates per registered domain per week. The duplicate-certificate limit is
the one that a diskless enclave runs into.

## Production issuances

Each row is one certificate actually issued by the production directory. A
restart that failed to obtain one still consumed an order, so record it too and
say so.

Count is per **domain** (see the note below), so the two shims number their own
rows independently.

**zis-zaino.shieldedinfra.net**

| # | date | commit | note |
|---|---|---|---|
| 1 | 2026-08-04 | `82b72980`-era | first e2e deploy (8080 config); cert issued, then 502 on the port bug |
| 2 | 2026-08-04 | `16656476` | 8083 config, app `00ee815c` at 15.164.71.196. Cert issued clean, in-enclave Caddy, `Verify return code: 0`. gRPC still 502s: Caddy proxies HTTP/1.1 to the h2c-only shim (Caution-side h2c-upstream fix pending). |

Roughly two more this week before the 5/7-day duplicate-certificate limit binds
for this name; the window rolls, so #1 frees up ~2026-08-11.

**zis-lwd.shieldedinfra.net**

| # | date | commit | note |
|---|---|---|---|
| 1 | 2026-08-04 | `82b72980`-era | first e2e deploy (8080 config) |

Deliberately NOT redeployed to 8083 yet: it would hit the identical Caddy h2c
wall and spend an issuance to learn nothing zaino has not already shown. Held
until the Caution-side h2c fix lands.

Note that the two shims hold **different** names, so they have independent
duplicate-certificate budgets: five each, not five between them. Redeploying one
does not spend the other's allowance.

## Rolling-window check, before any production redeploy

Count the rows above for that enclave's domain in the last 7 days. At 4, stop
and use staging unless the deploy genuinely has to be production. Let's
Encrypt's own view of it is authoritative and can be checked against the
Certificate Transparency logs, which is also how an auditor would notice a
certificate this ledger does not list:

```bash
curl -s "https://crt.sh/?q=zis-zaino.shieldedinfra.net&output=json" \
  | python3 -c "import sys,json;[print(e['not_before'], e['issuer_name'][:40]) for e in json.load(sys.stdin)[:10]]"
```

That last point is worth stating plainly, because it is a security property and
not just bookkeeping: a certificate for these names that does not appear in this
file is either an unrecorded deploy or someone else's certificate for our
domain. The Auditor Role in the Zeronym design exists partly to watch for
exactly that.
