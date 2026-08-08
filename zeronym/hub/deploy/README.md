# Reproducible build: zero-indexer-hub

A StageX build that turns `zeronym/hub` into one static-musl binary, designed so
that two independent builds of the same commit produce **byte-identical** output.

Sibling of `zeronym/shim/deploy/`, which does the same for the shim. Read that
README first for the full determinism argument; this one records only what
differs for the hub.

## Why reproducibility is the whole point here especially

The hub is the one component that sees a migration transaction in **plaintext**.
The shim removes the transaction from the operator's link and hands the bytes to
the hub, which re-parses them (telemetry only) and broadcasts them to the
network. So the hub is exactly the party the architecture would otherwise have to
trust blindly.

The Zeronym trust model refuses that. It hands the auditor one job: rebuild the
hub from source, get the same hash, and check that hash against the one bound
into the enclave attestation. Reproducibility + attestation together say: the
code you read is the code holding your migration in plaintext, and it does
nothing with it but broadcast it. Neither half is worth anything without the
other.

## Files

| File | What it does |
|---|---|
| `assemble.sh` | Builds the throwaway build context out of `git archive HEAD`. Never touches the working tree. |
| `Containerfile` | The build itself. StageX bases pinned by digest, static musl, export stage, busybox runtime. |
| `build.sh` | One deterministic build: extracts the binary and packages the OCI image, printing both hashes. |
| `reproduce.sh` | The proof: two cold builds, compared against each other **and** against `EXPECTED_SHA256`, then assert `zebra/` is still clean. |
| `EXPECTED_SHA256` | The published binary hash. Re-baseline it (and this list) whenever a determinism ingredient changes. |
| `caution/` | The attested Nitro-enclave deploy (see `caution/README.md`). |

## What differs from the shim recipe

- **No `zaino/` in the context.** The hub speaks JSON-RPC to full nodes, not the
  CompactTxStreamer gRPC, so it has no `zaino-proto` dependency. `assemble.sh`
  archives only `zeronym/hub` and the `zebra/` pieces `zebra-chain` needs.
- **Binary `zero-indexer-hub`**, entrypoint `/zero-indexer-hub`.
- Everything else (StageX bases, `SOURCE_DATE_EPOCH=1`, `codegen-units=1`,
  `crt-static`, `--build-id=none`, committed lockfile, no BuildKit cache mounts,
  no TLS stack, no rocksdb, no `libzcash_script`) is identical to the shim.

## Determinism ingredients (re-baseline the hash when any change)

1. The `docker/dockerfile` frontend digest pinned on line 1 of `Containerfile`.
2. The StageX base digests (`pallet-rust`, `core-busybox`).
3. `RUSTFLAGS`, `SOURCE_DATE_EPOCH`, `CARGO_HOME`, `WORKDIR`.
4. The committed `Cargo.lock`, and the `zebra-chain` sources archived into the
   context (a `zebra/` subtree pull can move the hash; the reproduce workflow
   fires on those paths for that reason).

## Reproduce it

```sh
sh zeronym/hub/deploy/reproduce.sh
```

Two cold `linux/amd64` builds (Rosetta on an arm64 Mac is fine), compared to each
other and to `EXPECTED_SHA256`. `.github/workflows/zeronym-hub-reproduce.yml`
runs the same script on a native x86_64 runner as the independent second machine.
