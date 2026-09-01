# `README.md:71`'s auditor recipe omits both checks that decide where a user's plaintext actually goes, and substitutes Certificate Transparency for the certificate-binding check the platform actually provides

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/README.md:71` (the four-step recipe, and the only verification instruction any user-facing document gives); `audit-target/zeronym/shim/deploy/caution/OPERATORS.md:151-217` (the "Verify" section that implements it) and `:345-347` (the CT watch); `audit-target/zeronym/hub/deploy/caution/OPERATORS.md:97-124`; `audit-target/zeronym/deploy.sh:220` (the same instruction at the moment of use); `audit-target/zeronym/shim/deploy/caution/caution.hcl.tmpl:129-184` and `audit-target/zeronym/hub/deploy/caution/caution.hcl.tmpl:110-141` (the `unit "default" { env = { … } }` blocks the recipe never reads)
**Found by agent:** Global (focus areas G10 / G12 / G13 — the attestation and reproducibility chain, end to end)
**In scope of audit?** Yes — markdown claims are in scope as security claims; under ICTM a property users are told they get but do not get is itself a bug, and `README.md:71` is the substitute the product offers for trusting the operator.

## Description

`README.md:71` is the whole of what a wallet user is offered in place of trusting
their indexer operator:

> - **Auditors** verify an endpoint without trusting its operator: fetch its
>   attestation, check the PCRs against the AWS Nitro root, reproduce the build
>   and compare hashes, and check Certificate Transparency for a shadow
>   certificate.

Read against what the Caution platform actually does — established during this
audit by reading the platform's own public source at
`https://codeberg.org/caution/platform` and `https://git.distrust.co/public/bootproof.git`
rather than from in-repo comments — the recipe is wrong in four separate places,
and each error points the auditor away from a check that exists and toward one
that does not do the job.

**1. It never reads the configuration, which is the thing that decides where the
plaintext goes.** `ZIS_HUB_NYM` names the hub every diverted migration is sent to.
`ZIH_INDEXER_TLS` is the only thing standing between the hub's outbound batch and
a plaintext read. `ZIS_CAUTION_ATTESTATION` decides who answers `/attestation`.
All three live in `unit "default" { env = { … } }` in the deployed `caution.hcl`,
all three **are** measured (see Technical Details), and **all three are served,
verbatim, by the endpoint itself** in the `manifest.run_command` field of every
`/attestation` response. The recipe contains no step that looks. Every step in it
passes on a shim pointed at a hub the operator runs.

**2. "check the PCRs" does not say which, and one of the three is a constant.**
On Caution's EIF layout PCR2 is the measurement of an *absent* application
ramdisk: `sha384(0^48 ‖ sha384(""))` = `21b9efbc18480766…`, identical for every
Caution enclave that has ever booted, whatever code it runs — and PCR0 and PCR1
are computed over the identical byte stream, so they are necessarily equal to each
other. `deploy.sh:220` then tells the operator, at the moment of use, to expect
PCR0/1 to fail and to accept PCR2. (That instruction is separately filed as
`deploy-script-tells-operators-to-expect-pcr01-failure-and-accept-pcr2-alone.md`;
this issue is about the README sentence that leaves the door open for it by not
naming a criterion at all.)

**3. "reproduce the build and compare hashes" compares a hash nothing else uses.**
`caution verify` never reads `EXPECTED_SHA256`; `reproduce.sh` never computes a
PCR. The platform builds the app with `docker build -f <containerfile> .` and no
`--target`, i.e. the Containerfile's **last** stage (`runtime`), and measures that
filesystem into PCR0/PCR1; `reproduce.sh` builds the **`export`** stage and hashes
its tar. The two artefacts are different and nothing in the tree or the tooling
joins them.

