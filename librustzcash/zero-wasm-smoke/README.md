# `zero-wasm-smoke`

A wasm integration harness for `zcash_client_sqlite`, and nothing else. On every target
other than `wasm32-unknown-unknown` this crate is empty.

`scripts/wasm-check.sh` proves the crates *compile* for wasm. Compiling is not running:
the SQLite that `rusqlite` links against on that target is
[`sqlite-wasm-rs`](https://github.com/Spxg/sqlite-wasm-rs), which behaves differently
from the bundled C build in ways that only show up at runtime — a VFS has to be
registered before a database can be opened, there is exactly one connection, and nothing
is thread-safe. Separately, `std::time::SystemTime::now` panics on
`wasm32-unknown-unknown`, so `zcash_client_sqlite::util::SystemClock` cannot be used.

It also pins down the transport: `sync_once` names `zcash_client_backend::sync::run` with
`tonic_web_wasm_client::Client`, `MemoryBlockCache` and `WalletDb`, so if any of
`sync::run`'s bounds stop being satisfiable on wasm — the `Send + 'static` requirements on
the transport's response body are the fragile ones — this crate stops compiling. No sync is
run: that needs a gRPC-Web endpoint, and reaching for a public one would make this a live
network dependency rather than a test. The scanning that a sync drives is covered offline
instead: `tests/scan.rs` scans a cached block holding a note to the wallet.

Persistence is covered by `tests/persistence.rs`, which opens the wallet through
`WalletDb::for_path` over the `relaxed-idb` VFS from `sqlite-wasm-vfs` and checks that the
pages reach IndexedDB.

This crate exercises the path a browser wallet actually takes:

1. register a VFS,
2. open a connection and load the `rarray` module (`WalletDb::from_connection` requires
   the caller to do this; only `WalletDb::for_path` does it for you),
3. build a `WalletDb` with a `Clock` backed by JavaScript's `Date.now()`,
4. run `init_wallet_db`, applying every migration,
5. read back through the `WalletRead` API.

## Running

```sh
cd zero-wasm-smoke
RUSTFLAGS='--cfg getrandom_backend="wasm_js"' wasm-pack test --node
```

To include the Sapling parameter tests, point `ZCASH_PARAMS_DIR` at a directory holding
`sapling-spend.params` and `sapling-output.params` (they are 51 MiB and do not belong in
the repository, so they are fetched separately):

```sh
mkdir -p ~/.zcash-params && cd ~/.zcash-params
curl -O https://download.z.cash/downloads/sapling-spend.params
curl -O https://download.z.cash/downloads/sapling-output.params

ZCASH_PARAMS_DIR=$HOME/.zcash-params \
  RUSTFLAGS='--cfg getrandom_backend="wasm_js"' wasm-pack test --node
```

Without it `real_parameters_load` logs a skip; with it set but the files absent, it fails
rather than skipping quietly.

The `RUSTFLAGS` is not optional: `getrandom` 0.3 has no default backend for OS-less wasm,
and without it the build fails in a dependency rather than here.

Node is enough for the in-memory VFS, which is what most of these tests use.
`tests/persistence.rs` needs IndexedDB, so it runs in a browser and Node skips it:

```sh
RUSTFLAGS='--cfg getrandom_backend="wasm_js"' wasm-pack test --headless --chrome -- --test persistence
```

wasm-pack fetches a chromedriver matching the installed Chrome on first use.

`wasm-bindgen-test` is pinned at `0.3.54` or newer on purpose. With an older one the
tests compile, the `__wbgt_*` symbols are present in the `.wasm`, and the runner reports
`no tests to run!` and exits 0 — a silent pass covering nothing. If you ever see that
line, suspect the pairing between `wasm-bindgen` and `wasm-bindgen-test` before you
suspect your test.
