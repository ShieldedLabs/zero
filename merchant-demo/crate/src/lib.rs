//! [zero] @claude Recognising Zcash payments, and nothing else.
//!
//! A merchant needs to answer one question: did anyone pay invoice N, and how much? That
//! needs a viewing key and trial decryption. It does not need a wallet database, note
//! commitment trees, witnesses, or proving — which is fortunate, because those are what
//! make a full wallet expensive on wasm.
//!
//! The key held here **cannot spend**. It recognises incoming notes and nothing more, which
//! is what makes it reasonable to run this on a server that faces the internet.
//!
//! # Attributing a payment to an invoice
//!
//! Trial decryption returns the note *and the address it was sent to*. A viewing key can
//! mint an unlimited number of addresses, so giving each invoice its own address makes the
//! recipient identify the invoice — no memo, no cooperation from the payer's wallet, and no
//! second request to read one. See [`mint_addresses`].

use serde::{Deserialize, Serialize};
use zcash_keys::keys::{UnifiedIncomingViewingKey, UnifiedSpendingKey};
use zcash_protocol::consensus::{BlockHeight, Network, Parameters};

mod scan;

pub use scan::{scan_blocks, Payment};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod wasm;

/// An address minted for one invoice.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InvoiceAddress {
    /// The diversifier index this address actually came from.
    ///
    /// Not necessarily the index that was requested — see [`mint_addresses`].
    pub index: u32,
    /// The address, encoded for the network it was minted on.
    pub address: String,
}

/// Parses a network name.
pub fn network(name: &str) -> Result<Network, String> {
    match name {
        "main" => Ok(Network::MainNetwork),
        "test" => Ok(Network::TestNetwork),
        other => Err(format!("unknown network {other:?}")),
    }
}

/// Derives the encoded Unified Incoming Viewing Key for an account.
///
/// This is the only place a spending key appears, and it appears so that a merchant can
/// generate a viewing key to deploy. In production the seed lives wherever the funds are
/// managed and only the resulting string is handed to the merchant service.
pub fn derive_uivk(network_name: &str, seed_hex: &str, account: u32) -> Result<String, String> {
    let params = network(network_name)?;
    let seed = hex::decode(seed_hex.trim()).map_err(|e| format!("seed is not hex: {e}"))?;
    if seed.len() < 32 {
        return Err("seed must be at least 32 bytes".to_owned());
    }
    let account =
        zip32::AccountId::try_from(account).map_err(|_| "account index out of range".to_owned())?;
    let usk = UnifiedSpendingKey::from_seed(&params, &seed, account)
        .map_err(|e| format!("could not derive the spending key: {e:?}"))?;
    Ok(usk
        .to_unified_full_viewing_key()
        .to_unified_incoming_viewing_key()
        .encode(&params))
}

/// Mints `count` distinct addresses, one per invoice.
///
/// # Why the returned index matters
///
/// Roughly half of all diversifier indices do not produce a valid address, and the
/// underlying key API searches *forward* from the index it is given until it finds one that
/// does. Asking for indices 0..8 therefore yields far fewer than eight distinct addresses —
/// several requests land on the same one. A merchant that files invoices under the index it
/// asked for would give different invoices the same address and be unable to tell their
/// payments apart.
///
/// So the index in [`InvoiceAddress`] is the one the key actually used, and that is what a
/// caller must store against the invoice.
pub fn mint_addresses(
    network_name: &str,
    uivk: &str,
    start: u32,
    count: u32,
) -> Result<Vec<InvoiceAddress>, String> {
    let params = network(network_name)?;
    let uivk = UnifiedIncomingViewingKey::decode(&params, uivk)
        .map_err(|e| format!("could not decode the viewing key: {e}"))?;
    let dfvk = uivk
        .sapling()
        .as_ref()
        .ok_or_else(|| "the viewing key has no Sapling component".to_owned())?;

    let mut out = Vec::new();
    let mut next = start;
    while out.len() < count as usize {
        let Some((found_at, address)) = dfvk.find_address(zip32::DiversifierIndex::from(next))
        else {
            break;
        };
        let index = u32::try_from(u128::from(found_at))
            .map_err(|_| "diversifier index beyond u32".to_owned())?;
        out.push(InvoiceAddress {
            index,
            address: address_string(&params, &address),
        });
        next = index.checked_add(1).ok_or("ran out of indices")?;
    }
    Ok(out)
}

fn address_string(params: &Network, address: &sapling::PaymentAddress) -> String {
    use zcash_keys::address::Address;
    Address::from(*address).encode(params)
}

/// The ZIP 212 rules in force at `height`.
///
/// Hardcoding "enforced" is correct for the current chain and silently wrong for blocks
/// older than Canopy: decryption simply returns nothing, so payments are missed with no
/// error. Deriving it from the height costs nothing and removes that failure mode.
pub(crate) fn enforcement(
    params: &Network,
    height: BlockHeight,
) -> sapling::note_encryption::Zip212Enforcement {
    zcash_primitives::transaction::components::sapling::zip212_enforcement(params, height)
}

/// Exposed so callers can sanity-check the network they were configured with.
pub fn network_name(params: &Network) -> &'static str {
    if params.network_type() == zcash_protocol::consensus::NetworkType::Main {
        "main"
    } else {
        "test"
    }
}

/// Renders `text` as a QR code, as an SVG string.
///
/// Lives here rather than in JavaScript so the payment URI and the image a customer scans
/// are produced by the same code that understands the address — a mismatch between them is
/// a payment sent nowhere.
pub fn qr_svg(text: &str) -> Result<String, String> {
    use qrcode::{render::svg, EcLevel, QrCode};
    // Medium error correction: a payment URI is long enough that the highest level bloats
    // the image, and a screen is not a crumpled receipt.
    let code = QrCode::with_error_correction_level(text, EcLevel::M)
        .map_err(|e| format!("could not encode the QR code: {e}"))?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(220, 220)
        .quiet_zone(true)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build())
}
