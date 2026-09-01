# The enclave's TLS certificate binding is real but time-of-check-only: zeronym schedules no re-verification, wallets never check it, and the continuous signal `README.md:71` offers instead — Certificate Transparency — cannot carry the signal on a diskless enclave

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/README.md:26` (the unconditional "Protected → Broadcast contents" claim) and `:30-36` (the "Not protected" list, which omits certificate substitution); `audit-target/zeronym/README.md:71` (the auditor recipe, which names Certificate Transparency as the anti-substitution defence); `audit-target/zeronym/shim/deploy/caution/OPERATORS.md:151-217` (the "Verify" section — one-shot), `:132-137` (the documented NXDOMAIN window), `:341-343` (the diskless re-issuance rule) and `:345-347` (the CT watch, assigned to the operator); `audit-target/zeronym/hub/deploy/caution/OPERATORS.md:97-124` and `:244`; `audit-target/zeronym/deploy.sh:220` (the only verification instruction emitted at the moment of use, which runs nothing); `audit-target/zeronym/shim/deploy/caution/RESTARTS.md` (the issuance ledger the CT check would have to be compared against); `audit-target/zeronym/OPEN-QUESTIONS.md:108` (a real endpoint for which the CT signal is zero)
**Found by agent:** Local (file audit of `shim/src/tls.rs`), re-scoped by the validator after the G10/G12/G13 primary-source reversal
**In scope of audit?** Yes — `README.md` and both `OPERATORS.md` files are in scope "as security claims"; under ICTM a property users are told they get but do not get is itself a bug.

## Description

The single headline protection this product offers is stated unconditionally at
`README.md:26`, under **Protected**:

> - **Broadcast contents.** An Orchard-touching transaction is hidden from the
>   operator: the wallet's TLS terminates inside an attested enclave, not at the
>   operator's indexer.

The residual exposure is well known: the operator holds the DNS zone and the
public IP for the wallet-facing name, so they can obtain a second, entirely
legitimate CA certificate for that name and terminate the wallet's TLS themselves,
in front of the enclave, on a host they own. The wallet cannot tell — both
certificates chain to a public root in `webpki-roots`, both carry the expected
name.

**A cryptographic defence against exactly this exists and ships today.** The
platform's in-enclave Caddy publishes the SHA-256 of its own served leaf
certificate; `bootproofd` places that fingerprint *inside* the COSE-signed Nitro
attestation as `user_data.tls.certfp`; and `caution verify` compares it against
the leaf of the very TLS connection that carried the `/attestation` response. An
operator terminating 443 with their own certificate therefore **fails**
verification, with the message *"attested TLS certfp does not match the live leaf
certificate"*. (Derivation in Technical Details. This corrects the original
version of this issue, which asserted that no such binding existed — see
Validation Information.)

The finding is what surrounds that control, and it is four things that compound:

1. **Wallets get nothing from it.** The platform vendor states this as a design
   property of the mode: *"Attested TLS deliberately preserves ordinary browser
   HTTPS expectations, so the client does not validate Nitro evidence."* A wallet
   sees a valid WebPKI certificate for the right name in both the honest and the
   substituted case. So the protection at `README.md:26` is not a property of a
   wallet's connection; it is a property of somebody else having recently checked.

2. **Nobody has been asked to check on a schedule, and nothing does.** The same
   vendor documentation states the precondition of the mode plainly: *"To rely on
   Attested TLS, carefully verify fresh Nitro evidence against reviewed source and
   expected PCR0, PCR1, and PCR2 **on a regular schedule** and after relevant
   deployment, DNS, or certificate changes"*, and ships `Caution Canary
   --e2e-mode tls` for continuous enforcement. zeronym runs no canary. `deploy.sh`
   verifies nothing — its entire contribution is one printed log line
   (`deploy.sh:220`). Both `OPERATORS.md` "Verify" sections are one-shot manual
   steps performed once at deploy time, and the hub's runbook explicitly
   deprioritises verification after an incident (`:244`: *"`caution verify` is a
   further ~7 min but does NOT belong on the critical path — restore service
   first, verify after"*). A case-insensitive sweep of the whole target
   for `periodic|re-verif|reverif|on a schedule|scheduled verif|cron|canary`
   returns exactly two hits, and **neither is about verifying a live endpoint**:
   `hub/src/queue.rs:245` (OS entropy reseeding) and
   `shim/deploy/README.md:834` ("Independent re-verification, 2026-07-31"), which
   is a one-off *build*-reproducibility exercise against a since-superseded binary
   hash. No dated `caution verify` transcript exists for any deployed endpoint. **The window during which a substitution goes
   undetected is therefore unbounded in the shipped operating model.**

3. **The only continuously-available signal the README offers cannot carry the
   signal.** `README.md:71` names Certificate Transparency as the anti-substitution
   check. Two independent properties of this deployment make CT unreadable here:
   - **The enclave is the loudest source of noise in the log the auditor is asked
     to read.** The enclave is diskless: the platform's Caddy writes its ACME
     state to `/var/lib/caddy` inside the initramfs, which is tmpfs, so every
     restart is a fresh Let's Encrypt order, and Caddy renews on its own schedule
     besides. `shim/deploy/caution/OPERATORS.md:341-343` states the consequence in
     its own words — *"the enclave is diskless, so every restart is a fresh Let's
     Encrypt order, and every push spends one of the hostname's 5 weekly
     production issuances"*. A shadow certificate is one more row among rows
     identical in issuer, SANs and validity, appearing at times no external
     observer can predict.
   - **The signal can be zero, not merely noisy.** The shim is a drop-in behind
     an operator's existing public URL (`README.md:60`). An operator who already
     holds an unexpired certificate for that name — the normal case, since they
     ran the endpoint before the shim existed — issues nothing and generates **no
     CT record at all**. `OPEN-QUESTIONS.md:108` records exactly this for a real
     endpoint: *"The `zec.rocks` certificate. Its existing TLS cert is valid
     through October, so the scheme is ineffective for that domain until then."*

   The CT watch is additionally assigned, in the one place it becomes an
   operating instruction, to the operator — i.e. to the adversary the check exists
   to catch (`shim/deploy/caution/OPERATORS.md:345-347`) — and the artefact an
   independent party would have to compare CT against does not cover the fleet.
   `shim/deploy/caution/RESTARTS.md:114-118` states its own security property:
   *"a certificate for these names that does not appear in this file is either an
   unrecorded deploy or someone else's certificate for our domain. The Auditor
   Role in the Zeronym design exists partly to watch for exactly that."* But the ledger holds tables for exactly two hostnames,
   `zis-zaino.shieldedinfra.net` (`:76-83`) and `zis-lwd.shieldedinfra.net`
   (`:86-91`), both Shielded Labs' own; it has **no rows at all** for
   `test-shim-nym-1.shieldedinfra.net`, the hostname `deploy.env.example:13`
   ships; and it is already incomplete within its own document, since `:29`
   records a deploy to `zis-lwd-test-1.shieldedinfra.net` that appears in neither
   table. A third-party operator publishes no such ledger at all.

4. **`README.md` never states the residual.** Certificate substitution appears
   nowhere in the "Not protected" list at `README.md:30-36`, and `README.md:26`
   is unconditional. A reader is not told that the protection they are promised
   depends on a verification practice, let alone that the practice is unscheduled.

## Attack Scenario and Steps

The adversary is the indexer operator — adversary #1 in
`audit-context/AUDIT-INSTRUCTIONS.md` and the reason the product exists.

1. The operator deploys the shim honestly. `caution apps create` allocates an
   Elastic IP; the operator points `<tls-domain>` at it in their own DNS zone
   (`deploy.sh` writes the record into their Vultr zone). An auditor runs
   `caution verify --attestation-url https://<tls-domain>/attestation` at time
   `T0`, gets `✓ Base Nitro attestation and expected PCR0/1/2 verified`,
   `✓ TLS certificate binding verified` and `✓ Attestation verification PASSED`,
   and publishes that the endpoint is good. Wallets are pointed at it.
