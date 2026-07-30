# Zcash-in-a-TEE: proof of concept report

**Audience:** Shielded Labs leadership
**Author:** Mark Henderson
**Date:** 2026-07-26
**Status:** Live attested testnet node running and serving. Mainnet is a fast-follow gated on two Caution platform features.

## TL;DR

We built, deployed, and are now running a Zcash node inside a Trusted Execution
Environment (TEE): a single AWS Nitro enclave running **both** a full `zebrad`
validator and a `zainod` lightwallet indexer, from a **bit-for-bit reproducible,
remotely attested** build, on Caution's verifiable compute platform.

This is no longer just a proof of concept. A **testnet node is live and serving
right now**: the enclave is attested (a valid signed attestation document, not a
mock), and it answers real wallet queries (`GetLightdInfo`, `GetLatestBlock`,
and compact-block `GetBlockRange`) over the public internet while `zebrad` syncs
the testnet chain inside the enclave. To our knowledge this is the first
attested, reproducibly-built Zcash node of any kind.

The one thing we have not yet run is the same stack **against mainnet**, because
mainnet's ~280 GB chain state needs either persistent enclave disk or a
large-memory (~320 GB) enclave, both on Caution's near-term roadmap. Testnet's
state is ~15-20 GB, which fits a 32 GB enclave in RAM today, which is why the
testnet node runs now and the mainnet node is a fast-follow.

We recommend treating this as a success (a running, verifiable node, not a
slide), publishing a joint technical post with Caution, and scoping the mainnet
node as a fast-follow gated on those two specific Caution platform features.

## What we set out to do

Today, anyone running a Zcash lightwallet server (lightwalletd or zaino) can see
the addresses and transaction IDs that wallets query. The operator is a trusted
party. The goal of this work was to remove that trust: run the indexer inside a
TEE so that the code is cryptographically attested and the operator cannot see
inside the box, and to do it with a **reproducible** build so that anyone can
independently verify that the enclave is running exactly the reviewed source.

Partway through, the design sharpened into something stronger: put the validator
(`zebrad`) in the enclave too. That gives a single attestation covering the
whole path from consensus-validated chain data to the wallet-facing endpoint,
and it makes the zaino-to-zebra link a loopback hop that never leaves enclave
memory. To our knowledge this would be the first fully verifiable Zcash node of
any kind.

## What we achieved

All of the following are demonstrated with concrete evidence (see Appendix), and
items 1-4 are running live on the deployed testnet node right now.

1. **Reproducible build.** A single Containerfile builds both `zebrad` and
   `zainod` as fully static musl binaries using StageX (source-bootstrapped,
   digest-pinned base images). Two independent from-scratch CI builds produced
   byte-identical binaries. This is the foundation that makes attestation
   meaningful: anyone can rebuild from source and confirm the hashes.

2. **Attestation and boot on Caution.** We deployed the combined image to
   Caution's platform. Their builder compiled it, assembled it into an Enclave
   Image File, produced attestation measurements (PCRs), booted it on an AWS
   Nitro enclave, and reported it healthy with a public endpoint and a live
   attestation URL.

3. **Co-located supervisor.** A small init process starts `zebrad`, waits for
   its RPC, then starts `zainod` pointed at it over loopback, and ties their
   lifecycles together. Demonstrated end to end: zebra starts, opens its RPC,
   zaino launches, both run as one unit.

4. **Serving live from the enclave.** The deployed testnet enclave answers real
   wallet queries over the public internet: `GetLightdInfo`, `GetLatestBlock`,
   and compact-block `GetBlockRange` all return correct data while `zebrad`
   syncs the testnet chain inside the enclave, at a ~66 MiB indexer footprint.
   (We separately verified the same against mainnet data in a local rehearsal.)

Put together: the design is sound, every component is proven, and the full
stack is running and serving on testnet. What remains is scaling the same image
to mainnet, which is blocked by a platform limitation (state size vs enclave
disk/RAM), not a research risk.

## What we learned

- **The design is correct.** Co-locating validator and indexer in one enclave is
  not just convenient, it is the strongest option: one attestation, and the
  plaintext RPC hop never leaves the enclave.
- **zaino is diskless-friendly.** Its ephemeral mode holds only the recent
  ~1,000-block window plus the mempool in memory (tens of MB), so the indexer
  itself is a trivial fit for a RAM-only enclave.
- **The real cost is the validator's state, not the indexer.** A fully-synced
  mainnet `zebra` needs ~280 GB of state resident. With no persistent disk, that
  has to live in RAM (needing a ~320 GB enclave) or be re-established on every
  cold boot.
- **The first real deploy surfaced concrete, fixable bugs, not fundamental
  problems.** We found and fixed five issues across our image and Caution's
  platform (see below). None of them were architectural.

