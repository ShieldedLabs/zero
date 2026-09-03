//! [zero] @claude End-to-end checks for the demo wallet.
//!
//! These reach the network. `sync` against a real lightwalletd is the only way to know the
//! gRPC-Web transport works, and no local server speaks the protocol — the endpoint is
//! ChainSafe's public proxy, the same one WebZjs uses by default. Only public chain data
//! is read; nothing about the wallet is sent.

#![cfg(all(target_family = "wasm", target_os = "unknown"))]

use wasm_bindgen_test::{console_log, wasm_bindgen_test};
use zero_wasm_demo::{add_account, create_wallet, export_db, fetch_tip_height, summary};

const PROXY: &str = "https://zcash-mainnet.chainsafe.dev";
const SEED: &str = "0000000000000000000000000000000000000000000000000000000000000001";

/// Mainnet activated Sapling at 419_200 and has been running since 2016; any real tip is
/// far above this, and a number below it means the server answered with something else.
const PLAUSIBLE_MAINNET_TIP: u32 = 2_000_000;

fn field(value: &wasm_bindgen::JsValue, name: &str) -> wasm_bindgen::JsValue {
    js_sys::Reflect::get(value, &name.into()).expect("the summary has this field")
}

/// The gRPC-Web transport reaches a real lightwalletd.
#[wasm_bindgen_test]
async fn reaches_lightwalletd() {
    let tip = fetch_tip_height(PROXY.to_owned())
        .await
        .expect("the proxy answers");
    console_log!("mainnet tip: {tip}");
    assert!(
        tip > PLAUSIBLE_MAINNET_TIP,
        "tip {tip} is not a plausible mainnet height"
    );
}

/// A wallet database survives an export and re-import.
///
/// This is the whole persistence mechanism: `sqlite-wasm-rs` 0.5 has no persistent VFS, so
/// the page stores the exported database file itself. The restored wallet must report the
/// same account — a fresh database would report none.
#[wasm_bindgen_test]
fn persists_across_a_wipe() {
    create_wallet("main", &[]).expect("the wallet is created");
    let added = add_account("Test account", SEED, 3_400_000).expect("the account is added");
    let address = field(&added, "accounts");
    let address = js_sys::Reflect::get(&js_sys::Array::from(&address).get(0), &"address".into())
        .expect("the account has an address");

    let image = export_db().expect("the database exports");
    assert!(!image.is_empty(), "the exported database is empty");

    create_wallet("main", &[]).expect("a fresh wallet is created");
    let fresh = summary().expect("the fresh wallet reports a summary");
    assert_eq!(
        js_sys::Array::from(&field(&fresh, "accounts")).length(),
        0,
        "a fresh wallet should have no accounts"
    );

    create_wallet("main", &image).expect("the stored database is restored");
    let restored = summary().expect("the restored wallet reports a summary");
    let accounts = js_sys::Array::from(&field(&restored, "accounts"));
    assert_eq!(accounts.length(), 1, "the restored wallet lost its account");
    assert_eq!(
        js_sys::Reflect::get(&accounts.get(0), &"address".into()).ok(),
        Some(address),
        "the restored account has a different address"
    );
    console_log!("persisted and restored {} KiB", image.len() / 1024);
}

/// Server-streaming works over gRPC-Web.
///
/// `sync::run` streams compact blocks and subtree roots; if streaming did not work the
/// symptom would be a sync that hangs rather than one that errors, so test it on its own.
#[wasm_bindgen_test]
async fn streams_compact_blocks() {
    let tip = fetch_tip_height(PROXY.to_owned())
        .await
        .expect("the proxy answers");
    let started = js_sys::Date::now();
    let seen = zero_wasm_demo::probe_block_stream(PROXY.to_owned(), tip - 10, 10)
        .await
        .expect("the block stream completes");
    console_log!(
        "streamed {seen} blocks in {:.0} ms",
        js_sys::Date::now() - started
    );
    assert_eq!(seen, 10, "expected 10 blocks from the stream");
}
