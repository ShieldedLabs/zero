# `deploy.sh` tells the operator to expect a PCR0/PCR1 verification failure and to accept a PCR2 match alone — advice both `OPERATORS.md` files now say is wrong, because PCR2 is byte-identical across two entirely different binaries

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/deploy.sh:220` (the only verification guidance the deploy script emits); the same claim repeated at `audit-target/zeronym/shim/deploy/caution/README.md:131-136`; contradicted by `audit-target/zeronym/shim/deploy/caution/OPERATORS.md:189-209` and `audit-target/zeronym/hub/deploy/caution/OPERATORS.md:107-124`; the user-facing promise at `audit-target/zeronym/README.md:71`
**Found by agent:** Local (file audit of `deploy.sh`)
**In scope of audit?** Yes — `deploy.sh` and `*/deploy/**` are in scope, and markdown/operator claims are in scope "as security claims": under ICTM a documented property users are told they get but do not get is itself a bug.

## Description

`deploy.sh` never verifies anything about the artifact it deploys — it does not
run `build.sh`, does not run `reproduce.sh`, does not compare `EXPECTED_SHA256`
to anything, and does not run `caution verify`. Its **entire** contribution to
verification is one log line, printed at the end of every attested deploy
(`deploy.sh:220`):

```sh
log "verify with: caution verify (expect PCR0/1 FAILED on Caution's floating framework; PCR2 is the check that matters)"
```

That single line makes two assertions, and the project's own operator runbooks —
which were updated after measurements taken on 2026-08-14 — say both are wrong:

1. **"expect PCR0/1 FAILED"** — `shim/deploy/caution/OPERATORS.md:189-193`:

   > Older copies of this guide warned that verify reports PCR0/1 FAILED on a
   > healthy enclave, because Caution's builder fetched its framework from a
   > floating `main.tar.gz`. **That is fixed**: on the attested pair deployed
   > 2026-08-14 the manifest pinned both the enclave and framework sources to
   > commits, and **all three PCRs reproduced** on both … Expect a clean
   > `✓ Attestation verification PASSED`.

2. **"PCR2 is the check that matters"** — `shim/deploy/caution/OPERATORS.md:195-209`:

   > **Do not fall back to "PCR2 is the one that matters".** That advice
   > circulated while PCR0/1 were failing, and it is wrong on this platform.
   > Measured 2026-08-14 across the attested shim and hub — two entirely
   > different binaries:
   >
   > | | shim | hub |
   > |---|---|---|
   > | PCR0 | `accb679a…` | `218d1f64…` |
   > | PCR1 | `accb679a…` | `218d1f64…` |
   > | PCR2 | `21b9efbc…` | `21b9efbc…` **(identical)** |
   >
   > **PCR2 does not distinguish the application.** PCR0/PCR1 are what change
   > with it. So an attestation accepted on a PCR2 match alone would prove only
   > that *some* Caution enclave is running, not that it is running your
   > reviewed code — which is the entire claim. Require **all three** to
   > reproduce, and treat a PCR0/1 mismatch as a real finding about the
   > application until proven otherwise.

`hub/deploy/caution/OPERATORS.md:112-124` says the same thing in the same words.

So the deploy script trains the operator, at the exact moment they are about to
verify, to (a) **pre-accept the failure of the only two measurements that
distinguish one application from another**, and (b) **treat as sufficient the one
measurement the project has empirically shown to be identical between two
entirely different binaries**. This is the classic "train the user to ignore the
alarm" failure, and here the alarm is the whole product's trust root.

## Attack Scenario and Steps

1. An operator, or a third party acting as the "Auditor" role `README.md:71`
   defines, deploys or inspects an endpoint and runs `caution verify
   --attestation-url https://<tls-domain>/attestation`.
2. The application differs from the reviewed one, so PCR0/PCR1 differ. `caution
   verify` requires **all three** PCRs together — `Nitro::new(attestation_bytes,
   expected_nitro_pcrs)` is built from PCR0, PCR1 and PCR2 and `nitro.verify(...)`
   is a single all-or-nothing check (Caution platform `src/cli/src/lib.rs:7235-7256`).
   It therefore prints `✗ Attestation verification FAILED` and exits non-zero —
   and then prints a per-index breakdown (`src/cli/src/lib.rs:7313-7337`):

   ```
   ✗ Attestation verification FAILED
   PCR comparison:
     PCR0: MISMATCH
     PCR1: MISMATCH
     PCR2: match
   ```

   That breakdown is exactly the shape the bad advice was written for.
3. The verifier has been told in advance, by the project's own tooling
   (`deploy.sh:220`), to **expect** `PCR0/1 FAILED` and that `PCR2 is the check
   that matters`. They read `PCR2: match`, override the `FAILED` banner, and
   record the endpoint as verified.
4. Nothing about the application was actually checked. Per the project's own
   measurement, PCR2 `21b9efbc…` is produced by both the shim and the hub, so a
   PCR2 match is consistent with *any* Caution enclave — including a shim built
   with diversion disabled, with `RUST_LOG` raised, with the classifier
   predicate weakened, or an entirely unrelated binary.

**Attack Requirements and Assumptions:**

- No special attacker capability is needed to produce the *misleading guidance*;
  it is printed unconditionally on every attested deploy.
- To *exploit* it, a party who can influence what image runs — the operator, or
  the platform — deploys code other than the reviewed code. The operator is the
  primary adversary in this system's threat model and controls the deploy, so
  this is not a hypothetical actor.
- The verifier must be someone who follows the tooling's own printed advice
  rather than cross-reading `OPERATORS.md`. That is the realistic case: the
  deploy script's line is what appears on the terminal at the moment of use, and
  `shim/deploy/caution/README.md:133-135` independently repeats the same wrong
  advice, so two of the four in-tree sources agree with the script.
- **Honest counterweight:** an auditor who reads either `OPERATORS.md` gets the
  correct instruction, in bold, with the measurement table. The defect is a
  contradiction inside the tree, not a uniform overclaim — but the contradiction
  is resolved the wrong way by the artifact that speaks last and loudest.

## Impact on Users

`README.md:71` sells the following to users as the reason they do not have to
trust their indexer operator:

> **Auditors** verify an endpoint without trusting its operator: fetch its
> attestation, check the PCRs against the AWS Nitro root, reproduce the build and
> compare hashes, and check Certificate Transparency for a shadow certificate.

That promise is the substitute for trusting the operator, and it is the only one
a wallet user has. If the PCR check as *instructed by the deploy tooling* cannot
distinguish the reviewed shim from an arbitrary other enclave, then a "verified"
endpoint carries no more assurance than an unverified one, and the user's
protection reduces to trusting the operator after all — which is precisely what
the product exists to remove.

The failure is silent from the user's side: a wallet sees a valid certificate and
correct gRPC behaviour whether or not the enclave runs the reviewed code.

This composes with `wallet-facing-tls-identity-is-not-bound-to-the-attestation-and-the-ct-defence-cannot-work.md`:
that issue shows an auditor may be talking to a proxy rather than the enclave;
this one shows that even when they *are* talking to the enclave, the check they
are told to run does not bind the application.

## Technical Details / Code Analysis

**The line, in context** (`deploy.sh:206-221`, the attested-deploy publish
block):

```sh
if [ "$DEBUG" != 1 ] && [ -n "${APP_SOURCE:-}" ]; then
  APP_SOURCE_PUSH=${APP_SOURCE_PUSH:-$APP_SOURCE}
  APP_SOURCE_TAG=${APP_SOURCE_TAG:-deploy-$APP_ID}
  log "publishing the app-source to $APP_SOURCE_PUSH (tag $APP_SOURCE_TAG) ..."
  ( cd "$DEST" &&
    { git remote remove app-source 2>/dev/null || true; } &&
    git remote add app-source "$APP_SOURCE_PUSH" &&
    git push app-source HEAD:main &&
    git tag -f "$APP_SOURCE_TAG" &&
    git push -f app-source "refs/tags/$APP_SOURCE_TAG"
  ) >&2 || die "app-source publish FAILED. …"
  log "app-source published: $APP_SOURCE @ $APP_SOURCE_TAG"
  log "verify with: caution verify (expect PCR0/1 FAILED on Caution's floating framework; PCR2 is the check that matters)"
fi
```

This is the last thing an operator reads before the DONE banner, and it is the
only verification instruction the script gives.

**The corrected instruction** (`hub/deploy/caution/OPERATORS.md:107-124`):

```
Expect `✓ Attestation verification PASSED` with all three PCRs reproducing and
the TLS certificate binding verified. Measured on the first attested hub
(2026-08-14): all of PCR0/1/2 matched …

**Require all three PCRs. Do not accept a PCR2 match alone.** … measured
2026-08-14, the attested hub and the attested shim — two entirely different
binaries — produced **byte-identical PCR2** (`21b9efbc…`), while PCR0/PCR1
differed per application (`218d1f64…` hub, `accb679a…` shim). PCR2 does not
distinguish the application, so an attestation accepted on PCR2 alone would prove
only that *some* Caution enclave is running, not that it is running the reviewed
hub — which for the component holding plaintext migrations is the whole point.
```

**The full set of in-tree PCR claims, which do not agree with one another**
(established by `SURVEY.md` §9 observation 11 and re-checked here):

| Source | Claim |
|---|---|
| `deploy.sh:220` | expect PCR0/1 FAILED; **PCR2 is the check that matters** |
| `shim/deploy/caution/README.md:133-135` | **PCR2 (the application layer) is the check that matters** |
| `shim/deploy/caution/OPERATORS.md:189-209` | all three reproduce; **require all three**; PCR2 alone proves nothing |
| `hub/deploy/caution/OPERATORS.md:107-124` | all three reproduce; **require all three**; PCR2 alone proves nothing |
| `OPEN-QUESTIONS.md:86` | PCR0/PCR1 not reproducible; "PCR2 is the measurement that carries weight today" |
| `shim/deploy/README.md:1055-1056` | no PCR0, PCR1 or PCR2 has ever been computed from this image |

Two of these tell a verifier to accept a result the other two say proves nothing.
`deploy.sh:220` is the one that is executed.

**Note on what `deploy.sh` does *not* do.** It never binds the reproducible hash
to the attestation either: `EXPECTED_SHA256` is not read, compared, or mentioned
by this script, and per BRAINSTORM §R12-F nothing in the tree binds
`EXPECTED_SHA256` to any PCR — `caution verify` clones `app_sources`, rebuilds,
and compares PCRs without ever consulting it. So the two halves of the chain that
`README.md:71` asks an auditor to join ("check the PCRs … reproduce the build and
compare hashes") are joined nowhere, and the only PCR guidance the deploy path
gives is the incorrect one above.

## Recommendations

1. **Replace `deploy.sh:220` with the runbook's own text**, e.g.:
   `log "verify with: caution verify --attestation-url https://$TLS_DOMAIN/attestation — expect PASSED with ALL THREE PCRs reproducing. A PCR0/1 mismatch is a real finding; PCR2 alone does not distinguish the application (see OPERATORS.md)."`
2. **Fix the same claim at `shim/deploy/caution/README.md:131-136`**, and repair
   its cross-reference: the sentence sends the reader to `OPERATORS.md` "for the
   current PCR0/1 caveat", and `OPERATORS.md:189-194` is the document that
   *retracts* that caveat. (`OPEN-QUESTIONS.md:86` carries the same superseded
   rule and needs the same fix, but it is owned by a separate issue —
   `open-questions-pcr-entry-inverts-which-measurement-distinguishes-the-application.md`
   — and is deliberately **not** counted in this issue's severity.)
3. Consider having `deploy.sh` *run* `caution verify` on the attested path and
   fail the deploy if it does not report `PASSED`, rather than printing advice
   about it. A verification step that is only ever described is one that
   frequently does not happen.
4. Add a single authoritative statement of the PCR policy in one place and make
   every other document reference it, so this class of drift is structurally
   prevented — the tree currently carries six mutually inconsistent statements.

## Addendum (added by the `shim/deploy/caution/README.md` local audit, 2026-08-18) — three facts about the second location; nothing here changes the finding or its severity

This issue's Location already names `shim/deploy/caution/README.md:133-135`, so
the coordinator's question ("does the filed issue name this file too?") is
answered **yes** and no second issue was filed. Three corrections/additions:

1. **The exact range is `:131-136`, not `:133-135`.** The sentence begins on line
   131 and the caveat runs to 136. Verbatim:

   ```
   Third parties can run
   `caution verify --attestation-url https://<domain>/attestation` with no Caution
   account and no checkout. See OPERATORS.md for the current PCR0/1 caveat: a
   Caution-side unpinned-framework bug makes verify report FAILED on healthy
   enclaves, and PCR2 (the application layer) is the check that matters until
   their fix lands.
   ```

2. **The cross-reference is worse than the claim.** *"See OPERATORS.md for the
   current PCR0/1 caveat"* sends the reader to
   `shim/deploy/caution/OPERATORS.md:189-194`, which does not contain that caveat —
   it **retracts** it, in those words: *"Older copies of this guide warned that
   verify reports PCR0/1 FAILED on a healthy enclave, because Caution's builder
   fetched its framework from a floating `main.tar.gz`. **That is fixed** …
   **all three PCRs reproduced** on both … Expect a clean `✓ Attestation
   verification PASSED`."* So the README does not merely repeat stale advice; it
   attributes that advice to the document that repudiates it. A reader who trusts
   the summary is misinformed; a reader who follows the pointer gets the right
   answer and a contradiction. That asymmetry is worth one sentence in the report,
   because it is the same shape as `OPERATORS.md:33` → this file (recorded in
   BRAINSTORM §R18-A): the two documents each point at the other's wrong half.

3. **This file supplies the *operative* criterion, because the other reference
   document supplies none.** The `shim/deploy/README.md` audit established
   (`deploy-readme-says-no-enclave-no-eif-and-no-pcr-exist-which-its-own-current-row-refutes.md`,
   BRAINSTORM §R31-D) that the reproducible-build reference text asserts no PCR of
   any index has ever been computed, and that the strings `caution verify`,
   `PCR0`, `PCR1`, `PCR2` appear there **only** inside sentences denying any of it
   happened. `shim/deploy/caution/README.md` is therefore the only one of the two
   deploy reference documents that gives an auditor a PCR rule at all — and the
   rule it gives is the retracted one. Net effect on a document-following auditor:
   the reproducible-build half offers no criterion, the attestation half offers
   the wrong criterion, and only the two `OPERATORS.md` runbooks (which the
   attestation half misquotes) carry the correct one. `deploy.sh:220` then prints
   the wrong criterion at the moment of use.

Everything else in this issue was re-verified against `shim/deploy/caution/README.md`
at HEAD and is accurate as written.


## ADDENDUM (added 2026-08-18 by the G10/G12/G13 global audit) — PCR2 on Caution is a universal constant, and here is the derivation. This does not change the finding; it makes it far worse and answers the caveat `OPERATORS.md` itself flags as unresolved.

`shim/deploy/caution/OPERATORS.md:210-211` ends its measurement table with an
honest caveat: *"(The observation is empirical; we have not confirmed with Caution
which layer each index measures on their EIF layout.)"* That question is now
answered, from Caution's own public source
(`https://codeberg.org/caution/platform`, cloned during this audit) and
`aws-nitro-enclaves-image-format 0.4.0` from crates.io.

**Caution builds its EIF with exactly one ramdisk.**
`src/enclave-builder/templates/Containerfile.eif`, final stage:

```
RUN eif_build \
    --kernel /build/kernel/bzImage \
    --kernel_config /build/kernel/linux.config \
    --ramdisk /build/rootfs.cpio.gz \
    --output /build/enclave.eif \
    --pcrs_output /build/enclave.pcrs \
    --cmdline "reboot=k panic=1 pci=off nomodules console=ttyS0 ... nit.target=/run.sh"
```

**`EifBuilder::measure`** (`aws-nitro-enclaves-image-format-0.4.0/src/utils/mod.rs:664-692`):

```rust
self.image_hasher.write_all(&buffer[..]).unwrap();       // kernel  -> PCR0
self.bootstrap_hasher.write_all(&buffer[..]).unwrap();   // kernel  -> PCR1
self.image_hasher.write_all(&self.cmdline[..]).unwrap();
self.bootstrap_hasher.write_all(&self.cmdline[..]).unwrap();
for (index, mut ramdisk) in self.ramdisks.iter().enumerate() {
    ...
    self.image_hasher.write_all(&buffer[..]).unwrap();               // PCR0: all ramdisks
    if index == 0 { self.bootstrap_hasher.write_all(&buffer[..]).unwrap(); }   // PCR1: the first
    else          { self.customer_app_hasher.write_all(&buffer[..]).unwrap(); } // PCR2: the rest
}
```

With one ramdisk, three things follow **mathematically**:

1. **PCR0 and PCR1 are computed over the identical byte stream** — kernel ‖
   cmdline ‖ ramdisk0 — so **PCR0 == PCR1 on every Caution enclave**. That is
   exactly what this issue's own quoted table shows (`accb679a…` in both shim
   rows, `218d1f64…` in both hub rows). The table is not a transcription error.
2. **`customer_app_hasher` receives nothing at all.**
   `defs/eif_hasher.rs` with `new_without_cache` (`block_size == 0`) makes
   `finalize_reset()` return `sha384(<bytes written>)` and
   `tpm_extend_finalize_reset()` return `sha384(0^48 ‖ that)`. Therefore

   **PCR2 = sha384( 48 zero bytes ‖ sha384("") )**

   which computes to **`21b9efbc18480766…`** — the exact value both `OPERATORS.md`
   files record as PCR2 for *both* components. (Reproduce in one line:
   `python3 -c "import hashlib;print(hashlib.sha384(b'\0'*48+hashlib.sha384(b'').digest()).hexdigest())"`.)
3. So PCR2 is not "a measurement that happens not to distinguish the shim from the
   hub". **It is the measurement of an absent application ramdisk: a fixed
   constant, identical for every Caution enclave that has ever booted, whatever
   code it runs.**

**What this does to the severity of the advice at `deploy.sh:220`.** The line does
not tell an auditor to rely on a weak check. It tells them to pre-authorise the
failure of the only two measurements that exist, and to accept in their place a
value that is *the same for every Caution deployment on earth*. A verifier who
follows it runs a check that **cannot fail**, for any enclave, ever. The same
applies verbatim to `shim/deploy/caution/README.md:131-136` and to
`OPEN-QUESTIONS.md:86`, which asks security reviewers to ratify it.

**One correction to the runbooks' otherwise-correct advice, worth a line in the
report.** "Require **all three** to reproduce" is the right operational rule but
implies threefold redundancy that does not exist: there is **one** independent
measurement (PCR0, duplicated as PCR1) plus one constant. The right statement is:
*Caution's attestation carries a single measurement, over the kernel, the kernel
command line, and one ramdisk containing the whole enclave — EnclaveOS `init`,
`bootproofd`, `caddy`, `caddy-certfp.sh`, busybox, socat, the generated `run.sh`
(which carries the entire `unit.env`), `manifest.json`, and the application
filesystem. It appears twice, as PCR0 and PCR1.*

That last point also settles what the measurement **covers**, which is broader
than any zeronym document says: `run.sh` embeds every `ZIS_*`/`ZIH_*` value from
`caution.hcl`, so the configuration is inside the attestation even though it is
outside `EXPECTED_SHA256`. See `BRAINSTORM.md` §G10-A and §G12-A.

*(End of addendum.)*

## Validation Information

**Validated 2026-08-18. CONFIRMED at Medium.** Every load-bearing fact was
re-derived from primary sources rather than accepted from the file: the target
tree at HEAD, a fresh clone of the Caution platform
(`https://codeberg.org/caution/platform`, HEAD `6051734a`, 2026-08-18, plus the
earlier `1f8d8cb3` clone the global audit used), and
`aws-nitro-enclaves-image-format 0.4.0` from crates.io.

### The four claims, each checked

1. **`deploy.sh:220` is verbatim as quoted, and it is the only verification
   guidance the script emits.** Confirmed by reading `deploy.sh` in full: the
   strings `EXPECTED_SHA256`, `build.sh` and `reproduce.sh` appear nowhere as
   invocations, and `caution verify` appears only at `:198` (a comment), `:220`
   (this line) and `:332` (the DONE banner, which says only `run: caution
   verify` with no criterion). So the script prints advice about verification
   and performs none.

2. **The runbooks say the opposite, in bold, and they are the correct source.**
   `shim/deploy/caution/OPERATORS.md:189-193` retracts the PCR0/1-will-fail
   caveat ("**That is fixed** … **all three PCRs reproduced** on both … Expect a
   clean `✓ Attestation verification PASSED`"); `:195-209` retracts the
   PCR2-only rule and carries the measurement table;
   `hub/deploy/caution/OPERATORS.md:107-124` states the same in prose. Both
   verified in the target at HEAD.

3. **The "floating framework" premise the line relies on is false in the
   platform today.** Caution pins the framework to a commit:
   `require_platform_framework_commit()` (`src/api/src/builder.rs:331`) and
   `pin_archive_url_to_commit(..., &request.framework_commit, ...)` (`:1030`),
   with `framework_commit` recorded in the manifest
   (`src/api/src/builder.rs:174`, `main.rs:1959`, `:2702`). The only surviving
   `main.tar.gz` reference is a unit-test fixture (`build.rs:1090`). So a
   PCR0/PCR1 mismatch today is not the benign condition the line describes.

4. **PCR2 is a universal constant — the addendum's derivation is correct, and I
   reproduced it independently.**
   - `src/enclave-builder/templates/Containerfile.eif:297-302` calls `eif_build`
     with exactly **one** `--ramdisk /build/rootfs.cpio.gz` (identical in both
     platform clones).
   - `aws-nitro-enclaves-image-format-0.4.0/src/utils/mod.rs:660-691`
     (`EifBuilder::measure`) feeds kernel ‖ cmdline to both `image_hasher` and
     `bootstrap_hasher`, then feeds ramdisk index 0 to `bootstrap_hasher` and
     only indices `1..n` to `customer_app_hasher`. With one ramdisk the
     customer-app hasher is written **zero bytes**, and PCR0 and PCR1 are
     computed over the identical byte stream.
   - `src/utils/mod.rs:233-239` constructs all three with
     `EifHasher::new_without_cache(...)`, i.e. `block_size == 0`, so
     `finalize_reset()` returns `sha384(<bytes written>)`
     (`src/defs/eif_hasher.rs:85-90`) and `tpm_extend_finalize_reset()` returns
     `sha384(0^48 ‖ that)` (`:97-104`).
   - `src/utils/mod.rs:176-178` maps `image_hasher → PCR0`,
     `bootstrap_hasher → PCR1`, `app_hash → PCR2`.
   - Computed here:
     `sha384(0^48 ‖ sha384("")) =`
     `21b9efbc184807662e966d34f390821309eeac6802309798826296bf3e8bec7c10edb30948c90ba67310f7b964fc500a`.
     Both `OPERATORS.md` files record PCR2 as `21b9efbc…` for **both**
     components; the recorded prefix matches the derived value. The tables also
     record PCR0 == PCR1 per component (`accb679a…` shim, `218d1f64…` hub),
     which is exactly what the one-ramdisk layout forces. The tables are not
     transcription errors, and `OPERATORS.md:210-211`'s own caveat ("we have not
     confirmed with Caution which layer each index measures") is now answered.

   **Consequence, and it is the finding's real weight:** `deploy.sh:220` does
   not recommend a weak check. It recommends a check that **cannot fail for any
   Caution enclave**, because the value it tells the verifier to accept is
   identical for every enclave the platform has ever built, whatever code is
   inside it.

### The attack path is real, with one mechanical correction now folded into the body

`caution verify` is **all-or-nothing**: PCR0, PCR1 and PCR2 go into
`Nitro::new(...)` together and a mismatch on any of them produces
`✗ Attestation verification FAILED` plus a non-zero exit
(`src/cli/src/lib.rs:7235-7256`, `:7313-7338`). So following `deploy.sh:220`
requires a human to **override an explicit tool-level failure** — it is not a
silent pass. The original Attack Scenario implied verify would report a
per-PCR verdict without an overall failure; that has been corrected in place.

This does not weaken the finding, because the tool prints the per-index
breakdown (`PCR0: MISMATCH / PCR1: MISMATCH / PCR2: match`) directly beneath the
failure banner, which is precisely the reading `deploy.sh:220` pre-authorises.
Training a verifier to expect and discount an alarm is the whole mechanism here,
and the alarm in question is the only application-distinguishing measurement the
platform produces.

### Why Medium, and not higher or lower

- **Not High.** It grants an attacker nothing on its own; a second actor (an
  operator or platform deploying code other than the reviewed code) is required,
  and the two `OPERATORS.md` runbooks give the correct rule in bold with the
  measurement table. `caution verify` also fails loudly. Exploitation needs the
  verifier to follow the wrong one of two conflicting in-tree instructions.
- **Not Low.** The instruction is emitted by the tooling **at the moment of
  use**, on every attested deploy, and the second carrier
  (`shim/deploy/caution/README.md:131-136`) is the only one of the two deploy
  *reference* documents that gives an auditor any PCR rule at all — the other
  (`shim/deploy/README.md:1055-1056`) asserts no PCR has ever been computed. The
  check being disabled is the one that distinguishes the reviewed shim from any
  other enclave, and `README.md:71` sells that check to users as the substitute
  for trusting their operator. A defect that switches off the trust root's only
  discriminating measurement, in the project's own tooling, is squarely Medium.

### Scope, and how double-counting was avoided

The superseded PCR2-only rule survives in **three** carriers. This issue owns
**two**:

- `deploy.sh:220` (the tooling, at the moment of use), and
- `shim/deploy/caution/README.md:131-136` (the deploy reference document, whose
  cross-reference additionally attributes the retracted rule to the file that
  retracts it).

The third, `OPEN-QUESTIONS.md:86`, is owned by
`open-questions-pcr-entry-inverts-which-measurement-distinguishes-the-application.md`
and is **excluded** from this issue's severity — it is referenced here only so
the census is complete. Recommendation 2 was rewritten to say so.

Two neighbouring facts are also owned elsewhere and are **not** counted here:
that nothing joins `EXPECTED_SHA256` to any measured value
(`reproduce-never-builds-the-runtime-stage-that-the-enclave-and-pcr0-are-built-from.md`,
`hub-caution-readme-says-the-attestation-binds-the-running-binary-to-expected-sha256.md`),
and that `README.md:71` names no PCR criterion at all
(`auditor-recipe-omits-the-two-checks-that-decide-where-plaintext-goes-and-names-a-defence-the-platform-does-not-rely-on.md`).
This issue is specifically about a document naming the **wrong** criterion, not
about documents naming **none**.

### One correction to the runbooks that the report should carry

"Require **all three** to reproduce" is the right operational rule but implies a
threefold redundancy that does not exist. Caution's attestation carries **one**
independent measurement — over the kernel, the kernel command line, and the single
ramdisk containing the entire enclave (EnclaveOS `init`, `bootproofd`, `caddy`,
`caddy-certfp.sh`, busybox, socat, the generated `run.sh` carrying the whole
`unit.env`, `manifest.json`, and the application's `runtime`-stage filesystem) —
reported twice as PCR0 and PCR1, plus one constant. That TCB is considerably
larger than the "single 4.4 MB static binary" the manifests describe.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
