<!-- [zero] @claude This file is a Zero-local addition; it is not upstream. -->

# WebAssembly support

Status of the `librustzcash` crates on WebAssembly, and the recipe for building
them. Everything in the "builds today" table below was verified on this branch
against `wasm32-unknown-unknown` with the workspace's pinned toolchain
(`rust-toolchain.toml`, currently 1.88) and **no source changes** other than the
`zcash_proofs` feature split described under [Gotchas](#gotchas).

Reproduce with:

```sh
scripts/wasm-check.sh                  # wasm32-unknown-unknown (browser)
scripts/wasm-check.sh wasm32-wasip1    # WASI
```

The script builds each crate/feature combination through a synthetic consumer
crate rather than building the workspace, because `cargo build --workspace`
pulls in dev-dependencies and `zcash_client_sqlite`, neither of which
cross-compile. Upstream CI does the same thing for `wasm32-wasip1`
(`.github/workflows/ci.yml`, job `build-nodefault`).

## Builds today

| Crate / features | `wasm32-unknown-unknown` |
|---|---|
| `zcash_protocol`, `zcash_address`, `zcash_encoding`, `equihash`, `f4jumble` | yes — already `#![no_std]` |
| `zcash_primitives`, `zcash_keys`, `zcash_transparent`, `pczt` | yes — already `#![no_std]` |
| `zcash_client_backend` + `transparent-inputs`, `pczt` | yes |
| `zcash_client_backend` + `lightwalletd-tonic`, `sync` | yes (but see [Transport](#transport)) |
| `zcash_proofs` + `prover` | yes |
| `zcash_client_sqlite` | **no** — see [Storage](#storage) |
| `zcash_client_backend` + `tor` | **no** — arti needs real sockets |

Two things that look like they should be blockers and are not:

- `secp256k1-sys` 0.10's bundled C compiles for `wasm32-unknown-unknown` with
  plain clang and no sysroot.
- `zcash_script` 0.4.x is pure Rust, `#![no_std]`, `build = false`. The old C++
  FFI is gone, so `transparent-inputs` no longer drags a C++ toolchain in.

A release `cdylib` exporting a single key-derivation entry point over
`zcash_client_backend` + `zcash_keys` + `zcash_proofs`, built with
`opt-level = "z"`, LTO and `strip`, came out at **1.8 MB**.

## Consumer-side flags

None of these are librustzcash bugs; they are the standard wasm tax, and the
consuming crate has to set them.

```toml
# Cargo.toml of the wasm crate
getrandom     = { version = "0.2", features = ["js"] }
getrandom_03  = { package = "getrandom", version = "0.3", features = ["wasm_js"] }
uuid          = { version = "1", features = ["js"] }   # only if you use uuid
```

```sh
RUSTFLAGS='--cfg getrandom_backend="wasm_js"'
```

Both major versions of `getrandom` are in the lockfile and both need handling;
0.3 needs the `cfg` as well as the feature.

**`multicore` must stay off.** `halo2_proofs` 0.3.5 contains a hard

```
compile_error!("The multicore feature flag is not supported on wasm32 architectures without atomics")
```

and `zcash_client_sqlite`'s *default* feature set enables `multicore`. See
[Threads](#threads) for what it takes to turn it back on.

## Gotchas

### `zcash_proofs::prover`

Upstream gates `pub mod prover` on `local-prover` or `bundled-prover` — the two
features that supply Sapling parameters. A wasm host has neither: it fetches the
parameters over the network and wants `LocalTxProver::from_bytes`. Enabling
`local-prover` to reach it pulls `directories` → `home`, which does not build for
`wasm32-unknown-unknown`:

```
error[E0425]: cannot find function `home_dir_inner` in the crate root
```

This branch adds a `prover` feature that exposes the module without any
parameter-sourcing dependency; `local-prover` and `bundled-prover` both imply it,
so nothing else changes. Enable `zcash_proofs = { default-features = false,
features = ["prover"] }` and supply the ~47 MiB of Sapling parameters yourself.

Worth upstreaming. Note that `librustzcash/CLAUDE.md` requires a discussed and
team-acknowledged GitHub issue before a PR.

### Wall-clock time

`SystemTime::now()` panics on `wasm32-unknown-unknown`. The core crates are
clean: the only call site in the entire workspace is
`zcash_client_sqlite/src/util.rs:23`. `zcash_client_backend` takes `SystemTime`
as a parameter throughout, so once the SQLite backend is replaced the problem is
gone.

## Storage

`zcash_client_sqlite` is the real blocker. `libsqlite3-sys` 0.35's bundled build
dies at

```
sqlite3/sqlite3.c:15049:10: fatal error: 'stdio.h' file not found
```

because `wasm32-unknown-unknown` has no libc at all. On `wasm32-wasip1` the same
build script does emit the correct flags (`-D_WASI_EMULATED_MMAN`,
`-D_WASI_EMULATED_GETPID`, `-D_WASI_EMULATED_SIGNAL`,
`-D_WASI_EMULATED_PROCESS_CLOCKS`, `-DSQLITE_THREADSAFE=0`), so there it is only
a missing wasi-sdk sysroot — a toolchain install, not a code change.

For the browser, the options are:

1. **A new backend implementing `zcash_client_backend`'s `WalletRead` /
   `WalletWrite` traits over IndexedDB.** The most work, and the most control.
2. **Keep the SQL schema and swap the driver** — SQLite compiled to wasm
   (official `sqlite3.wasm`, or wa-sqlite) behind a `rusqlite`-shaped shim, with
   OPFS or IndexedDB for persistence. Reuses every migration in
   `zcash_client_sqlite/src/wallet/init/migrations`, which is a lot of tested
   logic to not rewrite.

Do **not** plan around `zcash_client_memory`. It was merged into the workspace in
August 2025, then extracted to `zcash/zcash_client_memory` and removed from
librustzcash in June 2026 (upstream PR #2436), and upstream documents it as
unsupported and not usable as a wallet storage backend.

## Transport

`lightwalletd-tonic` compiles for wasm only because tonic's `transport` feature
stays off — there is no HTTP/2 socket in a browser. You need either
`tonic-web-wasm-client` against a lightwalletd (or Zaino) fronted by a grpc-web
proxy, or your own `fetch`-based transport implementing tonic's service trait.

## Threads

`rayon` does not work on stock `wasm32-unknown-unknown`; `std::thread::spawn`
panics. Parallelism is possible — ChainSafe's WebZjs ships it — but the cost is
specific and should be understood before it is committed to:

- **Nightly, pinned.** `wasm-bindgen-rayon` requires `-Z build-std` to rebuild
  `std` with `-C target-feature=+atomics,+bulk-memory,+mutable-globals`, which is
  nightly-only, and its own docs recommend pinning an exact nightly. That
  conflicts with this workspace's stable `rust-toolchain.toml` pin, so the
  threaded build has to be a separate toolchain in a separate crate.
- **You ride nightly regressions.** In August 2025 a `std` refactor broke the
  atomics build outright (`cannot find function current_os_id in module imp`,
  rust-lang/rust#145101) until rust-lang/rust#145096 landed. That is the failure
  mode to expect: not subtle miscompiles, but a hard build break on a nightly
  bump, fixed upstream within days.
- **Cross-origin isolation.** `SharedArrayBuffer` requires COOP/COEP headers on
  every response, which constrains hosting and breaks third-party embeds. This is
  an infrastructure decision, not a build flag.
- **`--target web` only.** `wasm-bindgen-rayon` does not support bundler targets.
- **The main thread cannot block** on the rayon pool, so the wasm module has to be
  instantiated inside a dedicated Worker.
- **Maintenance.** `wasm-bindgen-rayon` 1.3.0 (December 2024) is the newest
  release, but ~139k recent downloads say it is in wide use rather than abandoned.

What it buys, from ChainSafe's `zcash-wasm-benchmark` (Firefox, M2 MacBook Air,
4-thread pool): trial decryption around 5,000 actions/outputs per second, ~14–20 s
for a 90-day block range and ~55 minutes for full history from Orchard
activation. Halo2 proving stays expensive — 5.4 s for one spend, 12.2 s for five,
23.7 s for ten, 122 s for twenty.

Also note the 32-bit address space: 4 GiB is the hard ceiling and Orchard proving
is the memory-hungry step. Benchmark proving early.
