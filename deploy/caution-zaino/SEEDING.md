# Seeding the validator: copy a synced zebra state instead of resyncing

Goal: stand up the zero-zebra v21 validator next to the enclave (same VPC
subnet) without the multi-day initial sync, by copying one of our synced
mainnet caches. Every claim here was verified against the vendored zebra
source (zebra-state config.rs / constants.rs / disk_db.rs / upgrade.rs).

## Version compatibility

The state format has a MAJOR version in the directory path
(`state/vNN/mainnet`) and minor/patch in a plain-text `version` file inside
that directory. zero-zebra v21 vendors zebrad 6.2.0, state format 28.0.0
(verified at the v21 tag: zebra/zebra-state/src/constants.rs:37).

| Source cache | Into zero-zebra v21 (format 28) | Verdict |
|---|---|---|
| v28 (k8s node snapshotted AFTER its v21 update) | direct open | cleanest, zero migration |
| v27 (local `zcash-zebrad-1` volume, zebrad 5.1.0) | auto-restore | zebra renames v27 to v28 at startup, then a seconds-fast Ironwood backfill; watch for "moved state cache", "launching upgrade task", "Zebra automatically upgraded the database format" |
| v26 or older | ignored | silent fresh sync from genesis |

CRITICAL: place a v27 cache at `state/v27/mainnet` and let zebra do the
rename itself. Hand-renaming the directory to v28 makes zebra assume the
Ironwood backfill already ran, and it panics later at the missing
`ironwood_*` data. Only major minus one is restorable.

## What to copy

- REQUIRED: the whole `<cache_dir>/state/vNN/mainnet/` directory (v27 or v28
  per the table above), INCLUDING the `version` file inside it (a missing
  version file makes zebra assume major.0.0 and can skip needed upgrades),
  extracted at the SAME `state/vNN/mainnet` relative path on the destination.
- OPTIONAL: `<cache_dir>/network/mainnet.peers` (peer bootstrap, safe
  cross-machine) and `<cache_dir>/non_finalized_state/mainnet/` (tiny, only
  meaningful after a clean shutdown).
- NEVER: `.cookie` (RPC auth secret, regenerated each start).

The state embeds no absolute paths, hostnames, or machine identity; it is
fully relocatable. A stale RocksDB `LOCK` file in the copy is harmless.

## Copy rules (the part that bites)

1. The source zebrad must be STOPPED (clean shutdown preferred; it flushes
   SSTs and syncs the WAL). An atomic filesystem/EBS/PVC snapshot of a
   running node is also acceptable (RocksDB replays the WAL; at worst the
   last in-flight block rolls back). A plain file copy of a RUNNING node is
   NOT safe: zebra uses no RocksDB checkpoint API, so compaction can rewrite
   files mid-copy and the result panics at startup.
2. The destination MUST be configured for Mainnet. The network lives only in
   the directory path; on a mismatch zebra silently ignores the copy and
   syncs from genesis.
3. chown the copied tree to the uid of the image's zebra user on the
   destination (check with `docker run --rm <image> id`).

## Transfer

276 GB (2026-07 size; grows slowly). Pick by available bandwidth:

| Path | Realistic time |
|---|---|
| cloud to cloud (S3 multipart or rsync between instances, 1 Gbps+) | about 1 hour |
| 300 Mbps link | about 2 h |
| 100 Mbps link | about 6 h |
| home upload (~35 Mbps), e.g. magic-wormhole from the Mac | 17 h+ |

zstd compression buys roughly 10-15 percent (chain data is mostly
incompressible); worth it on slow links only. magic-wormhole works fine
server-to-server but is single-stream; prefer S3 multipart or rsync for this
size.

Preferred source: snapshot the k8s zebra AFTER updating it to zero-zebra v21
(same image as the destination, so the snapshot is already format 28). The
local `zcash-zebrad-1` volume is the backup source (v27, from zebrad 5.1.0,
takes the auto-restore path).

## Restore and verify

```sh
# on the validator instance; tarball contains state/vNN/mainnet (do not rename vNN)
docker volume create zebra-cache
docker run --rm -v zebra-cache:/dest -v "$PWD/zebra-state.tar.zst:/src.tar.zst:ro" \
  alpine sh -c 'apk add --no-cache zstd tar >/dev/null && tar --zstd -xf /src.tar.zst -C /dest && chown -R <uid>:<gid> /dest'
# start zero-zebra v21 with cache_dir on that volume, then:
curl -s -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getblockchaininfo","params":[]}' \
  http://<private-ip>:8232/ | jq '.result.blocks, .result.estimatedheight'
```

Expect the height to start at the snapshot height and the tail to catch up at
a few hundred blocks per minute (observed ~300/min on modest hardware; a
3-week gap closes in under 2 hours).

## Trust note

A copied state is trusted input: the destination does not re-verify it. Only
seed from caches we produced ourselves, transferred over authenticated
channels (ssh, S3 with credentials, wormhole codes exchanged out of band).
