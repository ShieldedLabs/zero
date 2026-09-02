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
network dependency rather than a test.

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

The `RUSTFLAGS` is not optional: `getrandom` 0.3 has no default backend for OS-less wasm,
and without it the build fails in a dependency rather than here.

Node is enough for the in-memory VFS, which is what these tests use. The `sahpool`/OPFS
VFS a real wallet wants is browser-only and needs a dedicated Worker, so it cannot be
covered here; `wasm-pack test --headless --firefox` would be the place to add that once a
browser driver is available in CI.

`wasm-bindgen-test` is pinned at `0.3.54` or newer on purpose. With an older one the
tests compile, the `__wbgt_*` symbols are present in the `.wasm`, and the runner reports
`no tests to run!` and exits 0 — a silent pass covering nothing. If you ever see that
line, suspect the pairing between `wasm-bindgen` and `wasm-bindgen-test` before you
suspect your test.