2. At any later time `T1` the operator obtains a second certificate for
   `<tls-domain>` from any public CA. They control the DNS zone (DNS-01) and the
   public IP (HTTP-01), so this is one `certbot` invocation and no privilege they
   do not already hold. If they already have an unexpired certificate for the
   name, they skip this step and generate no CT record.
3. On the parent host they stop forwarding 443 to the enclave and terminate it
   themselves, re-originating to the enclave behind. They own the parent host;
   this is a firewall rule and a Caddy/nginx config, not an exploit.
4. From `T1` onward every wallet request is readable in the clear at the
   operator, including every `SendTransaction` body — every Orchard-touching
   migration the product exists to hide — together with the source IP that sent
   it. That is the exact IP → transaction → balance join `README.md:54`
   identifies as "the attack".
5. Detection, in order of what is actually available:
   - **`caution verify` would catch it**, at any moment anyone chose to run it.
     Nothing runs it. There is no canary, no cron, no deploy-time gate, no
     published transcript, and no document asking anyone to repeat it. The
     auditor's `T0` verdict carries no expiry and is the only one on record.
   - **The wallet cannot catch it**, by the mode's design.
   - **Certificate Transparency cannot catch it**, for the two reasons in
     Description point 3.
6. If and when someone does re-verify, the substitution is exposed — permanently
   and unambiguously. The operator's exposure is therefore a function of how
   often anyone re-verifies, which today is: never, on the evidence in the
   repository.

