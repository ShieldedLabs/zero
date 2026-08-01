//! Live-generated transaction vectors for the turnstile classifier.
//!
//! `classify_vectors.rs` is the fast path: committed bytes, no test-only
//! features. This file is its tripwire. It builds the same transaction shapes
//! in memory with zebra-chain's own V6 helpers and re-asserts the predicate, so
//! if zebra-chain's wire format ever moves, this fails and tells us the
//! committed fixtures are stale rather than letting the classifier quietly
//! parse a dead format.
//!
//! It needs the `proptest-impl` feature to reach
//! `transaction::arbitrary::fake_v6_transaction`. That feature is a
//! dev-dependency only, so the shipped binary never links proptest.
//!
//! To regenerate the committed fixtures, run `regenerate_fixtures` with
//! `ZIS_WRITE_FIXTURES=1`:
//!
//! ```text
//! ZIS_WRITE_FIXTURES=1 cargo test --test classify_generated regenerate_fixtures -- --ignored
//! ```

use zebra_chain::{
    amount::{Amount, NegativeAllowed},
    block, ironwood,
    orchard::{Flags, ShieldedDataV6},
    parameters::NetworkUpgrade,
    serialization::ZcashSerialize,
    transaction::{
        arbitrary::{fake_v6_orchard_shielded_data, fake_v6_transaction},
        LockTime, Transaction,
    },
};
use zero_indexer_shim::classify::{classify, classify_with_evidence, Class};

/// Real V6 wire bytes with the given pool value balances.
///
/// Sign convention, the whole point of the predicate: a POSITIVE balance is
/// value LEAVING that pool, a NEGATIVE balance is value ENTERING it. So an
/// Orchard exit is `orchard > 0`, whatever the Ironwood balance is.
///
/// `ironwood_zats: None` omits the Ironwood bundle entirely.
fn v6_bytes(orchard_zats: i64, ironwood_zats: Option<i64>) -> Vec<u8> {
    let orchard_vb: Amount<NegativeAllowed> = orchard_zats.try_into().expect("valid amount");

    // fake_v6_orchard_shielded_data emits a canonically sized zero-filled halo2
    // proof, so the librustzcash round-trip inside zebra's V6 deserializer
    // accepts these bytes. This is the same helper zebra's own V6 round-trip
    // test uses.
    let orchard = ShieldedDataV6::new(fake_v6_orchard_shielded_data(
        Flags::ENABLE_SPENDS | Flags::ENABLE_OUTPUTS,
        orchard_vb,
        1,
    ));

    let ironwood = ironwood_zats.map(|zats| {
        let vb: Amount<NegativeAllowed> = zats.try_into().expect("valid amount");
        ironwood::ShieldedData::new(ShieldedDataV6::new(fake_v6_orchard_shielded_data(
            Flags::ENABLE_SPENDS | Flags::ENABLE_OUTPUTS,
            vb,
            1,
        )))
    });

    fake_v6_transaction(NetworkUpgrade::Nu6_3, Some(orchard), ironwood)
        .zcash_serialize_to_vec()
        .expect("v6 transaction serializes")
}

/// Real V5 wire bytes carrying an Orchard bundle with the given value balance.
///
/// V5 is where Orchard bundles first appeared, and a V5 Orchard spend leaks the
/// same fact as a V6 one, so the classifier must catch it with no version guard
/// to help it. Built by constructing the variant directly, because zebra-chain's
/// arbitrary helpers only offer a V6 constructor; the Orchard bundle helper is
/// shared, since V6 wraps the same `orchard::ShieldedData` this takes.
fn v5_orchard_bytes(orchard_zats: i64) -> Vec<u8> {
    let orchard_vb: Amount<NegativeAllowed> = orchard_zats.try_into().expect("valid amount");

    Transaction::V5 {
        network_upgrade: NetworkUpgrade::Nu5,
        lock_time: LockTime::unlocked(),
        expiry_height: block::Height(0),
        inputs: Vec::new(),
        outputs: Vec::new(),
        sapling_shielded_data: None,
        orchard_shielded_data: Some(fake_v6_orchard_shielded_data(
            Flags::ENABLE_SPENDS | Flags::ENABLE_OUTPUTS,
            orchard_vb,
            1,
        )),
    }
    .zcash_serialize_to_vec()
    .expect("v5 transaction serializes")
}

#[test]
fn generated_orchard_exit_into_ironwood_is_a_migration() {
    let bytes = v6_bytes(250_000, Some(-240_000));
    let evidence = classify_with_evidence(&bytes);
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::Migration);
    assert_eq!(evidence.version, "V6");
    assert_eq!(evidence.orchard_vb, 250_000);
    assert_eq!(evidence.ironwood_vb, -240_000);
}

