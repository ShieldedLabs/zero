# The verification criterion the documentation names does not exist: eleven passages tell an auditor to check the reproduced binary hash "against the one bound into the enclave attestation", and no attestation contains such a value

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: Worst instance and exemplar: `audit-target/zeronym/hub/deploy/caution/README.md:3-6` (the claim) together with `:55-63` (the "Verify the attestation" procedure built on it). Ten sibling passages carrying the same criterion: `hub/deploy/README.md:18-23`; `hub/deploy/caution/caution.hcl.tmpl:10-17`; `hub/deploy/build.sh:7-9`; `hub/deploy/Containerfile:21-29`; `shim/README.md:179-184`; `shim/deploy/README.md:19-21`; `shim/deploy/README.md:1172-1173`; `shim/deploy/Containerfile:33-37`; `shim/deploy/build.sh:8-10`; `shim/deploy/caution/caution.hcl.tmpl:8-18`. Supporting evidence: `hub/deploy/reproduce.sh:25-35`, `:47-53`, `:79-91`; `hub/deploy/Containerfile:148-149`, `:154-183`; `hub/deploy/caution/assemble-caution.sh:504-519`; Caution platform `src/enclave-builder/src/docker.rs:69`, `src/api/src/builder.rs:1034-1049`, `src/enclave-builder/src/manifest.rs:84-91`, `src/cli/src/lib.rs:6432-6467`.
**Found by agent:** Local (audit of `hub/deploy/caution/README.md`), extended by the `hub/deploy/README.md` and `EXPECTED_SHA256` local audits and by the G10/G12/G13 global audit. **Merged during validation (2026-08-18) with `hub-caution-readme-verify-step-presents-a-bare-elf-hash-as-confirming-the-enclave-measurement.md`, which described the same defect one paragraph further down the same file.**
**In scope of audit?** Yes — `*/deploy/**` and documented security claims are both in scope; AUDIT-INSTRUCTIONS states that "the reproducible-build and attestation chain **is** the trust model here".

> **Note on the filename.** It is narrower than the issue: eight other files (three confirmed issues, two plausible ones, `globals/G10-G12-G13-…`, `PROGRESS.md`, `BRAINSTORM.md`) reference this file by name, so it was deliberately not renamed. Use the title, not the filename.

## Description

Zeronym's trust model is stated the same way in eleven places across ten files:
rebuild the binary from source, get the published hash, and **check that hash
against the hash bound into the enclave attestation**. The hub's rationale
document states it in the strongest form of all:

> `hub/deploy/caution/README.md:3-6`
> ```
> The hub receives diverted migrations in plaintext and broadcasts them to the
> Zcash network. That is exactly why it runs as an attested enclave: the
> attestation binds the running binary to `../EXPECTED_SHA256`, so an auditor who
> reproduces the build knows the code holding the plaintext is the code they read.
> ```

**No attestation this platform produces contains a binary hash.** Caution builds
the app image with `docker build -f <containerfile> .` and no `--target`
(`src/enclave-builder/src/docker.rs:69`), stages the resulting filesystem into the
EIF's single ramdisk, and measures that ramdisk into PCR0/PCR1. The attested
manifest has a `binary` field (`src/enclave-builder/src/manifest.rs:88`) and the
Containerfile deploy path passes **`None`** for it (`src/api/src/builder.rs:1047`).
`grep -r EXPECTED_SHA256` over the entire Caution platform source returns
**nothing**. So there is no value in any attestation that `EXPECTED_SHA256` could
be compared against, and no tool anywhere performs such a comparison.

The mechanism that does connect code to measurement is different:
`caution verify` clones the manifest's `app_sources` repository, rebuilds the EIF
from it, and compares PCR0/1/2 against the live attestation
(`src/cli/src/lib.rs:6432-6467`). **Nine of the ten files carrying the claim never
mention it.** In the hub's `deploy/` half outside `caution/` — `README.md`,
`build.sh`, `reproduce.sh`, `Containerfile`, `assemble.sh` — the strings `PCR`,
`PCR0`, `PCR1`, `PCR2` and `caution verify` appear **zero** times (grep-verified),
while `hub/deploy/README.md:18` tells the reader the trust model "hands the
auditor **one job**", and that job is the non-existent comparison.