**4. It names Certificate Transparency as the anti-substitution defence, when the
platform ships a cryptographic one and the recipe omits it.** Caution's Attested
TLS puts the SHA-256 of the DER-encoded **leaf certificate** into the
Nitro-signed `user_data.tls.certfp`, and `caution verify` compares it against the
leaf of the very WebPKI-validated, redirect-disabled TLS connection that carried
the `/attestation` response. An on-path operator terminating 443 with their own
certificate for the same name therefore **fails** verification. That is the check
an auditor should be told to require — by name, as the output line `✓ TLS
certificate binding verified`. Instead they are told to read crt.sh, which
`attested-tls-binding-is-verified-once-by-hand-if-ever-so-operator-certificate-substitution-has-an-unbounded-undetected-window.md`
shows cannot work here. (Precisely stated after validation: both runbooks do
*mention* the binding, but only as narration of expected output —
`shim/deploy/caution/OPERATORS.md:192-193` and
`hub/deploy/caution/OPERATORS.md:108`. Neither they nor `README.md:71` tell a
verifier to **require** that line, and the string `certfp` appears nowhere in
zeronym at all.) The binding also has two paths on which
`caution verify` prints `Attestation verification PASSED` **without** performing
it — `--pcrs` mode, and the **raw-IP** flow (`--attestation-url https://<ip>/…`)
when the configured domain has no DNS answer or does not resolve to the pinned
deployment IP (`TlsVerification::SkippedNoDns`). Corrected during validation:
`SkippedNoDns` is reachable **only** from the `TlsConnection::PinnedIp` branch
(`src/cli/src/lib.rs:221-241`, `:6902-6932`), **not** from the
`--attestation-url https://<domain>/attestation` flow both runbooks prescribe —
on that flow the certfp comparison is unconditional. So the NXDOMAIN window
`OPERATORS.md` documents after each deploy does **not** produce a silent skip on
the documented flow; it produces a failed HTTPS fetch. The residual is narrower
than originally written but real: a verifier who follows the raw-IP shortcut, or
whose domain has been repointed at a relay, gets `PASSED` with a warning line
instead of a failure, and no zeronym document tells them that line is
disqualifying.

## Attack Scenario and Steps

The adversary is the indexer operator: adversary #1 in the threat model and the
reason the product exists.

1. The operator stands up their own `zero-indexer-hub`. It is public code in the
   same repository, needs no consortium membership, and publishes its own Nym
   address at `GET /nym-address`.
2. They deploy the genuine, reproducible shim image with
   `HUB_NYM=<their own hub>` in `deploy.env`. `assemble-caution.sh:345-355` writes
   `ZIS_HUB_NYM = "<their hub>"` into the manifest's `env` block; the enclave is
   built, attested and healthy. They publish the assembled tree as `--app-source`,
   because they must: without it verification is impossible and the endpoint looks
   suspicious.
3. An auditor performs, in order, exactly the four steps of `README.md:71`:
   - *fetch its attestation* — succeeds, on a genuine Nitro document;
   - *check the PCRs against the AWS Nitro root* — **succeeds**, because the PCRs
     are reproduced from the published tree, which is the tree that was deployed;
   - *reproduce the build and compare hashes* — succeeds, because
     `EXPECTED_SHA256` is a hash of the binary, and the binary is byte-identical
     whichever hub it names (`shim/src/config.rs:110-114` states this as a design
     goal);
   - *check Certificate Transparency* — nothing anomalous, because nothing
     anomalous happened to the certificate.
4. The auditor reports the endpoint as verified. Every Orchard-touching
   transaction from every wallet using it is delivered in plaintext into an
   enclave the operator provisioned.
5. **The evidence was one field away the whole time.** `caution verify` staged and
   rebuilt a tree containing `ZIS_HUB_NYM = "<their hub>"`, and the endpoint
   itself returns that string in the `manifest.run_command` of every
   `/attestation` response. Nothing in the recipe, in either `OPERATORS.md`
   "Verify" section, or in `deploy.sh`'s printed advice tells anyone to look at it,
   or that it is there.

**Attack Requirements and Assumptions:**

- The operator must be willing to run a second enclave and publish the repointed
  tree. Both are the documented deploy path; `deploy.env.example` ships the hub's
  configuration alongside the shim's.
- The attack is **detectable** — that is what distinguishes this issue from an
  attestation-coverage gap. It is detectable only by a step the product never
  asks for, and the omitted step costs one HTTP request.
