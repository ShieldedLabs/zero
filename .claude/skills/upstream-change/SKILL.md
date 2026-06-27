---
name: upstream-change
description: Prepare an upstream PR from Zero changes to one component (zcashd|zebra|zaino|zallet|orchard|librustzcash). Splits the prefix history or extracts the relevant commits, rebases onto fresh upstream, reformats to the project's CONTRIBUTING style, runs upstream tests, and drafts the PR. Use when asked to upstream/contribute/send a fix back to upstream.
---

# upstream-change

Turn an in-tree change into a clean, focused PR against the real upstream repo.
Per MAINTENANCE.md, most contributable work should originate upstream-first; use
this skill when a change started life in Zero and now needs to go up, or to carry
a `[zero]`/`[upstream-pending]` fix upstream.

## Inputs

- **component**: zcashd | zebra | zaino | zallet | orchard | librustzcash
- **commit range or SHAs**: the in-tree commit(s) to upstream (must be the change
  you want; confirm scope with the user).

## Component map

| Component | Prefix    | Upstream repo                 | Default base | CONTRIBUTING |
|-----------|-----------|-------------------------------|--------------|--------------|
| zcashd    | `zcashd/` | zcash/zcash                   | `master`     | CONTRIBUTING.md |
| zebra     | `zebra/`  | ZcashFoundation/zebra         | `main`       | CONTRIBUTING.md |
| zaino     | `zaino/`  | zingolabs/zaino               | `dev`        | CONTRIBUTING.md / AGENTS.md |
| zallet    | `zallet/` | zcash/wallet                  | `main`       | CONTRIBUTING.md / AGENTS.md |
| orchard   | `orchard/`| zcash/orchard                 | `feat/ironwood` | README.md (no CONTRIBUTING) |
| librustzcash | `librustzcash/` | zcash/librustzcash      | `main`       | CONTRIBUTING.md |

## Per-component contribution gates

Some upstreams enforce a contribution policy that must be satisfied **before** a
PR is opened. Always read the component's own `CLAUDE.md` / `AGENTS.md` /
`CONTRIBUTING.md` in the vendored prefix first; treat any gate there as binding.

**zebra (mandatory gate).** `zebra/CLAUDE.md` defines a hard PR-compliance gate.
Before doing any PR-prep work for zebra, confirm with the user, in these terms:

- Has this change been discussed with the Zebra team in a GitHub issue or Discord?
- What is the issue link / number?
- Has a Zebra **team member** acknowledged it? (An issue opened the same day with
  no maintainer response does **not** satisfy the gate.)

If the gate is not satisfied: do not prepare or push a PR. Offer to help draft or
refine the upstream issue, and tell the user the PR would likely be closed without
prior team discussion. **Maintainer bypass:** if the user is a Zebra maintainer,
the gate is skipped. Check with:
`gh api repos/ZcashFoundation/zebra --jq '.permissions.maintain'` (true = bypass).

Other components: no blocking gate known as of the import baseline, but re-check
their AGENTS/CONTRIBUTING each time, since upstream policy changes.

## Procedure

1. **Scope check.** Confirm the target commit(s) only concern this component's
   prefix. If a commit mixes `<prefix>/` with Zero glue or another prefix, isolate
   the prefix portion (e.g. `git format-patch` filtered to the prefix, or
   reconstruct a focused diff). Never send Zero glue upstream.

2. **Contribution gate.** Satisfy the component's gate above before any further
   work. For zebra this is mandatory and blocking; stop here if it is not met.

3. **Fresh upstream checkout.** In a scratch dir, clone or worktree the upstream
   at its latest base branch:
   `git clone --depth=50 <upstream-url> /tmp/zero-up-<component>` then checkout
   the base branch (or a chosen base tag).

4. **Read their conventions.** Read the upstream CONTRIBUTING / AGENTS file and
   recent merged PRs to match commit format, sign-off, branch naming, and test
   expectations.

5. **Apply our change.** Rebase/cherry-pick the isolated change onto fresh
   upstream, stripping the `<prefix>/` path component so paths are repo-root
   relative. Resolve any drift against current upstream. Split into atomic,
   single-concern commits; add DCO sign-off if the project requires it.

6. **Test upstream.** Run the upstream project's own test suite from the scratch
   checkout. Fix until green or report blockers.

7. **Draft the PR.** Write a description in their style: problem, change, testing,
   any follow-ups. Keep it self-contained (a maintainer should not need Zero
   context).

8. **Stage the push (outward-facing: confirm first).** Pushing to a fork and
   opening a PR is an external action. Prepare the branch and PR body, then ask
   the user before pushing. If `gh` is unavailable, push the branch to the user's
   fork and provide the compare URL for them to open the PR.

9. **Track it.** If the change is also carried in-tree as a stopgap, ensure its
   commit is tagged `[upstream-pending #N]` with the PR number (amend the subject
   if it was committed before the PR existed). That tag is the record:
   `git log --grep='upstream-pending'` lists outstanding carries, and the carry
   is dropped on the next subtree pull after #N merges. No ledger to update.

## Notes

- One concern per PR. If the in-tree commit bundles several fixes, produce several
  PRs.
- Prefer rebasing onto the upstream **base branch** so the PR merges cleanly, not
  onto our vendored snapshot.
- Do not force-push to upstream branches; only to our own fork's PR branch.
