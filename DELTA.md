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

**Standing conflict rule for all security-contact entries below:** the Zero
security contact (`security@shieldedlabs.net`) always wins. On `subtree pull`,
if upstream edits the same lines, resolve in favor of the Zero contact and keep
the surrounding upstream text.

## zcashd

### Security reporting contact -> security@shieldedlabs.net
- **Status:** permanent
- **Files:** `zcashd/SECURITY.md` (§Receiving Disclosures)
- **Why:** route Zero-fork vulnerability reports to Shielded Labs. Replaced the
  upstream Signal-group + GitHub-advisories mechanism (ECC-internal, N/A to the
  fork) with our email.
- **Upstream PR:** n/a (Zero-only)

## zebra

### Security reporting contact -> security@shieldedlabs.net
- **Status:** permanent
- **Files:** `zebra/SECURITY.md` (§Receiving Disclosures)
- **Why:** route Zero-fork reports to Shielded Labs. **Follow-up:** the ZF PGP
  key remains in the file and does not cover Zero; publish a Zero PGP key and
  replace it.
- **Upstream PR:** n/a (Zero-only)

## zaino

### Security reporting contact -> security@shieldedlabs.net
- **Status:** permanent
- **Files:** `zaino/README.md` (§Security Vulnerability Disclosure)
- **Why:** route Zero-fork reports to Shielded Labs. Replaced the Zingo
  Matrix/CONTRIBUTING pointer and `zingodisclosure@proton.me` with our email.
- **Upstream PR:** n/a (Zero-only)

## zallet

### Security reporting contact -> security@shieldedlabs.net
- **Status:** permanent
- **Files:** `zallet/README.md` (new §Security Vulnerability Disclosure)
- **Why:** upstream README had no security contact; added a Zero disclosure
  section pointing to our email.
- **Upstream PR:** n/a (Zero-only)

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
