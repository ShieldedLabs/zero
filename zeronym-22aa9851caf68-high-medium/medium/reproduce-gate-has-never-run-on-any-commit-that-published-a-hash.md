# The only automated gate on the attested binaries does not run on `push`, so on a branch that receives only direct pushes it runs only when a human dispatches it — neither hash published at HEAD has ever been checked, the last run of each workflow reported DOES NOT REPRODUCE, and the reference document teaches auditors that a red result is expected

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `.github/workflows/zeronym-shim-reproduce.yml:24-38` and `.github/workflows/zeronym-hub-reproduce.yml:18-32` (the trigger sets, in the enclosing monorepo); `audit-target/zeronym/shim/deploy/EXPECTED_SHA256:1` and `audit-target/zeronym/hub/deploy/EXPECTED_SHA256:1`; `audit-target/zeronym/shim/deploy/reproduce.sh:80-120` (the verdict); `audit-target/zeronym/shim/deploy/README.md:36-46`, `:260-264`, `:283-285`, `:473-477`, `:1066-1090`; the shipped verification instruction at `audit-target/zeronym/hub/deploy/caution/assemble-caution.sh:503-519` and `audit-target/zeronym/shim/deploy/caution/assemble-caution.sh:585-604` (the `PROVENANCE` blocks); `audit-target/zeronym/deploy.sh` (no verification step anywhere); repository history and the GitHub Actions run history of both workflows
**Found by agent:** Local (file audit of `shim/deploy/README.md`); raised by the `hub/deploy/reproduce.sh` audit as BRAINSTORM §R26-F; owns coordinator open item 6e
**In scope of audit?** Yes. `*/deploy/**` including `EXPECTED_SHA256` and `reproduce.sh` is explicitly in scope, and `audit-context/AUDIT-INSTRUCTIONS.md` states that "the reproducible-build and attestation chain **is** the trust model here, so a break in it is a security finding, not tooling noise." Markdown claims are in scope "as security claims". The two workflow files sit one directory above `zeronym/` but are the enforcement point for the in-scope scripts, and `shim/deploy/assemble.sh:116` instructs maintainers to keep their filter list in sync with an in-scope file.

> **FILENAME NOTE — read before quoting it.** This file's *name* encodes the
> original claim *"has never run on any commit that published a hash"*, which
> validation **refuted** (see "The claim that had to be corrected"). The name is
> retained only because `PROGRESS.md`, `BRAINSTORM.md` and two other issue files
> reference it. **Use the title, not the filename.**
>
> **SCOPE AND OWNERSHIP.**
> - The **`paths:` filter** on the `pull_request` trigger — the separate false-green
>   hazard this same repository diagnosed on 2026-08-07 and removed from its two
>   other PR workflows — is owned by
>   `zeronym-reproduce-workflows-keep-the-paths-filter-false-green-the-project-diagnosed-and-removed-from-its-two-sibling-workflows.md`
>   (Low). This file owns the **missing `push:` trigger**, the **re-baseline lag**,
>   and the **document that sanctions the resulting red state**. Report them
>   adjacently; do not count the filter twice.
> - The **fail-open verdict** (an empty or unreadable `EXPECTED_SHA256` yields
>   `REPRODUCES` and exit 0) is owned by
>   `reproduce-reports-reproduces-and-exits-zero-when-the-published-hash-comparison-is-skipped.md`.
> - The **hash appearing with three different values** across `EXPECTED_SHA256` and
>   the two deploy READMEs is owned by
>   `shim-published-binary-hash-has-three-different-values-across-expected-sha256-and-the-two-deploy-readmes.md`.

## Description

`reproduce.sh` is the project's only automated gate of any kind. Both zeronym
workflows are a single `run: sh .../reproduce.sh` step, and no `cargo test`,
`nextest`, `clippy`, `fmt`, `audit` or `deny` job exists anywhere in the
repository. `shim/deploy/README.md:39-46` describes that gate as the
"independent-second-machine half of the claim" and `:37` describes re-baselining
the published hash as "an explicit, reviewable edit".

