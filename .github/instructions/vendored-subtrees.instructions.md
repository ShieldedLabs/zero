---
applyTo: "zcashd/**,zebra/**,zaino/**,zallet/**,orchard/**,librustzcash/**,lightwalletd/**"
---

# Reviewing vendored upstream code

These directories are git subtrees of upstream projects. We did not write most
of this code, we cannot unilaterally change its conventions, and upstream
reviews its own work.

## Scope

- Review only the lines this pull request changes, and code those lines
  directly break.
- Do not review unchanged upstream code that merely appears in the diff
  context.
- A subtree update pull request imports many files at once. On those, the
  interesting content is the conflict resolutions and any file that carries a
  `[zero]` change — not the upstream commits being imported. Reviewing
  upstream's own code here produces noise, not findings.

## What matters most in this code

- **Divergence between implementations.** `zebra/` and `zcashd/` must agree on
  consensus, and `zaino/` and `lightwalletd/` serve the same protocol. A change
  to one that is not mirrored where it needs to be is a real bug even when
  everything compiles and each project's own tests pass. Say so explicitly when
  a change looks one-sided.
- **Carrying a `[zero]` change correctly.** When a diff touches code near an
  existing `[zero]` modification, check whether the change preserves that
  modification's intent or silently reverts it. Subtree merges are where our
  deltas get lost.
- **Consensus rules the diff does not show.** A version gate, branch ID, or
  upgrade check that omits one case is the highest-cost bug class here and it
  never looks wrong locally. If a change adds a new transaction version,
  protocol version, or upgrade, verify every place that switches on that
  dimension handles the new case.

## Style

Do not report style, formatting, naming, or idiom differences in vendored
code. It follows upstream's conventions on purpose, and changing it creates
merge conflicts on the next subtree pull.