- No software vulnerability, platform break, or on-path position is required.
- An auditor who happens to read the reproduced `caution.hcl` in the build
  artefacts directory that `caution verify` prints — which Caution's own
  documentation tells them to do ("Inspect the staged source, **configuration**,
  generated build recipe, and manifest there before deciding whether to trust what
  the verified workload does") — catches it. zeronym's documents never relay that
  instruction.

## Impact on Users

`README.md:71` is the only thing standing between "trust your indexer operator"
and "you do not have to". A user reading it believes that someone performing those
four steps has established that this endpoint diverts their migration away from
the operator. Nobody performing those four steps has established that. What they
have established is narrower and should be stated as such: *some* attested Caution
enclave, built from a published tree, is answering — with the contents of that
tree, and therefore the destination of the plaintext, unexamined.

The same omission covers the hub's `ZIH_INDEXER_TLS`, whose absence would let the
hub's parent host read every batch before publication, and
`ZIS_CAUTION_ATTESTATION`, whose absence changes who answers the attestation
endpoint on a non-Caution deployment.

The failure is silent from the user's side: the wallet sees a valid certificate
and correct gRPC behaviour either way.

## Technical Details / Code Analysis

### The configuration is measured, and it is served

**Measured.** The chain, read in Caution's platform source:

`src/caution-config/src/lib.rs:253-274` — every literal string in `unit.env`
becomes a shell `export` line:

```rust
    pub fn run_command_string(&self) -> Result<String, FromStrError> {
        let mut out = String::new();
        if let Some(env) = &self.env {
            for (key, expr) in env {
                let value = match expr { Expression::String(s) => s, _ => continue };
                if !is_valid_env_key(key) { return Err(FromStrError::InvalidEnvKey(key.clone())); }
                let quoted = shlex::try_quote(value).map_err(|_| FromStrError::UnquotableCommand)?;
                out.push_str("export "); out.push_str(key); out.push('=');
                out.push_str(&quoted); out.push('\n');
            }
        }
        ...
```

`src/api/src/main.rs:2413-2417` passes it as `run_command`;
`src/enclave-builder/src/build.rs:355-361` and `:468` substitute it into
`run.sh.template` at `{{USER_CMD}}`; `src/enclave-builder/templates/Containerfile.eif`
copies `run.sh` into `/build/initramfs/run.sh`, cpio-archives the initramfs into
`rootfs.cpio.gz`, and passes it as the EIF's single `--ramdisk`. Per
`aws-nitro-enclaves-image-format-0.4.0/src/utils/mod.rs:664-692`, that ramdisk is
measured into PCR0 and PCR1. It is measured a second time via `manifest.json`,
which carries `run_command` as a field (`src/enclave-builder/src/manifest.rs:14-45`)
and is also copied into the initramfs.

So `shim/src/config.rs:110-114`'s design goal —

```rust
    /// ... Tuning it here changes the enclave config, NOT the binary,
    /// so `EXPECTED_SHA256` and the reproducibility trail stay put. ...
```

— is true of `EXPECTED_SHA256` and **false of the PCRs**. The configuration is
inside the attestation; it is simply not inside the artefact the README's third
step compares.

**Served.** `bootproofd` returns the manifest alongside the signed document
(`crates/bootproofd/src/routes/nonced_attestation.rs:20-29, 78-120`):

```rust
pub struct NoncedAttestationResponse {
    /// A base64 encoded attestation document.
    pub document: String,
    /// The manifest used to build the enclave.
    pub manifest: serde_json::Value,
}
```

so the missing check is:

```sh
curl -s -X POST "https://<domain>/attestation" \
  -H 'content-type: application/json' -d '{"nonce":"'"$(head -c32 /dev/urandom | base64)"'"}' \
| jq -r '.manifest.run_command'
```

which prints, for a shim, the literal `export ZIS_HUB_NYM='…'` line. The response
manifest is deliberately outside the COSE payload and is therefore unsigned on its
own; a completed `caution verify` is what certifies it, because the same string is
inside the ramdisk the PCRs cover.

### The certificate binding the recipe does not mention

In the enclave, `src/enclave-builder/templates/caddy-certfp.sh` polls the served
certificate every 60 s and publishes its fingerprint:

```sh
    if /usr/bin/openssl s_client -connect "${tls_address}" -servername "${caddy_domain}" \
        -showcerts -verify_return_error -verify_hostname "${caddy_domain}" \
        -purpose sslserver -CAfile "${ca_file}" ... ; then
        certfp="$( /usr/bin/openssl x509 -in "${served_leaf}" -noout -fingerprint -sha256 \
                   | cut -d= -f2 | tr -d ': ' | tr 'A-F' 'a-f' )"
        ...
            if printf '{"tls":{"mode":"tls","domain":"%s","certfp":"%s"}}\n' \
                "${caddy_domain}" "${certfp}" >"${metadata_tmp}" ...
```

`bootproofd` passes `/metadata.json` as the attestation's `user_data`
(`nonced_attestation.rs:88-118` → `crates/bootproof/src/format/nitro.rs:22-62`),
and `caution verify` enforces it (`src/cli/src/lib.rs:353-384`):

```rust
    anyhow::ensure!(user_data.tls.mode == "tls", "attested TLS mode is not tls");
    anyhow::ensure!(user_data.tls.domain == expected.domain, ...);
    anyhow::ensure!(user_data.tls.certfp.len() == 64 && ... , "attested TLS certfp is not lowercase SHA-256 hex");
    anyhow::ensure!(user_data.tls.certfp == observed_certfp,
        "attested TLS certfp does not match the live leaf certificate");
```

where `observed_certfp` is `sha256` of the leaf DER of the same
redirect-disabled, WebPKI-validated response
(`src/cli/src/lib.rs:6892-6963`). `expected.domain` comes from the *reproduced*
`caution.hcl` (`tls_expectation_from_config`, `:286-312`), so it too is
PCR-bound. The project has observed this working:
`shim/deploy/caution/OPERATORS.md:189-194` records the 2026-08-14 pair verifying
"with the TLS certificate binding verified".

The two paths that skip it and still print PASSED are
`TlsVerification::PcrOnly` (`--pcrs`) and `TlsVerification::SkippedNoDns`
(`src/cli/src/lib.rs:6919-6932`), reported as
`"TLS certificate binding: not performed (--pcrs)"` and
`"TLS certificate binding: skipped because the configured domain has no DNS answer"`.

### PCR2, and why "check the PCRs" needs a criterion

Caution's `Containerfile.eif` calls `eif_build` with exactly one `--ramdisk`.
`aws-nitro-enclaves-image-format-0.4.0/src/utils/mod.rs:674-691` sends ramdisk 0
to the bootstrap hasher and only ramdisks 1..n to the customer-app hasher, so the
customer-app hasher receives nothing; `defs/eif_hasher.rs`'s
`tpm_extend_finalize_reset()` then returns `sha384(0^48 ‖ sha384(""))`, which
computes to `21b9efbc18480766…` — the exact value both `OPERATORS.md` files record
as "PCR2, identical across shim and hub". By the same token PCR0 and PCR1 are fed
the identical byte stream (kernel ‖ cmdline ‖ ramdisk0) and are therefore equal,
which is also what those tables record. "Check the PCRs" without a criterion is
satisfied by checking a constant.

### The hash that is compared to nothing

`src/enclave-builder/src/docker.rs:69` — `format!("docker build -f {} .", containerfile)`,
no `--target`, so the platform builds the final stage (`runtime`).
`*/deploy/reproduce.sh` builds `--target export`. `caution verify` never reads
`EXPECTED_SHA256`; `reproduce.sh` never produces a PCR.

## Recommendations

1. **Rewrite `README.md:71` as five steps, with criteria.** Suggested:
   *"Auditors verify an endpoint without trusting its operator: run `caution
   verify --attestation-url https://<domain>/attestation` from a Linux/x86_64
   checkout and require all of `✓ Base Nitro attestation and expected PCR0/1/2
   verified`, `✓ TLS certificate binding verified` and `✓ Attestation verification
   PASSED` — a run that says `TLS certificate binding: skipped` or `not performed`
   has not verified the endpoint; confirm the commit it staged is the commit you
   reviewed; read `unit.env` in the reproduced `caution.hcl` (or
   `.manifest.run_command` from the `/attestation` response) and confirm
   `ZIS_HUB_NYM` is a hub you trust and `ZIS_CAUTION_ATTESTATION` is absent or
   true; and separately run `sh zeronym/shim/deploy/reproduce.sh` against
   `EXPECTED_SHA256`, noting that nothing joins that hash to the attestation."*
2. **Publish the expected `ZIS_HUB_NYM` value.** The hub's Nym address is public by
   design (`GET /nym-address`). Shielded Labs should publish the canonical value
   next to the README so "is this the right hub?" is a string comparison rather
   than a judgement call, and note that a shim naming any other hub is not a
   zero-indexer deployment.
3. **Add the same steps to both `OPERATORS.md` "Verify" sections** and replace
   `deploy.sh:220`'s printed advice with them.
4. **Drop Certificate Transparency from the recipe, or demote it.** It is the
   weakest of the available defences here and its presence displaces the strong
   one. Keep it as a supplementary monitoring recommendation with an honest note
   that the enclave's own re-issuance makes the channel noisy.
5. **Consider having `deploy.sh` run `caution verify` on the attested path** and
   fail the deploy unless all three success lines appear. A verification step that
   is only ever described is one that frequently does not happen.

Cross-references: `operators-runbook-attributes-the-hub-destination-to-the-binary-hash-and-egress-rules-neither-of-which-binds-it.md`
(the same trust boundary — confirmed Low, renamed from
`shim-config-hub-identity-is-unattested-unobservable-operator-configuration.md`);
`deploy-script-tells-operators-to-expect-pcr01-failure-and-accept-pcr2-alone.md`
(the PCR criterion, with the PCR2 derivation appended);
`attested-tls-binding-is-verified-once-by-hand-if-ever-so-operator-certificate-substitution-has-an-unbounded-undetected-window.md`
(the CT half — confirmed Medium, renamed from
`wallet-facing-tls-identity-is-not-bound-to-the-attestation-and-the-ct-defence-cannot-work.md`);
`reproduce-never-builds-the-runtime-stage-that-the-enclave-and-pcr0-are-built-from.md`
and `hub-caution-readme-says-the-attestation-binds-the-running-binary-to-expected-sha256.md`
(the hash half).

---

## ADDENDUM (Global audit G30/G32/G17, 2026-08-18) — two amendments to the recommendations, and a fifth omission the recipe has

Nothing above is retracted. Three things found while sweeping adversary #1's
full capability set bear directly on this issue's recommendations.

**1. Recommendation 2 must require exact list equality, not membership.**
`NymHandle::submit` sends every diverted transaction to **every** address in
`ZIS_HUB_NYM` (`shim/src/nym.rs:602`, `:642`), and the list is uncapped
(`shim/src/config.rs:262-289`). An operator who *appends* their own hub rather
than repointing gets a plaintext copy of every migration while the canonical hub
keeps publishing normally, so nothing observable changes anywhere — and a
checker asking "does `ZIS_HUB_NYM` name the canonical hub?" answers yes.
`README.md:90` compounds it by stating that submit *rotates* which address it
targets, which is true only of lookups. Filed as
`shim-submits-every-migration-to-every-configured-hub-so-an-operator-appends-their-own-and-gets-a-plaintext-copy-with-nothing-breaking.md`.

**2. There is a fifth omission, and it is larger than the four above: the recipe
never establishes that the tree `caution verify` reproduced from is
zero-indexer.** `caution verify` clones `app_source.urls` at `app_source.commit`
from the attested manifest, reads the `caution.hcl` from that clone, and rebuilds
the EIF from that clone (`src/cli/src/lib.rs:6432-6473`). Those URLs are the
operator's `--app-source`, and the repository is a per-deployment assembled tree
containing the full Rust source, so it is never `ShieldedLabs/zero` by
construction. No allow-list, no signature, no expected hash, and — since the
Containerfile deploy path passes `binary: None`
(`src/api/src/builder.rs:925-940`) — no binary hash in the attestation to fall
back on. Filed as
`caution-verify-reproduces-from-a-repository-the-operator-nominates-so-nothing-binds-the-attested-code-to-zeronym.md`
(High). Recommendation 1's rewritten `README.md:71` should gain a step for it.

**3. Two `caution.hcl` blocks are outside every measurement and should be named
in recommendation 1 as things to read rather than to verify by tool:**
`network.ingress` (the CIDRs reach only the security group — narrowing them can
force wallets through an operator-run relay) and `debug.ssh_keys` (which the
platform honours **irrespective of `debug.enabled`**, opening port 22 to
`0.0.0.0/0` on the enclave's parent host — see the addendum on
`hub-manifest-debug-block-claims-ssh-keys-render-empty-and-ssh-is-closed-under-attestation.md`).
Neither changes a PCR, so `caution verify` cannot speak to either; both are
visible in the published `caution.hcl` to a reader who is told to look.

## Validation Information

**Validated 2026-08-18. CONFIRMED at Medium.** All four omissions were checked in
the target at HEAD and against a **fresh** clone of the Caution platform
(`https://codeberg.org/caution/platform`, HEAD `6051734a`, 2026-08-18), the
`bootproof` repository, and `aws-nitro-enclaves-image-format 0.4.0` — not taken
on trust from the filing global audit.

### The recipe, and what the tree actually contains

`README.md:71` is verbatim as quoted, and it is the only verification instruction
in any user-facing document.

The two decisive greps over the whole `audit-target/zeronym` tree:

- **`run_command` and `.manifest`: zero occurrences.** No document, script, test
  or comment anywhere in zeronym mentions the field that carries the enclave's
  entire environment, even though `bootproofd` returns it in the body of every
  `/attestation` response (`crates/bootproofd/src/routes/nonced_attestation.rs:20-29`,
  `:82-85`, `:120` — `NoncedAttestationResponse { document, manifest }`). **Nobody
  is told the value is there, let alone told to read it.**
- **`certfp`: zero occurrences.** The string `TLS certificate binding` appears
  twice, both as *expected output* in a runbook narrative
  (`shim/deploy/caution/OPERATORS.md:192-193`, `hub/deploy/caution/OPERATORS.md:108`),
  never as a criterion to require and never with a warning that a `skipped` or
  `not performed` line invalidates the run. `Certificate Transparency` / `crt.sh`
  appears **seven** times, including in the `README.md:71` recipe itself.

### The measurement chain, re-derived

`ZIS_HUB_NYM` really is inside the attestation: `unit "default" { env = { … } }`
(`shim/deploy/caution/caution.hcl.tmpl:129-186`, filled by
`shim/deploy/caution/assemble-caution.sh:345-355`) →
`UnitConfig::run_command_string()` emits `export ZIS_HUB_NYM='…'`
(`src/caution-config/src/lib.rs:253-274`) → `{{USER_CMD}}` in the generated
`run.sh` (`src/enclave-builder/src/build.rs:468`,
`templates/run.sh.template:180`) → `/build/initramfs/run.sh` → the EIF's **single**
`--ramdisk` (`templates/Containerfile.eif:297-302`) → PCR0/PCR1
(`aws-nitro-enclaves-image-format-0.4.0/src/utils/mod.rs:660-691`, `:176-178`).
So the value is measured **and** disclosed, and the recipe still contains no step
that reads it. That is precisely open item 7a's sentence — *measurement discloses
a value; it never detects a change* — and this is the issue that owns the
"nobody is told to look" half of it.

The certfp binding also exists exactly as described:
`templates/caddy-certfp.sh` publishes `{"tls":{"mode","domain","certfp"}}` to
`/metadata.json`; `bootproofd` passes it as the COSE-signed `user_data`; and
`validate_attested_tls()` (`src/cli/src/lib.rs:354-385`) requires
`mode == "tls"`, `domain ==` the domain from the *reproduced* `caution.hcl`, a
64-char lowercase-hex `certfp`, and `certfp == sha256(leaf DER)` of the same
redirect-disabled, WebPKI-validated response. The recipe names crt.sh instead.

### The attack scenario holds under the 6q reversal

Checked specifically, because 6q reversed two neighbouring findings:
`deploy.sh:206-219` publishes the **deployed** tree as `--app-source`, and
`caution verify` reproduces PCRs from that tree
(`src/cli/src/lib.rs:7235-7305`), so an operator who repoints or appends
`ZIS_HUB_NYM` produces an enclave that verifies against itself and prints
`✓ Attestation verification PASSED`. Every one of the four documented steps
passes on a shim pointed at a hub the operator runs. The evidence is one field
away in the response body of a request the auditor already made.

### Corrections made during validation

1. **The `SkippedNoDns` claim was narrowed** (see the Description). It is
   reachable only on the raw-IP flow, not on the `https://<domain>` flow both
   runbooks prescribe, so the NXDOMAIN-window remark does not apply to the
   documented path. The residual — `PASSED` printed alongside a skip warning that
   no zeronym document calls disqualifying — survives.
2. **Omission 4 was softened from "the recipe omits it" to "the recipe omits it
   and nothing requires it".** Both runbooks *do* mention the binding, as
   expected output. What is missing is the requirement and the failure criterion.

### Severity: Medium — and this is the explicit ownership decision the coordinator asked for

One harm — *an operator can point users' plaintext somewhere else and every
documented check still passes* — is spread across four files. The allocation set
by the earlier validation of
`operators-runbook-attributes-the-hub-destination-to-the-binary-hash-and-egress-rules-neither-of-which-binds-it.md`
is retained deliberately, and **this issue absorbs the "no document tells anyone
to perform the check" harm at Medium while the siblings stay where they are**:

| Issue | Owns | Severity |
|---|---|---|
| `shim-submits-every-migration-to-every-configured-hub-…-appends-their-own-…` | the **capability** (fan-out to every address, ack unread) | High (confirmed) |
| `caution-verify-reproduces-from-a-repository-the-operator-nominates-…` | the **capability** (nothing binds the reproduced tree to zeronym) | High (confirmed) |
| **this issue** | the **procedure gap**: `README.md:71` never reads the configuration, and names CT instead of the binding | **Medium** |
| `operators-runbook-attributes-the-hub-destination-…` | the **false assurance** ("an operator cannot silently repoint it") | Low (confirmed) |
| `attested-tls-binding-is-verified-once-by-hand-if-ever-…` | the **time-of-check-only** binding | Medium (confirmed) |

The reverse allocation (this issue Low, the runbook issue Medium) was considered
and rejected: `README.md:71` is the *only* verification instruction any
user-facing document gives, it is what the product offers in place of trusting
the operator, and the omission it contains is the enabling condition for **both**
confirmed Highs — whereas the runbook sentence is a false assurance in a document
only operators read. Medium here and Low there is the right way round.

**Not High.** It is not itself an attack; the exploitable capabilities are filed
and graded above, and grading this High would count the same harm twice. **Not
Low.** The omitted check costs one HTTP request, the omitted criterion is one
output line, and per the G10/G12/G13 sweep this is the cheapest high-value fix in
the entire attestation area.

### What is *not* counted in this severity

- **Omission 2** (no PCR criterion) — the wrong-criterion half is owned by
  `deploy-script-tells-operators-to-expect-pcr01-failure-and-accept-pcr2-alone.md`
  (confirmed Medium). This issue keeps only the narrower point that the recipe
  names no criterion at all, which is what leaves the door open for it.
- **Omission 3** (the hash joined to nothing) — owned by
  `reproduce-never-builds-the-runtime-stage-that-the-enclave-and-pcr0-are-built-from.md`
  and `hub-caution-readme-says-the-attestation-binds-the-running-binary-to-expected-sha256.md`.
- **The addendum's "fifth omission"** (`caution verify` reproduces from an
  operator-nominated repository) — since filed and **confirmed High** as
  `caution-verify-reproduces-from-a-repository-the-operator-nominates-so-nothing-binds-the-attested-code-to-zeronym.md`.
  It remains here only as a step the rewritten recipe must gain.