Four verified facts, taken together, say what the gate currently delivers:

1. **Neither workflow runs on `push`.**
   `.github/workflows/zeronym-shim-reproduce.yml:24-26` is `workflow_dispatch:` +
   `pull_request:` only; the hub's `:18-20` is the same. Across the **entire run
   history of both workflows — 51 shim runs and 18 hub runs — there is not one
   `push` event.**
2. **`main` receives direct pushes, not pull requests.** The last 60 commits of
   `main` contain zero merge commits from PRs and zero PR-numbered subjects; the
   newest `Merge pull request` is 61 commits back (`b174d78`, 2026-08-13). The
   consequence is visible in the run history: **every run of either workflow since
   2026-08-13 has been a manual `workflow_dispatch`**, with one exception (shim run
   44, a `pull_request` on a side branch, which failed). Before that date the
   workflow did fire automatically on PRs and did confirm published hashes — see
   the correction in Validation Information.
3. **`EXPECTED_SHA256` is re-baselined in its own commit, long after the source
   change it describes.** Windows of up to **22 commits containing 16 successive
   compiled-source changes** are on the record (`c68d320`, 2026-08-11 →
   `0fda2c2`, 2026-08-12; independently recomputed during validation). During each
   such window `sh zeronym/shim/deploy/reproduce.sh` prints
   `zero-indexer-shim: DOES NOT REPRODUCE` for an entirely honest committed tree.
4. **Neither hash published at HEAD has ever been the subject of a run.** The last
   run of the shim workflow on a `main` commit at or before HEAD was on
   `e91170ed` (2026-08-17, runs 48 and 49) and **failed**; the last hub run on a
   `main` commit at or before HEAD was on the same `e91170ed` (run 17) and
   **failed**. The re-baselines that fixed those failures — `ff1fccd` (hub) and
   `0be521a` (shim) — landed afterwards, and no run of either workflow has been
   made on them or on any of the 14 commits between `e91170ed` and HEAD.

**Mitigating fact, to be stated prominently wherever this is reported: at HEAD
neither hash is stale.** `git log 0be521a..HEAD -- <shim compiled inputs>` and
`git log ff1fccd..HEAD -- <hub compiled inputs>` are both empty, so no compiled
input has changed since either re-baseline. The defect is that the gate has not
been run to *confirm* that, not that a wrong hash is currently published.

**The contradiction the file was assigned to resolve.** `shim/deploy/README.md`
states the same-commit rule four times (`:263-264`, `:285`, `:475-477`, `:1078`)
— *"`EXPECTED_SHA256` **must move in that same commit**"*, *"Those two must never
drift apart"* — and its normative three-state table names only working-tree
states, each resolved by a single commit. **The practice — source first, hash many
commits later — is none of those three states.** It is covered only by the loose
sentence that follows the table at `:1088-1090`:

> A red `reproduce.sh` after a commit that touched compiled source is the
> tripwire working, and it has already caught one drifted hash on its first live
> test.

That sentence is a conflation. The incident it cites (`:368-396`) is a case where
the published hash was **wrong** — a value that "never corresponded to any commit"
— not a case where it was merely **late**. Generalised to "red after a compiled
change is fine", it converts the gate's only alarm state into an expected one, in
the document an auditor is told to read.

## Attack Scenario and Steps

There is no single-step exploit. This is the degradation of the assurance
mechanism the product's trust model rests on, and it has two realistic
consequences.

**Scenario A — a compiled change reaches a published hash with no independent
machine ever executing the gate.**

1. A change to compiled source lands on `main` by direct push, the normal mode
   (60 of the last 60 commits). No `pull_request` event fires; the workflow has no
   `push` trigger; no run occurs.
2. The author later pushes a hash-only commit re-baselining `EXPECTED_SHA256` to
   whatever their own machine produced (`3fcbaa9` and `0be521a` are exactly this
   shape — one file changed each). Again no run occurs.
