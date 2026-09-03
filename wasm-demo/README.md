# `zero-wasm-demo`

A Zcash wallet that runs in a browser tab: librustzcash compiled to
`wasm32-unknown-unknown`, with a real SQLite wallet database, syncing from a lightwalletd
over gRPC-Web, and persisting across reloads.

This is a sample project, not a product — no spending, one seed, no key management worth
the name. It exists to show that the wasm build is a viable base for a real wallet, and to
be a starting point for one. It is not published anywhere.

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
| Export, wipe and restore the wallet (1.1 MiB) | 290 ms |
| Reach lightwalletd over gRPC-Web (`GetLatestBlock`) | 320 ms |
| Stream 10 compact blocks over gRPC-Web | < 1 s |

The released module is **4.1 MB** of wasm (`opt-level = "s"`, LTO, `wasm-opt`), which
includes SQLite and the whole wallet backend.

## How syncing works — and where it currently stops

`zcash_client_backend::sync::run` is the whole state machine; this crate supplies the three
things it needs on wasm.

- **Transport.** `tonic`'s own transport wants an HTTP/2 socket, which a browser does not
  have, so `lightwalletd-tonic` is enabled *without* `lightwalletd-tonic-transport` and
  [`tonic-web-wasm-client`] provides a `GrpcService` over `fetch`. It speaks gRPC-Web, so
  the server must too. The default endpoint is ChainSafe's public proxy,
  `https://zcash-mainnet.chainsafe.dev` — the same one WebZjs uses. A plain lightwalletd
  will not answer a browser; it needs a gRPC-Web proxy or a server-side `tonic-web` layer.
- **A block cache.** `zcash_client_backend` ships no `BlockCache` implementation, so
  `src/block_cache.rs` is one, keyed by height in a `BTreeMap`. That is not tidiness:
  `scan_cached_blocks` calls `with_blocks` repeatedly while working through a range, and a
  `Vec` implementation has to clone and re-sort the whole cache on every call to guarantee
  ascending order. Over data that includes full transaction bodies, that alone took
  minutes.
- **State across `await`s.** The wallet lives in a `thread_local` rather than a
  `#[wasm_bindgen]` struct, because a `RefCell` borrow cannot be held across an await.
  `syncNow` takes the whole wallet out for its duration and puts it back; a second
  concurrent call finds it missing and is refused.

**Status: a sync does not currently complete, and the cause is not yet established.**
Under Node it gets through the full request sequence — `GetSubtreeRoots` (Sapling 1128,
Orchard 769, Ironwood 2), `GetLatestBlock`, `GetAddressUtxosStream`, `GetBlockRange`,
`GetTreeState`, every one answering with a 200 in about 220 ms — and then stalls at
roughly 6% CPU, issuing no further requests, for over ten minutes. It behaves the same for
a five-block range as for a hundred, so it is a fixed cost or a hang rather than slow
scanning.

Two things point away from librustzcash and towards the host: a unary call
(`fetchTipHeight`) and a server-streaming call (`probeBlockStream`) each complete in well
under a second in isolation, and **the stall point moves between runs** — sometimes after
`GetTreeState`, sometimes after an earlier `GetLatestBlock`. That pattern fits a connection
pool wedged by response bodies that are never fully drained, which Node's `fetch` and a
browser's handle differently. It has not been reproduced in a browser, which is the next
thing to do.

[`tonic-web-wasm-client`]: https://crates.io/crates/tonic-web-wasm-client

## How persistence works

Not through SQLite. As published, `sqlite-wasm-rs` 0.5 ships only the in-memory VFS — the
OPFS and IndexedDB backends its README describes were split out after 0.4 and are not in
the version `rusqlite` depends on. So instead of SQLite writing through to storage, the
page takes the finished database file and stores it: `exportDb()` hands back the byte image
via `MemVfsUtil::export_db`, the page puts it in IndexedDB, and `createWallet(network,
bytes)` imports it again on load.

That has one consequence worth internalising: **a write is durable only once the page has
stored the export.** The demo exports after every mutation. A real wallet would want to
debounce that, and would outgrow the approach once the database is large enough that
copying it per write hurts — at which point it needs a genuine persistent VFS.

## What it does not do

**No spending.** Sapling parameters load in wasm in 382 ms and Orchard proofs work, but an
Orchard proving key costs about 9 s to build and a two-action bundle about 12 s to prove,
single-threaded. See `librustzcash/WASM.md`; those numbers are why threads matter.

**No key management.** The seed is a text box. Do not put real funds behind it.

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
