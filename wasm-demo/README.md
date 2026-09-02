# `zero-wasm-demo`

A browser wallet demo over the wasm build of librustzcash. Key derivation and a real
SQLite wallet database, running in a page with no server and no network.

This is a sample project, not a wallet. It exists to show what the wasm build can actually
do today and to be a starting point for something real. It is not published anywhere.

## Running

```sh
cd zero/wasm-demo
RUSTFLAGS='--cfg getrandom_backend="wasm_js"' wasm-pack build --target web --out-dir www/pkg --release
cd www && python3 -m http.server 8734
```

Then open <http://127.0.0.1:8734/index.html>. The `RUSTFLAGS` is not optional: `getrandom`
0.3 has no default backend for OS-less wasm.

## What it does

Measured in Chrome on an M-series Mac:

| step | time |
|---|---|
| Derive a Unified Address from a seed | 44 ms |
| Create the wallet database, applying all 71 migrations | 130 ms |
| Create an account and derive its address | 68 ms |
| Read wallet state back | 0.1 ms |

The released module is **4.1 MB** of wasm (`opt-level = "s"`, LTO, `wasm-opt`), which
includes SQLite and the whole wallet backend.

## What it does not do

**No sync.** `zcash_client_backend::sync::run` compiles for wasm and its transport bounds
are satisfied by `tonic-web-wasm-client` — see `librustzcash/zero-wasm-smoke` — but it
needs a lightwalletd or Zaino speaking gRPC-Web to talk to, which is an infrastructure
decision rather than a code one. Until that exists there is nothing to sync against, so
balances are zero and the chain height is whatever the account birthday set.

**No persistence.** The wallet lives for the lifetime of the page. `rusqlite` 0.39 links
`sqlite-wasm-rs` on this target, and as published that crate ships only the in-memory VFS;
the OPFS and IndexedDB backends its README describes were split out after 0.4 and are not
reachable from the version `rusqlite` depends on. Closing that gap is the prerequisite for
a wallet that survives a reload.

**No spending.** Sapling parameters load in wasm in 382 ms and Orchard proofs work, but an
Orchard proving key costs about 9 s to build and a two-action bundle about 12 s to prove,
single-threaded. See `librustzcash/WASM.md`; those numbers are why threads matter.

## Notes for anyone building on this

- `WalletDb::from_connection` requires the caller to have called
  `rusqlite::vtab::array::load_module`. Only `WalletDb::for_path` does it for you, and it
  needs a filesystem, so a browser has to use `from_connection`. Skipping it compiles and
  then fails at runtime inside any query that binds a list parameter.
- `std::time::SystemTime::now` panics on `wasm32-unknown-unknown`, so
  `zcash_client_sqlite::util::SystemClock` cannot be used. `JsClock` here is the minimum
  replacement.
- The connection is held outside `WalletDb` and borrowed per call. `WalletDb` does not
  expose its connection, so anything wanting raw SQL alongside the wallet API needs this
  shape — which is also what `from_connection` documents for pooling.
- `console_error_panic_hook` is worth the dependency: without it a Rust panic surfaces as
  an unexplained `unreachable executed` in the JS console.
