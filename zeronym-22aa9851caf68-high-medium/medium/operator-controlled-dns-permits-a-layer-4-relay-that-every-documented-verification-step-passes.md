# The auditor recipe never looks at the network path, so a plain TCP forwarder on the operator's own DNS record hands them the wallet leg while all four documented checks — and the platform's strongest check — pass, correctly and without a Certificate Transparency trace

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/README.md:71` (the auditor procedure, the claim under test); `audit-target/zeronym/shim/deploy/caution/OPERATORS.md:59` ("A DNS name you control"), `:116-131` (the expected CNAME shape), `:139-142` (the only interposition warning, with a rationale that excludes this case), `:178` (the verify invocation), `:340` (redeploy changes app id and IP); `audit-target/zeronym/deploy.sh:65-69` and `:82-97` (`set_dns_record`), `:162-172` and `:182-195` (the record written and rewritten on every deploy), `:206-221` (what is published to `APP_SOURCE`); `audit-target/zeronym/shim/deploy/caution/assemble-caution.sh:585-601` (`PROVENANCE`, which records the domain but not the app id); `audit-target/zeronym/shim/deploy/caution/caution.hcl.tmpl:79-126` (in-enclave TLS termination)
**Found by agent:** Global, focus area G29 — established while settling whether the operator can observe the wallet→shim leg in an attested deployment
**In scope of audit?** Yes — `README.md` and `OPERATORS.md` are in scope as security claims, and `deploy.sh` is in scope as part of the trust chain.

> **ANTI-DOUBLE-COUNT, APPLIED DURING VALIDATION.** The *capability* this
> interposition yields — the wallet's source IP, connection timing and cleartext
> TLS record lengths, and from them the exact `|tx|` and the IP↔transaction↔amount
> join — is **already graded inside the confirmed High**
> `core-linkage-survives-in-the-attested-deployment-…md`, which enumerates this as
> **route 1C of its step 1** and names this file as its separate filing. **This
> issue is graded on the detection gap only**, and its severity must never be
> added to that High. The filed addendum's recommendation of **Medium → High is
> rejected** for that reason, on the precedent recorded in coordinator item 7a.
>
> **The relay does not decrypt anything.** TLS terminates on the Caddy inside the
> enclave; the forwarder sees ciphertext. Any sentence implying otherwise is
> wrong and must not be written.

## Description

`README.md:71` is the project's whole auditor-facing verification contract:

> - **Auditors** verify an endpoint without trusting its operator: fetch its
>   attestation, check the PCRs against the AWS Nitro root, reproduce the build and
>   compare hashes, and check Certificate Transparency for a shadow certificate.

All four checks are aimed at one interposition: an operator who **terminates** the
wallet's TLS in front of the enclave. That needs a certificate for the
wallet-facing name, which is why Certificate Transparency is named as the
defence.

**A second interposition costs nothing, leaves no record, and every one of those
four checks passes — as does the strongest check the platform itself offers.** The
operator points the wallet-facing DNS record at a host they own that forwards TCP
port 443 to the Caution endpoint without terminating TLS.

| Documented check | Under a layer-4 TCP forwarder |
|---|---|
| Fetch `https://<tls-domain>/attestation` | Reaches the enclave through the forwarder; the document is genuine and AWS-signed. **Passes.** |
| PCRs against the AWS Nitro root | Nothing about the image changed. **Passes.** |
| Reproduce the build, compare hashes | Unrelated to the network path. **Passes.** |
| Certificate Transparency for a shadow certificate | **No certificate is issued.** The forwarder presents none; the enclave's Caddy still serves the only certificate for the name. Nothing appears in any CT log. **Passes vacuously.** |
| `caution verify`'s attested-TLS `certfp` binding (not in the recipe, but the platform's strongest check) | The verifier's TLS session still terminates on the in-enclave Caddy, so the leaf is unchanged and the attested `certfp` matches. **Passes — and passes *correctly*.** |

The last row is the point, and it is sharper than "the recipe omits a check": the
platform's binding answers *"did my TLS session terminate inside the attested
enclave?"*, to which the answer under a forwarder is genuinely **yes**. The
binding is sound. It is **orthogonal to this attack by construction**, because
the attack does not touch the key, the certificate or the enclave.

What the forwarder yields is **metadata**: the wallet's source IP, connection
timing, and every TLS record length in both directions (the record length field
is cleartext in every TLS version). That is exactly the input the confirmed High
`core-linkage-…md` needs for its step 1, and exactly the thing the enclave
architecture otherwise denies the operator — `README.md:27` sells the source-IP
property to users, and G30/G4 confirmed from platform source that the wallet's IP
provably never enters the enclave.

