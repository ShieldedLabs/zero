# Changelog

Zero release notes. One `## vN` section per release, newest first: grouped
one-liners, succinct and complete. `release.yml` embeds the matching section
in the GitHub release body and refuses to release without one. Stage upcoming
entries under `## Unreleased`, then retitle the section to the version (with
date) before dispatching the release.

## v28 - 2026-09-02

### Fixed

- zcashd: the mainnet end-of-service halt moves from block 3,471,448
  (~2026-09-04) to 3,527,448 (~2026-10-23), with the shutdown warning from
  block 3,511,320 (~2026-10-08). Every release from v20 through v27 embedded
  the v20 release height, so all of them halt at 3,471,448; there is no
  runtime override, only a new binary moves it. Testnet and regtest are not
  affected. (#59)
- zebra: the mainnet end-of-support halt moves from block 3,545,960
  (~2026-11-08) to 3,564,960 (~2026-11-24), the horizon of upstream zebra
  6.3.0, with the warning from block 3,548,832 (~2026-11-10). v26 and v27
  both reported 3,545,960 because neither re-vendored zebra. Zero now carries
  the release height and re-sets it at every release. (#59)

## v27 - 2026-09-02

### Performance

- Zebra block verification reuses transparent script verification from mempool
  admission (#45): a transaction verified on entry to the mempool skips the
  interpreter, ZIP-244 sighash, and signature checks when the block that mines
  it is verified, keyed by `WtxId` so an authorizing-data twin misses.
- Zebra block-path UTXO lookups overlap up to 64-way instead of awaiting one
  state round trip per input (#46).
- Measured together on a full-block shape of 7 transactions with 1000
  transparent inputs each, state serving lookups under 1ms of latency: 2.9 s
  to 59 ms for a block whose transactions were mempool-verified, and the
  lookup wall alone drops 2.8 s to 0.37 s cold (#49 has the full matrix).

### Added

- A worst-case transaction verification benchmark for zebra-consensus (#49),
  byte-comparable with Zakura's copy of the same benchmark so the two nodes
  can be A/B measured on identical inputs, plus Zero-only cold/warm cases for
  the 7x1000 transparent block shape with configurable state latency.
- CI runs the zebra-consensus criterion benches on every zebra-touching
  commit and posts timings to the job summary (#48).

### Subtrees

- zallet re-vendored at v0.1.0-beta.3: upstream split the workspace into a
  `zallet` launcher plus per-backend crates, and our z_listunspent and sync
  patches moved onto `zallet-core` (#54 carried the deploy fallout: the
  z3-stack image builds the zaino backend via `BACKEND=zaino`, `zallet.toml`
  selects it and opts into the non-loopback RPC bind guard, and the smoke
  probes accept the new sync-gate answer).
- zaino updated to upstream 0.8.0 (#37).
- orchard tracks upstream main now that Ironwood ships in 0.15.x;
  librustzcash and lightwalletd re-vendored and pinned to recorded tags;
  zcashd build scripts fixed for arm64 macOS (native arch detection, portable
  bdb, Apple-Silicon crc32).

### zero-indexer

- The shim and hub embed a Nym mixnet driver end to end: the hub publishes
  its mixnet address and the clearnet submit path is closed, submissions are
  padded so size stops fingerprinting a migration, and both sides get
  reproducible StageX builds with attested deploys and a one-command
  `deploy.sh`.
- Hardening wave from the external security review: bounded reads, queues,
  timeouts and fan-out across shim and hub, fail-closed expiries, ordered
  shutdown, publish-before-serve for queued migrations, and status endpoints
  (`/nym-status`) so a dead mixnet client stops reading as healthy.
- The book renames Zeronym to zero-indexer; operator and failover runbooks
  rewritten from measured behavior.

### CI

- cargo vet supply-chain gate: every Rust dependency change must land with a
  matching audit or exemption diff under the component's `supply-chain/`
  directory (new stores for zaino and orchard; zebra's and zcashd's brought
  back in sync with their trees; enforced by `cargo-vet.yml`).
- z3-smoke hardening (#56, #57): third-party actions pinned by commit SHA,
  `packages: write` scoped to the one job that pushes build cache, and
  cache-warm-only build legs can no longer redden the stack gate when a
  registry throttles a cache pull.
- Copilot reviews commits pushed straight to main, with custom instructions;
  the zeronym test jobs actually run their suites (including mixnet-driver
  tests) and the hub reproduce gate runs on push, not only on PRs.

## v26 - 2026-08-11

### Added

- `coinbase_tx_version = 5` under `[mining]` in zebra pins `getblocktemplate` to V5
  coinbase transactions. At Ironwood activation zebrad templates switched to the V6
  format, and mining-pool software that splices extranonce data into the coinbase
  broke parsing them; F2Pool works around it by running a hand-patched zebrad
  (upstream's answer, ZcashFoundation/zebra#10909, is documentation only). The pin
  gives the same result as a supported config option and stays consensus-valid.
  Unset keeps the network-upgrade default. A V5 pin with a miner address whose only
  receiver is Orchard is rejected at startup, since paying an Orchard receiver after
  NU6.3 needs the V6-only Ironwood output.

### Fixed

- Zebra now prefers the first-received chain on equal-work ties, as the protocol
  spec requires, instead of tie-breaking on the tip block hash
  (ZcashFoundation/zebra#11240, reported by F2Pool with production logs of losing
  blocks they had won by 0.4s of propagation). Siblings always tie on work in
  Zcash, so the old rule turned won propagation races into coin flips and let an
  equal-work sibling arriving arbitrarily late displace an already-adopted tip.
  Blocks are stamped with a receipt order when committed (zcashd `nSequenceId`
  analogue); strictly more work still always wins, and receipt order survives
  invalidate/reconsider.

### CI

- `z3-smoke` and `z3-regtest` no longer use GitHub's `paths:` trigger filter.
  Both now register on every pull request and decide in a 20-second `Path gate`
  job whether the expensive jobs run, computed from `git diff` against the
  merge commit's base. PR #20 (the v25 zebra security update) matched the old
  filters on 107 of its 144 changed files and GitHub still created no Actions
  check suite for it, so a 144-file consensus change merged with no stack CI
  and nothing on the PR page showing anything was missing. Each workflow also
  gained a `required` job that reports on every PR, pass or skip, so "the
  workflow never ran" can no longer look identical to "the workflow passed".
  The release `workflow_call` gate is unchanged and still runs the full stack
  unconditionally.

## v25 - 2026-08-06

### Security

- zebra: three fixes from the Zcash Foundation's upcoming security release,
  applied ahead of it as `[upstream-pending]` carries on top of upstream
  `main` (all drop on the next zebra subtree pull):
  - Blocks above the sync lookahead limit no longer score the peer that served
    them. The far-ahead hash comes from a malicious `FindBlocks` response that
    carries no peer attribution, so the follow-up download is answered by an
    independently chosen honest peer, and scoring it let an attacker get honest
    peers banned throughout initial block download (GHSA-qhr3-cvch-5fh2).
  - A block download answered with a canonical header and a rewritten coinbase
    height is re-requested immediately instead of being rediscovered a sync
    round later, and the serving peer is scored when a parent block Zebra
    already holds proves the claimed height wrong (GHSA-g95h-hw6g-pvgv,
    reported upstream by @zakura-security).
  - Peers that gossip consensus-invalid blocks are scored for misbehavior
    again. The inbound download cleanup downcast errors to `VerifyBlockError`,
    but the gossip verifier is a `BlockVerifierRouter` returning `RouterError`,
    so the downcast never matched and such peers were never banned
    (GHSA-8hh2-hrf2-cqf4).

### Changed

- zebra: subtree updated from v6.2.0 to upstream `main` (8e9ff3b2cb, past
  v6.2.3), the exact base of the Foundation's upcoming release. Brings the
  6.2.1 to 6.2.3 hardening releases (NU6.3 activation-window and peer
  connectivity fixes) plus unreleased work: MAX_MONEY value-pool enforcement
  (#10817), the block/mempool transaction-verifier split (#11095), singleton
  FindBlocks downloads (#11165), inbound address canonicalization (#11129),
  bans that clear the whole address book for an IP (#11173), zec.rocks default
  seeders (#11096), `getdeprecationinfo` (#11097), `getblocksubsidy` NU6-era
  metadata (#11172), indexer gRPC stream limits (#10980), and a
  `getblocktemplate` coinbase-cache fix (#10954). State format is unchanged
  (28); no resync needed. Binaries self-report zebrad 6.2.3 until upstream's
  release bumps the version.
- Dropped six zebra carries that merged or were superseded upstream: #11113,
  #11050, #11053, #11061 (superseded by the narrower require-while-syncing
  rule), GHSA-2p4c-3q4q-p463 (#11054), and GHSA-8gxx-hc65-vv82 (#11052).
  Still carried: #10732 FindBlocks stall gating.

### Added

- zebra: two Prometheus metrics for diagnosing slow block acceptance.
  `zebra_consensus_transaction_duration_seconds` splits transaction verification
  into `phase="utxo_fetch"` (one state round trip per transparent input) and
  `phase="checks"` (scripts, signatures, proofs), each labelled
  `request="block"` or `"mempool"`; the ratio shows whether a node is bound by
  state lookups or cryptography. `rpc_submitblock_inflight` gauges how many
  submitted blocks are being verified at once. Needs the `prometheus` feature
  and a `[metrics] endpoint_addr`, both already set in our images.

### Fixed

- zebra: `submitblock` no longer discards a solved block when the miner's client
  disconnects. Verification ran on the RPC connection, so a client timeout
  cancelled it mid-flight and the block vanished with no commit and no log line.
  Verification now completes regardless of the client, the block is still gossiped
  once the client has gone, and every outcome is logged. (50e7e57e05, 92f499b7e1)
- zebra: `submitblock` answers `duplicate-inconclusive` for a block still being
  verified, instead of verifying it a second time. A miner retrying after a
  timeout would otherwise double the work on an already-slow node; `duplicate`
  would wrongly imply the node holds a validated copy. (46b3926a4d)

### Testing

- Three regression tests cover the submitblock disconnect and resubmission
  paths, each checked to fail without the fixes above.
  `zebra-rpc/examples/submitblock_abandon_repro.rs` reproduces it on Regtest: a
  2,000-input block taking ~350ms was lost 3/3 when abandoned at 43/88/177ms,
  and commits after the fix. It also shows the block path reuses none of the
  mempool's verification work. (c9908b5dcc, a3c60db9f8)

## v24 - 2026-07-28

### Fixed

- zebra: peers are no longer penalized for relaying transactions with the
  adjacent NU6.2 or NU6.3 consensus branch ID within 40 blocks either side of
  NU6.3 activation. Around the boundary a peer's tip can legitimately sit on
  the other side of it, and Zebra scored that as misbehavior and banned the
  peer, shedding honest peers exactly when the peer set matters most. Ironwood
  activates on Mainnet at block 3,428,143. Carried from upstream v6.2.3, which
  is an optional hardening release with no security advisories attached; taken
  for the activation-window behavior specifically. (upstream #11113, 157e96f93e)

### Testing

- The z3 smoke probes assert that the deployed zebrad reports the NU6.3 branch
  ID and pins activation at 3,428,143. A stale image passed every other probe
  while silently lacking both the consensus rules and the grace window above;
  the 2026-07-17 cached-layer incident shipped that exact class of mismatch.
- New `qa/log-filter-injection-test.sh` covers the zebrad-log-filter fix from
  v23 with seven assertions (command substitution, backticks, metacharacters,
  backslash preservation, passthrough). Verified to fail against the pre-fix
  script. It runs as a seconds-long `quick-checks` job ahead of the image
  builds, since zebra-utils scripts are not in the runtime image and cannot be
  stack-probed.

### CI

- z3-smoke and z3-regtest now trigger on vendored crate sources
  (`zebra/zebra-*`, `zebra/zebrad`, zaino and zallet packages, lockfiles), not
  just Dockerfiles and deploy config. Previously a PR that changed consensus or
  wallet code but no Dockerfile ran no CI at all, which is how v21's zebra
  security carries reached the release dispatch untested and why the
  release-time `workflow_call` gate was added. Gating the release is the
  backstop; running on the PR is the fix. Scoped to crate sources so docs and
  changelog edits do not trigger a three-image build.

## v23 - 2026-07-27

### Security

- zebra: ZIP-317 policy is now applied to mempool transactions before any
  expensive cryptographic verification, instead of after. This completes the
  GHSA-2p4c-3q4q-p463 mitigation: v21 banned peers that send invalid shielded
  proofs, but the node had already paid for proof verification by the time the
  ban landed. An unauthenticated peer could force full Halo2/Groth16
  verification on transactions that could never be mined. Rated high upstream,
  and specific to NU6.3/Ironwood-active nodes, which is every image we ship.
  (upstream #11053, 3b7467e7b8)
- zebra: `zebrad-log-filter` no longer executes log text as shell. It used GNU
  sed's `e` flag with log-line content interpolated into the executed string,
  so piping logs through the filter ran attacker-influenced text as commands.
  Not shipped or invoked by any image, but operators run it by hand against
  live logs. (upstream #11050, 196815dbe1)

Scope note: upstream zebra v6.2.2 carries a third security fix, redacting the
Elasticsearch password from the startup config dump (#11051). It is deliberately
not carried. That code is behind the `elasticsearch` feature, which none of our
builds enable (`FEATURES=release_max_level_info,progress-bar,prometheus`), and
the fix changes a public field type, so carrying it would add a breaking
zebra-state API change with no effect on anything we ship. It arrives on the
next zebra subtree pull.

Both carries are marked `[upstream-pending]` and drop on the next zebra subtree
pull that reaches v6.2.1 / v6.2.2.

## v22 - 2026-07-27

### Fixed

- zcashd: `z_gettreestate` help documents the `ironwood` object's pre-activation presence with a null `finalRoot`
- zcashd: raw-tx JSON emits `ironwood` only for exact-v6 transactions (ZFUTURE no longer shows an empty bundle)
- zcashd: `GetHistoryAt`'s size-check error names the invalid record size
- zcashd: Updated source documentation and clarified error messages

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