The hub's rationale document then converts the claim into a procedure:

> `hub/deploy/caution/README.md:55-63`
> ```
> ## Verify the attestation
>
> `caution verify` (from the assembled directory; or `POST /attestation`) returns
> the measurement bound to the running EIF. Confirm it against a local reproduce:
>
> ```sh
> git checkout <the PROVENANCE commit>
> sh zeronym/hub/deploy/reproduce.sh   # must print the hash in ../EXPECTED_SHA256
> ```
> ```

The step it presents as the confirmation is a **self-contained determinism check
on the auditor's own checkout**: `reproduce.sh` reads `EXPECTED_SHA256` out of the
very tree it just built (`:25-35`), and reads no attestation, no PCR and no byte
from any deployment. It prints `zero-indexer-hub: REPRODUCES` for a commit that
has never been deployed anywhere. And the commit it is run against is named by the
party being audited: `<the PROVENANCE commit>` comes from an unsigned text file the
operator's own assemble run wrote (`assemble-caution.sh:504-519`).

Under ICTM this is the defect the audit instructions call out directly: a property
the reader is told they get, which the mechanism named cannot deliver.

## Attack Scenario and Steps

The relevant adversary is **whoever causes the deployed enclave to run code other
than the published tree** — a careless operator who deployed a different commit or
a hand-patched image, a third party who substitutes the image, or Caution's own
build path substituting a different `caution.hcl`. The victim is the auditor or
shim operator whose job is to notice.

1. The deployed hub runs an image that does not correspond to the published
   source — say a build from an uncommitted local edit, or a redeploy of an older
   commit than the one published.
2. An auditor is pointed at the documentation. `hub/deploy/README.md:34` sends the
   reader to `caution/README.md` for the attested deploy; `shim/deploy/caution/OPERATORS.md:33`
   sends the shim's reader to its own `deploy/caution/README.md`.
3. The auditor performs the check the documents name: `git checkout <PROVENANCE
   commit>`, `sh reproduce.sh`, confirm it prints the hash in `EXPECTED_SHA256`.
4. It passes — because it is a check on the auditor's own checkout. Two cold builds
   of a clean commit agree with each other and with the hash that commit publishes,
   regardless of what is deployed.
5. The auditor concludes, in the document's own words, that "the code holding the
   plaintext is the code they read". The divergence is not detected, and would have
   been detected by `caution verify`, which rebuilds the EIF and compares PCRs.

**Attack Requirements and Assumptions:**

- No network attacker and no privileged position is involved. This is a
  false-assurance defect: the documented check cannot observe the deployment.
- **Bounded honestly: against a *deliberate* operator the correct check is also
  insufficient**, because `caution verify` reproduces from the repository the
  operator nominated (`caution-verify-reproduces-from-a-repository-the-operator-nominates-….md`,
  confirmed **High**). What this issue uniquely costs is detection of *accidental*
  divergence and of substitution by anyone other than the operator — exactly the
  cases `caution verify` does catch.
- **Mitigations that must travel with this finding.** The correct procedure exists
  in three places and is stated well: `hub/deploy/caution/OPERATORS.md:97-122` and
  `shim/deploy/caution/OPERATORS.md:151-211` both prescribe
  `caution verify --attestation-url https://<domain>/attestation` and require all
  three PCRs; `shim/deploy/caution/README.md:17-26` states the decomposition
  correctly and **explicitly denies this issue's claim** (see Technical Details
  §5). `zeronym/README.md:71`'s auditor bullet also names PCRs. An operator or
  auditor who reaches a runbook is not misled by the criterion — they are misled
  only about which artefact is load-bearing.
