# `caution verify` reproduces the enclave from a repository the operator nominates, so a clean `Attestation verification PASSED` proves the enclave runs *the operator's* code — nothing in the tree, the tooling or any document joins that code to zeronym

**Severity**: High
**Validation Status**: Confirmed
**Location**: `audit-target/zeronym/shim/deploy/caution/caution.hcl.tmpl:8-18` (the "WHAT IT PROVES" claim) and `:20-27` (the `build { }` block carrying `__APP_SOURCE__`); `audit-target/zeronym/shim/deploy/caution/assemble-caution.sh:64`, `:99`, `:427-443`, `:589-604`; `audit-target/zeronym/shim/deploy/assemble.sh:60-120` (the full Rust source is what gets published); `audit-target/zeronym/deploy.sh:130-136` (`DEBUG=0` requires `APP_SOURCE`) and `:197-222` (the automated publish step); `audit-target/zeronym/README.md:71` (the auditor recipe); `audit-target/zeronym/shim/deploy/caution/README.md:124-138`; `audit-target/zeronym/shim/deploy/caution/OPERATORS.md:150-200` ("Verify"); `audit-target/zeronym/hub/deploy/caution/OPERATORS.md:97-124`; identical structure in `audit-target/zeronym/hub/deploy/caution/assemble-caution.sh` and `hub/deploy/assemble.sh`
**Found by agent:** Global, focus area G30/G32/G17 — the indexer operator's full capability sweep after the `unit.env`/certfp reversals
**In scope of audit?** Yes — `*/deploy/**` is in scope because "the reproducible-build and attestation chain **is** the trust model here", and markdown claims are in scope as security claims.

## Description

After the two reversals recorded in coordinator open item 6q, the attestation
chain for this product is much stronger than the audit first believed:

- `unit.env` — every `ZIS_*`/`ZIH_*` value **and their absence** — is measured
  into PCR0/PCR1, so configuration cannot be changed without changing a
  measurement;
- `caution verify` binds the attested enclave to the **leaf certificate** of the
  very TLS connection that carried the attestation, so an operator terminating
  wallet TLS in front of the enclave fails verification.

Both of those bind the enclave to **the tree the operator published**. Nothing
binds that tree to zeronym.

`caution verify` reads `app_source.urls` and `app_source.commit` out of the
attested manifest, clones that repository at that commit, reads the
`caution.hcl` **from that clone**, rebuilds the whole EIF **from that clone**,
and compares the resulting PCR0/1/2 against the live attestation
(`src/cli/src/lib.rs:6432-6467`, `:6470-6473`). The URL is whatever the operator
passed to `assemble-caution.sh --app-source`. There is no allow-list, no
signature, no expected hash, and no comparison against
`github.com/ShieldedLabs/zero`. There cannot be: the published repository is a
**derived, per-deployment tree** produced by `assemble-caution.sh`, so it is
never Shielded Labs' repository by construction — `OPERATORS.md:77-78` instructs
the operator to *"Create an empty **public** git repository first"*.

That published tree contains the complete compiled input: `zeronym/shim/**`
(including `src/classify.rs`, `src/intercept.rs`, `src/proxy.rs`),
`zebra/zebra-chain`, `zaino/packages/zaino-proto`,
`zeronym/vendor/nym-upgrade-mode-check`, and the `Containerfile` that builds
them (`shim/deploy/assemble.sh:66`, `:93-96`, `:106-108`, `:117-118`,
`:144-179`). An operator who edits any of it and publishes the edit gets an
enclave that reproduces **exactly**, because the thing being reproduced is their
edit.

And the one artefact that could have closed the loop is absent. The attested
manifest has a `binary` field (`src/enclave-builder/src/manifest.rs:22-23`), but
the Containerfile deploy path passes `None` for it
(`src/api/src/builder.rs:925-940` — the fourth positional argument to
`EnclaveManifest::new`, whose signature is at
`src/enclave-builder/src/manifest.rs:84-91`), so **no hash of the running binary
appears anywhere in the attestation**. `EXPECTED_SHA256` is a zeronym-only file
that the Caution platform never reads — the string does not occur anywhere in
the platform source. With no binary hash in the attestation and no tie from the
app-source tree to upstream, **the chain has exactly one unbound link, and it is
the link that decides what code handles users' transactions.**

The manifest states the opposite as the reason the deploy exists
(`shim/deploy/caution/caution.hcl.tmpl:8-15`):