**Attack Requirements and Assumptions:**

- The attacker must be the operator of the endpoint, or anyone who can obtain a
  CA-issued certificate for the name and get on path — which, for the holder of
  the name's DNS zone and IP, is the same person.
- No software vulnerability is required. Every step uses authority the operator
  already holds by construction.
- No wallet change, no user action, and no wallet-visible error is involved.
- **What limits the attack, and why this is Medium rather than High:** unlike the
  configuration-level attacks elsewhere in this audit, a working detector exists,
  it is cheap (~7 minutes), it needs no Caution account and no checkout of the
  operator's, and anyone in the world may run it at any time. A rational operator
  must weigh permanent exposure against the gain. The defect is that nothing
  converts that latent detector into an actual one.

## Impact on Users

Every user of an affected endpoint loses the product's headline protection
silently and completely, for as long as nobody re-verifies. The operator recovers
exactly what the shim was deployed to deny them: source IP, timing, and the full
plaintext of every Orchard-touching broadcast — joinable against the public chain
retrospectively and permanently.

Under ICTM the finding stands independently of whether any operator ever does it:
`README.md:26` tells users this is **Protected**, unconditionally, and the "Not
protected" list immediately below does not mention certificate substitution. The
true statement is *"protected as long as somebody re-runs `caution verify`"*, and
the shipped operating model contains no such somebody. A user who reads the
README holds a belief about their protection that the deployment does not support.

## Technical Details / Code Analysis

### The binding that exists (and which the original version of this issue denied)

Established from the Caution platform's own public source
(`codeberg.org/caution/platform`) and `bootproofd`
(`git.distrust.co/public/bootproof.git`), both cloned during this audit.

1. **In the enclave.** `src/enclave-builder/templates/caddy-certfp.sh` runs on a
   60-second loop, connects to the enclave's own Caddy with
   `openssl s_client -connect 127.0.0.1:443 -servername <domain>
   -verify_return_error -verify_hostname <domain> -purpose sslserver -CAfile …`,
   takes the SHA-256 of the served leaf, and writes
   `{"tls":{"mode":"tls","domain":"<d>","certfp":"<lowercase hex>"}}` to
   `/metadata.json`.
2. **`bootproofd`** passes `/metadata.json` as `user_data` into
   `Nitro.generate(user_data, nonce)`, i.e. **inside the COSE-signed document**
   (`crates/bootproofd/src/routes/nonced_attestation.rs:88-118`).
3. **`caution verify`** enforces all four fields
   (`src/cli/src/lib.rs:354-384`):

   ```rust
   anyhow::ensure!(user_data.tls.mode == "tls", "attested TLS mode is not tls");
   anyhow::ensure!(user_data.tls.domain == expected.domain, …);
   anyhow::ensure!(user_data.tls.certfp.len() == 64 && …lowercase hex…,
       "attested TLS certfp is not lowercase SHA-256 hex");
   anyhow::ensure!(user_data.tls.certfp == observed_certfp,
       "attested TLS certfp does not match the live leaf certificate");
   ```

   `observed_certfp` is `sha256(leaf DER)` of the **same** WebPKI-validated,
   redirect-disabled HTTPS response that carried `/attestation`
   (`src/cli/src/lib.rs:6892-6912`), and `expected.domain` is read from the
   *reproduced* `caution.hcl` and is therefore itself PCR-bound
   (`tls_expectation_from_config`, `:291-312`).

`shim/deploy/caution/OPERATORS.md:189-194` records the project observing this
work on the 2026-08-14 attested pair, "with the TLS certificate binding
verified".