- The exemplar file compounds this: its own worked assemble block omits
  `--app-source`, so for the enclave *it* builds, `caution verify` refuses outright
  and the reproduce is all the reader has left. Filed separately as
  `hub-caution-readme-worked-deploy-builds-an-unverifiable-mixnetless-hub-and-offers-debug.md`
  (Low).

## Impact on Users

A wallet user's entire protection against the hub — the one component that by
design sees their migration in plaintext alongside every other user's — rests on
the claim that third parties can check what code it runs. Eleven passages tell
those third parties to check something that says nothing about a deployment, and
seven of the eleven present it as the auditor's whole task — the two "one job"
sentences (`hub/deploy/README.md:18`, `shim/deploy/README.md:19`), the two
"gives the auditor the job of" Containerfile comments, the two "the claim that
matters" `build.sh` headers, and `shim/deploy/README.md:1172-1173`'s "the
load-bearing claim". A hub or shim running code nobody reviewed would pass
every check those documents describe.

The harm is not a live exploit. It is that the audit step the product's threat
model depends on does not close, and that a reader who does the documented work
believes it has.

## Technical Details / Code Analysis

**1. What `EXPECTED_SHA256` is, and what the enclave is built from.**

`hub/deploy/reproduce.sh:47-53` builds `--target export` twice and hashes the
result. That stage is, in its entirety (`hub/deploy/Containerfile:148-149`):

```dockerfile
FROM scratch AS export
COPY --from=builder /usr/local/bin/zero-indexer-hub /zero-indexer-hub
```

The platform builds the Containerfile's **last** stage, `runtime`
(`Containerfile:154-183`), because it passes no `--target`:

```rust
format!("docker build -f {} .", containerfile)
```
(Caution platform `src/enclave-builder/src/docker.rs:69`)

That image becomes the EIF's single ramdisk, and PCR0/PCR1 are computed over
kernel ‖ cmdline ‖ that one ramdisk — which also contains the generated `run.sh`
(carrying the whole `unit.env`), `manifest.json`, `bootproofd`, `caddy`,
`caddy-certfp.sh`, EnclaveOS `init`, busybox and socat.

