# Delta ledger

Every intentional divergence of Zero from its vendored upstreams. This is the
human-readable mirror of `git log --grep='^\[zero\]'`. Keep it current on every
`[zero]` commit and whenever an `[upstream-pending]` carry is added or dropped.

Status legend: **permanent** (Zero-only, never upstreamed) | **pending #N**
(temporary carry of upstream PR N) | **upstreamed #N** (merged upstream, kept
only until next subtree pull drops it).

## Import baseline

Pristine subtree imports (no delta yet). Recorded upstream SHAs:

| Component | Prefix    | Upstream@branch              | Import SHA   |
|-----------|-----------|------------------------------|--------------|
| zcashd    | `zcashd/` | zcash/zcash@master           | `e06de4dab`  |
| zebra     | `zebra/`  | ZcashFoundation/zebra@main   | `ce067a989`  |
| zaino     | `zaino/`  | zingolabs/zaino@dev          | `4befbbb2f`  |
| zallet    | `zallet/` | zcash/wallet@main            | `d457b5515`  |

## zcashd

_No divergence yet._

## zebra

_No divergence yet._

## zaino

_No divergence yet._

## zallet

_No divergence yet._

---

Entry template:

```
### <short title>
- **Status:** permanent | pending #N | upstreamed #N
- **Commit(s):** <sha> (`[zero] ...` or `[upstream-pending #N] ...`)
- **Files:** <prefix>/path, ...
- **Why:** one line.
- **Upstream PR:** <link or n/a>
```