```
# WHAT IT PROVES, which is the entire reason to deploy it. The Zeronym trust
# model asks an auditor to rebuild the shim from source, reach the published
# hash, and check that hash against the one bound into the enclave attestation.
# Reproducibility alone proves only that source and binary agree; attestation
# alone proves only that SOME binary runs in a genuine enclave. Together they
# say: the code you read is the code that is running, and the operator cannot
# see the traffic.
```

and `shim/deploy/caution/README.md:129-131` says verify *"rebuilds the image from
the published `app_sources` repo, and compares measurements. It is what turns
'they say this is the code' into something checkable."* It turns it into
something checkable **against the operator's own tree**. "The code you read is
the code that is running" is true only if the code an auditor reads is the code
in the operator's `app_sources` repository — and every document points them at
`github.com/ShieldedLabs/zero` instead (`OPERATORS.md:60`, `PROVENANCE`,
`README.md:71`).

## Attack Scenario and Steps

Attacker: the indexer operator — adversary #1, and the party the product exists
to defend against.

1. The operator clones `github.com/ShieldedLabs/zero` at the commit everyone is
   auditing and makes one edit. Any of these suffices, and none changes the
   product's observable behaviour for a wallet:
   - in `shim/src/intercept.rs`, on the divert path, additionally POST the
     transaction bytes to an address the operator owns. The enclave has
     **unrestricted outbound** — the `egress { }` allow-list is reduced by the
     platform to the boolean `!egress.is_empty()` and the parent applies
     `iptables -A FORWARD -i enclave0 -o $DEFAULT_IFACE -j ACCEPT`
     (already filed as
     `enclave-egress-allowlist-is-discarded-by-the-platform-and-both-enclaves-have-unrestricted-outbound.md`),
     so there is no network control to defeat;
   - or, more cheaply and with no new traffic at all, weaken
     `classify::is_orchard_touching` for a chosen shape so those transactions
     take `Route::PassThrough` and arrive at the operator's own indexer in the
     clear;
   - or simply log what `log_verdict` is careful not to log, to a destination of
     their choosing.
2. They run `assemble-caution.sh --app-source https://github.com/<them>/zero-indexer-shim-deploy ...`
   exactly as documented, push to Caution, and push the assembled tree to their
   own public repository — which `deploy.sh:197-222` does automatically and which
   `deploy.sh:134` makes **mandatory** for any non-debug deploy.
3. An auditor performs the recipe at `README.md:71` and the "Verify" section at
   `OPERATORS.md:150-160`:
   - `caution verify --attestation-url https://<tls-domain>/attestation` ->
     clones the operator's repository, rebuilds, and prints
     `Base Nitro attestation and expected PCR0/1/2 verified`,
     `TLS certificate binding verified`, `Attestation verification PASSED`.
     **All of it is true.**
   - `sh zeronym/shim/deploy/reproduce.sh` in a checkout of
     `github.com/ShieldedLabs/zero` -> prints `REPRODUCES`, because the upstream
     source really does reproduce the upstream `EXPECTED_SHA256`. **Also true,
     and about a different artefact entirely.**
4. The auditor reports the endpoint as verified. Neither command ever compared
   the operator's tree to Shielded Labs'.

A second, quieter use of the same gap: because `debug.ssh_keys`,
`network.ingress` CIDRs and `resources` reach only terraform and never the EIF,
the operator can publish an `app_sources` tree whose `caution.hcl` shows
`ssh_keys = []` while the deployed manifest carried a key, and **every PCR still
reproduces**. The published manifest is therefore not evidence about any
unmeasured field.

**Attack Requirements and Assumptions:**

- **Access needed:** none beyond being the operator. No platform break, no
  on-path position, no cryptographic weakness, no software vulnerability.
- **Cost:** one source edit and one `git push`. The publish step is already
  automated by `deploy.sh`.
- **What makes this realistic:** publishing an `app_sources` URL is *required*
  for an attested deploy (`deploy.sh:134`), so the operator is not doing anything
  unusual; the repository they publish is expected to be theirs and new; and its
  contents are a large derived tree that nobody is instructed to diff or read.
