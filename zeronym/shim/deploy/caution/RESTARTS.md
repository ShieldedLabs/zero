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
