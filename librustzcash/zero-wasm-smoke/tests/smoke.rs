//! [zero] @claude Runtime smoke tests for `zcash_client_sqlite` under wasm.
//!
//! These live in `tests/` rather than a `#[cfg(test)] mod tests`, because that is where
//! `wasm-bindgen-test` collects them from.

#![cfg(all(target_family = "wasm", target_os = "unknown"))]

use std::time::{Duration, UNIX_EPOCH};

use rusqlite::Connection;
use secrecy::SecretVec;
use wasm_bindgen_test::wasm_bindgen_test;
use zcash_client_backend::data_api::WalletRead;
use zcash_client_sqlite::{util::Clock, wallet::init::init_wallet_db};

use zero_wasm_smoke::{open_wallet, JsClock};

/// Every migration applies against the wasm SQLite build.
///
/// This is the test that would catch a divergence between `sqlite-wasm-rs` and the
/// bundled C build: the migrations are the densest use of SQLite features in the
/// crate.
#[wasm_bindgen_test]
fn migrations_apply() {
    let mut db = open_wallet();
    init_wallet_db(&mut db, Some(SecretVec::new(vec![0u8; 32]))).expect("all migrations apply");

    // A second run must be a no-op rather than an error; this is the path every
    // subsequent wallet open takes.
    init_wallet_db(&mut db, Some(SecretVec::new(vec![0u8; 32])))
        .expect("re-running migrations is idempotent");
}

/// Reads go through the schema the migrations just built.
#[wasm_bindgen_test]
fn reads_against_a_migrated_database() {
    let mut db = open_wallet();
    init_wallet_db(&mut db, Some(SecretVec::new(vec![0u8; 32]))).expect("migrations apply");

    assert!(db
        .get_account_ids()
        .expect("queries the accounts table")
        .is_empty());
    assert_eq!(db.chain_height().expect("queries the blocks table"), None);
    assert_eq!(
        db.get_wallet_birthday().expect("queries the blocks table"),
        None
    );
}

/// The `rarray` virtual table works.
///
/// The wallet binds `Vec`-valued parameters through `rarray` (note selection excludes,
/// lock owners), so a build where the module is missing or broken would compile and
/// then fail only on those queries. Test it directly rather than waiting for one.
#[wasm_bindgen_test]
fn rarray_module_is_usable() {
    use std::rc::Rc;

    let conn = Connection::open_in_memory().expect("the in-memory VFS is the default");
    rusqlite::vtab::array::load_module(&conn).expect("loads the rarray module");
    let values: Vec<rusqlite::types::Value> = vec![1i64.into(), 3i64.into()];
    let selected: Vec<i64> = conn
        .prepare("SELECT value FROM rarray(?1) ORDER BY value")
        .expect("prepares an rarray query")
        .query_map([Rc::new(values)], |row| row.get(0))
        .expect("runs an rarray query")
        .collect::<Result<_, _>>()
        .expect("reads rarray rows");
    assert_eq!(selected, vec![1, 3]);
}

/// The clock the wallet is handed actually reads a time.
#[wasm_bindgen_test]
fn js_clock_reads_a_plausible_time() {
    // 2020-01-01, comfortably in the past and comfortably after the epoch: this fails
    // if `Date::now` returned 0 or the conversion lost the value.
    let earliest = UNIX_EPOCH + Duration::from_secs(1_577_836_800);
    assert!(JsClock.now() > earliest);
}
