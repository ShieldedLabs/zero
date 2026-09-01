# The `repair check-witnesses` command

Every note the wallet can spend needs a *witness*: a path proving the note's commitment is
in the chain's note commitment tree. `zallet repair check-witnesses` tries to construct one
for each note the wallet believes is currently spendable, and reports the block ranges that
would have to be rescanned to rebuild the ones it could not.

By default the command only reports. Each range is printed to standard output as
`start..end`, with the end exclusive, and nothing is printed if every note has a witness:

```
$ zallet repair check-witnesses
2999500..2999750
3000100..3000200
```

Pass `--queue-rescan` to queue those ranges to be rescanned the next time the wallet syncs:

```
$ zallet repair check-witnesses --queue-rescan
2999500..2999750
```

This is the only way the command modifies the wallet. Rescanning happens on the next
`zallet start`, and only affects how the wallet reads the chain — no funds are at risk
either way.

## What this command cannot tell you

It examines only notes the wallet holds. A wallet's note commitment tree can disagree with
the chain's at positions where the wallet has no note of its own, and this command will not
notice: it will report that every spendable note has a witness while the wallet is still
broken.

So a clean result here is not proof that the wallet's trees are sound. In particular, if
`zallet start` is exiting with a note commitment tree conflict, this command is unlikely to
find anything, and the tool you want is
[`zallet repair truncate-wallet`](truncate-wallet.md) — roll the wallet back to before the
conflicting data and let it rescan.
