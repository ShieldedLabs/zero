# Zcash-in-a-TEE: proof of concept report

**Audience:** Shielded Labs leadership
**Author:** Mark Henderson
**Date:** 2026-07-26
**Status:** Proof of concept complete. Not production-ready. Two platform-side blockers remain.

## TL;DR

We built and proved a proof of concept for running Zcash infrastructure inside a
Trusted Execution Environment (TEE): a single AWS Nitro enclave running **both**
a full `zebrad` validator and a `zainod` lightwallet indexer, from a
**bit-for-bit reproducible, remotely attestable** build, on Caution's verifiable
compute platform.

Every layer works and is demonstrated: the reproducible build, the attestation,
the enclave boot, the co-located supervisor, and zaino serving real mainnet
compact blocks to a wallet client. The one thing we have not yet run is the full
stack **serving inside the enclave against mainnet**, because that requires a
fully-synced ~280 GB chain state, and the enclave has no persistent disk yet.
That is a platform limitation on Caution's side, not a flaw in our design, and
both of the pieces needed to close it are on their roadmap.

We recommend treating this as a successful PoC, publishing a joint technical
post with Caution, and scoping the production node as a fast-follow gated on two
specific Caution platform features.

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

## What we achieved (the proof of concept)

All of the following are demonstrated with concrete evidence (see Appendix).

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

4. **Serving real mainnet data.** The exact `zainod` binary from the combined
   image, in ephemeral (diskless) mode, serves live mainnet compact blocks to a
   wallet client: `GetLightdInfo`, `GetLatestBlock`, and `GetBlockRange` all
   return correct data, at a ~66 MiB memory footprint.

Put together: the design is sound and every component is proven. What remains is
an integration step blocked by a platform limitation, not a research risk.

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

**A live mainnet node serving inside the enclave.** This needs a fully-synced
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

- **Now:** PoC complete and reproducible. Fixes committed.
- **Testnet demo (optional, ~half a day):** a testnet chain syncs in ~4 hours,
  small enough to sync inside the enclave and prove the whole stack serving
  live and attested. This is the cheapest path to a *running* public demo, at
  the cost of the enclave re-syncing on each cold boot.
- **Mainnet node (fast-follow):** gated on Caution's disk support (~2 weeks) and
  a large-memory/BYOC enclave. Once both exist, the same reproducible image
  becomes the first fully verifiable Zcash node. State can be seeded from a
  synced cache to skip the initial sync (documented and verified).

## Recommendation

1. **Call the PoC a success.** The hard, novel parts (reproducible build,
   attestation, co-located enclave, diskless indexer serving) are done and
   proven.
2. **Publish jointly with Caution.** A short technical post is drafted. It is
   good for Shielded Labs (leadership in privacy-preserving infra) and for
   Caution (a real, demanding workload on their platform).
3. **Scope the production node as a fast-follow**, explicitly gated on Caution's
   disk support and a large-memory enclave. Keep the relationship warm; Anton is
   an eager partner.
4. **Consider the testnet demo** if we want something live and attested to point
   at before the mainnet blockers clear.

## Appendix: evidence

- **Reproducible build:** two cold CI builds, identical SHA-256:
  `zebrad 2f982c0d…eeadd`, `zainod 358432a1…b5c4b`.
- **Caution deploy:** `Docker image built, building EIF... Complete!` ->
  `Build complete` -> `Deployment successful!`, public IP and attestation URL
  issued, app state `running`.
- **Co-located supervisor:** `supervisor: starting zebrad` -> zebra
  `Opened RPC endpoint at 127.0.0.1:8232` -> `supervisor: starting zainod` ->
  `zainod started, version: 0.6.0-rc.1`, both processes running.
- **Serving:** `GetLightdInfo` -> `chainName: main, blockHeight: 3425949`;
  `GetLatestBlock` -> `3425949`; `GetBlockRange` -> compact blocks returned;
  ~66 MiB RSS.
- **Repo:** `deploy/caution-zaino/` (single-indexer and combined enclave build
  contexts, configs, supervisor, seeding runbook, CI). Reproducibility and
  build checks are enforced in CI.