- **What limits it, stated plainly and prominently:**
  - **The malicious code is public.** It sits in a public repository the operator
    advertised, permanently, under a commit hash bound into an AWS-signed
    attestation. For a *named* operator that is durable, attributable evidence,
    and a real deterrent.
  - **A cheap mechanical check does exist** — re-run `assemble-caution.sh` from
    the claimed upstream commit with the operator's own parameters and `diff -r`
    against the published tree. `assemble.sh` builds from `git archive HEAD`,
    which stamps deterministic mtimes (`assemble.sh:13-15`), so the comparison is
    expected to be exact modulo the values substituted into `caution.hcl`.
    **No zeronym document describes it, and no tool performs it.**
  - The `caution verify` output *does* print `App source: <url> commit: <sha>`
    (`src/cli/src/lib.rs:7100-7104`), so an attentive auditor sees the URL — but
    seeing a URL is not being told what it must contain, and no canonical value
    can be published because the tree is per-deployment.

## Impact on Users

`README.md:71` is the entire substitute this product offers for trusting an
indexer operator: *"**Auditors** verify an endpoint without trusting its
operator."* A user is entitled to read that as "someone competent can establish
that this endpoint runs the reviewed zero-indexer." Nobody performing the
documented steps establishes that. What they establish is: *a genuine AWS Nitro
enclave, built by Caution from a tree this operator published, is answering at
this hostname, and the certificate terminating my TLS session is the one that
enclave holds.*

The gap is not narrow. Arbitrary code inside the enclave is the strongest
position in the whole system: it sees every wallet's queries and every
Orchard-touching transaction in plaintext at the moment the wallet sends it, it
has unrestricted outbound to ship them anywhere, and it can decline to divert at
all. Every invariant in `THREATMODEL.md` §6.2 (P1-P5) and §6.3 (C1, C2) is a
property of the shipped source code and therefore falls with it.

The failure is silent on the user's side: a wallet sees a valid certificate,
correct gRPC behaviour, and a normal txid whichever tree is running.

It is also the *meta*-failure of this audit's whole attestation area. Every other
finding about the deployment — configuration measurement, the certfp binding, the
classifier's soundness, the fail-closed discipline — is a statement about the
code in `github.com/ShieldedLabs/zero`. If nothing binds the running enclave to
that code, none of those statements is a property of any particular endpoint.

## Technical Details / Code Analysis

### 1. What `caution verify` reproduces from

Caution platform, `src/cli/src/lib.rs:6432-6467` (read from the public source at
`https://codeberg.org/caution/platform`, whose location is named by
`shim/deploy/caution/OPERATORS.md:44-46`):

```rust
        let app_source_dir = if let Some(source) = local_source {
            Some(source.path.clone())
        } else if let Some(ref manifest) = external_manifest {
            let app_source = manifest.app_source.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Manifest does not contain app_source - cannot reproduce without source URL"
                )
            })?;
            let archive_urls: Vec<String> = app_source
                .urls
                .iter()
                .filter_map(|url| self.git_url_to_archive_urls(url, &app_source.commit).ok())
                .flatten()
                .collect();
            ...
            Some(
                self.download_and_extract_app_source_with_git_fallback(...).await?,
            )
```

and `:6470-6473`:

```rust
        let measured_config = if let Some(ref app_dir) = app_source_dir {
            Some(self.read_config_from_dir(app_dir)?)
```

Both the *source* and the *manifest that describes how to build it* come from
`app_source.urls`. The only validation applied to that URL anywhere in the verify
path is a `git ls-remote` reachability preflight
(`preflight_app_source_ref`, `src/cli/src/lib.rs:7898-7960`).

Note the follow-on: because `expected.domain` for the TLS certificate-fingerprint
check is derived from that same reproduced `caution.hcl`
(`tls_expectation_from_config`, `src/cli/src/lib.rs:286-312`), the strong new
binding established in open item 6q is *also* rooted in the operator's tree. It
proves the enclave serves the domain the operator's own config names.

### 2. There is no binary hash in the attestation to fall back on

`src/enclave-builder/src/manifest.rs:84-91` defines the constructor:

```rust
    pub fn new(
        app_source: Option<AppSource>,
        enclave_source: EnclaveSource,
        framework_source: FrameworkSource,
        binary: Option<String>,
        run_command: Option<String>,
        metadata: Option<String>,
    ) -> Self {
```

and the Containerfile deploy path calls it at `src/api/src/builder.rs:925-940`:

```rust
    let mut manifest = enclave_builder::EnclaveManifest::new(
        app_source,
        enclave_builder::EnclaveSource::GitArchive { ... },
        enclave_builder::FrameworkSource::GitArchive { ... },
        None,                       // <- `binary`
        request.run_command.clone(),
        None,
    );
```

