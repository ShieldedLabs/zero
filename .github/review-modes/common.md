## This repository

- `zcashd/`, `zebra/`, `zaino/`, `zallet/`, `orchard/`, `librustzcash/` and
  `lightwalletd/` are git subtrees of upstream projects. Our changes to them
  carry `[zero]` in the commit subject.
- `zeronym/` is first-party code.
- Most of the line count is upstream code we did not write. Review the lines
  this pull request changes, not the surrounding code.

## Report only concrete failures

- Give specific inputs or state, and the wrong output, panic, corruption or
  loss that follows.
- Do not report "what if this assumption changes later", or "this could be a
  problem if X" without showing how X happens.
- If you cannot construct the failure, mark the comment low confidence or say
  nothing.
- Do not report behaviour that is intended, or enforced elsewhere. Check that
  the enforcement exists before assuming it does.

## How to find defects

- Start at consensus checks, signature and hash computation, key handling,
  serialization boundaries and money arithmetic. Ask what protects each one,
  then verify that protection exists.
- Never assume the caller validates. Open the caller and check.
- Treat each defect as a template: look for the same mistake elsewhere in the
  diff, and for the matching flaw in the inverse operation.
- Read error paths, early returns and cleanup code first. They get less testing
  than happy paths.
- Read anything marked TODO, FIXME, HACK or "temporary" in the diff.
- Identify which values are caller- or attacker-controlled, and follow them to
  where they are used.

## Protocol constants are not missing validation

Do not report these as unchecked assumptions:

- Equihash solution size, 1344 bytes. Fixed by the protocol; `SOLUTION_SIZE`
  arithmetic is not an overflow risk.
- `MAX_BLOCK_BYTES` and `MAX_MONEY`. Enforced during validation; code
  downstream of validation may rely on them.
- Proof, note and commitment sizes. Fixed by the circuits.

Do report code that relies on such a bound *before* the validation that
enforces it has run, or that applies a protocol guarantee to input carrying no
such guarantee.

## Give consensus-critical changes the most scrutiny

A change here can be catastrophic while looking ordinary, because the rule it
breaks is not visible in the diff. Scrutinize:

- Signature hash computation.
- Anything gated on a transaction version, consensus branch ID or network
  upgrade activation height.
- Serialization and deserialization, especially round-trip consistency.
- Chain selection, validation ordering and mempool acceptance.

## Comment style

- Anchor each comment to the line the defect is on.
- State the defect in the first sentence.
- Do not summarize the pull request, and do not comment to praise.
- Do not report formatting, naming or style preferences.
