# Copilot code review instructions

## What this repository is

Zero vendors seven upstream Zcash components as git subtrees, one per top-level
directory: `zcashd/`, `zebra/`, `zaino/`, `zallet/`, `orchard/`,
`librustzcash/`, `lightwalletd/`. Our own changes to vendored code are marked
`[zero]` in the commit subject. `zeronym/` is first-party code, not vendored.

Most of the line count in this repo is upstream code we did not write. Review
what this pull request changes, not the surrounding vendored code.

## The bar for reporting something

Before writing a comment, answer: **can a real attacker actually exploit this in
production, to affect real users?**

- Report it only if you can describe a concrete attack path or failure path:
  specific inputs or state, and the wrong output, panic, or corruption produced.
- "What if this assumption changes later" is not a finding. Neither is "this
  could be a problem if X", where you cannot show how X happens.
- If you cannot construct the failure, either say so explicitly and mark the
  comment low confidence, or say nothing.

Prefer five findings that are all real over twenty where three are.

## Severity

- Real bugs with no security consequence are still worth reporting — flag them
  as low severity rather than dropping them. An off-by-one that wastes work, a
  logic slip, a misleading error path: all valid.
- Behaviour that is intended, or enforced elsewhere, is not a finding.

## Where to look first

- **Start at the danger and work backward.** Begin at consensus checks, signature
  and hash computation, key handling, serialization boundaries, and money
  arithmetic. Ask what should have protected that code, then verify the
  protection exists — navigate to the caller rather than assuming it validates.
- **One bug implies more.** If you find a mistake, check whether the same
  pattern appears elsewhere in the diff, and whether the inverse operation has
  the matching flaw.
- Error paths, early returns, and cleanup code get less testing than happy
  paths. Weight them accordingly.
- Anything marked TODO, FIXME, HACK, or "temporary" in the diff.

## Constants that are protocol-enforced, not missing validation

This is a consensus codebase. Several values that look like unchecked
assumptions are fixed by the Zcash protocol, and flagging them is noise:

- **Equihash solution size (1344 bytes).** Defined by the protocol for mainnet
  parameters. Every valid solution is exactly this size; changing it is a
  consensus break. `SOLUTION_SIZE` arithmetic is not an overflow risk.
- **`MAX_BLOCK_BYTES`, `MAX_MONEY` (21000000 * COIN).** Protocol constants
  enforced during validation. Code downstream of validation may rely on them.
- **Fixed proof, note, and commitment sizes.** Determined by the circuits.

The real bug in this class is code that assumes such a bound *before* the
validation that enforces it has run, or that applies a protocol guarantee to
attacker-controlled input that carries no such guarantee. Look for that
instead.

## Consensus-critical changes deserve the most scrutiny

A change can be catastrophic here while looking completely ordinary, because
the rule it breaks is not visible in the diff. Give extra attention to:

- Signature hash computation, and anything that gates behaviour on a
  transaction version, consensus branch ID, or network upgrade.
- Serialization and deserialization, especially round-trip consistency.
- Chain selection, block and transaction validation ordering, and mempool
  acceptance.
- Anything where two of the vendored implementations must agree with each
  other. A change to one that is not mirrored in the other is a divergence bug
  even when both compile and pass their own tests.

## Comment style

- Anchor every comment to the specific line, and state the defect in one
  sentence before explaining it.
- Do not summarize what the pull request does, and do not comment to praise.
- Do not report formatting, naming, or style preferences; linters cover those.