### Issues found and fixed

Ours:
- Parallel compilation of two Rust workspaces exhausted the builder; serialized
  the stages.
- The supervisor invoked `zebrad start --config` (wrong flag order); zebra never
  started. Corrected to `zebrad -c <file> start`.
- An empty CA-certificate directory could break the indexer's HTTP client at
  startup; now ship a pinned CA bundle.
- The enclave `unit.command` path did not match where the binary was installed;
  fixed, and CI now asserts the two agree.

Caution's platform (reported to them):
- Their EIF assembler fails on any image built from their own recommended
  busybox base, because that base has a dangling `/lib` symlink. We reported it
  with a root-cause and a suggested fix.
- Slow source upload and a couple of CLI UX rough edges (silent SSH-key
  registration, no `--qr` hint for passkey users), all reported.

Finding these is exactly the value of doing a real deploy, and it is what our
Caution counterpart asked us to surface.

## What is not done, and why

The testnet node is live and serving (see above). The remaining gap is:

**A live MAINNET node serving inside the enclave.** This needs a fully-synced
~280 GB `zebra` state available to the enclave. Two Caution platform features
gate it, both on their roadmap:

1. **Persistent disk for enclaves** (they estimate ~2 weeks out). Until then,
   state must live in RAM and is lost on every restart.
2. **Large-memory / bring-your-own-compute enclaves.** Caution's fully-managed
   tier currently caps at 16 vCPU / 16 GB. A RAM-only mainnet node needs ~320 GB,
   which means either a raised cap or deploying into our own AWS account (BYOC).

Neither is our engineering to do; both are normal platform maturation.

## Cost

- **PoC:** negligible. A few dollars of Caution credits for the deploy attempts.
- **RAM-only mainnet node (bridge, until disk support lands):** an
  r6i.12xlarge-class instance (48 vCPU / 384 GB) is roughly **$2,200/month**.
  This is temporary. Once Caution ships enclave disk support, the same design
  runs on a much smaller, cheaper instance.
- Anton at Caution has offered retroactive-grant support and is motivated to
  make this work, as it stress-tests and improves his platform.

## Timeline and what shipping looks like

- **Now (done):** testnet node **live and serving, attested**, in a 32 GB
  fully-managed enclave. Reproducible build, fixes committed. `caution verify`
  can be run against it to confirm the running enclave matches this source.
  (The enclave re-syncs testnet on each cold boot, ~hours, since there is no
  persistent disk yet; fine for a demo kept warm.)
- **Mainnet node (fast-follow):** gated on Caution's disk support (~2 weeks) and
  a large-memory/BYOC enclave. Once both exist, the same reproducible image
  becomes the first fully verifiable Zcash mainnet node. State can be seeded
  from a synced cache to skip the initial sync (documented and verified).

## Recommendation

1. **Call it a success.** Not a slide deck: a running, attested, verifiable
   testnet node serving real wallet queries, from a reproducible build. The
   hard, novel parts are done and demonstrated on live infrastructure.
2. **Publish jointly with Caution.** A short technical post is drafted. It is
   good for Shielded Labs (leadership in privacy-preserving infra) and for
   Caution (a real, demanding workload on their platform).
3. **Scope the mainnet node as a fast-follow**, explicitly gated on Caution's
   disk support and a large-memory enclave. Keep the relationship warm; Anton is
   an eager partner.
4. **Run `caution verify` against the live testnet node** to capture the
   end-to-end "running enclave matches reviewed source" result as the capstone
   evidence for the post.

## Appendix: evidence

- **Reproducible build:** two cold CI builds, identical SHA-256:
  `zebrad 2f982c0d…eeadd`, `zainod 358432a1…b5c4b`.
- **Attested deploy (testnet):** `Build complete` -> `Waiting for health
  check... Complete! (1m35s)` -> `Deployment successful!`, app state `running`,
  32 GB fully-managed enclave.
- **Attestation live:** a nonce challenge to the enclave's `/attestation`
  endpoint returns a real ~6.8 KB signed attestation document (0 bytes in debug
  mode, where attestation is disabled).
- **Co-located supervisor:** `supervisor: starting zebrad` -> zebra
  `Opened RPC endpoint at 127.0.0.1:8232` -> `supervisor: starting zainod` ->
  `zainod started`, both processes running, no crash loop.
- **Serving (live testnet, over the internet):** `GetLightdInfo` ->
  `chainName: test`, height climbing as it syncs; `GetLatestBlock` -> current
  height; `GetBlockRange 100..104` -> 5 compact blocks; ~66 MiB indexer RSS.
- **Repo:** `deploy/caution-zaino/` (single-indexer and combined enclave build
  contexts, configs, supervisor, seeding runbook, CI). Reproducibility and
  build checks are enforced in CI.