**The prerequisite is documented as the operator's, not as a trust assumption.**
`shim/deploy/caution/OPERATORS.md:59`: *"**A DNS name you control** for wallets."*

## Attack Scenario and Steps

Attacker: the shim operator, or anyone who obtains control of the wallet-facing
DNS record — a compromised or compelled registrar or DNS provider, or an insider.

1. Deploy the shim exactly as `deploy.sh` does, with `DEBUG=0` and an
   `APP_SOURCE`. The enclave is genuine, the build reproduces, `caution verify`
   prints `Attestation verification PASSED`.
2. Stand up a TCP forwarder on a host the operator owns — a dozen lines of
   `socat`, an `nginx stream` block, or one `iptables` DNAT rule — forwarding
   `:443` to `<app-id>.apps.caution.sh:443`.
3. Repoint the wallet-facing record from the CNAME `deploy.sh:169-172` wrote to an
   A record for the forwarder.
4. Capture. Every wallet connection now traverses the operator's host: source IP,
   timing to the millisecond, and TLS record lengths in both directions.
5. Run the join described in `core-linkage-…md` steps 2–5.

**Attack Requirements and Assumptions:**

- **Access needed:** control of the wallet-facing DNS record — which the runbook
  states as a prerequisite the operator must hold — and any host to run a TCP
  forwarder on. No AWS account, no platform account, no certificate, no
  interaction with the enclave.
- **Which party holds the DNS name, per deployment shape:** the operator, in
  *both* shapes. `deploy.sh:65-69` requires `TLS_DOMAIN` to be a label under a
  Vultr zone the operator holds, and `set_dns_record` (`:82-97`) deletes and
  rewrites every record for that name using the operator's own `VULTR_API_KEY`.
  Nothing about the DNS record differs between fully-managed and BYOC.
- **Where the capability is *marginal*:** in **BYOC** (`OPERATORS.md:66-69`,
  `caution init --byoc`) the parent host is already in the operator's own AWS
  account, so they hold the accepting socket without any DNS trickery and the
  forwarder adds nothing. `deploy.sh` does not use BYOC.
- **Where the capability is *load-bearing*:** in the shape `deploy.sh` performs —
  `caution apps create`, which `OPERATORS.md:64` defines as *"**Fully managed**:
  in Caution's AWS account"* — **the parent host belongs to the Caution platform,
  not to the operator**, and with `DEBUG=0` no `--ssh-key` is passed
  (`deploy.sh:128-134`), so no `debug.ssh_keys` entry is rendered. In that
  deployment the forwarder is one of only two routes by which the operator
  reaches the wallet leg at all; the other is hand-running
  `assemble-caution.sh --ssh-key` without `--debug`, which `deploy.sh` never
  does.
- **What makes it durable:** `OPERATORS.md:340` records that a redeploy produces a
  *"new app id AND new IP"*, and `deploy.sh:162-195` rewrites the record on every
  deploy — so a third party watching resolution has no stable baseline, and a
  change is the expected state rather than an alarm.
- **What it costs the attacker:** nothing detectable. It spends no ACME issuance,
  so it does not appear in `RESTARTS.md`'s ledger; it produces no CT entry; it
  moves no PCR; and it is reversible in one DNS edit.
- **The one partial observable, stated honestly:** in the honest configuration
  `<tls-domain>` is a CNAME to `<app-id>.apps.caution.sh`
  (`OPERATORS.md:116-122`), and under a forwarder it is not. So a third party who
  has read the *operator runbook* can check the **suffix** of the resolution
  chain. They cannot check the **value**, because no zeronym artefact publishes
  the app id (see the `PROVENANCE` block below), and `README.md:71` — the only
  text addressed to auditors — never mentions DNS at all.

## Impact on Users

Wallet users are told at `README.md:71` that an endpoint can be verified *"without
trusting its operator"*, and at `README.md:26-27` that the operator cannot see
their broadcast contents and that the on-chain transaction carries no link to
their IP. For the property that matters most to them — that the operator cannot
associate their IP address with the transaction they broadcast — the stated
verification is not achievable by the stated procedure, because the procedure
never looks at the network path and its one anti-interposition check (CT) is
blind to the interposition that costs nothing.

The practical consequence: an auditor can run all four documented checks, publish
that the endpoint is verified, and be wrong about exactly the property users act
on. Users have no other signal — no wallet checks attestation, and the linkage
that follows is permanent and retrospective, because the chain is public forever.

## Technical Details / Code Analysis

**1. The record is the operator's, and the deploy driver rewrites it.**
`shim/deploy/caution/OPERATORS.md:59`:

```
- **A DNS name you control** for wallets.
```

`deploy.sh:65-69` pins `TLS_DOMAIN` under an operator-held Vultr zone, and
`:169-172` writes the record:

```sh
REC_TYPE=CNAME
REC_DATA="$APP_ID.apps.caution.sh"
[ "$DNS_CNAME_TRAILING_DOT" = 1 ] && REC_DATA="$REC_DATA."
set_dns_record "$REC_TYPE" "$REC_DATA"
```

with `set_dns_record` (`deploy.sh:82-97`) deleting **every** existing record for
that name first. The record is therefore fully under operator control, is
rewritten routinely, and its value is not published anywhere an auditor is told
to look.

**2. The only interposition warning in the runbook covers only the detectable
case.** `shim/deploy/caution/OPERATORS.md:139-142`:

> The record must be **DNS-only**: a Cloudflare-proxied (orange cloud) record
> terminates TLS at Cloudflare, which destroys the in-enclave-key property the
> whole attestation argument rests on, and blocks the ACME challenge so no
> certificate ever issues. Both failures are silent.

Both named consequences are consequences of *terminating* TLS. A layer-4
forwarder does neither. TLS passes through end to end, so the in-enclave-key
property is intact; and issuance succeeds normally, because the enclave's Caddy
has **no port-80 path at all** — its only vsock listener is 443
(`/bin/socat VSOCK-LISTEN:443,reuseaddr,fork TCP:127.0.0.1:443` in the platform's
`run.sh` template) — so validation is TLS-ALPN-01 on 443, which a TCP forwarder
relays like any other connection. The rule "must be DNS-only" is stated with a
rationale that does not cover the case that matters.

**3. The manifest's own property is preserved, which is why nothing detects it.**
`shim/deploy/caution/caution.hcl.tmpl:81-89`:

```
    # `e2e_encryption { enabled = true }` is Caution's in-enclave TLS
    # termination … the private key is generated and held inside the enclave and
    # the operator never holds it, which is the property the whole attestation
    # argument depends on.
```

Correct, and unaffected. The design protects the *key* and therefore the
*content*. It does not protect the *path*, and the source IP is a property of the
path.

**4. The platform's `certfp` binding passes, and passes correctly.**
`caution verify` computes the SHA-256 of the leaf certificate of *its own*
connection and requires it to equal the value the enclave signed into the Nitro
attestation (`src/cli/src/lib.rs`, `validate_attested_tls`):

```rust
    anyhow::ensure!(user_data.tls.mode == "tls", "attested TLS mode is not tls");
    anyhow::ensure!(user_data.tls.domain == expected.domain, …);
    anyhow::ensure!(user_data.tls.certfp == observed_certfp,
        "attested TLS certfp does not match the live leaf certificate");
```

The attested value is produced inside the enclave by `caddy-certfp.sh`, which
opens a local TLS connection to the enclave's own Caddy and publishes
`sha256(leaf DER)` into `/metadata.json`, from which `bootproofd` places it in the
COSE-signed `user_data`. Under a forwarder neither side of that equality changes.

**5. The platform ships a check that WOULD catch it, and the recipe steers away
from it.** `caution verify` has two TLS paths. `tls_connection` returns
`AttestationResponse` when `--attestation-url` is `https://<configured domain>/…`,
and `PinnedIp(ip)` when it names a raw address. Only the second path resolves the
domain and compares:

```rust
fn dns_contains_deployment_ip(domain: &str, deployment_ip: IpAddr, addresses: &[SocketAddr]) -> Result<bool> {
    …
    anyhow::ensure!(
        addresses.iter().any(|address| address.ip() == deployment_ip),
        "configured TLS domain {} does not resolve to deployment IP {}", domain, deployment_ip);
```

An auditor who learns the deployment's real address **independently** and passes
it as the attestation URL gets a **hard failure** under a forwarder, because
`<tls-domain>` resolves to the forwarder and not to that address. That is
recommendation 1 of this issue, already implemented in the tool.

It is never invoked, for three reasons that are all zeronym's:
`README.md:71` and `OPERATORS.md:178` both use the domain form; **no zeronym
artefact publishes the app id or the managed hostname** — `assemble-caution.sh`
writes `PROVENANCE` with `serves: $TLS_DOMAIN` and the source commit, and the
tree pushed to `APP_SOURCE` (`deploy.sh:206-221`) is the commit assembled
*before* `caution apps create` ran, so it cannot contain the app id; and
`OPERATORS.md:340` conditions readers to expect the address to change on every
redeploy. Note the failure mode of a half-informed attempt: resolving
`<tls-domain>` yields the forwarder's address, `dns_contains_deployment_ip`
compares it against itself and matches, and the pinned connection then reaches
the forwarder and gets the enclave's relayed leaf — so the check **passes**. It
only works with an out-of-band source for the deployment address, which is what
recommendation 3 asks the project to publish.