3. `deploy.sh` is then run. It performs no verification: `EXPECTED_SHA256`,
   `build.sh`, `reproduce.sh` and `caution verify` appear in it only inside log
   strings and a guard on `APP_SOURCE`, never as invocations. The enclave is built
   and deployed from that tree.
4. The published hash for the running enclave has therefore been produced by one
   machine, on a commit no automated job has ever seen, and re-checked by nobody.
   The document that calls this "an explicit, reviewable edit" (`:37`) describes a
   review that does not occur: no reviewer (direct push), no second machine (no
   run), and for the shim's two most recent hashes no in-tree record either.
5. A modification introduced anywhere in the compiled input set at step 1 — in
   particular in `zeronym/vendor/nym-upgrade-mode-check`, which is compiled into
   both enclaves and is watched by no workflow, no `paths:` entry, no dirty-tree
   check and no test — is absorbed into the next re-baseline as though intended.

**Scenario B — an auditor cannot distinguish an honest late re-baseline from a
substitution.**

1. An auditor follows the published provenance of a live enclave. Both
   `assemble-caution.sh` scripts write a `PROVENANCE` file into the pushed app
   repository pairing `source commit: $SHA` with `expected binary: $EXPECTED`
   (read from the deploying tree's `EXPECTED_SHA256`) and instruct
   `git checkout $SHA; sh zeronym/<c>/deploy/reproduce.sh`.
2. If the deploy happened inside a re-baseline window — a recurring state of
   `main` lasting up to 22 commits — `$SHA` compiles a binary that is not
   `$EXPECTED`, so the published instruction **fails on an honest deployment**.
   The project recorded this as having already happened: commit `175f375`
   (2026-08-13) says *"Until now the assembled `PROVENANCE` quoted the pre-change
   hub hash and told a reader to verify with `reproduce.sh` — a claim that was
   simply false."*
3. The auditor consults the reference document and reads `:1088-1090`: a red run
   after a compiled-source commit "is the tripwire working". They are told the
   alarm they just triggered is expected.
4. That is the reading an operator running a substituted binary needs them to
   make. The gate's single output has been given two meanings that — in the
   document's own phrase about a different pair of states (`:1070-1072`) — "look
   identical from the outside and mean opposite things".

**Attack Requirements and Assumptions:**
- Scenario A requires only the project's normal workflow; no attacker is needed
  for the gate to be absent. An attacker *exploiting* it needs the ability to land
  a compiled change — a compromised maintainer account, a malicious subtree pull,
  or a change to the ungated vendored crate. That is a supply-chain position, not
  a remote one.
- Honest bound on what the gate would catch even if it ran: `reproduce.sh` detects
  **non-determinism and hash staleness**, not malice. A deterministic malicious
  commit reproduces cleanly and is re-baselined normally. What the gate protects is
  the *binding* between committed source and the published hash that auditors and
  `caution verify` rely on — which is exactly the binding the trust model needs and
  the only one anybody outside the project can check.
- Scenario B requires no attacker capability at all to *occur*; a malicious
  operator merely benefits, because the honest-red state supplies cover.
- **As of HEAD neither hash is stale** (verified above), so no auditor is being
  misled *today* by a wrong value. What is true today is that nobody has confirmed
  it, and the last recorded evidence for both components is a failure.

## Impact on Users

Wallet users never check a hash. Auditors and third-party indexer operators do,
and they are the entire mechanism by which a user gets any assurance that the
enclave in front of their wallet runs reviewed code. `shim/deploy/README.md:19-27`
states this without hedging: "the deliverable here is not a Dockerfile that
builds. It is a hash anyone can independently recompute."

What that mechanism delivers today:

- **Both hashes published at HEAD have no confirmation of any kind.** No run has
  been performed against either, and the last runs of both workflows failed.
- **The window in which the committed tree contradicts its own published hash is a
  normal, recurring state of `main`** — up to 22 commits — with no bound and no
  automatic alarm.
- **Both shipped deploy paths write a `PROVENANCE` file whose verification
  instruction fails inside that window**, so the one artefact a third party is
  handed to check a live enclave against can be wrong for benign reasons.
- **The document an auditor reads to interpret a red result tells them to expect
  one.**

Combined with the separately-filed fail-open verdict, the gate is weaker in both
directions at once: it goes green when it compared nothing, it goes red when
nothing is wrong, and on `main` it does not run unless a human remembers to press
it. Two of the project's own recorded incidents (2026-08-01, 2026-08-13) were each
caught by this gate and by nothing else, and the 2026-08-13 one could only have
been caught by a manual dispatch, since no automatic trigger existed for `main`.

## Technical Details / Code Analysis

**1. The trigger set.** `.github/workflows/zeronym-shim-reproduce.yml:24-38`:

```yaml
on:
  workflow_dispatch:
  pull_request:
    paths:
      - ".github/workflows/zeronym-shim-reproduce.yml"
      - "zeronym/shim/**"
      - "zebra/Cargo.toml"
      - "zebra/zebra-chain/**"
      - "zebra/zebra-test/**"
      - "zaino/Cargo.toml"
      - "zaino/packages/zaino-proto/**"
```

There is no `push:`. The same file's header asserts the property the trigger set
cannot deliver for `main`:

```
# This runner is the INDEPENDENT SECOND MACHINE. … a matching hash on a native
# x86_64 runner is what upgrades the claim from "deterministic on one host" to
# "deterministic across hosts", which is the property the Auditor Role actually
# needs.
```

The hub's workflow is the same shape and was created on **2026-08-09**, two days
*after* the project removed the `paths:`-filtered `pull_request` construct from
`z3-smoke.yml` and `z3-regtest.yml` in `30d6852` (2026-08-07) with a long in-file
write-up of why it is unsafe. Neither zeronym workflow's `on:` block has been
modified since it was written. *(That construct is the sibling issue's subject;
noted here only because it compounds the same blind spot.)*

**2. The run census.** Retrieved from the GitHub Actions API for both workflows,
complete history:

| workflow | total runs | `push` events | runs since 2026-08-13 | of which manual |
|---|---|---|---|---|
| `zeronym-shim-reproduce` | 51 | **0** | 15 | 14 (1 `pull_request` on a side branch, failed) |
| `zeronym-hub-reproduce` | 18 | **0** | 13 | 13 |

The most recent runs bearing on the audited HEAD:

| workflow | run | date | event | head | conclusion |
|---|---|---|---|---|---|
| shim | 49 | 2026-08-17 15:55Z | `workflow_dispatch` | `e91170ed` | **failure** |
| shim | 48 | 2026-08-17 15:48Z | `workflow_dispatch` | `e91170ed` | **failure** |
| hub | 17 | 2026-08-17 15:48Z | `workflow_dispatch` | `e91170ed` | **failure** |

and there are **14 commits between `e91170ed` and HEAD**, including both
re-baselines, with no run on any of them.

**3. The verdict a stale window produces.** `shim/deploy/reproduce.sh:88-120`:

```sh
if [ -z "$EXPECTED" ]; then
	echo "NOTE: no published hash to compare against (EXPECTED_SHA256 missing"
	echo "      or explicitly cleared). Self-consistency only."
elif [ "$h1" = "$EXPECTED" ]; then
	echo "MATCHES PUBLISHED: $EXPECTED"
else
	echo "FAIL: this host disagrees with the PUBLISHED hash."
	…
	fail=1
fi
…
if [ "$fail" = 0 ]; then
	echo "zero-indexer-shim: REPRODUCES"
else
	echo "zero-indexer-shim: DOES NOT REPRODUCE (see FAIL lines above)"
fi
```

At `e91170e` the shim's `EXPECTED_SHA256` still held `2199d281…` while `proxy.rs`
had moved, so this printed `FAIL: this host disagrees with the PUBLISHED hash` and
exited 1 — which is exactly what runs 48 and 49 recorded. The same output is what
an auditor would get from a substituted binary.

**4. The re-baseline history.** Every transition of both `EXPECTED_SHA256` files
was extracted with `git log --full-history --format='%H' --reverse main -- <path>`
(plain `git log -- <path>` prunes side-branch commits and undercounts). The
same-commit rule was kept **five** times — `1c616c3` (file creation), `c161012`,
`d8306d7`, `3357bd1` on the shim and `a746496` on the hub — the last of them on
**2026-08-11**. Since then, **all fifteen later transitions are late** (8 shim + 7
hub, across 10 distinct commits). The worst window, recomputed independently during
validation: `c68d320` (2026-08-11) → `0fda2c2` (2026-08-12) is **22 commits
containing 16 compiled-source changes to the shim**, and the hub's equivalent is
23 commits. `0be521a`'s four-commit window is one of the *smaller* ones.

**5. The provenance consequence.** `hub/deploy/caution/assemble-caution.sh:503-519`
(the shim's copy at `:585-604` is the same block):

```sh
EXPECTED=$(cat "$ZERO_ROOT/zeronym/hub/deploy/EXPECTED_SHA256" 2>/dev/null || echo "unrecorded")
cat > "$DEST/PROVENANCE" <<EOF
…
source commit:   $SHA
expected binary: $EXPECTED

The binary inside this EIF should hash to the value above. Verify with:
  git clone https://github.com/ShieldedLabs/zero && cd zero
  git checkout $SHA
  sh zeronym/hub/deploy/reproduce.sh
EOF
```

`$SHA` is the deploying tree's `HEAD` and `$EXPECTED` is read from the same tree,
so a deploy inside a window ships an EIF whose published verification instruction
is guaranteed to fail. A missing or unreadable `EXPECTED_SHA256` here yields the
literal string `unrecorded` rather than an error.

**6. The project's own record of two failures of this control.**

- `105c21d2` (2026-08-01), *"re-baseline the build hash to the value proven from
  committed source"*: *"The published hash was wrong, CI caught it… `c6b7738f`
  never corresponded to any commit. It was measured before the predicate change
  had landed… **Run 30688786506 shows as FAILED and that is the point.**"* This is
  the incident `shim/deploy/README.md:368-396` writes up and the only one behind
  `:1088-1090`'s "the tripwire working" sentence — confirming that the sentence
  generalises a *wrong-hash* case to cover the *late-hash* practice.
- `aad6523e` (2026-08-13), *"re-baseline both deploy hashes to measured builds"*:
  *"**zeronym-hub-reproduce failed on main.** The published `c8cee172` was measured
  at `0ba0b9a620` and never refreshed after `6217fbd3c1` edited
  `hub/src/nym_driver.rs`… The shim's `418ce662` went stale the same way…
  Re-baselined to `8b5ec3fa`, but that is **ONE build on one host, which is exactly
  how the hub's bad value arose**."* The run history shows the runs on that date
  were `workflow_dispatch`, so "failed on main" was found by a human pressing the
  button.

**7. The public status statement that existed for 26 minutes — reported as a
factual sequence, with no motive imputed and none implied.**

- `53bab86` (2026-08-17 12:23:01 -0400, *"state what the reproduce jobs actually
  measured"*) **softened an existing, stronger claim**. Its commit body: *"The
  README asserted both hashes fail at tip of main. True of the last runs
  (e91170ed, 2026-08-17), but two re-baselines landed afterwards and nothing has
  run against them, so the honest claim is 'last measured failing, current state
  unverified'."* The resulting paragraph read:

  > **Not yet verifiable.** The enclaves are attested and running, but an auditor
  > cannot yet tie either back to a public commit. PCR2 (the application binary)
  > reproduces; PCR0 and PCR1 do not, so `caution verify` reports FAILED on healthy
  > enclaves. The reproduce jobs run on pull requests and manual dispatch, **not on
  > every push**; they last ran 2026-08-17 on `e91170ed` and both reported DOES NOT
  > REPRODUCE, with re-baselines landed since and no run against them. The live
  > pair's provenance also fails: the shim's cites a non-public commit, the hub's a
  > hash its own cited commit does not produce.

- `fa17e92` (12:49:17 -0400, *"Protected as bullets; drop the verifiability
  paragraph"*) removed it 26 minutes later, together with the two cross-references
  pointing at it. **The removal commit's own message states what is being removed:**
  *"Note this removes the README's only disclosure that PCR0/PCR1 do not reproduce,
  that the reproduce jobs do not run on push, and that the live pair's provenance
  does not check out."*
- **One sentence in that paragraph was genuinely stale**, which is a sufficient and
  innocent explanation for removing the paragraph rather than editing it: the
  PCR0/PCR1 claim is superseded by `shim/deploy/caution/OPERATORS.md:188-193`
  (*"**That is fixed**: on the attested pair deployed 2026-08-14 … **all three PCRs
  reproduced** on both"*).
- **The two other sentences were accurate and remain accurate at HEAD**, and this
  audit verified both independently against the Actions API rather than taking the
  project's word: no `push` trigger and no `push` run in 69 runs; and runs 48/49
  (shim) and 17 (hub) on `e91170ed` all reported failure with the re-baselines
  landing afterwards and no run against them.
- Nothing at HEAD replaces the statement: a grep of every `.md` under `zeronym/`
  for "not on every push", "DOES NOT REPRODUCE", "last ran", "unverified" or "not
  yet verifiable" returns nothing, while `zeronym/README.md`'s auditor bullet still
  says "reproduce the build and compare hashes" with no caveat.

**This finding does not depend on the deletion.** Facts 1-4 in the Description are
established from the workflow files, the commit history and the Actions API alone.
The deletion is reported because it is the only place the project has ever
described the gap, and because its absence leaves the public claim sheet asserting
an auditor procedure with no statement of what that procedure currently
establishes.

## Recommendations

1. **Add `push:` (restricted to `main`) to both reproduce workflows.** This is the
   single change that turns the window from unobserved into observed, and it costs
   one line per workflow. Both historical failures of this control were detected by
   `reproduce.sh` and by nothing else, and the 2026-08-13 one required a human to
   dispatch it. Do this together with the sibling issue's recommendation to move
   path filtering into a job, so a dropped event cannot be silent either.
2. **Make the re-baseline atomic again.** The document already derives the correct
   procedure at `:508-513`: measure from a working-tree overlay, commit source +
   `EXPECTED_SHA256` + the README row together, then re-run `reproduce.sh` against
   the commit with a live comparison. `c161012`, `d8306d7`, `3357bd1` and `a746496`
   show it is achievable; the last time it was done was 2026-08-11.
3. **Delete or narrow `shim/deploy/README.md:1088-1090`.** As written it tells
   auditors that the gate's failure output is expected. Replace it with the precise
   statement: *a red `reproduce.sh` means the published hash does not describe this
   tree; there is no benign case, and if you see one it is a defect in our release
   process, not in your build.*
4. **Restore an accurate status statement to `zeronym/README.md`.** The paragraph
   removed by `fa17e92` contained one stale sentence (PCR0/PCR1) and two that
   remain true at HEAD (the push-trigger gap; the current hashes having had no run).
   Correct the stale sentence and keep the rest, rather than leaving the public
   claim sheet with no statement of what its auditor procedure currently
   establishes.
5. **Have `assemble-caution.sh` refuse to write a `PROVENANCE` block pairing a
   `source commit` with an `expected binary` it has not verified against that
   commit** — or, at minimum, have it print a loud warning when the tree has
   compiled-input commits newer than the last `EXPECTED_SHA256` change. Having
   `deploy.sh` gate on a passing `reproduce.sh` is the stronger version of the same
   fix.

## Validation Information

**Verdict: CONFIRMED, Medium. The mechanical facts hold and are now much stronger
than as filed, but the headline claim as originally written is FALSE and has been
replaced. Coordinator open item 6e is closed by this file.**

### The claim that had to be corrected

The original title and Description asserted that the gate *"has never executed on
any commit that published a hash"*. **That is false, and would have been a false
positive in the report.** Retrieved from the GitHub Actions API:

- shim run **34**, 2026-08-09, event `pull_request`, head `a77d9f8fb1`,
  conclusion **success** — and `a77d9f8fb1`'s `zeronym/shim/deploy/EXPECTED_SHA256`
  is `51ccefed3eda14a5…`, the value that then landed on `main` in `d8306d7`.
- shim run **36**, 2026-08-11, event `pull_request`, head `2cf872d76b`,
  conclusion **success** — carrying `f498f82240711872…`, the value that landed in
  `3357bd1`.

`shim/deploy/README.md:293` corroborates this from the project's side
(*"the first shim hash since `51ccefed` to be machine-checked at all"*), as does
the `51ccefed` row's *"Cross-machine confirmed: a native x86_64 CI runner and a
local arm64 build under Rosetta agree."* So the gate did work automatically, on
PR heads, through 2026-08-11.

The corrected and verified claim, which is what the file now says, is narrower and
still serious: **the gate has no `push` trigger; `main` has taken only direct
pushes since 2026-08-13; consequently every run of either workflow since that date
has been manual; and neither hash published at the audited HEAD has ever been the
subject of a run, with the last run of each workflow reporting failure.**

### Evidence obtained during validation that the filing did not have

The unauthenticated GitHub Actions API was queried for the complete run history of
both workflows (`/repos/ShieldedLabs/zero/actions/workflows/{324882157,329724684}/runs`).
This replaces the filing's inference with direct evidence:

- 51 shim runs and 18 hub runs, **zero `push` events** in either.
- Events by era: `pull_request` dominates through 2026-08-11; from 2026-08-13
  onward all but one run is `workflow_dispatch`.
- shim runs 48/49 and hub run 17, all on `e91170ed` (2026-08-17), all
  **failure** — an exact, independent confirmation of the sentence the project
  published and then removed.
- No run exists on `ff1fccd`, `0be521a`, or any of the 14 commits between
  `e91170ed` and the audited HEAD `62baea8`.

*(Note for reproduction: upstream `main` has advanced past the audited snapshot.
Shim runs 50/51 and hub run 18 are on commits **after** `62baea8` and are outside
this audit's scope; run 51 succeeded on a post-audit commit. The audit target was
confirmed byte-identical to `audit-context/zero` at `62baea8`.)*

### Facts re-verified against the target and the repository

- Trigger sets: `zeronym-shim-reproduce.yml:24-38` and
  `zeronym-hub-reproduce.yml:18-32` — `workflow_dispatch` + `pull_request` with a
  `paths:` list, no `push:`. Neither `on:` block has been edited since it was
  written (`git log -S'pull_request'` on the shim file returns only `1c616c3`,
  2026-07-31).
- `git log --format='%h %s' -60 main | grep -cE 'Merge pull request|\(#[0-9]+\)'`
  → **0**; the newest PR merge `b174d78` sits at position **61**. One non-PR merge
  (`d1dc077`) is inside the window and fires no `pull_request` event either.
- Re-baseline positions in `main`: `0be521a` (11), `ff1fccd` (12), `3fcbaa9` (17),
  `bfa9ad7` (30), `68a1f6f` (36), `3c52859` (42), `175f375` (60) — all after
  `b174d78`, therefore all direct pushes.
- Worst window recomputed from scratch: `c68d320`→`0fda2c2` = **22 commits, 16
  compiled-source changes**. The addendum's table is sound; "up to 23 commits and
  16 successive compiled-source changes" is the correct statement, not "one to
  four".
- `git log 0be521a..62baea8 -- <shim compiled inputs>` → **0 commits**;
  `git log ff1fccd..62baea8 -- <hub compiled inputs>` → **0 commits**.
  **Neither hash is stale at HEAD.**
- `shim/deploy/README.md`: `:36-46` (the ledger of what each artefact proves),
  `:260-264`, `:283-285`, `:473-477`, `:1066-1090` (the three-state table and the
  "tripwire working" sentence) — all quoted verbatim and confirmed.
- `shim/deploy/reproduce.sh:80-120` — the verdict logic as quoted.
- `deploy.sh` — `grep -n 'EXPECTED_SHA256\|reproduce.sh\|caution verify\|build.sh'`
  returns four hits, all inside log strings or an `APP_SOURCE` guard; none is an
  invocation. The "no verification in `deploy.sh`" claim holds.
- `PROVENANCE` blocks at `hub/deploy/caution/assemble-caution.sh:503-519` and
  `shim/deploy/caution/assemble-caution.sh:585-604` — as quoted, including the
  `unrecorded` fallback.
- The commit texts of `105c21d2`, `aad6523e`, `175f375`, `53bab86` and `fa17e92`
  — all read directly from `git log`/`git show` and quoted accurately.
- `OPERATORS.md:188-193` — the 2026-08-14 all-three-PCRs-reproduced measurement
  that makes the deleted paragraph's PCR sentence stale.

### Two further claims in the filed text that were corrected

1. *"`shim/deploy/README.md:662` still says '**Nothing about the build is
   outstanding**'"* — **misleading as cited.** Line 662 sits inside
   `### Re-baseline, 2026-08-01 (second): the predicate widened`, a dated
   historical write-up of one re-baseline, not a present-tense status claim. The
   sentence has been removed from this issue; the same passage's *"no PCR has been
   computed from this binary and no attestation document exists"* is stale in a
   different way and is owned by
   `deploy-readme-says-no-enclave-no-eif-and-no-pcr-exist-which-its-own-current-row-refutes.md`.
2. The `paths:` filter was argued inside this issue's Recommendation 1. It is kept
   only as a cross-reference; the finding belongs to the sibling Low.

### Handling of the deleted paragraph (coordinator item 6h)

Reported above as a **dated factual sequence with no motive imputed**, and three
facts are stated alongside it so it cannot be read as an accusation: `53bab86`
*softened* an already-stronger claim rather than being the first disclosure; the
paragraph contained one genuinely stale sentence (PCR0/PCR1), which fully explains
removing the paragraph rather than editing it; and `fa17e92`'s own commit message
explicitly enumerates the disclosure being lost. **The finding does not rest on the
deletion** — every load-bearing fact is established from the workflow files, the
commit graph and the Actions API. Recommendation 4 asks for the accurate parts to
be restored, which is a documentation request, not an allegation.

### Why Medium

- **Not High.** No user's funds or privacy is directly lost; exploitation of
  Scenario A requires an already-privileged supply-chain position; the control
  demonstrably works when it is run (it caught both recorded incidents); the gate
  detects staleness and non-determinism rather than malice; and at HEAD neither
  hash is actually stale.
- **Not Low.** This is the project's only automated gate on the artefacts the whole
  trust model names, and `AUDIT-INSTRUCTIONS.md` states that a break in that chain
  is a security finding rather than tooling noise. Both published hashes are
  currently unconfirmed and the last recorded evidence for each is a failure; the
  stale window is a recurring, unbounded state of `main` up to 22 commits long; the
  shipped `PROVENANCE` artefact hands third parties an instruction that fails
  inside it; and the reference document teaches the reader to dismiss the gate's
  only alarm. The affected population — auditors and third-party operators — is the
  entire mechanism by which an end user gets any assurance at all.
- **Remediation is cheap and disproportionately effective:** recommendation 1 is
  one line per workflow.

DO NOT DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`docs/SECURITY-ISSUE-TEMPLATE.md`).
