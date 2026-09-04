//! [zero] @claude The JavaScript surface.
//!
//! Kept thin on purpose: this is the boundary where a scan range turns into a list of
//! payments, and everything about invoices, storage, and policy lives in the TypeScript
//! layer above. The split matters because those are the parts a merchant will want to
//! change, and they should not require rebuilding wasm to do it.

use std::collections::HashMap;

use futures_util::StreamExt;
use wasm_bindgen::prelude::*;
use zcash_client_backend::proto::service::{
    compact_tx_streamer_client::CompactTxStreamerClient, BlockId, BlockRange, ChainSpec,
};
use zcash_keys::keys::UnifiedIncomingViewingKey;

use crate::{derive_uivk, mint_addresses, network, qr_svg, scan_blocks, InvoiceAddress, Payment};

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

fn js<T: serde::Serialize>(value: &T) -> Result<JsValue, JsError> {
    serde_wasm_bindgen::to_value(value).map_err(|e| JsError::new(&e.to_string()))
}

/// Derives the viewing key a merchant service should be deployed with.
#[wasm_bindgen(js_name = deriveViewingKey)]
pub fn derive_viewing_key(
    network_name: &str,
    seed_hex: &str,
    account: u32,
) -> Result<String, JsError> {
    derive_uivk(network_name, seed_hex, account).map_err(|e| JsError::new(&e))
}

/// Mints `count` invoice addresses starting the search at `start`.
///
/// Returns `{ index, address }` pairs. Store the returned `index`, not the one requested:
/// see `mint_addresses` in the Rust crate for why they differ.
#[wasm_bindgen(js_name = mintAddresses)]
pub fn mint_addresses_js(
    network_name: &str,
    uivk: &str,
    start: u32,
    count: u32,
) -> Result<JsValue, JsError> {
    let addresses: Vec<InvoiceAddress> =
        mint_addresses(network_name, uivk, start, count).map_err(|e| JsError::new(&e))?;
    js(&addresses)
}

/// The current chain tip.
#[wasm_bindgen(js_name = tipHeight)]
pub async fn tip_height(url: String) -> Result<u32, JsError> {
    let mut client = CompactTxStreamerClient::new(tonic_web_wasm_client::Client::new(url));
    let tip = client
        .get_latest_block(ChainSpec::default())
        .await
        .map_err(|e| JsError::new(&format!("lightwalletd request failed: {e}")))?;
    Ok(tip.into_inner().height as u32)
}

/// Scans `[from, from + count)` and returns the payments found.
///
/// `addresses` is a JS object mapping encoded address to invoice index.
#[wasm_bindgen(js_name = scanRange)]
pub async fn scan_range(
    url: String,
    network_name: String,
    uivk: String,
    addresses: JsValue,
    from: u32,
    count: u32,
) -> Result<JsValue, JsError> {
    let params = network(&network_name).map_err(|e| JsError::new(&e))?;
    let key = UnifiedIncomingViewingKey::decode(&params, &uivk)
        .map_err(|e| JsError::new(&format!("could not decode the viewing key: {e}")))?;
    let addresses: HashMap<String, u32> =
        serde_wasm_bindgen::from_value(addresses).map_err(|e| JsError::new(&e.to_string()))?;

    let mut client = CompactTxStreamerClient::new(tonic_web_wasm_client::Client::new(url));
    let mut stream = client
        .get_block_range(BlockRange {
            start: Some(BlockId {
                height: u64::from(from),
                hash: vec![],
            }),
            end: Some(BlockId {
                height: u64::from(from + count.saturating_sub(1)),
                hash: vec![],
            }),
            ..Default::default()
        })
        .await
        .map_err(|e| JsError::new(&format!("block range request failed: {e}")))?
        .into_inner();

    // Scanned per block rather than collected first: a range can be thousands of blocks and
    // holding them all costs memory for no benefit, since each is finished with once its
    // notes have been tried.
    let mut payments: Vec<Payment> = Vec::new();
    while let Some(block) = stream.next().await {
        let block = block.map_err(|e| JsError::new(&format!("block stream failed: {e}")))?;
        payments.extend(scan_blocks(&params, &key, &addresses, &[block]));
    }
    js(&payments)
}

/// Renders a payment URI as a scannable SVG QR code.
#[wasm_bindgen(js_name = qrSvg)]
pub fn qr_svg_js(text: &str) -> Result<String, JsError> {
    qr_svg(text).map_err(|e| JsError::new(&e))
}