**Why `shim/src/tls.rs` is not the relevant code.** `ServerTls` never runs on any
deployment this repository ships: `shim/deploy/caution/caution.hcl.tmpl:163-176`
leaves `ZIS_TLS_DOMAIN` deliberately unset (*"the in-enclave Caddy declared in the
http block above owns the certificate"*). Wallet-facing TLS is terminated by the
platform's Caddy, which does the binding above. See
`servertls-is-unreachable-in-every-attested-deployment-the-repo-ships.md`.

### The two documented paths that print PASSED without the binding

`src/cli/src/lib.rs:7261-7291`:

```rust
let tls = if pcr_only {
    TlsVerification::PcrOnly
} else if let Some(ref expected) = expected_tls {
    self.verify_tls_binding(expected, &payload, &attestation_url, attestation_leaf.as_deref())
        .await.context("TLS certificate binding failed")?
} else {
    TlsVerification::NotApplicable
};
```

- `--pcrs` yields `TlsVerification::PcrOnly`, printed as *"TLS certificate
  binding: not performed (--pcrs)"*, and `✓ Attestation verification PASSED`
  still prints below it.
- `TlsVerification::SkippedNoDns` (`:6913-6932`) is reached only on the **raw-IP**
  attestation-URL flow, when the configured domain has no DNS answer or does not
  resolve to the deployment IP. `shim/deploy/caution/OPERATORS.md:132-137`
  documents an NXDOMAIN window of about a minute after every deploy, which is when
  an operator is most likely to reach for the raw IP.

Caution's own documentation warns about both (*"Do not treat that result as
Attested TLS verification"*; *"Do not use `--pcrs` for this check"*). **No zeronym
document mentions either**, nor tells a verifier to require the
`✓ TLS certificate binding verified` line. (The README half of that omission is
filed separately as
`auditor-recipe-omits-the-two-checks-that-decide-where-plaintext-goes-and-names-a-defence-the-platform-does-not-rely-on.md`.)

### Nothing in the tree performs or schedules a verification

Every occurrence of `caution verify` in the target is a one-shot manual
instruction or a comment about one: `deploy.sh:132`, `:198`, `:220`, `:332`;
`shim/deploy/caution/OPERATORS.md:32`, `:92`, `:158`, `:178`;
`shim/deploy/caution/README.md:30`, `:126`, `:132`;
`hub/deploy/caution/OPERATORS.md:100`, `:244`; `hub/deploy/caution/README.md:57`;
plus the assembler's `--app-source` warnings. `deploy.sh:220` — the only
verification guidance emitted at the moment of use — prints advice and runs
nothing:

```sh
log "verify with: caution verify (expect PCR0/1 FAILED on Caution's floating framework; PCR2 is the check that matters)"
```

(That line's *content* is separately wrong and separately filed as
`deploy-script-tells-operators-to-expect-pcr01-failure-and-accept-pcr2-alone.md`;
what matters here is that it is a `log` call, not a check.)

### Why the CT channel is unreadable, mechanically

`src/enclave-builder/templates/run.sh.template:104-126` starts Caddy with
`HOME=/var/lib/caddy XDG_DATA_HOME=/var/lib`, directories created by `mkdir -p`
inside the initramfs. The enclave has no persistent storage, so certificate and
ACME account state do not survive a restart: every restart is a fresh order.
`caddy-certfp.sh`'s 60-second re-publication loop exists precisely because the
served certificate changes underneath the attestation. The project's own runbook
budgets for this (5 production issuances per name per week) and `RESTARTS.md`
exists to manage it. So the enclave emits a stream of CT rows for the same name,
at times an external observer cannot predict, and a shadow certificate is one more
such row.

## Recommendations

In order of value:

1. **Make the check happen on a schedule and publish the result.** Adopt
   `Caution Canary --e2e-mode tls`, or an equivalent cron'd
   `caution verify --attestation-url https://<domain>/attestation` run by a party
   other than the operator, and publish a dated, per-endpoint transcript that
   includes the `✓ TLS certificate binding verified` line. Attested TLS is only as
   strong as the frequency of that check, and today the frequency is zero. This is
   the platform vendor's own stated precondition for the mode zeronym has chosen,
   and relaying it is the whole fix.
2. **Correct `README.md:26` and the "Not protected" list.** Move certificate
   substitution into "Not protected", and state the residual as it is: *the
   operator can terminate the wallet's TLS with their own certificate for the same
   name; the wallet cannot detect it; `caution verify` can and does, but only when
   somebody runs it, and nothing schedules a run.*
3. **Have `deploy.sh` run the check rather than describe it,** and fail the
   attested deploy unless all three success lines appear — including
   `✓ TLS certificate binding verified`. A verification step that is only ever
   printed is one that frequently does not happen.
4. **Stop assigning the detection to the adversary, and demote CT.**
   `shim/deploy/caution/OPERATORS.md:345-347` should name who *other than the
   operator* watches, and against what published record. If CT is kept at all it
   should be labelled supplementary, with an honest note that the enclave's own
   re-issuance makes the channel noisy and that a drop-in deployment on a name the
   operator already certifies produces no signal whatsoever. If the answer is the
   "Auditor Role", the issuance ledger must be a published, machine-readable,
   per-endpoint artefact that third-party operators maintain — not a hand-edited
   table in Shielded Labs' own repo that is already missing rows.
5. **Warn about the two skip paths.** Any documented verification step must state
   that `TLS certificate binding: not performed (--pcrs)` and
   `TLS certificate binding: skipped because the configured domain has no DNS
   answer` are **failures for this purpose**, notwithstanding the
   `✓ Attestation verification PASSED` printed beneath them.

Cross-references:
`auditor-recipe-omits-the-two-checks-that-decide-where-plaintext-goes-and-names-a-defence-the-platform-does-not-rely-on.md`
(the `README.md:71` half — CT named instead of the binding);
`operator-controlled-dns-permits-a-layer-4-relay-that-every-documented-verification-step-passes.md`
(the *weaker but wholly undetectable* interposition: no certificate, no CT trace,
and the certfp binding still verifies, because TLS is never terminated);
`shim-operators-runbook-tls-name-instruction-names-the-wrong-adversary-and-cannot-be-satisfied.md`
(the only pre-deployment defence, and why it cannot be satisfied);
`open-questions-operator-error-alarm-asks-operators-to-accept-a-signal-that-fires-on-every-restart-by-design-and-to-apply-mitigations-they-cannot.md`
(`OPEN-QUESTIONS.md:110`'s "auto-renewal disabled" mitigation, which a diskless
enclave cannot implement);
`servertls-is-unreachable-in-every-attested-deployment-the-repo-ships.md`
(why `shim/src/tls.rs` is not the code that terminates wallet TLS).

## Validation Information

**Validated 2026-08-18. CONFIRMED at Medium, after a full re-scope.** The issue
as originally filed was titled *"Nothing binds the wallet-facing TLS identity to
the enclave attestation, and the Certificate-Transparency check the README gives
auditors as the defence cannot detect operator substitution"*, was graded High,
and rested on an analysis of `shim/src/tls.rs`. **Its first clause is false and
its attack scenario does not succeed as it was written.** Those parts have been
removed rather than softened, because leaving them would have put a false positive
in `confirmed/`. What is above is the subset that survives primary-source
verification, plus the residual that the reversal exposed.

### What was refuted, and how

Re-derived by the validator from the platform's public source, not accepted from
the earlier correction note:

- **A binding exists, and it is precisely the original issue's own
  recommendation 1.** `validate_attested_tls` (`src/cli/src/lib.rs:354-384`) was
  read in full, together with its caller `verify_tls_binding` (`:6892-6961`) and
  the `TlsConnection::AttestationResponse` branch that supplies `observed_certfp`
  from the leaf of the same connection that carried `/attestation`. The original
  Description point 3 (*"The attestation is fetched over the channel under attack,
  with no channel binding"*) is **withdrawn**: that binding is exactly what
  exists.
- **Attack-scenario steps 3-5 therefore fail as written.** An operator
  terminating 443 with a second legitimate certificate cannot produce the
  enclave's private key and cannot alter the signed `user_data`, so
  `caution verify` returns *"attested TLS certfp does not match the live leaf
  certificate"* and verification fails.
- **The analysis targeted dead code.** `ZIS_TLS_DOMAIN` is deliberately unset in
  the shipped manifest (`shim/deploy/caution/caution.hcl.tmpl:163-176`, read in
  place), so `ServerTls` never runs and `NoCache`'s effects are the platform
  Caddy's, not `tls.rs`'s. The original issue's `NoCache` argument survives only
  because the platform Caddy is diskless for the same reason — that translation
  was verified against
  `src/enclave-builder/templates/run.sh.template:104-126` rather than assumed.
- **The `ZIS_CAUTION_ATTESTATION` addendum is retired.** The in-enclave Caddyfile
  generated at `run.sh.template:107-121` gives `/attestation` its own `handle`
  block reverse-proxying `127.0.0.1:49502` (bootproofd), with the bare `handle`
  as fallback. Caddy sorts `handle` blocks by path specificity, so `/attestation`
  is answered by Caddy and never reaches the shim; `shim/src/proxy.rs`'s
  `Route::CautionAttestation` arm is unreachable in any `mode = "tls"` Caution
  deployment, which is every deployment this repository ships.

### What survives, and why it is a finding rather than an inherent limitation

Four claims were checked and all four hold:

1. **Wallets never validate the binding.** Verified against the platform vendor's
   own documentation: *"Attested TLS deliberately preserves ordinary browser HTTPS
   expectations, so the client does not validate Nitro evidence."* This is a
   property of the mode, not a defect — but `README.md:26` states the resulting
   protection unconditionally and `README.md:30-36` omits the residual, and that
   is the defect.
2. **The mode's own precondition is unimplemented.** The same vendor
   documentation: *"To rely on Attested TLS, carefully verify fresh Nitro
   evidence … on a regular schedule and after relevant deployment, DNS, or
   certificate changes"*, and *"For continuous enforcement, Caution Canary
   supports an Attested TLS profile configured with `--e2e-mode tls`."* Every
   `caution verify` reference in the target was enumerated (fifteen of them, listed
   in Technical Details); all are one-shot, and none mentions repetition. This is
   what makes it a defect rather than an inherent limitation: zeronym selected a
   mode with a stated operational precondition and relays none of it.
3. **CT cannot carry the signal here.** Both legs verified in the target: the
   diskless-enclave churn (`run.sh.template:104-126` writes Caddy state into the
   initramfs; `shim/deploy/caution/OPERATORS.md:341-343` states the 5-issuances-per-week
   consequence in the project's own words) and the pre-existing-certificate case
   (`README.md:60` drop-in; `OPEN-QUESTIONS.md:108` for a named real endpoint).
   This half of the original issue is unchanged and is the part three other
   plausible issues cite as their reason for treating CT as unavailable — which is
   the second reason this issue is confirmed rather than invalidated.
4. **The two skip paths are real but thin.** `--pcrs` is a self-describing flag
   whose help text reads *"Compare against PCRs from file without TLS certificate
   binding"*, so it belongs at the bottom of the recommendation list rather than in
   the finding. `SkippedNoDns` is reachable only on the raw-IP flow
   (`tls_connection`, `src/cli/src/lib.rs:221-241` — an HTTPS URL whose host equals
   the configured domain always takes the binding branch), which narrows it
   considerably from how the correction note framed it. Both are recorded at their
   true weight above.

### Why Medium

Impact is maximal for the affected endpoint's users — complete, silent loss of the
product's headline protection, with the source IP joined to the transaction
plaintext. Two things hold it below High. First, a working cryptographic detector
exists, is cheap, and is runnable by anyone in the world without an account or a
checkout, so the operator's exposure is permanent and unbounded in time rather
than nil; that is a materially different risk calculus from
`shim-submits-every-migration-to-every-configured-hub-…` (High), where no check
anyone is asked to run catches the attack at all. Second, the surrounding
documentation defects — the recipe naming CT instead of the binding, the
unsatisfiable name-choice instruction, the layer-4 relay variant — are each filed
and graded separately, and this issue is deliberately scoped to what none of them
owns: that the binding is time-of-check-only, that nothing schedules the check,
and that `README.md:26` states the resulting protection unconditionally.

It is held above Low because the undetected window is not merely long but
*unbounded*, no verification transcript exists for any endpoint on the record, and
the ICTM gap is in the product's single headline claim rather than in a
supporting document.

### Considered and rejected during validation

- **Marking the whole issue Invalid and letting the recipe issue absorb the
  remainder.** Rejected on two grounds. (a) After the reversal, no other issue
  owns the certificate-substitution attack itself; the recipe issue owns the
  omission in `README.md:71`, the DNS issue owns the *non*-terminating relay, and
  the name-choice issue owns the pre-deployment mitigation — none of them states
  the attack, its detector, and the absence of a schedule. (b) Three other
  plausible issues cite this file by name as the establishment of "CT cannot work
  here"; invalidating it would strand that reference while the underlying claim is
  true.
- **Keeping the High.** Rejected: the control the original issue asked for exists
  and works, so the grade must reflect a scheduling and disclosure gap rather than
  a missing control.
- **Splitting "no scheduled verification" from "CT cannot work".** Rejected as
  over-fragmentation: they are one argument — the continuous signal offered is
  unreadable, and the readable signal is not continuous — and separating them
  would leave two issues each of which is unpersuasive alone.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
