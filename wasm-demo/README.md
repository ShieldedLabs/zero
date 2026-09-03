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

**Status: a sync does not complete, and the cause is now located.** The transport is
fine — verified in Chrome and under Node, both unary (`GetLatestBlock`, 320 ms) and
server-streaming (10 compact blocks, under a second). A full sync gets through every
request — `GetSubtreeRoots` for all three pools, `GetLatestBlock`,
`GetAddressUtxosStream`, `GetBlockRange`, `GetTreeState` — reaches
`Scanning ChainTip(..)`, and then runs at **99% CPU indefinitely**. A Chrome renderer
burned **68 minutes of CPU** on a five-block range without finishing; Node behaves the
same. It is not proportional to the range: three blocks and a hundred behave alike.

Sampling the stack (`sample <pid>`) puts essentially all of that time in
`v8::internal::wasm::memory_copy_wrapper` — wasm `memory.copy`. So the cost is bulk
memory movement inside SQLite, not trial decryption or proving, and it is a fixed cost
paid before any block is scanned.

That fixed cost is the subtree roots. The log reports 1128 Sapling subtrees and 769
Orchard ones, and `sync::run` ingests all of them into the shard trees before scanning
anything. Each insert reads and rewrites a shard BLOB through SQLite, in a VFS whose
backing store is wasm linear memory. Roughly nineteen hundred of those, over BLOBs that
hold up to 2^16 leaves each, is the wall. It is consistent with ChainSafe's finding that
tree and witness work — not decryption — dominated their browser sync.

What that means for a browser wallet: the first sync cannot be a single blocking call.
It needs to ingest subtree roots incrementally across many turns of the event loop, with
progress reported and the ability to resume, and it probably wants threads (see
`librustzcash/WASM.md`) — or a wallet birthday recent enough that the shard trees start
nearly empty. None of that is a librustzcash bug; it is a consequence of running the
existing sync design against wasm memory.

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
