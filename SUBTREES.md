# Subtrees

Zero vendors its seven upstream components as **git subtrees**, one per top-level
directory. Our changes ("our versions") live directly in this repo, and we can
still pull upstream updates or split changes back out when useful.

## Layout

| Prefix    | Upstream                                      | Branch   | Remote      |
|-----------|-----------------------------------------------|----------|-------------|
| `zcashd/` | https://github.com/zcash/zcash.git            | `master` | `up-zcashd` |
| `zebra/`  | https://github.com/ZcashFoundation/zebra.git  | `main`   | `up-zebra`  |
| `zaino/`  | https://github.com/zingolabs/zaino.git        | `dev`    | `up-zaino`  |
| `zallet/` | https://github.com/zcash/wallet.git           | `main`   | `up-zallet` |
| `orchard/`| https://github.com/zcash/orchard.git          | `main`   | `up-orchard` |
| `librustzcash/` | https://github.com/zcash/librustzcash.git | `main` | `up-librustzcash` |
| `lightwalletd/` | https://github.com/zcash/lightwalletd.git | `master` | `up-lightwalletd` |

Notes:
- **zaino** tracks `dev` (its active default), not `stable`.
- **zallet** is the `zallet` crate, which lives in the `zcash/wallet` repo.
- **orchard** tracks `main` and is pinned to a release tag. The `feat/ironwood`
  branch it used to track was merged into `main` and shipped as the 0.15.x line,
  so orchard is now an ordinary tagged dependency with no special handling.
- **zcashd** is a supported fork on a transition path with a hardcoded end-of-life
  date; it is not intended for long-term reliance.
- **librustzcash** is the shared Rust crate workspace (`zcash_primitives`,
  `zcash_client_backend`, `zcash_keys`, `zip32`, and friends) that the Z3 stack
  (zaino, zallet) builds on. Tracks `main`, pinned to a release cohort: it
  publishes per-crate tags cut together, so pull the anchor tag
  (`zcash_client_sqlite-<version>`) rather than the branch tip. Currently the
  2026-08-19 cohort, which is what zallet builds against.
- **lightwalletd** is the original Go light client server (Zaino serves the same
  protocol in the Z3 stack). Vendored as the platform for private-lookup (PIR)
  experimentation. Tracks `master`, pinned to a release tag (currently `v0.5.4`).

## Why subtrees (not submodules)

- Working tree is self-contained: clone Zero and you have all the source, no
  separate `git submodule update` step.
- Our edits commit normally alongside the vendored code; no detached-HEAD dance.
- Upstream history is squashed on import, so Zero's log stays readable while the
  squash commit still records the upstream SHA for future pulls.

## Remotes (one-time setup)

```sh
git remote add up-zcashd https://github.com/zcash/zcash.git
git remote add up-zebra  https://github.com/ZcashFoundation/zebra.git
git remote add up-zaino  https://github.com/zingolabs/zaino.git
git remote add up-zallet https://github.com/zcash/wallet.git
git remote add up-orchard https://github.com/zcash/orchard.git
git remote add up-librustzcash https://github.com/zcash/librustzcash.git
git remote add up-lightwalletd https://github.com/zcash/lightwalletd.git
```

## Pull upstream updates

```sh
git fetch up-<name>
git subtree pull --prefix=<dir> up-<name> <branch> --squash
```

For example, to refresh zebra:

```sh
git fetch up-zebra
git subtree pull --prefix=zebra up-zebra main --squash
```

## Push our changes back upstream (optional)

Do **not** use `git subtree push` for this. We upstream changes through a proper
fork and a focused PR, not by splitting our squashed in-tree history. See the
upstreaming flow in [MAINTENANCE.md](MAINTENANCE.md) (and the `upstream-change`
skill it references).