So the `binary` field an auditor might hope to compare against `EXPECTED_SHA256`
is `None` for every zeronym deploy, and `EXPECTED_SHA256` itself is never read by
any Caution code. (For completeness, and in Caution's favour: `enclave_source`
and `framework_source` **are** pinned by the platform to
`git.distrust.co/public/enclaveos` and Caution's own `FRAMEWORK_SOURCE`, so the
platform's half of the image is not operator-nominated. Only the application half
is.)

### 3. What the published tree contains

`shim/deploy/assemble.sh` (invoked by `assemble-caution.sh`) copies, from
`git archive HEAD`:

```sh
git -C "$ZERO_ROOT" archive HEAD -o "$STAGE/shim.tar" zeronym/shim          # :66
git -C "$ZERO_ROOT" archive HEAD -o "$STAGE/zebra.tar" \                    # :93-96
	zebra/Cargo.toml zebra/zebra-chain zebra/zebra-test
git -C "$ZERO_ROOT" archive HEAD -o "$STAGE/zaino.tar" \                    # :106-108
	zaino/Cargo.toml zaino/packages/zaino-proto
git -C "$ZERO_ROOT" archive HEAD -o "$STAGE/vendor.tar" \                   # :117-118
	zeronym/vendor/nym-upgrade-mode-check
```

i.e. the entire compiled input, as source. `assemble-caution.sh:427-443` then
records the publication URL in the manifest's `build` block:

```sh
if [ -n "$APP_SOURCE" ]; then
	cat > "$APP_SRC_FILE" <<EOF

    # Where this assembled repository is published. 'caution verify' clones
    # this URL and rebuilds, so its root must be THIS directory, not the zero
    # monorepo, and the deployed commit must be pushed there on main and
    # tagged: the manifest pins branch AND commit.
    app_sources = ["$APP_SOURCE"]
EOF
```

The comment is accurate about the mechanism — *"its root must be THIS directory,
not the zero monorepo"* — and is the clearest statement in the repository that the
verified tree is not the upstream tree. It is not read as a warning anywhere.

### 4. `PROVENANCE` is a claim, not a binding

`assemble-caution.sh:589-604` writes, into the published repository:

```
source repo:     github.com/ShieldedLabs/zero
source commit:   $SHA
expected binary: $EXPECTED

The binary inside this EIF should hash to the value above. Verify with:
  git clone https://github.com/ShieldedLabs/zero && cd zero
  git checkout $SHA
  sh zeronym/shim/deploy/reproduce.sh
```

Every field is plain text in a repository the operator owns and can edit before
pushing, `$EXPECTED` is transcribed from the operator's own working tree without
being verified, and the instruction it gives sends the reader to a *different*
repository, whose reproduce output is compared to a file in that same different
repository. The procedure is internally consistent and externally unanchored.

### 5. What the two documented commands actually establish

| Command | Binds | Does not bind |
|---|---|---|
| `caution verify --attestation-url https://<domain>/attestation` | live PCR0/1/2 <-> the operator's published tree; attested `certfp` <-> the leaf of this TLS session; attested `domain` <-> the domain in the operator's published `caution.hcl` | the operator's tree <-> zeronym |
| `sh zeronym/shim/deploy/reproduce.sh` | upstream source <-> upstream `EXPECTED_SHA256`, on this machine | either side <-> anything the enclave contains |

The two chains never meet.

## Recommendations

1. **Add the missing step to `README.md:71` and to both "Verify" sections:**
   *"Re-run `assemble-caution.sh` from `github.com/ShieldedLabs/zero` at the
   commit the endpoint's `PROVENANCE` names, with the parameters the published
   `caution.hcl` shows, and `diff -r` the result against the `app_sources`
   repository `caution verify` cloned. A `caution verify` PASS without this step
   proves the enclave runs the operator's published code, not zero-indexer."*
   This is the whole fix and it costs one paragraph.
2. **Ship the diff as a script.** `zeronym/shim/deploy/verify-app-source.sh
   <app-source-url> <commit>` — clone, re-assemble from upstream, `diff -r`, exit
   non-zero on any difference outside the substituted `caution.hcl` values. The
   determinism `assemble.sh:13-15` already guarantees is what makes this
   mechanical.
