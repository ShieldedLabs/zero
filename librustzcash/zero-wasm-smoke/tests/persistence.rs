//! [zero] @claude A wallet database persisted to IndexedDB.
//!
//! IndexedDB exists only in a browser, so unlike the rest of the suite this file runs
//! under `wasm-pack test --headless --chrome`, not `--node`.

#![cfg(all(target_family = "wasm", target_os = "unknown"))]

use rand_chacha::{rand_core::SeedableRng, ChaChaRng};
use secrecy::SecretVec;
use sqlite_wasm_rs::WasmOsCallback;
use sqlite_wasm_vfs::relaxed_idb::{install, RelaxedIdbCfgBuilder};
use wasm_bindgen::prelude::*;
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
use zcash_client_backend::data_api::{chain::ChainState, AccountBirthday, WalletRead, WalletWrite};
use zcash_client_sqlite::{wallet::init::init_wallet_db, WalletDb};
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, Network};

use zero_wasm_smoke::JsClock;

wasm_bindgen_test_configure!(run_in_browser);

/// The VFS name, which `relaxed-idb` also uses as the IndexedDB database name.
const VFS_NAME: &str = "zero-smoke-idb";

/// The account's birthday: a testnet height well past Sapling activation.
const BIRTHDAY_HEIGHT: u32 = 2_000_000;

/// The wallet's file name within the VFS.
const WALLET_FILE: &str = "wallet.db";

/// How long to wait for `relaxed-idb`'s background commit to reach IndexedDB.
const COMMIT_POLL_MS: u32 = 50;
const COMMIT_POLL_LIMIT: u32 = 100;

#[wasm_bindgen(inline_js = r#"
export async function idb_page_count(name) {
  const db = await new Promise((resolve, reject) => {
    const req = indexedDB.open(name);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
  try {
    return await new Promise((resolve, reject) => {
      const req = db.transaction("blocks").objectStore("blocks").count();
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
  } finally {
    db.close();
  }
}

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
"#)]
extern "C" {
    /// The number of stored blocks in the `relaxed-idb` database called `name`, read
    /// from IndexedDB directly rather than through the VFS's in-memory copy.
    async fn idb_page_count(name: &str) -> JsValue;
    async fn sleep(ms: u32);
}

fn open(path: &str) -> WalletDb<rusqlite::Connection, Network, JsClock, ChaChaRng> {
    WalletDb::for_path(
        path,
        Network::TestNetwork,
        JsClock,
        ChaChaRng::seed_from_u64(0),
    )
    .expect("opens the wallet on the default VFS")
}

/// `WalletDb::for_path` works unchanged over a persistent VFS registered as the
/// default, and what it writes reaches IndexedDB.
#[wasm_bindgen_test]
async fn wallet_persists_to_indexeddb() {
    install::<WasmOsCallback>(
        &RelaxedIdbCfgBuilder::new()
            .vfs_name(VFS_NAME)
            .clear_on_init(true)
            .build(),
        true,
    )
    .await
    .expect("installs relaxed-idb as the default VFS");

    {
        let mut db = open(WALLET_FILE);
        init_wallet_db(&mut db, None).expect("migrations apply");
        let birthday = AccountBirthday::from_parts(
            ChainState::empty(BlockHeight::from_u32(BIRTHDAY_HEIGHT), BlockHash([0; 32])),
            None,
        );
        db.create_account("persisted", &SecretVec::new(vec![1u8; 32]), &birthday, None)
            .expect("creates an account");
    }

    // Commits reach IndexedDB from a background task, after the transaction that
    // produced them has returned.
    let mut pages = 0.0;
    for _ in 0..COMMIT_POLL_LIMIT {
        pages = idb_page_count(VFS_NAME).await.as_f64().unwrap_or(0.0);
        if pages > 0.0 {
            break;
        }
        sleep(COMMIT_POLL_MS).await;
    }
    assert!(pages > 0.0, "the wallet's pages reached IndexedDB");

    let db = open(WALLET_FILE);
    assert_eq!(db.get_account_ids().expect("reads accounts").len(), 1);
}
