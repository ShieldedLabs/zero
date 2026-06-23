---
name: update-subtree
description: Pull upstream updates into one Zero subtree (zcashd|zebra|zaino|zallet) on a review branch, summarize incoming commits, surface conflicts against our [zero] delta, and run that component's tests. Use when asked to update/sync/pull a component from upstream.
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
   digest: count, notable changes, anything touching files we have `[zero]`
   divergence on (cross-check DELTA.md).

6. **Conflict pre-check.** For each file listed in DELTA.md for this component,
   note whether the incoming range touches it. Flag high-risk files before the
   pull.

7. **Pull.** `git subtree pull --prefix=<prefix> <remote> <branch|tag> --squash`.

8. **Resolve conflicts (if any).** For each conflict, read both sides and the
   relevant DELTA.md entry / `[zero]` commit rationale, propose a resolution that
   preserves our intent, and present it for human review. Do not invent behavior
   changes; preserve our delta unless the user says otherwise. After resolving:
   `git add` + complete the merge.

9. **Test.** Run the component's test command. Report pass/fail with output. For
   zcashd, a full build is heavy; do a build smoke unless asked for the full
   suite.

10. **Update bookkeeping.** If any `[zero]` delta was affected, update DELTA.md
    (new SHAs, changed line refs, dropped `[upstream-pending]` carries that have
    now merged upstream). Commit doc changes with a plain message.

11. **Report.** Summarize: commits taken, conflicts and how resolved, test
    result, delta changes. Leave the branch for the user to review and merge to
    `main`. Do not push or merge without confirmation.

## Notes

- One component per run. To update several, run repeatedly.
- If the pull is clean and tests pass, still stop for human review before merge.
- A carry marked `[upstream-pending #N]` whose PR has merged upstream should be
  dropped here: confirm the upstream commit is in the incoming range, then revert
  the carry commit and note it in DELTA.md.
