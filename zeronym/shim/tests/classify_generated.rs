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
    ironwood,
    orchard::{Flags, ShieldedDataV6},
    parameters::NetworkUpgrade,
    serialization::ZcashSerialize,
    transaction::arbitrary::{fake_v6_orchard_shielded_data, fake_v6_transaction},
};
use zero_indexer_shim::classify::{classify, classify_with_evidence, Class};

/// Real V6 wire bytes with the given pool value balances.
///
/// Sign convention, the whole point of the predicate: a POSITIVE balance is
/// value LEAVING that pool, a NEGATIVE balance is value ENTERING it. So an
/// Orchard -> Ironwood migration is `orchard > 0` and `ironwood < 0`.
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

#[test]
fn generated_migration_is_classified_as_migration() {
    let bytes = v6_bytes(250_000, Some(-240_000));
    let evidence = classify_with_evidence(&bytes);
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::Migration);
    assert_eq!(evidence.version, "V6");
    assert_eq!(evidence.orchard_vb, 250_000);
    assert_eq!(evidence.ironwood_vb, -240_000);
}

#[test]
fn generated_shield_and_deshield_shapes_are_pass_through() {
    // Shield-shaped: value enters Orchard, leaves Ironwood. Wrong direction.
    assert_eq!(
        classify(&v6_bytes(-250_000, Some(240_000))),
        Class::PassThrough
    );

    // Deshield-shaped: value leaves Orchard, no Ironwood bundle at all.
    assert_eq!(classify(&v6_bytes(250_000, None)), Class::PassThrough);

    // Both pools losing value: not a turnstile crossing.
    assert_eq!(
        classify(&v6_bytes(250_000, Some(240_000))),
        Class::PassThrough
    );

    // Both pools gaining value.
    assert_eq!(
        classify(&v6_bytes(-250_000, Some(-240_000))),
        Class::PassThrough
    );
}

#[test]
fn a_fee_sized_migration_still_classifies() {
    // The predicate is sign-based, not magnitude-based: a one-zatoshi crossing
    // is as much a migration as a large one.
    let bytes = v6_bytes(1, Some(-1));
    assert_eq!(classify(&bytes), Class::Migration);
}

#[test]
fn zero_orchard_balance_is_the_known_predicate_boundary() {
    // Documented, deliberate behaviour, not an accident. The locked predicate
    // requires orchard_value_balance STRICTLY > 0, so a V6 transaction that
    // nets its Orchard balance to exactly zero while value enters Ironwood
    // classifies as PassThrough. Spending an Orchard note always yields a
    // positive Orchard balance, so this shape should not occur in practice, but
    // it is the one false-negative path in the predicate and it is an open
    // question for the design owners. If the predicate is ever widened to
    // `>= 0`, this test is the thing that must change with it.
    let bytes = v6_bytes(0, Some(-240_000));
    assert_eq!(classify(&bytes), Class::PassThrough);
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
