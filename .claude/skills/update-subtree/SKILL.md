---
name: update-subtree
description: Pull upstream updates into one Zero subtree (zcashd|zebra|zaino|zallet|orchard|librustzcash|lightwalletd) on a review branch, summarize incoming commits, surface conflicts against our [zero] delta, and run that component's tests. Use when asked to update/sync/pull a component from upstream.
---

# update-subtree

Pull a single vendored component up to its upstream, safely and reviewably.
Never run on `main`; never auto-merge a dirty result.

## Component map

| Component | Prefix    | Remote      | Branch   | Test command |
|-----------|-----------|-------------|----------|--------------|
| zcashd    | `zcashd/` | `up-zcashd` | `master` | `cd zcashd && ./zcutil/build.sh -j$(nproc)` (build-only smoke) |
| zebra     | `zebra/`  | `up-zebra`  | `main`   | `cd zebra && cargo test --workspace` |
| zaino     | `zaino/`  | `up-zaino`  | `dev`    | `cd zaino && cargo test --workspace` |
| zallet    | `zallet/` | `up-zallet` | `main`   | `cd zallet && cargo test --workspace` |
| orchard   | `orchard/`| `up-orchard`| `feat/ironwood` | `cd orchard && cargo test` |
| librustzcash | `librustzcash/` | `up-librustzcash` | `main` | `cd librustzcash && cargo test --workspace` |
| lightwalletd | `lightwalletd/` | `up-lightwalletd` | `master` | `cd lightwalletd && go test ./...` |

If the user named a release tag, use it in place of the branch (preferred per
MAINTENANCE.md tag-pinning policy).

## Procedure

1. **Preconditions.** Confirm `git status` is clean and current branch is `main`
   (or ask). Get today's date via `date +%F`.

2. **Branch.** `git checkout -b subtree-update/<component>-<date>`.

3. **Fetch.** `git fetch <remote>`.

4. **Find our last import point.** The previous squash commit message contains
   `Squashed '<prefix>' content from commit <sha>`. Get it:
   `git log --grep="Squashed '<prefix>'" -1 --format=%b` and parse the SHA.

5. **Summarize incoming.** Show what we are about to take:
   `git log --oneline <oldsha>..<remote>/<branch>` (or `..<tag>`). Write a short
   digest: count, notable changes, and anything touching files we have diverged
   on. Our divergence for this component is
   `git log --grep='^\[zero\]' --name-only -- <prefix>`.

6. **Conflict pre-check.** Compute our diverged files
   (`git log --grep='^\[zero\]' --name-only -- <prefix>`), intersect with the
   incoming changed files (`git diff --name-only <oldsha>..<remote>/<branch>`),
   and flag the overlap as high-risk before the pull.

7. **Pull.** `git subtree pull --prefix=<prefix> <remote> <branch|tag> --squash`.

8. **Resolve conflicts (if any).** For each conflict, read both sides and the
   `[zero]` commit that introduced our change
   (`git log --grep='^\[zero\]' -- <conflicted-path>`). Default to **ours wins**
   (keep the Zero change, keep the rest of the upstream update) unless that
   commit body says to yield. Present the resolution for human review; do not
   invent behavior changes. After resolving: `git add` + complete the merge.

9. **Test.** Run the component's test command. Report pass/fail with output. For
   zcashd, a full build is heavy; do a build smoke unless asked for the full
   suite.

10. **Report.** Summarize: commits taken, conflicts and how resolved, test
    result. Leave the branch for the user to review and merge to `main`. Do not
    push or merge without confirmation.

## Notes

- One component per run. To update several, run repeatedly.
- If the pull is clean and tests pass, still stop for human review before merge.
- A carry marked `[upstream-pending #N]` whose PR has merged upstream should be
  dropped here: confirm the upstream commit is in the incoming range, then revert
  the carry commit. The revert is the record; no ledger to update.
