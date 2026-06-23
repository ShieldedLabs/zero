# Subtrees

Zero vendors its five upstream components as **git subtrees**, one per top-level
directory. Our changes ("our versions") live directly in this repo, and we can
still pull upstream updates or split changes back out when useful.

## Layout

| Prefix    | Upstream                                      | Branch   | Remote      |
|-----------|-----------------------------------------------|----------|-------------|
| `zcashd/` | https://github.com/zcash/zcash.git            | `master` | `up-zcashd` |
| `zebra/`  | https://github.com/ZcashFoundation/zebra.git  | `main`   | `up-zebra`  |
| `zaino/`  | https://github.com/zingolabs/zaino.git        | `dev`    | `up-zaino`  |
| `zallet/` | https://github.com/zcash/wallet.git           | `main`   | `up-zallet` |
| `orchard/`| https://github.com/zcash/orchard.git          | `feat/ironwood` | `up-orchard` |

Notes:
- **zaino** tracks `dev` (its active default), not `stable`.
- **zallet** is the `zallet` crate, which lives in the `zcash/wallet` repo.
- **orchard** tracks the `feat/ironwood` feature branch (not a release tag),
  because the Ironwood work we need only lives there. Pull it deliberately, not
  routinely; switch to a tag once upstream cuts an Ironwood release.
- **zcashd** is a supported fork on a transition path with a hardcoded end-of-life
  date; it is not intended for long-term reliance.

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