- **The addendum's `debug.ssh_keys` and `network.ingress` points** — owned by
  `hub-manifest-debug-block-claims-ssh-keys-render-empty-and-ssh-is-closed-under-attestation.md`
  and `attested-enclave-console-is-reopenable-from-the-parent-because-debug-mode-is-a-launch-flag-and-ssh-keys-is-not-gated-on-it.md`.

So this issue's Medium rests on **omissions 1 and 4 only** — the two that are
unique to `README.md:71` and owned nowhere else.

### One constraint on Recommendation 1 that the report must carry

The check this issue recommends — read `ZIS_HUB_NYM` out of
`.manifest.run_command` — terminates in an **unanchored value**. A Nym address is
not a name that gets authenticated; it *is* the recipient's key material, so an
auditor who performs the check obtains a base58 string with nothing to compare it
against (open item 6z, and
`hub-nym-identity-has-no-trust-anchor-and-the-one-the-project-already-owns-is-never-applied-to-it.md`).
Recommendation 2 (publish the canonical value) is therefore **not optional and
must ship first or together** — and the comparison must be **exact whole-list
equality**, not membership, because `NymHandle::submit` sends to every address in
the list; and a **missing** `ZIS_HUB_NYM` line in `.manifest.run_command` must be
treated as a failure, not a pass, since `run_command_string()` silently skips
`env::vault(...)` entries (`src/caution-config/src/lib.rs:253-263`).

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
