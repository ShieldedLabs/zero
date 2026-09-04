//! [zero] @claude Attribution still works now that invoices use Unified Addresses.
//!
//! A UA is a bundle of receivers and a payment arrives at one of them, so matching is on
//! receiver bytes rather than on the address string a customer was shown. This checks the
//! indirection did not break the link between a payment and its invoice.

use std::collections::HashSet;

use zcash_client_backend::scanning::testing::fake_compact_block;
use zcash_keys::keys::{UnifiedIncomingViewingKey, UnifiedSpendingKey};
use zcash_merchant_core::{derive_uivk, mint_addresses, network, scan_blocks};
use zcash_primitives::block::BlockHash;
use zcash_protocol::value::Zatoshis;

const SEED: &str = "0000000000000000000000000000000000000000000000000000000000000001";
/// Past testnet Canopy so ZIP 212 enforcement is on.
const HEIGHT: u32 = 2_500_000;

fn sapling_dfvk() -> sapling::zip32::DiversifiableFullViewingKey {
    let params = network("main").unwrap();
    let seed = hex::decode(SEED).unwrap();
    let usk = UnifiedSpendingKey::from_seed(&params, &seed, zip32::AccountId::ZERO).unwrap();
    usk.to_unified_full_viewing_key().sapling().unwrap().clone()
}

fn paid_block() -> zcash_client_backend::proto::compact_formats::CompactBlock {
    fake_compact_block(
        HEIGHT.into(),
        BlockHash([0; 32]),
        sapling::Nullifier([0; 32]),
        &sapling_dfvk(),
        Zatoshis::const_from_u64(250_000),
        false,
        None,
    )
}

#[test]
fn invoices_get_distinct_unified_addresses() {
    let uivk = derive_uivk("main", SEED, 0).expect("viewing key derives");
    let minted = mint_addresses("main", &uivk, 0, 8).expect("addresses mint");

    assert_eq!(minted.len(), 8);
    for a in &minted {
        assert!(a.address.starts_with("u1"), "not unified: {}", a.address);
    }
    let distinct: HashSet<_> = minted.iter().map(|a| a.address.clone()).collect();
    assert_eq!(distinct.len(), 8, "two invoices would share an address");

    // Not 0..8: indices that yield no valid address are skipped, and filing an invoice under
    // the requested index rather than the returned one is what makes two invoices collide.
    let indices: Vec<u32> = minted.iter().map(|a| a.index).collect();
    assert_ne!(indices, (0..8).collect::<Vec<_>>(), "expected gaps in the indices");
}

#[test]
fn a_payment_is_attributed_to_its_invoice() {
    let params = network("main").unwrap();
    let uivk = UnifiedIncomingViewingKey::decode(&params, &derive_uivk("main", SEED, 0).unwrap())
        .unwrap();
    let minted = mint_addresses("main", &derive_uivk("main", SEED, 0).unwrap(), 0, 8).unwrap();
    let indices: Vec<u32> = minted.iter().map(|a| a.index).collect();

    let found = scan_blocks(&params, &uivk, &indices, &[paid_block()]);
    assert_eq!(found.len(), 1, "expected one payment");
    assert_eq!(found[0].zatoshis, 250_000);
    assert!(
        found[0].index.is_some(),
        "payment not attributed to any invoice — receiver matching is broken"
    );
    assert!(indices.contains(&found[0].index.unwrap()));
}

#[test]
fn a_payment_outside_the_invoice_set_is_still_reported() {
    let params = network("main").unwrap();
    let uivk = UnifiedIncomingViewingKey::decode(&params, &derive_uivk("main", SEED, 0).unwrap())
        .unwrap();
    // Watching nothing: the money still arrived and must not vanish from the report.
    let found = scan_blocks(&params, &uivk, &[], &[paid_block()]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].index, None);
}