3. **Correct `caution.hcl.tmpl:8-15` and `shim/deploy/caution/README.md:129-131`.**
   Replace *"the code you read is the code that is running"* and *"turns 'they say
   this is the code' into something checkable"* with what the platform delivers:
   *"the code **in the repository this manifest names** is the code that is
   running; confirming that repository is zero-indexer is a separate step, and it
   is step N of the Verify section."* The same sentence appears in the hub's
   manifest and should be corrected there too.
4. **State that the published `caution.hcl` is not evidence about unmeasured
   fields.** `debug.ssh_keys`, `network.ingress` CIDRs and `resources` reach only
   terraform, so a published tree can differ from the deployed manifest in those
   fields with every PCR still reproducing.
5. **Ask Caution for a manifest `binary` value on the Containerfile path**, or for
   a documented way to bind `EXPECTED_SHA256` to a measurement. That would let an
   auditor check the running binary against a value Shielded Labs publishes, and
   would make recommendation 1 a fallback rather than the only control.
6. Until 1-3 exist, `README.md:71` should not say *"without trusting its
   operator"*.

Cross-references — this issue is the system-level statement the following stop
short of, and it should be reported alongside them rather than merged into any
one of them:
`auditor-recipe-omits-the-two-checks-that-decide-where-plaintext-goes-and-names-a-defence-the-platform-does-not-rely-on.md`
(the same recipe, for *configuration* rather than *code*);
`hub-caution-readme-says-the-attestation-binds-the-running-binary-to-expected-sha256.md`
and `reproduce-never-builds-the-runtime-stage-that-the-enclave-and-pcr0-are-built-from.md`
(why the hash half cannot work);
`enclave-egress-allowlist-is-discarded-by-the-platform-and-both-enclaves-have-unrestricted-outbound.md`
(why injected code has somewhere to send the plaintext);
`assemble-git-archive-honours-gitattributes-so-the-build-context-is-not-the-committed-tree.md`
(the same outcome reached through *upstream* rather than through the operator).

## Validation Information

**Verdict: CONFIRMED. Severity: High** (filed High with an explicit Medium
counter-argument; **the calibration is resolved in favour of High** — reasoning
below).

**Every mechanical claim re-verified from primary sources.** The Caution platform
clone (`codeberg.org/caution/platform`) used by the filing agent was still on
disk and was re-read directly:

- `src/cli/src/lib.rs:6432-6467` — `caution verify` builds `app_source_dir` from
  `manifest.app_source.urls` @ `commit`, with only a `git ls-remote` preflight
  (`preflight_app_source_ref`, `:7898-7960`). No allow-list, no signature.
  Confirmed verbatim.
- `:6470-6473` — `measured_config` is read *from that clone*
  (`read_config_from_dir`), so the `caution.hcl` driving the rebuild is the
  operator's. Confirmed.
- `tls_expectation_from_config` (`:286-312`) consumes that same reproduced
  config, so the certfp binding established by reversal 6q is itself rooted in
  the operator's tree. Confirmed — this is a genuine second-order consequence and
  it strengthens the finding.
- `src/enclave-builder/src/manifest.rs:84-91` — the constructor's fourth
  parameter is `binary: Option<String>`; `src/api/src/builder.rs:925-940` passes
  `None` positionally. **Confirmed by reading the signature, not by counting
  arguments in prose.** No binary hash enters the attestation.
- `grep -r EXPECTED_SHA256` over the whole platform source returns **nothing**:
  the file exists only in zeronym (`shim/deploy/EXPECTED_SHA256`) and no Caution
  code path reads it. This closes the "surely something compares the hash"
  objection.
- `src/cli/src/lib.rs:7100-7104` — verify does print
  `App source: <url> commit: <sha>`. The disclosure is real; what is missing is
  anything to compare it against.

**Verified in `audit-target/zeronym/`:**

- `deploy.sh:130-136` — `DEBUG=0` **dies** without `APP_SOURCE`, so an attested
  deploy always nominates a repository; `:197-222` — `deploy.sh` pushes the
  assembled tree to that repository automatically and tags it. The operator is
  not doing anything unusual.
- `assemble-caution.sh:427-443` — the `app_sources = [...]` block, with the
  comment stating outright that the published root *"must be THIS directory, not
  the zero monorepo"*.
- `assemble-caution.sh:589-604` — `PROVENANCE`, a plain-text claim in a
  repository the operator controls, pointing the reader at a different repository.
- `caution.hcl.tmpl:8-15` and `shim/deploy/caution/README.md:129-131` — the two
  overclaims, quoted accurately.
