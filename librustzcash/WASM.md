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

`zero/wasm-demo` is a browser demo built on all of this: key derivation and a
real wallet database in a page, with no server. Start there for a feel of what
works today.

The scripts above only prove the crates *compile*. For proof that the wallet
database also *runs* on the target — migrations applied, queries executed, `rarray` working —
and for what proving costs, see `zero-wasm-smoke/`. For whether a threaded build
is reachable, see `scripts/wasm-atomics-check.sh`.

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
| `zcash_client_sqlite` + `orchard`, `transparent-inputs` | yes — via `sqlite-wasm-rs`, see [Storage](#storage) |
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

`zcash_client_sqlite` builds for `wasm32-unknown-unknown` on this branch. What
stood in the way was a version pin rather than anything architectural.

**rusqlite has supported `wasm32-unknown-unknown` since 0.38** (December 2025).
On `cfg(all(target_family = "wasm", target_os = "unknown"))` it swaps
`libsqlite3-sys` for [`sqlite-wasm-rs`], which ships SQLite already compiled to
wasm — so there is no C build, no libc and no sysroot involved. Verified:
rusqlite 0.38 and 0.40 with exactly the feature set `zcash_client_sqlite` asks
for (`time`, `array`, `uuid`, `hooks`) build clean for
`wasm32-unknown-unknown` on stable 1.93.

[`sqlite-wasm-rs`]: https://github.com/Spxg/sqlite-wasm-rs

This workspace pinned rusqlite 0.37 (July 2025), which predates that. Three
things stood between there and the bump; all three are resolved on this branch,
and the workspace is now on rusqlite 0.39:

1. ~~**`arti-client` 0.35 pins rusqlite 0.37.**~~ **Done on this branch.**
   `libsqlite3-sys` declares `links = "sqlite3"`, so two versions cannot coexist
   in one graph, and Cargo resolves optional dependencies whether or not their
   feature is enabled — turning `tor` off does not help, and target-gating the
   `arti-client` dependency does not either, since resolution covers every
   target. The fix was to move arti forward to 0.43, whose `tor-dirmgr` accepts
   `rusqlite >=0.36, <0.40`. With that in place `rusqlite 0.39` resolves
   cleanly. See the comment above `arti-client` in the workspace `Cargo.toml`
   for why 0.43 and not the latest.
2. ~~**`schemerz-rusqlite` has no release past 0.370.0.**~~ **Done on this
   branch.** Upstream publishes one release per `rusqlite` minor and has not gone
   past 0.370.0, so nothing published spans the range this workspace needs to move
   across. `zero-vendor/schemerz-rusqlite` is a local build of that release with
   the requirement widened to `>=0.37, <0.40`; its suite passes against rusqlite
   0.37, 0.38 and 0.39. Delete it once `zcash/schemerz` publishes a release for
   the `rusqlite` version this workspace settles on.
3. ~~**`zcash_client_sqlite` needs a mechanical migration.**~~ **Done on this
   branch.** rusqlite 0.38 removed the `ToSql`/`FromSql` impls for `u64` and
   `usize` — they were lossy in both directions — which cost 85 call sites across
   10 files. `src/sql.rs` is now the single place that conversion happens:
   `SqlU64` to bind, `RowExt::{get_u64, get_opt_u64, get_usize}` to read, both
   checked. See [Storage](#storage).

The `bundled` feature also had to stop applying to wasm: it is now declared in a
`[target.'cfg(not(all(target_family = "wasm", target_os = "unknown")))'.dependencies]`
block in `zcash_client_sqlite/Cargo.toml`, since there is nothing to bundle on
that target.

Then mind what `sqlite-wasm-rs` actually provides, because it is less than its
README suggests. It is **not thread-safe** (SQLite is compiled
`-DSQLITE_THREADSAFE=0`, and `JsValue` cannot cross threads) and **no VFS
supports multiple connections**, so all database access stays on one thread even
if trial decryption is parallelised.

More importantly: **as published, `sqlite-wasm-rs` 0.5.5 ships only the in-memory
VFS.** The `sahpool` (OPFS) and `relaxed-idb` (IndexedDB) backends its README
describes were split out after 0.4 — 0.4.x depended on `indexed_db_futures` and
`web-sys`, 0.5.5 depends only on `js-sys`, `rsqlite-vfs` and `wasm-bindgen`, and
`rsqlite-vfs` 0.1.1 contains `memvfs` and nothing else. `rusqlite` 0.39 pulls
`sqlite-wasm-rs` 0.5, so a wallet built through `rusqlite` today gets a database
that does not survive a page reload.

That makes persistence a real piece of work rather than a VFS registration call.
The options are to vendor or revive a persistent VFS against `rsqlite-vfs`'s
traits, to pin the older `sqlite-wasm-rs` line and give up `rusqlite`'s built-in
wasm support, or to wait for the backends to reappear upstream.

Do **not** plan around `zcash_client_memory`. It was merged into the workspace in
August 2025, then extracted to `zcash/zcash_client_memory` and removed from
librustzcash in June 2026 (upstream PR #2436), and upstream documents it as
unsupported and not usable as a wallet storage backend.

## Transport

`lightwalletd-tonic` compiles for wasm only because tonic's `transport` feature
stays off — there is no HTTP/2 socket in a browser. The client side is settled on
this branch: [`tonic-web-wasm-client`] 0.9 (which targets tonic 0.14, the version
this workspace pins) satisfies every bound `sync::run` imposes on its transport,
including the `Send + 'static` requirements on the response body that a wasm
transport usually cannot meet. `zero-wasm-smoke::sync_once` names `sync::run`
with concrete wasm types, so that stays true or the build breaks.

[`tonic-web-wasm-client`]: https://crates.io/crates/tonic-web-wasm-client

**The open question is server-side, and it is an infrastructure decision.**
`tonic-web-wasm-client` speaks gRPC-Web; plain gRPC over HTTP/2 will not answer
it. So the lightwalletd or Zaino the wallet talks to has to be fronted by a
gRPC-Web proxy (Envoy, or `grpcwebproxy`), or grow a server-side `tonic-web`
layer. Writing a bespoke `fetch`-based transport instead does not avoid this —
it is the same wire protocol either way, just reimplemented.

`sync::run` also needs a `BlockCache`, and `zcash_client_backend` ships no
implementation of that trait. `zero-wasm-smoke::MemoryBlockCache` is one, and
in-memory is the right shape for a browser: compact blocks are scanned and
discarded, so there is nothing worth persisting. Note the trait requires
`Send + Sync`, so a cache holding JS values directly will not satisfy it.

## Threads

`rayon` does not work on stock `wasm32-unknown-unknown`; `std::thread::spawn`
panics. Parallelism is possible — ChainSafe's WebZjs ships it — but the cost is
specific and should be understood before it is committed to:

- **Nightly, pinned.** `wasm-bindgen-rayon` requires `-Z build-std` to rebuild
  `std` with `-C target-feature=+atomics,+bulk-memory,+mutable-globals`, which is
  nightly-only, and its own docs recommend pinning an exact nightly. That
  conflicts with this workspace's stable `rust-toolchain.toml` pin, so the
  threaded build has to be a separate toolchain in a separate crate. It does
  build: see `scripts/wasm-atomics-check.sh`, which covers the whole stack
  including `sqlite-wasm-rs`, and passes on nightly 1.95.0.
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

## Remaining work

Roughly in dependency order. Nothing here is speculative — each item is either
verified above or a direct consequence of something verified above.

1. ~~**Unblock the rusqlite bump.**~~ Done: arti is at 0.43 and `rusqlite 0.39`
   resolves. Two follow-ups this leaves behind — `cargo vet` audits for the
   bumped crates, and confirming CI runs on the 1.90 toolchain this branch
   pins.
2. ~~**Release `schemerz-rusqlite` for rusqlite 0.38+.**~~ Done locally via
   `zero-vendor/schemerz-rusqlite`. Still worth asking `zcash/schemerz` for a real
   release, and reporting the trailing-comma bug that disables its test suite
   (see that directory's README).
3. ~~**Migrate `zcash_client_sqlite` off the `u64`/`usize` SQL conversions.**~~
   Done, via `zcash_client_sqlite/src/sql.rs`. `bundled` is now target-conditional.
   With this, **every crate in the workspace that a browser wallet needs builds
   for `wasm32-unknown-unknown`.** What is left is integration, not porting.
4. **Pick and wire a VFS.** The in-memory half is done and proven twice:
   `zero-wasm-smoke` runs the database under Node, and `zero/wasm-demo` runs it
   in Chrome — 71 migrations applied in 130 ms, accounts created and read back.
   The persistent half is harder than "register `sahpool`": the OPFS and
   IndexedDB VFSes are **not in the `sqlite-wasm-rs` version `rusqlite` depends
   on** (see [Storage](#storage)). Somebody has to supply one.
5. ~~**Transport.**~~ Done and exercised against a live server.
   `tonic-web-wasm-client` satisfies `sync::run`'s bounds, and `zero/wasm-demo`
   talks to ChainSafe's public gRPC-Web proxy from Chrome: unary calls in 320 ms,
   server-streaming in under a second. A server-side decision remains — the
   endpoint must speak gRPC-Web, so lightwalletd/Zaino needs a proxy or a
   `tonic-web` layer.

   **But a full `sync::run` does not complete on wasm.** It reaches the scanning
   phase and then runs at 99% CPU indefinitely — 68 minutes of Chrome renderer
   CPU on a five-block range, and the same for three blocks or a hundred, so the
   cost is fixed rather than per-block. Stack sampling puts nearly all of it in
   wasm `memory.copy`, i.e. bulk memory movement inside SQLite. The fixed cost is
   subtree-root ingestion: 1128 Sapling and 769 Orchard subtrees, each insert
   reading and rewriting a shard BLOB through a VFS backed by wasm linear memory.
   A browser wallet therefore cannot treat the first sync as one blocking call —
   it needs incremental ingestion across event-loop turns, resumability, and
   probably threads. See `zero/wasm-demo/README.md`.
6. ~~**Parameter delivery.**~~ Done, apart from choosing the cache. Real
   parameters load inside wasm: `zero-wasm-smoke` reads all 51 MiB from the host
   and builds a working `LocalTxProver`, in **382 ms** including SHA-256
   verification and Groth16 deserialisation. Parsing is therefore not the cost —
   the 47 MiB download is, so it wants the Cache API or IndexedDB behind it.
   Note that `LocalTxProver::from_bytes` **panics** on parameters with unexpected
   hashes, and a browser cache can serve truncated or stale bytes; use
   `zero_wasm_smoke::params::verify_sapling_parameters`, which returns an error
   there so the wallet can drop the cache entry and re-fetch rather than abort
   the wasm module.
7. ~~**Benchmark proving before designing around it.**~~ Done, and the numbers
   are the constraint on this whole effort. Measured in `zero-wasm-smoke`, on
   wasm under Node on an M-series Mac, single-threaded:

   | operation | time |
   |---|---|
   | Sapling parameters, verify and parse (51 MiB) | 0.4 s |
   | Orchard `VerifyingKey::build` | 7.2 s |
   | Orchard `ProvingKey::build` | 8.9 s |
   | Orchard 2-action bundle proof | 12.1 s |

   Sapling is a non-issue: a large download that parses in under half a second.
   **Orchard is the problem, and the proving key is the sharper half of it.** It
   has no trusted setup, so there is nothing to download and nothing Rust can
   cache between page loads — every session pays nine seconds of arithmetic
   before it can prove anything, and then twelve more for the smallest real
   bundle. A first Orchard spend is therefore about 21 seconds of blocked CPU.
   Consistent with ChainSafe's 5.4 s for one spend on four threads.

   Three ways out, and they are not exclusive: build the proving key eagerly in a
   Worker at startup so it is warm by the time the user spends; enable threads
   (item 8) — the only lever that moves both numbers; or move proving off-device,
   which for Orchard means handing out the witness and is a privacy decision, not
   an engineering one.
8. **Threads.** No longer optional, given item 7 — it is the only lever that
   moves both Orchard numbers. The build risk is now retired:
   `scripts/wasm-atomics-check.sh` rebuilds `std` with
   `+atomics,+bulk-memory,+mutable-globals` and compiles the **whole** stack that
   way — `orchard` with `multicore`, `rayon`, `wasm-bindgen-rayon`, and
   `zcash_client_sqlite` with its prebuilt `sqlite-wasm-rs` SQLite. It passes on
   nightly 1.95.0 (2026-01-25). That was the piece most likely to fail: a
   non-atomics object cannot link into an atomics module.

   What is left is runtime, and it needs a browser: `wasm-bindgen-rayon` supports
   only `--target web`, its pool is Web Workers, and `SharedArrayBuffer` needs
   cross-origin isolation. Measuring a speedup needs a browser driver in CI —
   the same gap that blocks the OPFS half of item 4. Note the SQLite layer stays
   single-threaded regardless; threads buy proving and trial decryption, not
   storage.
