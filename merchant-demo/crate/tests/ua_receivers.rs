//! [zero] @claude The address handed to a customer must not advertise a pool nobody watches.
//!
//! `mint_addresses` asks for every receiver the key can produce, and the scanner reads only
//! Sapling and Orchard. If a viewing key ever carried a transparent component, the Unified
//! Address would include a t-address that no code path scans — money sent there would arrive
//! and never be noticed. This pins the two sets together.

use zcash_keys::keys::{UnifiedAddressRequest, UnifiedSpendingKey};
use zcash_protocol::consensus::Network;

#[test]
fn the_invoice_address_advertises_only_pools_the_scanner_reads() {
    let params = Network::MainNetwork;
    let mut seed = [0u8; 32];
    seed[31] = 1;
    let usk = UnifiedSpendingKey::from_seed(&params, &seed, zip32::AccountId::ZERO).unwrap();
    let uivk = usk.to_unified_full_viewing_key().to_unified_incoming_viewing_key();
    let (ua, _) = uivk
        .find_address(
            zip32::DiversifierIndex::from(0u32),
            UnifiedAddressRequest::AllAvailableKeys,
        )
        .unwrap();

    assert!(ua.sapling().is_some(), "expected a Sapling receiver to scan");
    assert!(ua.orchard().is_some(), "expected an Orchard receiver to scan");
    assert!(
        ua.transparent().is_none(),
        "the invoice address advertises a transparent receiver, but `scan_blocks` reads only \
         Sapling and Orchard — a customer paying that receiver would never be credited"
    );
}
