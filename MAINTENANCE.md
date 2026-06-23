# Maintaining Zero

Zero vendors four upstreams as git subtrees (see [SUBTREES.md](SUBTREES.md) for
the mechanics). This file is the **policy**: how we make changes so that two
flows stay cheap forever.

1. **Upstream useful fixes** back to zcash/zebra/zaino/zallet.
2. **Pull useful updates** down from those upstreams.

Both costs scale with **how far our tree diverges from upstream**. So the entire
discipline reduces to one rule: *keep our intentional delta small, explicit, and
classified.*

## The classification rule (do this before you start a change)

Every change is one of two kinds. Decide up front.

### Upstream-bound (bugfix, perf, general feature)

Work **upstream-first**: make the change in a clone of the real upstream repo,
open a normal PR there, and let it flow home via `git subtree pull`. Do **not**
originate it in Zero.

If we need the fix in Zero before it merges upstream, cherry-pick the commit
in-tree as a **temporary** carry, marked `[upstream-pending #<PR>]`, and drop it
on the next subtree pull once it lands.

Use the `upstream-change` skill to prepare the PR branch from an existing in-tree
commit when a change started life in Zero by accident.

### Zero-only (will never go upstream)

Commit directly in-tree, prefixed `[zero]`. Examples:

- zcashd Ironwood support + the **hardcoded end-of-life** logic.
- Enterprise packaging, config defaults, deployment glue.
- Z3 integration wiring across components.
- Branding / Shielded Labs specifics.

Record every Zero-only divergence in [DELTA.md](DELTA.md).

## Per-component policy

The rule is not uniform. Bias differs by how alive the upstream is.

| Component | Upstream state | Default bias |
|-----------|----------------|--------------|
| `zebra`   | Active (ZF)        | Upstream-first, strongly |
| `zaino`   | Active (Zingo)     | Upstream-first, strongly |
| `zallet`  | Active (ECC)       | Upstream-first, strongly |
| `zcashd`  | Winding down; we hardcode EOL | Mostly Zero-only; upstream only clear bugfixes |

## Commit-message convention

A subject has two independent parts: an optional **divergence marker** then a
**conventional-commit type**.

The one question that decides the marker: **does the commit touch a vendored
subtree dir (`zcashd/`, `zebra/`, `zaino/`, `zallet/`)?**

- **Yes** -> lead with a marker, then the type:
  - `[zero] <type>: ...`            - permanent Zero-only divergence.
  - `[upstream-pending #N] <type>: ...` - temporary carry of an unmerged upstream
    PR; dropped on the next subtree pull after #N merges.
- **No** (our own files: README, SUBTREES.md, MAINTENANCE.md, DELTA.md,
  `.claude/`) -> just the type, no marker.

The type is the usual `feat` / `fix` / `doc` / `skill` / `chore` / etc.

Examples:

| Commit | Touches vendored dir? | Subject |
|--------|-----------------------|---------|
| Security contact in `zebra/SECURITY.md` | yes, permanent | `[zero] fix: route security reporting to Shielded Labs` |
| Stopgap fix awaiting upstream PR        | yes, temporary | `[upstream-pending #42] fix: ...` |
| New maintenance doc                     | no             | `doc: ...` |
| New skill                               | no             | `skill: ...` |
| Subtree import/merge                    | yes, auto      | (git's own message, left untouched) |

`git log --grep='^\[zero\]'` is the canonical list of our permanent delta;
`git log --grep='upstream-pending'` is our outstanding carries. `DELTA.md` is the
human-readable, PR-linked mirror of both.

## Pulling upstream (downstream flow)

- **Don't** track moving branches forever. Pin to upstream **release tags** when
  one exists; pull `main`/`dev`/`master` only deliberately.
- Pull **one component at a time, on its own branch, with tests**, never blind on
  `main`.
- Use the `update-subtree` skill: it fetches, summarizes incoming commits, pulls
  `--squash` on a branch, surfaces conflicts with our `[zero]` delta for review,
  and runs that component's test suite.
- The weekly **upstream-watch** scheduled job reports what is new and the
  conflict risk against our delta. It never merges on its own.

## Upstreaming (upstream flow)

- Use the `upstream-change` skill: it splits the relevant prefix history, rebases
  onto fresh upstream, reformats commits to that project's CONTRIBUTING style,
  runs their tests, and drafts the PR.
- Keep PRs focused and atomic. One concern per PR. Never bundle Zero glue into an
  upstream PR.
- When a PR merges, remove any matching `[upstream-pending]` carry on the next
  subtree pull.

## Why this is tractable now

The historically miserable parts of fork maintenance are exactly the mechanical
ones agents handle well: 3-way conflict resolution informed by our recorded
rationale, splitting/rebasing/reformatting commits into clean upstream PRs,
triaging large batches of incoming upstream commits, and keeping DELTA.md
current. Humans review; agents do the surgery.