#[test]
fn every_orchard_exit_is_a_migration_whatever_the_destination() {
    // The destination pool does not gate the verdict. Each of these moves value
    // out of a pool that NU6.3 closed to new value, which is the identifying
    // event, and each one used to pass through in the clear.

    // Out to transparent or Sapling: no Ironwood bundle at all.
    assert_eq!(classify(&v6_bytes(250_000, None)), Class::Migration);

    // Out of Orchard and out of Ironwood in the same transaction, landing
    // somewhere transparent or Sapling.
    assert_eq!(
        classify(&v6_bytes(250_000, Some(240_000))),
        Class::Migration
    );

    // Into Ironwood, the original migration shape.
    assert_eq!(
        classify(&v6_bytes(250_000, Some(-240_000))),
        Class::Migration
    );
}

#[test]
fn a_v5_orchard_spend_is_a_migration() {
    // The version guard is gone, and this is why it can be. zebra-chain's
    // orchard_value_balance() reads orchard_shielded_data(), which is
    // version-agnostic, so a pre-Ironwood V5 Orchard spend is caught by exactly
    // the same predicate. Under the old `tx.version == V6` conjunct this was a
    // PassThrough.
    let evidence = classify_with_evidence(&v5_orchard_bytes(250_000));
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::Migration);
    assert_eq!(evidence.version, "V5");
    assert_eq!(evidence.orchard_vb, 250_000);
    assert_eq!(evidence.ironwood_vb, 0, "a V5 has no Ironwood bundle");
}

#[test]
fn value_entering_orchard_is_pass_through() {
    // Consensus-invalid post-NU6.3 and kept as a directionality probe: nothing
    // left the Orchard pool, so nothing was revealed about legacy holdings.
    assert_eq!(
        classify(&v6_bytes(-250_000, Some(240_000))),
        Class::PassThrough
    );
    assert_eq!(
        classify(&v6_bytes(-250_000, Some(-240_000))),
        Class::PassThrough
    );
    assert_eq!(classify(&v5_orchard_bytes(-250_000)), Class::PassThrough);
}

#[test]
fn a_fee_sized_orchard_exit_still_classifies() {
    // The predicate is sign-based, not magnitude-based: a one-zatoshi withdrawal
    // from a closed pool is as identifying as a large one.
    assert_eq!(classify(&v6_bytes(1, Some(-1))), Class::Migration);
    assert_eq!(classify(&v6_bytes(1, None)), Class::Migration);
}

#[test]
fn zero_orchard_balance_is_correctly_a_pass_through() {
    // Under Zooko's rule the risk is that value LEFT the Orchard pool; an
    // Orchard bundle that nets to exactly zero moved no value out of it (pure
    // same-receiver change, which is still possible post-NU6.3 because Orchard
    // is closed to new VALUE, not to activity). Whatever entered Ironwood
    // alongside it came from transparent or Sapling. So this is the behaviour
    // the ruling's criterion specifies, and this test pins it.
    //
    // It is not a claim that the case leaks nothing: a net-zero bundle can
    // still spend legacy notes and publish their nullifiers. Whether the
    // predicate should widen to cover that is Zooko's call, open in the shim
    // README, and it would change this expectation if he says yes.
    assert_eq!(classify(&v6_bytes(0, Some(-240_000))), Class::PassThrough);
    assert_eq!(classify(&v6_bytes(0, None)), Class::PassThrough);
}

#[test]
fn generated_bytes_survive_the_full_consumption_check() {
    // A freshly serialized transaction must consume its bytes exactly, or the
    // classifier's trailing-bytes guard would reject every real transaction.
    let bytes = v6_bytes(250_000, Some(-240_000));
    assert!(classify_with_evidence(&bytes).error.is_none());
}

/// Rewrite the committed fixtures in `tests/fixtures/`. Ignored by default.
///
/// The bytes are not reproducible: the dummy Orchard action comes from
/// proptest's entropy-seeded `TestRunner::default()`, so each run produces
/// different but equally valid bytes. Never assert on their hashes.
#[test]
#[ignore = "writes tests/fixtures/, run explicitly with ZIS_WRITE_FIXTURES=1"]
fn regenerate_fixtures() {
    assert!(
        std::env::var("ZIS_WRITE_FIXTURES").is_ok(),
        "set ZIS_WRITE_FIXTURES=1 to confirm overwriting the committed fixtures"
    );

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for (name, bytes) in [
        ("v6_migration", v6_bytes(250_000, Some(-240_000))),
        ("v6_reverse", v6_bytes(-250_000, Some(240_000))),
        ("v6_orchard_only", v6_bytes(250_000, None)),
    ] {
        std::fs::write(dir.join(format!("{name}.bin")), &bytes).expect("fixture written");
        println!("{name}: {} bytes", bytes.len());
    }
}