**6. `network.ingress` is unmeasured, so the forwarder can be made mandatory.**
`ingress { cidr_ipv4 = … }` is consumed by the platform's security-group
generation, not by the enclave image: `EnclaveManifest`
(`src/enclave-builder/src/manifest.rs`) has fields for `app_source`,
`enclave_source`, `framework_source`, `binary`, `run_command`, `metadata` and the
component commits, and **none for `network` or `resources`** — so changing
ingress changes no PCR. An operator who narrows the shim's ingress from
`0.0.0.0/0` to their forwarder's address turns the interposition from a *default*
into an *enforcement*: a wallet that pins the real deployment address, or an
auditor attempting the raw-IP flow above, can no longer connect at all.

## Recommendations

1. **Add the network path to the auditor procedure.** `README.md:71` should
   instruct auditors to resolve `<tls-domain>` and confirm it points at the
   Caution-managed target for the app id named in the published artefacts. This
   is one `dig` and it converts an invisible interposition into an observable
   one. Pair it with the already-recommended check that `ZIS_HUB_NYM` in
   `.manifest.run_command` matches `https://<hub-domain>/nym-address`.
2. **Publish the expected resolution target.** Record the app id and its managed
   hostname in `PROVENANCE` (which `assemble-caution.sh` already writes) or
   alongside the PCRs, so recommendation 1 becomes mechanical, scriptable and
   continuously monitorable — the same spirit as a CT watch. Without this, the
   platform's own `dns_contains_deployment_ip` check cannot be used, and a
   half-informed attempt at it passes.
3. **Restate the "DNS-only" rule with the correct rationale.**
   `OPERATORS.md:139-142` should say that *any* interposition on the
   wallet-facing record — including one that does not terminate TLS — defeats the
   source-IP property, and that the rule is not self-enforcing because a
   pass-through forwarder breaks nothing an operator would notice and issues no
   certificate.
4. **Say plainly in `README.md` that attestation does not cover the network
   path.** List the things it does not bind together: the hub the shim diverts to,
   the route by which a wallet reaches the enclave, and (per the
   `network.ingress` fact above) who may reach it. State that `certfp` proves the
   session terminated in the enclave and proves nothing about what the session
   traversed.
5. **Have an auditor check `network.ingress` in the published `caution.hcl`.**
   `resources` and `network` are the two manifest blocks covered by no
   measurement; a narrowed ingress is a sign the operator intends the
   interposition to be unavoidable.

## Validation Information

**Validated 2026-08-18. VERDICT: CONFIRMED. Severity HELD at Medium; the
addendum's recommended Medium → High is REJECTED. Every platform claim was
re-derived from the Caution source clone (`codeberg.org/caution/platform` @
`1f8d8cb`) rather than inherited, and every zeronym citation was checked against
the target at HEAD.**

### 1. What was verified from primary sources

- **`validate_attested_tls`** (`src/cli/src/lib.rs:354-385`) compares
  `user_data.tls.certfp` against `observed_certfp` and additionally pins
  `user_data.tls.mode == "tls"` and `user_data.tls.domain == expected.domain`.
- **`observed_certfp` is the leaf of the verifier's own connection.** On the
  documented `https://<domain>/attestation` form, `tls_connection`
  (`:221-241`) returns `AttestationResponse` and `verify_tls_binding`
  (`:6892-6912`) hashes the leaf of that very response. **A layer-4 forwarder
  changes neither operand.**
- **`caddy-certfp.sh`** (`src/enclave-builder/templates/caddy-certfp.sh`) is a
  loop that `openssl s_client`s the enclave's own Caddy on `127.0.0.1:443` with
  `-verify_return_error -verify_hostname`, and writes
  `{"tls":{"mode":"tls","domain":…,"certfp":…}}` to `/metadata.json`. The attested
  value is therefore the *enclave's* leaf, unconditionally.
- **`dns_contains_deployment_ip`** (`:261-278`) is reached only from the
  `PinnedIp` branch (`:6902`, `:6930`), i.e. only when `--attestation-url` names a
  raw address. Confirmed by reading both call sites.
