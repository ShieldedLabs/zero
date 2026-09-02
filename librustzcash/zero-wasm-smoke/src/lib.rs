//! [zero] @claude A runtime smoke test for `zcash_client_sqlite` on
//! `wasm32-unknown-unknown`. See README.md.
//!
//! Everything here is gated on the wasm target; on every other target this crate is empty,
//! so it costs a native `cargo test --workspace` nothing.

#![cfg(all(target_family = "wasm", target_os = "unknown"))]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand_chacha::{rand_core::SeedableRng, ChaChaRng};
use rusqlite::Connection;
use zcash_client_backend::{
    proto::service::compact_tx_streamer_client::CompactTxStreamerClient, sync,
};
use zcash_client_sqlite::{util::Clock, WalletDb};
use zcash_protocol::consensus::Network;

pub mod block_cache;
pub mod params;

pub use block_cache::MemoryBlockCache;

/// The wallet database type this harness uses.
pub type SmokeWalletDb = WalletDb<Connection, Network, JsClock, ChaChaRng>;

/// The gRPC transport a browser wallet uses.
///
/// `tonic`'s own transport needs an HTTP/2 socket, which a browser does not have, so
/// `lightwalletd-tonic` is enabled without `lightwalletd-tonic-transport` and the client is
/// built over `fetch` instead. [`tonic_web_wasm_client::Client`] speaks gRPC-Web, so the
/// server must too: either `lightwalletd`/Zaino behind a gRPC-Web proxy, or a server-side
/// `tonic-web` layer. Plain gRPC over HTTP/2 will not answer it.
pub type SmokeTransport = tonic_web_wasm_client::Client;

/// Connects to a gRPC-Web endpoint.
pub fn connect(base_url: String) -> CompactTxStreamerClient<SmokeTransport> {
    CompactTxStreamerClient::new(tonic_web_wasm_client::Client::new(base_url))
}

/// Runs one pass of the sync state machine.
///
/// This exists to pin down that [`sync::run`]'s bounds are all satisfiable on wasm with a
/// real transport — the `Send + 'static` requirements on the response body are the ones
/// that most often are not. It is not exercised at runtime: there is no gRPC-Web endpoint
/// to talk to in the test environment, and reaching out to a public one would be a live
/// network dependency rather than a test.
pub async fn sync_once(
    client: &mut CompactTxStreamerClient<SmokeTransport>,
    params: &Network,
    cache: &MemoryBlockCache,
    db: &mut SmokeWalletDb,
    batch_size: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    sync::run(client, params, cache, db, batch_size)
        .await
        .map_err(|e| format!("sync failed: {e:?}").into())
}

/// A [`Clock`] backed by JavaScript's `Date.now()`.
///
/// `SystemTime::now` panics on `wasm32-unknown-unknown` — `std` has no clock to read — so
/// `zcash_client_sqlite::util::SystemClock` cannot be used there. Anything running the
/// wallet in a browser has to supply a clock of this shape.
#[derive(Clone, Copy)]
pub struct JsClock;

impl Clock for JsClock {
    fn now(&self) -> SystemTime {
        // `Date::now` is milliseconds since the Unix epoch, as an f64. It is monotonic
        // enough for the wallet's purposes and always positive in any real environment.
        UNIX_EPOCH + Duration::from_millis(js_sys::Date::now() as u64)
    }
}

/// Opens a wallet database against the in-memory VFS.
///
/// Two things a native caller gets for free have to be done by hand here:
///
/// - a VFS must exist. `sqlite-wasm-rs` registers its in-memory VFS as the default, which
///   is what makes this work with no setup; a persistent wallet would register `sahpool`
///   (OPFS, dedicated Worker only) instead.
/// - the `rarray` module must be loaded. `WalletDb::for_path` does this, but it goes
///   through `Connection::open` on a filesystem path, so wasm callers use
///   `WalletDb::from_connection` and take on the obligation themselves. Skipping it
///   compiles, and then fails at runtime inside any query using `rarray`.
pub fn open_wallet() -> WalletDb<Connection, Network, JsClock, ChaChaRng> {
    let conn = Connection::open_in_memory().expect("the in-memory VFS is the default");
    rusqlite::vtab::array::load_module(&conn).expect("loads the rarray module");
    WalletDb::from_connection(
        conn,
        Network::TestNetwork,
        JsClock,
        ChaChaRng::seed_from_u64(0),
    )
}
