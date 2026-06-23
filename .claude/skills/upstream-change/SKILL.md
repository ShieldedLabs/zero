---
name: upstream-change
description: Prepare an upstream PR from Zero changes to one component (zcashd|zebra|zaino|zallet). Splits the prefix history or extracts the relevant commits, rebases onto fresh upstream, reformats to the project's CONTRIBUTING style, runs upstream tests, and drafts the PR. Use when asked to upstream/contribute/send a fix back to upstream.
---

# upstream-change

Turn an in-tree change into a clean, focused PR against the real upstream repo.
Per MAINTENANCE.md, most contributable work should originate upstream-first; use
this skill when a change started life in Zero and now needs to go up, or to carry
a `[zero]`/`[upstream-pending]` fix upstream.

## Inputs

- **component**: zcashd | zebra | zaino | zallet
- **commit range or SHAs**: the in-tree commit(s) to upstream (must be the change
  you want; confirm scope with the user).

## Component map

| Component | Prefix    | Upstream repo                 | Default base | CONTRIBUTING |
|-----------|-----------|-------------------------------|--------------|--------------|
| zcashd    | `zcashd/` | zcash/zcash                   | `master`     | CONTRIBUTING.md |
| zebra     | `zebra/`  | ZcashFoundation/zebra         | `main`       | CONTRIBUTING.md |
| zaino     | `zaino/`  | zingolabs/zaino               | `dev`        | CONTRIBUTING.md / AGENTS.md |
| zallet    | `zallet/` | zcash/wallet                  | `main`       | CONTRIBUTING.md / AGENTS.md |

## Procedure

1. **Scope check.** Confirm the target commit(s) only concern this component's
   prefix. If a commit mixes `<prefix>/` with Zero glue or another prefix, isolate
   the prefix portion (e.g. `git format-patch` filtered to the prefix, or
   reconstruct a focused diff). Never send Zero glue upstream.

2. **Fresh upstream checkout.** In a scratch dir, clone or worktree the upstream
   at its latest base branch:
   `git clone --depth=50 <upstream-url> /tmp/zero-up-<component>` then checkout
   the base branch (or a chosen base tag).

3. **Read their conventions.** Read the upstream CONTRIBUTING / AGENTS file and
   recent merged PRs to match commit format, sign-off, branch naming, and test
   expectations.

4. **Apply our change.** Rebase/cherry-pick the isolated change onto fresh
   upstream, stripping the `<prefix>/` path component so paths are repo-root
   relative. Resolve any drift against current upstream. Split into atomic,
   single-concern commits; add DCO sign-off if the project requires it.

5. **Test upstream.** Run the upstream project's own test suite from the scratch
   checkout. Fix until green or report blockers.

6. **Draft the PR.** Write a description in their style: problem, change, testing,
   any follow-ups. Reference the corresponding DELTA.md entry context but keep the
   PR self-contained (a maintainer should not need Zero context).

7. **Stage the push (outward-facing: confirm first).** Pushing to a fork and
   opening a PR is an external action. Prepare the branch and PR body, then ask
   the user before pushing. If `gh` is unavailable, push the branch to the user's
   fork and provide the compare URL for them to open the PR.

8. **Track it.** Update DELTA.md: mark the in-tree change `pending #N` once the PR
   exists; note it should be dropped on the next subtree pull after it merges.

## Notes

- One concern per PR. If the in-tree commit bundles several fixes, produce several
  PRs.
- Prefer rebasing onto the upstream **base branch** so the PR merges cleanly, not
  onto our vendored snapshot.
- Do not force-push to upstream branches; only to our own fork's PR branch.