**Fairness point, and it is the reason the claim reads plausibly:** the runtime
stage copies the binary *through* the export stage
(`Containerfile:175-177`, "so the bytes an auditor hashes and the bytes that ship
are provably the same file"), so the deployed binary really is bit-identical to the
hashed one. The claim is wrong about the **mechanism**, not about the bytes: the
attestation commits to a filesystem containing that binary, but publishes no digest
of it, so the commitment is only checkable by rebuilding the whole EIF.

**2. Nothing joins the two values — verified on both sides.**

- Zeronym side: the only functional readers of `EXPECTED_SHA256` are
  `hub/deploy/reproduce.sh:34` (compares against a *locally built* ELF) and
  `hub/deploy/caution/assemble-caution.sh:504`, which interpolates the string into a
  plain-text `PROVENANCE` file (`:505-519`). `PROVENANCE` is written to the
  assembled repo root, and `Containerfile:107-109` copies only `zebra/`, `zaino/`
  and `zeronym/` into the builder — so it enters no image layer, no EIF and no PCR.
  It is an unsigned, operator-written text file whose last three lines are the same
  loop the README prescribes.
- Platform side: `grep -r EXPECTED_SHA256` over `codeberg.org/caution/platform`
  (HEAD `6051734`) returns **no matches**, and the manifest's binary hash is never
  populated:

```rust
let mut manifest = enclave_builder::EnclaveManifest::new(
    app_source,
    enclave_builder::EnclaveSource::GitArchive { … },
    enclave_builder::FrameworkSource::GitArchive { … },
    None,                                   // <-- `binary: Option<String>`
    request.run_command.clone(),
    None,
);
```
(`src/api/src/builder.rs:1034-1049`; signature at `src/enclave-builder/src/manifest.rs:84-91`)

**3. The reproduce step is self-referential and its commit is operator-chosen.**

`hub/deploy/reproduce.sh:25-35`:

```sh
ZERO_ROOT="$(git rev-parse --show-toplevel)"
HERE="$ZERO_ROOT/zeronym/hub/deploy"
…
if [ "${EXPECTED+set}" != "set" ]; then
	EXPECTED=$(cat "$HERE/EXPECTED_SHA256" 2>/dev/null || echo "")
fi
```

After `git checkout <commit>`, both the sources built and the hash compared come
from that commit. The script's own header is honest about this (`:2-9`: "build …
twice from cold, check that the two binaries are byte-identical, AND check that
they equal the hash this repo publishes"); it never claims to say anything about a
deployment. Note also `:79-81`: if `EXPECTED_SHA256` is empty or missing the
comparison is skipped with a `NOTE:` and the script still exits 0, so the README's
`# must print the hash in ../EXPECTED_SHA256` comment describes an outcome the
script does not enforce in that state (owned separately by
`reproduce-reports-reproduces-and-exits-zero-when-the-published-hash-comparison-is-skipped.md`).

`$SHA` in `PROVENANCE` is `git rev-parse HEAD` of the operator's tree, and
`$EXPECTED` is read from the same tree (`assemble-caution.sh:504-519`). The auditor
is asked to check out a commit the audited party named and confirm it is internally
consistent.

**4. The census — eleven passages, ten files, all verified verbatim at HEAD.**

| # | File and lines | Wording |
|---|---|---|
| 1 | `hub/deploy/caution/README.md:3-6` | "the attestation **binds the running binary** to `../EXPECTED_SHA256`" |
| 2 | `hub/deploy/README.md:18-23` | "hands the auditor **one job** … check that hash against the one bound into the enclave attestation" |
| 3 | `hub/deploy/caution/caution.hcl.tmpl:10-17` | "check that hash against the one bound into the enclave attestation" **and** "The binary under audit is the one recorded in `deploy/EXPECTED_SHA256`" |
| 4 | `hub/deploy/build.sh:7-9` | "The binary hash is the claim that matters, because that is what gets bound into the enclave attestation" |
| 5 | `hub/deploy/Containerfile:21-29` | "matching it against the hash bound into the enclave attestation" |
| 6 | `shim/README.md:179-184` | "so an auditor can match it against the hash bound into an enclave attestation" |
| 7 | `shim/deploy/README.md:19-21` | "hands the auditor **one job** … check that hash against the one bound into the enclave attestation" |
| 8 | `shim/deploy/README.md:1172-1173` | "The binary hash is the load-bearing claim, because that is what an enclave attestation binds" |
| 9 | `shim/deploy/Containerfile:33-37` | "matching it against the hash bound into the enclave attestation" |
| 10 | `shim/deploy/build.sh:8-10` | "that is what gets bound into the enclave attestation" |
| 11 | `shim/deploy/caution/caution.hcl.tmpl:8-18` | "check that hash against the one bound into the enclave attestation" **and** "The binary under audit is the one recorded in `deploy/EXPECTED_SHA256`" |

Two of these (#3, #11) are the **manifests that are rendered and pushed to the
platform**, i.e. the worst possible home for a claim about a file the platform
never reads. Two more (#5, #9) are the files the project itself calls the entire
definition of the build.

The grep that finds them all — the phrase breaks across a newline in shell and
Containerfile comments, so single-line greps miss half the set:

```
grep -rniE "bound into|binds? (the )?(running )?binary|attestation binds|hash (is )?bound" audit-target/zeronym
```
plus a whitespace-flattening pass for the comment-wrapped instances.

**5. The one artefact in the tree that states it correctly, quoted so the fix has a
model.** `shim/deploy/caution/README.md:17-26`:

```
A Nitro attestation binds a measurement of the loaded image into a signed
document …

| | proves | does not prove |
|---|---|---|
| reproducible build | source and published hash agree | that hash is what runs |
| attestation alone | *some* image runs in a real enclave | which source produced it |
| both | the code you read is the code serving you | |
```

followed by "`caution verify` rebuilds from source and compares". The row
"reproducible build … **does not prove** that hash is what runs" is the exact
negation of the eleven passages in the table above. (Its Verify section at `:29-30` then names `caution verify` as the mechanism, which is correct.)

**6. Boundaries — what this issue does NOT own.** Stated explicitly so the report
does not count one harm twice.

- *That no binary hash enters the attestation, and that `caution verify` rebuilds
  from a repository the operator nominates* — owned by the confirmed **High**
  `caution-verify-reproduces-from-a-repository-the-operator-nominates-so-nothing-binds-the-attested-code-to-zeronym.md`.
  This issue owns the **documentation census** and the substituted criterion.
- *The retracted "PCR2 is the check that matters" advice* (`deploy.sh:220`,
  `shim/deploy/caution/README.md:131-136`) — owned by the confirmed **Medium**
  `deploy-script-tells-operators-to-expect-pcr01-failure-and-accept-pcr2-alone.md`,
  which also establishes that PCR2 is `sha384(0^48 ‖ sha384(""))`, a constant
  across every Caution enclave. Disjoint passages from the eleven above.
- *`README.md:71`'s auditor recipe and its omissions 1 and 4* — owned by the
  confirmed **Medium** `auditor-recipe-omits-the-two-checks-that-decide-where-plaintext-goes-and-names-a-defence-the-platform-does-not-rely-on.md`,
  whose severity explicitly excludes the `EXPECTED_SHA256` omission and reserves it
  for this issue.
- *`shim/deploy/caution/caution.hcl.tmpl:12-14`'s separate "the operator cannot see
  the traffic" clause* — owned by `shim-manifest-header-tells-an-auditor-that-attestation-proves-the-operator-cannot-see-the-traffic.md`.
  Only the `:9-10` and `:17-18` clauses belong here.
- *That `reproduce.sh` builds `export` rather than `runtime`* — owned by
  `reproduce-never-builds-the-runtime-stage-that-the-enclave-and-pcr0-are-built-from.md`.
- *The exemplar file's worked deploy* (`:34-53`) — owned by
  `hub-caution-readme-worked-deploy-builds-an-unverifiable-mixnetless-hub-and-offers-debug.md`.

## Recommendations

1. Replace `hub/deploy/caution/README.md:3-6` with the mechanism that exists:
   *"…that is exactly why it runs as an attested enclave: `caution verify` clones
   the manifest's `app_sources`, rebuilds the EIF, and compares all three PCRs
   against the attestation the running enclave produces. `EXPECTED_SHA256` is a
   separate, weaker claim — that this source tree builds deterministically to a
   known binary — and is not part of any attestation measurement."*
2. Replace `:55-63` with the runbook's procedure verbatim
   (`hub/deploy/caution/OPERATORS.md:99-110`):
   `caution verify --attestation-url https://<hub-domain>/attestation`, expect
   `✓ Attestation verification PASSED` **and** `✓ TLS certificate binding
   verified`. Do not instruct the auditor to take the commit under test from
   `PROVENANCE`; the binding commit is the one the manifest pins in `app_sources`
   (branch **and** commit), which `caution verify` reads from the attested manifest.
   Keep the reproduce, relabelled honestly: *"Separately, `reproduce.sh` checks that
   the published source builds deterministically to `EXPECTED_SHA256`. That is a
   property of the repository, not of any deployment."*
3. Apply the same correction to all ten sibling passages in the table above — this
   is a sweep, not an edit. In particular, delete the `EXPECTED_SHA256` sentence
   from **both** `caution.hcl.tmpl` files, and repair the two "one job" sentences
   (`hub/deploy/README.md:18`, `shim/deploy/README.md:19`), which foreclose the real
   criterion rather than merely omitting it.
4. If a real hash-to-measurement binding is wanted, two options exist and neither is
   implemented: (a) publish the reproduced PCR triple next to `EXPECTED_SHA256` at
   the same commit, so a single recorded value is comparable to something the
   enclave emits; or (b) have `reproduce.sh` build the `runtime` stage and record a
   hash of the same filesystem the platform stages.

## Validation Information

**Verdict: CONFIRMED, Medium.** Validated 2026-08-18 against the target at HEAD
(`62baea8`, confirmed byte-identical to `audit-context/zero` at that commit), the
monorepo git history, and a local clone of the Caution platform
(`codeberg.org/caution/platform`, HEAD `6051734`). Every line reference in the
census table was re-read verbatim; every platform claim was re-derived from source
rather than taken from the earlier audit notes.

**This issue absorbed a second filed issue.**
`hub-caution-readme-verify-step-presents-a-bare-elf-hash-as-confirming-the-enclave-measurement.md`
described `:55-63` — the procedure — while this file described `:3-6` — the claim
the procedure implements. They are one harm in one 70-line document, with one fix,
and reporting them separately would double-count. That file has been moved to
`invalid/` carrying a MERGED banner; **it is not a refuted finding**, and its
surviving content is §3 and Recommendation 2 above.

**What was verified (all independently re-derived):**

- `docker build -f <containerfile> .` with no `--target` (`src/enclave-builder/src/docker.rs:69`) ⇒ the platform builds the `runtime` stage; `reproduce.sh:51` builds `--target export`.
- `EnclaveManifest::new(…, None, …)` on the Containerfile path (`src/api/src/builder.rs:1047`) ⇒ **no binary hash in any attestation**.
- `grep -r EXPECTED_SHA256` over the whole platform tree: **zero matches**.
- `caution verify` without `app_sources` bails with *"Manifest does not contain app_source - cannot reproduce without source URL"* (`src/cli/src/lib.rs:6432-6437`).
- `reproduce.sh` reads `EXPECTED_SHA256` from the working checkout (`:33-34`) and exits 0 with only a `NOTE:` when it is empty (`:79-81`, `fail` unset on that branch).
- `PROVENANCE` is written to the assembled repo root (`assemble-caution.sh:505`) while the build context copies only `zebra/ zaino/ zeronym/` (`Containerfile:107-109`) ⇒ unmeasured.
- Zero occurrences of `PCR`/`PCR0`/`PCR1`/`PCR2`/`caution verify` in `hub/deploy/README.md`, `build.sh`, `reproduce.sh`, `Containerfile`, `assemble.sh` and in `shim/README.md`; the only three hits in `shim/deploy/README.md` (`:664`, `:1055-1056`) are stale "no PCR has ever been computed" statements, not verification instructions.

**Five corrections applied against the filed texts. Do not restore them.**

1. **"Eleven passages across nine files" → ten files.** Recounted: hub carries five (in five files), shim six (in five files, because `shim/deploy/README.md` carries two). The passage count of eleven is right; the file count was not.
2. **The hub's manifest carries *both* clauses too.** The filed text said only the shim's `caution.hcl.tmpl` carried the "binary under audit is the one recorded in `deploy/EXPECTED_SHA256`" sentence alongside the binding sentence. `hub/deploy/caution/caution.hcl.tmpl:16-17` carries it verbatim as well.
3. **The absorbed issue's claim that the prescribed verification "consumes nothing from the running enclave" was too strong and has been dropped.** `:57` does tell the reader to run `caution verify`, which *is* the correct check. The accurate defect is that the document **subordinates** it — "Confirm it against a local reproduce" makes the local determinism check the arbiter — and that the same file's assemble block removes verify's ability to run at all.
4. **The absorbed issue's point (d) — "the invocation form given is the one the runbook says fails" — is not supportable as stated and was struck.** `caution apps create` **does** write `.caution/deployment.json` (`src/cli/src/lib.rs:5297`), so a bare `caution verify` in the assembled directory can resolve the app, contradicting `hub/deploy/caution/OPERATORS.md:101-105`. The real, smaller defect in that invocation is different: `get_attestation_url()` returns `http://<public-ip>/attestation` (`:6094-6108`), which takes the raw-IP branch of `tls_connection` (`:221-241`); on that branch the certificate binding can be **skipped** with only a warning while `✓ Attestation verification PASSED` still prints (`:6902-6932`, `:7284-7306`). The runbook's `--attestation-url https://<domain>/attestation` form takes the strong branch. That residual is **already owned** by the confirmed Medium `attested-tls-binding-is-verified-once-by-hand-if-ever-….md` and is recorded here only as a cross-reference; it is not counted in this issue's severity.
5. **The claim is wrong about the mechanism, not about the bytes.** The filed addendum's "the attestation and `EXPECTED_SHA256` are computed over different artefacts" is true but incomplete: `Containerfile:175-177` copies the binary *through* the export stage, so the deployed binary is bit-identical to the hashed one. Stated in Technical Details §1 so a developer reading the fix is not told something they can immediately falsify.

**Exploitability, assessed honestly.** No attacker capability is created. Against a
deliberate operator the documented check and the correct check are both
insufficient, because verification reproduces from the operator's own published
tree (confirmed High, item 7a: *measurement discloses a value; it never detects a
change*). The residual this issue owns is real but narrower than the filed text
implied: `caution verify` detects **accidental** divergence (wrong commit,
hand-patched image, a substituted `caution.hcl` from the platform's own build path)
and substitution by any party other than the operator; the documented criterion
detects neither, and in seven of the eleven passages it is presented as the
auditor's complete job.

**Severity: Medium, and the reasoning for not grading it higher or lower.**

- *Not High.* No user data is exposed and no attacker is enabled; the fix is prose.
  The exploitable mechanism is owned at High elsewhere.
- *Not Low, unlike the closest precedent.*
  `operators-runbook-attributes-the-hub-destination-to-the-binary-hash-and-egress-rules-neither-of-which-binds-it.md`
  was deliberately held to Low because it was one property in one runbook whose
  exploitable halves were owned at Medium and High. This is different in two ways
  that matter: the footprint is eleven passages in ten files including both pushed
  manifests and both Containerfiles, and in nine of those ten files **no correct
  criterion is stated anywhere**, so the reader is not merely given a weak reason —
  they are given a complete and wrong instruction set.
- *Consistent with the sibling Mediums.*
  `deploy-script-tells-operators-to-expect-pcr01-failure-and-accept-pcr2-alone.md`
  is Medium for two carriers of a check that cannot fail; this is the same shape at
  five times the footprint for a check that does not exist.
- *No double count.* The boundary list in Technical Details §6 was written before
  the severity was set, and each neighbouring issue's owned scope was re-read to
  confirm the reserved allocation (in particular `auditor-recipe-omits-…`'s
  explicit exclusion of omission 3, recorded in `PROGRESS.md` item 7k).

**Positives that must survive into the report, so this does not read as a
condemnation of the hub's documentation.**

- `hub/deploy/README.md` is, on the record of this audit, the **strongest deploy
  document in the tree** (`PROGRESS.md` row for that file): it diagnoses its own
  siblings' build-context errors, names the vendored `nym-upgrade-mode-check` that
  the shim's reference omits, transcribes no literal hash and is therefore immune to
  the shim's three-way hash drift, and gives the most honest statement of the mixnet
  network relaxation in the tree. It carries passage #2 and nothing else here.
- `hub/deploy/caution/OPERATORS.md` — the runbook sitting in the same directory as
  the exemplar — gets this entirely right, including the "require all three PCRs,
  do not accept a PCR2 match alone" argument (`:112-122`).
- `shim/deploy/caution/README.md:17-26` is the one artefact in the tree that states
  the decomposition correctly, and it should be the template for the sweep.
- The exemplar file is also right about things the audit should credit: `:18-21`
  gets `--indexer-tls` right and emphatically, and `:22-25` gets the HTTP/1.1
  vs h2c distinction right for a non-obvious platform reason.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