- **The enclave has no port-80 ingress.** `run.sh.template` starts exactly one
  wallet-facing vsock listener, `VSOCK-LISTEN:443 → TCP:127.0.0.1:443`, and the
  parent's Caddy binds `:80` only for health, `/attestation` and a 308 redirect
  (`terraform/modules/aws/nitro-enclave/user-data.sh`). So ACME must be
  TLS-ALPN-01 on 443, and a TCP forwarder relays it unchanged — the filed claim
  that issuance still succeeds is **correct**.
- **`EnclaveManifest`** (`src/enclave-builder/src/manifest.rs:14-45`) has no
  `network` or `resources` field. The `network.ingress`-is-unmeasured claim is
  **correct**.
- **`deploy.sh` publishes no app id.** `assemble-caution.sh` commits the tree and
  writes `PROVENANCE` with `serves: $TLS_DOMAIN`, `source commit`,
  `expected binary` — and no app id, which does not exist yet.
  `deploy.sh:210-215` pushes that same commit to `APP_SOURCE`. Verified by
  reading both scripts end to end.

### 2. Corrections applied to the filed text

- **The "What limits it further" hedge is withdrawn and replaced by a precise
  per-shape statement.** The filed text hedged on whether the operator already
  owns the parent host. They do in **BYOC** (where the forwarder is marginal) and
  they do **not** in the fully-managed path `deploy.sh` performs (where it is
  load-bearing). Both are now stated, with the deciding citations
  (`OPERATORS.md:64` vs `:66-69`, `deploy.sh:156`, `deploy.sh:128-134`). The
  audit's `THREATMODEL.md` asserts operator ownership of the parent host
  unconditionally; that assertion is false in a managed deploy and was not
  inherited here.
- **A partial observable was ADDED against the filing, because omitting it would
  have overstated the finding.** `OPERATORS.md:116-122` documents that the record
  should be a CNAME to `<app-id>.apps.caution.sh`, so the *suffix* of the
  resolution chain is checkable by anyone who has read the operator runbook. This
  does not refute the issue — the *value* is uncheckable because no artefact
  publishes the app id, and `README.md:71` (the auditor-facing text) never
  mentions DNS — but a report that claimed "no observable exists" would be
  falsifiable in one `dig`. Recommendation 2 is strengthened accordingly.
- **The "relay" vocabulary is retained only for the operator's own forwarder.**
  Per coordinator item 7h, the Nitro **parent host** is described nowhere in this
  file as a relay: it is the enclave's router and DNS resolver. The forwarder in
  this attack is a distinct thing on a distinct host.
- **No plaintext claim appears anywhere.** The forwarder sees ciphertext; the
  metadata channel is record lengths and timing. Stated explicitly in the banner.
- **The Location line numbers were tightened** (`deploy.sh:162-172` and `:182-195`
  for the two `set_dns_record` calls, rather than the filed `:166-192`; the
  `PROVENANCE` block added; `caution.hcl.tmpl:79-126` for the http block).

### 3. Why Medium, and why not High

**Not High**, for one reason and it is decisive: the *capability* is already
graded inside the confirmed High `core-linkage-…md`, whose step 1 enumerates this
as route 1C by name and whose validation already verified the same platform
mechanics. Grading this High would count one loss twice. Coordinator item 7a
records the governing precedent — a validator rejecting a recommended severity to
avoid multi-counting a harm owned elsewhere — and it applies here in the upward
direction.

**Not Low**, for three reasons. (i) The falsified text is not an incidental
comment: it is the entire auditor-facing verification contract on the front page
of the project, and `README.md:71`'s claim is a completeness claim
(*"without trusting its operator"*) that the four listed checks cannot support.
(ii) The gap is **structural**, not stale: no check in the recipe looks at the
network path, and the platform's strongest check is orthogonal to this attack by
construction rather than by oversight — so a reader cannot repair it by being
more careful. (iii) A usable remedy exists in the platform *today*
(`dns_contains_deployment_ip`) and is unreachable only because zeronym never
publishes the one value it needs; a finding whose fix is "publish a string you
already have" and which is currently blocking a working check is worth more than
Low.

This grade matches the closest precedent in `issues/confirmed/`:
`attested-tls-binding-is-verified-once-by-hand-if-ever-…md` (Medium), which is the
same shape — a real binding, verified once by hand if ever, with `README.md:26`
stating the resulting protection unconditionally. The two are **siblings, not
duplicates**: that one is about a check that exists and is not repeated; this one
is about an interposition that check cannot see at all. Report them together
under one heading — *attestation says nothing about how a wallet reaches the
enclave* — and count them once each.

### 4. Nothing was withdrawn

Every mechanical claim in the filing and its addendum survived re-derivation. The
changes above are a rejected severity escalation, a withdrawn hedge replaced by a
per-shape statement, one added observable that bounds the finding honestly, and
tightened citations.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
