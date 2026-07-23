# Changelog

Zero release notes. One `## vN` section per release, newest first: grouped
one-liners, succinct and complete. `release.yml` embeds the matching section
in the GitHub release body and refuses to release without one. Stage upcoming
entries under `## Unreleased`, then retitle the section to the version (with
date) before dispatching the release.

## Unreleased

## v21 - 2026-07-22

### Security

- zebra: peers submitting invalid shielded proofs are now banned. Failed
  Orchard/Ironwood Halo2 proofs, Orchard binding signatures, and Sprout
  JoinSplit signatures previously collapsed to a zero misbehaviour score,
  letting a peer force expensive verification indefinitely.
  (GHSA-2p4c-3q4q-p463, 036f233273)
- zebra: known-block queries drain rejected-block notifications before
  checking sent hashes, so an honest block body sharing a header hash with a
  rejected body is accepted immediately instead of stalling sync as a
  duplicate. (GHSA-8gxx-hc65-vv82, 81b51213b3)

### Fixed

- zebra: outbound peer connections require the peer to advertise NODE_NETWORK,
  and rejected peers are recorded so they are not redialed. Without this, the
  current mainnet peer population (dominated by non-serving services=0 nodes
  since the zcashd EoS halt) fills all outbound slots and stalls fresh syncs
  at genesis indefinitely. Verified A/B: patched node syncs from the same
  seeders where stock zebra 6.2.x stalls. Upstreamed as
  ZcashFoundation/zebra#11061. (2af34bee90)

## v20 - 2026-07-21

- zcashd: Ironwood is now fully supported (mainnet and testnet)
- zcashd: Bumped the EOL date to ~2026-09-03 (restores the original 7-week EOL window)
- zcashd: Various bug fixes, hardening, and regression tests

## v19 - 2026-07-18

### Fixed

- zebra: `invalidateblock` no longer aborts the node when built with the
  `progress-bar` feature (all shipped images): the chain-metrics code expected
  a fork length that is legitimately absent after a rewind. Found by the qa
  reorg scenario; still unfixed upstream as of zebra v6.2.0. (161d97c940)

### CI

- GHCR images build on a 16-vCPU runner. (caeb8ef47f)

## v18 - 2026-07-17

### Fixed

- zaino/zallet: the chain-index loop survives initial validator sync instead
  of exiting while zebra is still working through checkpoints. (41d4342ded)

### CI

- z3-smoke probes run on GitHub-hosted runners and fail fast when a stack
  container is crash-looping. (a4e25d212b, 41cb5ba43b)

## v17 - 2026-07-17

### Security

- zebra subtree bumped 6.0.0 to 6.1.0, picking up four upstream advisories:
  block-template size reservation, quadratic transparent-value validation,
  sync stalling from rejected block bodies, and misbehavior scoring for peers
  pushing consensus-invalid transactions. (fe5ba7cf4a)

### Fixed

- zallet: a reorg landing between the scan cursor and stored-ahead block rows
  no longer crash-loops the wallet with a fatal `BlockConflict`; the wallet
  now rewinds and rescans. (c1e9f3a2fc)
- zaino: the chain-index sync loop tolerates a syncing validator instead of
  exiting after ~45s of `MissingBlockError`. (1c1c3029ea)
- zallet: fork-pinned zaino with the syncing-validator patience backport.
  (d80dd3217d)

### Performance

- zallet: spend-search history ingestion is batched. (1f66886141)

### Deploy

- Shutdown grace periods raised across both bundles (zcashd 5m, zebra /
  zaino / zallet 2m; systemd `TimeoutStopSec=300`), and zebra's
  1-connection-per-IP Docker pitfall documented. (7e80df24dd)

### Testing

- qa/regtest-harness: new reorg regression scenario (live-node
  `invalidateblock` + `generate` under a running wallet, restart variant),
  plus golden-chain snapshots, release-binary runs, and parallel scenario
  groups. (2d746000d2, d05b22eaa9, 0a74cf3d4f)

### Docs

- zebra SECURITY.md: Zero/zebra vulnerabilities are disclosed to the Zakura
  project. (a9140af610, 3ea2eb72d1)

---

Releases v1 through v16 predate this changelog; see the git log between tags
and each release's assets.
