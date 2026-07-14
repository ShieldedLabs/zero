(note: this is a temporary file, to be added-to by anybody, and moved to
release-notes at release time)

Notable changes
===============

Deeper reorg tolerance, matching Zebra
--------------------------------------

The maximum chain reorganization depth the node will follow (`MAX_REORG_LENGTH`)
has been raised from 99 to 1000 blocks, matching the rollback window Zebra ships
since v5.2.0 (`MAX_BLOCK_REORG_HEIGHT`). Previously, if the network accepted a
reorg deeper than 99 blocks (as happens on testnet, particularly around network
upgrade activations), zcashd would shut down for safety and require a full
`-reindex`; a zcashd peered exclusively with a Zebra node could not recover at
all without reindexing, because its only peer kept offering the reorged chain.

Two consequences of the larger window:

- The node now follows reorgs of up to 1000 blocks automatically. Services that
  relied on the previous behavior (a transaction with 100 confirmations could
  never be rolled back without operator intervention) should re-evaluate their
  confirmation policy.
- The wallet's note witness cache (`WITNESS_CACHE_SIZE = MAX_REORG_LENGTH + 1`)
  grows in step: wallets holding unspent shielded notes retain 1001 witness
  snapshots per note instead of 100, increasing `wallet.dat` size and flush
  cost. Wallets with only transparent funds are unaffected.