- `shim/deploy/caution/OPERATORS.md:150-200` and `README.md:71` — the complete
  documented Verify procedure is `caution verify` + `reproduce.sh`. A repository-wide
  grep for `app_sources`/`app-source`/`APP_SOURCE` across all `*.md` returns
  seven hits, **none** of which asks anyone to compare the nominated tree against
  upstream. The "a diligent auditor could just diff it" objection therefore fails
  on the facts: nothing tells them to, and no canonical value exists to diff
  against because the tree is per-deployment by construction.

**One strengthening added during validation** (recommendation 4 and the second
paragraph of the attack): because `debug.ssh_keys`, `network.ingress` CIDRs and
`resources` reach only terraform and never the EIF, an operator can publish a
tree whose `caution.hcl` differs from the deployed manifest in exactly those
fields and **every PCR still reproduces**. So the published manifest is not
evidence about any unmeasured field — which matters directly to the step-1
precondition of the audit's headline finding, where `debug { enabled = false;
ssh_keys = [...] }` is one of the two routes to the parent host.

**Why the Medium argument loses.** The filing agent asked the validator to weigh
three points; each was considered and rejected:

1. *"The malicious source is permanently public and attributable."* True, and it
   is a deterrent — but a deterrent is not a control, and it only binds an
   operator with a reputation at stake. The audit's threat model explicitly
   includes hostile operators and notes that anyone can run the published image
   and produce a valid attestation. It also does nothing for the *user*, who has
   no way to act on evidence that only becomes legible after a forensic
   comparison nobody is asked to perform.
2. *"The fix is documentation, not code."* Cheapness of the fix is an argument for
   fixing it, not for grading it low. Coordinator item 7a states the governing
   distinction: **measurement discloses a value; it never detects a change.**
   Applied here it is worse than for the configuration siblings — `ZIS_HUB_NYM`
   at least has a canonical expected value that could be published and compared,
   whereas the `app_sources` tree is per-deployment, so the disclosure at
   `verify`'s `App source:` line terminates in a value with **no expected
   counterpart at all**. That is why the sibling
   `auditor-recipe-omits-...` sits at Medium and this does not.
3. *"It is really a platform property."* The platform's behaviour is reasonable
   for a general-purpose service. What is a zeronym defect is that three of its
   own documents assert the stronger property (`caution.hcl.tmpl:8-15`,
   `shim/deploy/caution/README.md:129-131`, `README.md:71`) and its documented
   procedure omits the only step that would deliver it. Markdown claims are in
   scope as security claims, and under ICTM a documented property users are told
   they get but do not get is itself the bug.

**Why High rather than Critical:** it requires the endpoint's own operator to be
deliberately malicious, the malicious code is public and attributable, no funds
can be stolen, and an auditor who *does* read the nominated tree finds it
immediately.

**Why High rather than Medium:** the capability obtained is total (arbitrary code
in the enclave nullifies every code-level invariant in `THREATMODEL.md` §6.2 and
§6.3 at once, with unrestricted outbound to exfiltrate), the cost is one commit
on a path the deploy script already automates, **every** documented check passes
and passes correctly, and — unlike the Medium siblings — there is no mechanical
check available at all until recommendation 2 is built. It also voids the single
sentence (`README.md:71`) that the product offers as its substitute for trusting
an operator.

**False-positive checks applied.**

- *§6 Intentional design?* The platform's reproduce-from-nominated-source model
  is intentional. The finding is not that model; it is the three zeronym
  documents that claim a stronger binding and the procedure that omits the
  bridging step. That is a defect, not a design choice.
- *§8 Requires prior compromise?* No. The operator has this by construction.
- *§4 Test/debug only?* No — the affected path is the **attested** deploy path;
  `DEBUG=0` is precisely the configuration that requires `--app-source`.
- *§1 Assumption an attacker cannot violate?* The assumption "the app_sources
  repository contains zeronym" is violated by a `git commit`.

**Double-counting guard for the report.** This must be presented as the *meta*
finding of the attestation family — the one that says why the other checks are
only as good as the tree they are run against — and **not** as another
independent count of "the operator obtains plaintext". The operator's cheaper
plaintext routes (`shim-submits-every-migration-to-every-configured-hub-...md`,
confirmed High; `log-verdict-logs-migration-value-balance-at-info.md`, confirmed
High) are separate mechanisms; the report should not sum three Highs into three
distinct losses of the same secret.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
