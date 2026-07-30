# Co-located enclave: zebrad + zainod in one EIF

The chosen architecture (2026-07-23): run a fully-synced zebra and zaino
together in a single Caution enclave. Anton's team owns the zebra packaging;
this directory is the zaino-side integration glue and the contract between the
two halves.

## Why this is the best option, not just the easiest

- **Single attestation.** One EIF measurement covers both processes, so the
  whole path from consensus-validated chain data to the wallet-facing gRPC is
  verifiable. Attesting zaino alone would only prove honest relay of possibly
  dishonest data; co-locating an attested zebra closes that gap.
- **The plaintext-RPC problem disappears.** zaino reaches zebra over
  `127.0.0.1:8232`, a hop that never leaves enclave memory. None of the
  external-zebra concerns (public-IP rejection, cleartext credentials, private
  networking) apply.
- **No new transport.** No STEVE, no enclave-to-enclave comms, no VPC peering.
- **State is not duplicated.** zaino runs ephemeral, so zebra holds the only
  copy of the ~276 GB state; zaino stays at ~77 MB and proxies to localhost.

## Cost, and why it is temporary

Sizing is driven by the RAM-only zebra: ~276 GB state on tmpfs + a few GB
process. That needs an r6i.12xlarge class parent (384 GiB, ~$2,200/mo). This
is a bridge cost: once Caution ships disk support (~2 weeks out), zebra's
state moves to a persistent volume and the same design runs on a far smaller,
cheaper instance. The beefy instance is only needed until then. Retroactive
grant funding (ZCG) is a candidate to cover the bridge.

## The single combined image (built and CI-verified here)

`Containerfile` builds both binaries static-musl in one multi-stage build and
ships a busybox runtime that runs both via `run-both.sh`:

- zebra stage: adapted from Anton Livaja's StageX recipe
  (ZcashFoundation/zebra#10491), which already solves rocksdb + libzcash_script
  under musl. Fed our vendored zebra 6.2.0.
- zaino stage: our already-CI-green `../overlay/Containerfile` recipe.

Build context = a repo with THREE subfolders: `zebra/`, `zaino/`, and
`orchard/`. orchard is required because zebra carries a `[zero]` patch
`orchard = { path = "../orchard" }`; it is copied to `/home/orchard` in the
build so that path resolves. `assemble-combined.sh` produces this context from
the zero repo; a standalone Caution repo needs the same three subfolders.

Build on x86 (`caution-z3.yml`, Blacksmith runner). Do not build under arm64
emulation: musl cross-build of two rust workspaces with rocksdb is far too slow.

### Reproducibility: who sets which flags

Two layers, split by ownership:

- In-recipe determinism (in the Containerfile, ours to set): pinned StageX
  base digests, `SOURCE_DATE_EPOCH=1`, `-C link-arg=-Wl,--build-id=none`,
  `-C codegen-units=1`, no incremental. These make the compiled binaries
  bit-for-bit reproducible.
- Image-packaging determinism (docker build flags, auto-added by the caution
  backend): `--output type=oci,rewrite-timestamp=true,force-compression=true`
  plus `SOURCE_DATE_EPOCH=1`, matching zcash/zallet `utils/build.sh` line 21.
  These normalise layer timestamps and compression so the OCI/EIF digest is
  reproducible. You do not add these yourself: `caution` supplies them, builds
  everything, then transplants the result into EnclaveOS (a minimal
  StageX-built Linux).

`build.sh` runs the same deterministic docker build locally (assemble context,
then the OCI build with those flags) for local reproduction / `caution verify`.
`caution-z3-reproduce.yml` proves the binary layer reproduces (two cold builds,
identical sha256); full EIF-digest reproduction is what `caution verify` checks
independently on Caution's side.

## Files

- `zainod-colocated.toml`: zaino profile, validator at localhost, ephemeral.
- `run-both.sh`: reference PID-1 supervisor that starts zebrad, waits for its
  RPC, then starts zainod, tearing down together. Caution runs one unit per
  enclave, so a supervisor is required.
- `zebrad-contract.toml`: the minimal zebra config zaino assumes (owned by the
  zebra packaging, documented here so the seam is explicit).
- `caution.hcl`: one-enclave sketch (320 GiB ask, gRPC ingress 8137, zebra
  P2P + DNS egress, single unit running the supervisor).

## Integration contract (what each side provides)

Zebra side (Anton's team):
- A zebrad in the combined image, config per `zebrad-contract.toml`: Mainnet,
  RPC on `127.0.0.1:8232`, cookie auth off, state cache_dir on a tmpfs.
- The ~276 GB state seeded into that tmpfs at enclave setup (see `../SEEDING.md`
  for the copy rules; format 28 for a v21-era binary). Because the enclave has
  no persistence yet, this seed re-runs on every cold start until disk support
  lands, so keep the enclave warm for the demo.

Zaino side (this repo):
- A static-musl `zainod` (the proven `../overlay/Containerfile` recipe) plus
  `zainod-colocated.toml` at `/etc/zaino/`.

Combined image (assembled Caution-side): both binaries + `run-both.sh` as the
`unit.default` command, in a busybox-class runtime (sh + wget for the health
gate). The reproducible build must cover both binaries for the attestation to
mean anything.

## Open questions for Anton

1. Who assembles the combined EIF: your builder from our zainod layer, or do
   we hand you a static zainod binary + the two config files + the supervisor?
2. Enclave egress for zebra P2P (TCP 8233) and DNS seeders (53): supported
   through the parent proxy? Any peer-set constraints?
3. Exact usable `memory_mb` inside the enclave on the r6i.12xlarge (parent
   overhead), so we size against real headroom over the 276 GB state.
4. How the 276 GB seed is pushed into the enclave tmpfs at setup (vsock copy
   from parent EBS?), and whether the enclave can be kept warm across the demo
   so the seed is paid once.
