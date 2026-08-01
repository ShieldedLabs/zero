//! Byte-vector tests for the turnstile classifier.
//!
//! These run against committed wire bytes, with no test-only zebra-chain
//! features involved: bytes in, verdict out, exactly as the shim sees them on
//! the SendTransaction path. `classify_generated.rs` regenerates equivalent
//! transactions in memory and is the tripwire for these fixtures going stale.
//!
//! The three V6 fixtures were produced by the generator in
//! `classify_generated.rs` (see the `regenerate_fixtures` note there). They are
//! not reproducible byte-for-byte, because the dummy Orchard action inside is
//! entropy-seeded, so they are captured once and committed. What the classifier
//! reads is the value balances, not the action content.

use zero_indexer_shim::classify::{classify, classify_with_evidence, Class};

/// V6, Orchard value balance +250_000 (value LEAVING Orchard),
/// Ironwood value balance -240_000 (value ENTERING Ironwood). An Orchard exit
/// into Ironwood: the shape the classifier was originally written for.
const V6_MIGRATION: &[u8] = include_bytes!("fixtures/v6_migration.bin");

/// V6, the same transaction with both balances negated: value entering Orchard,
/// leaving Ironwood.
///
/// This shape is **consensus-invalid after NU6.3** and cannot appear on chain:
/// a transaction-level rule forbids new value entering the Orchard pool, so the
/// chain predicate is Orchard pool value non-increasing and `orchard_vb >= 0`
/// always. (Orchard is closed to new *value*, not to activity: same-receiver
/// change still lands in the pool.)
///
/// It is kept deliberately, as a **directionality probe**. It pins that the
/// SIGN of the Orchard balance is what decides the verdict, which no realizable
/// post-NU6.3 transaction can test. Do not read it as a realistic pass-through;
/// for that see [`V4_COINBASE_HEX`], which is real mainnet bytes.
const V6_REVERSE: &[u8] = include_bytes!("fixtures/v6_reverse.bin");

/// V6 with an Orchard bundle and NO Ironwood bundle: Orchard value balance
/// +250_000, Ironwood 0. An Orchard withdrawal to transparent or Sapling, and
/// the case the old Orchard-to-Ironwood predicate passed through in the clear.
const V6_ORCHARD_ONLY: &[u8] = include_bytes!("fixtures/v6_orchard_only.bin");

/// A real mainnet V4 coinbase transaction. Transparent only, pre-V6, so its
/// Orchard value balance is zero and it is a genuine pass-through.
const V4_COINBASE_HEX: &str = "0400008085202f89010000000000000000000000000000000000000000000000000000000000000000ffffffff0503b0e72100ffffffff04e8bbe60e000000001976a914ba92ff06081d5ff6542af8d3b2d209d29ba6337c88ac40787d010000000017a914931fec54c1fea86e574462cc32013f5400b891298738c94d010000000017a914c7a4285ed7aed78d8c0e28d7f1839ccb4046ab0c87286bee000000000017a914d45cb1adffb5215a42720532a076f02c7c778c908700000000b0e721000000000000000000000000";

#[test]
fn v6_orchard_exit_into_ironwood_is_a_migration() {
    let evidence = classify_with_evidence(V6_MIGRATION);
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::Migration);
    assert_eq!(evidence.version, "V6");
    // The predicate, read off the real parsed bundle.
    assert_eq!(evidence.orchard_vb, 250_000, "value must LEAVE Orchard");
    // Evidence only. Where the value went does not gate the verdict; it is
    // logged so an operator can see the destination.
    assert_eq!(evidence.ironwood_vb, -240_000);
    assert_eq!(evidence.len, V6_MIGRATION.len());
    assert!(evidence.error.is_none());
    assert!(evidence.class.treat_as_migration());
}

/// Directionality probe on a consensus-invalid shape, see [`V6_REVERSE`].
#[test]
fn the_predicate_is_directional_not_symmetric() {
    let evidence = classify_with_evidence(V6_REVERSE);
    println!("{evidence:?}");

    // Value ENTERING Orchard. Nothing left the pool, so nothing was revealed
    // about legacy Orchard holdings. Note what this pins now that the Ironwood
    // conjunct is gone: the sign of the Orchard balance alone decides the
    // verdict. An implementation that keyed off "an Orchard bundle is present"
    // or off the magnitude would classify this as a migration and fail here.
    assert_eq!(evidence.class, Class::PassThrough);
    assert_eq!(evidence.version, "V6");
    assert_eq!(evidence.orchard_vb, -250_000);
    assert_eq!(evidence.ironwood_vb, 240_000);
    assert!(!evidence.class.treat_as_migration());
}

#[test]
fn an_orchard_exit_without_an_ironwood_bundle_is_a_migration() {
    let evidence = classify_with_evidence(V6_ORCHARD_ONLY);
    println!("{evidence:?}");

    // The change Zooko's ruling makes, pinned. Value left Orchard and there is
    // no Ironwood bundle at all, so it went to transparent or to Sapling. Under
    // the old Orchard-to-Ironwood predicate this was a PassThrough and the shim
    // handed it to the operator's indexer in the clear, leaking exactly the same
    // "this IP controls legacy Orchard funds" as an Ironwood-bound migration.
    assert_eq!(evidence.class, Class::Migration);
    assert_eq!(evidence.version, "V6");
    assert_eq!(evidence.orchard_vb, 250_000, "value LEFT Orchard");
    assert_eq!(
        evidence.ironwood_vb, 0,
        "no Ironwood bundle, and it does not matter"
    );
    assert!(evidence.class.treat_as_migration());
}

#[test]
fn transparent_pre_v6_is_pass_through() {
    // Real mainnet bytes, and the realistic pass-through: no Orchard bundle, so
    // orchard_vb is zero and the predicate does not fire. Nothing here depends
    // on the version being pre-V6; the dropped version guard is not what makes
    // this pass, the zero Orchard balance is.
    let bytes = hex::decode(V4_COINBASE_HEX).expect("fixture is valid hex");
    let evidence = classify_with_evidence(&bytes);
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::PassThrough);
    assert_eq!(evidence.version, "V4");
    assert_eq!(evidence.orchard_vb, 0);
    assert_eq!(evidence.ironwood_vb, 0);
    assert_eq!(evidence.inputs, 1);
    assert_eq!(evidence.outputs, 4);
}

#[test]
fn empty_body_is_unparseable() {
    let evidence = classify_with_evidence(&[]);
    assert_eq!(evidence.class, Class::Unparseable);
    assert_eq!(evidence.version, "unparseable");
    assert!(evidence.error.is_some());
    // Fail-safe for privacy: the caller must route this like a migration.
    assert!(evidence.class.treat_as_migration());
}

#[test]
fn garbage_is_unparseable() {
    assert_eq!(classify(&[0xde, 0xad, 0xbe, 0xef]), Class::Unparseable);
    assert_eq!(classify(&[0x00; 128]), Class::Unparseable);
    assert_eq!(classify(&[0xff; 128]), Class::Unparseable);
}

#[test]
fn truncated_migration_is_unparseable_not_pass_through() {
    // The dangerous failure would be classifying a damaged migration as an
    // ordinary transaction. It must land in the fail-safe bucket instead.
    let truncated = &V6_MIGRATION[..V6_MIGRATION.len() / 2];
    assert_eq!(classify(truncated), Class::Unparseable);
    assert!(classify(truncated).treat_as_migration());
}

#[test]
fn trailing_bytes_are_unparseable() {
    // zebra's deserializer stops at the end of the transaction and ignores the
    // rest, so without the full-consumption check this parses Ok and the shim
    // would classify a prefix of what the backing node actually receives.
    let mut trailing = V6_MIGRATION.to_vec();
    trailing.extend_from_slice(&[0xff; 16]);

    let evidence = classify_with_evidence(&trailing);
    assert_eq!(evidence.class, Class::Unparseable);
    assert!(evidence
        .error
        .as_deref()
        .is_some_and(|err| err.contains("trailing bytes")));
}

#[test]
fn single_byte_truncations_never_pass_through() {
    // Every prefix of a real migration is either a parse failure or a
    // trailing/short read. None of them may be classified as an ordinary
    // transaction, since each one is a damaged migration.
    for len in [1, 8, 64, 512, 4096, V6_MIGRATION.len() - 1] {
        assert_eq!(
            classify(&V6_MIGRATION[..len]),
            Class::Unparseable,
            "prefix of {len} bytes must not pass through"
        );
    }
}

#[test]
fn classify_matches_classify_with_evidence_everywhere() {
    for bytes in [
        V6_MIGRATION,
        V6_REVERSE,
        V6_ORCHARD_ONLY,
        &[][..],
        &[0xff; 32][..],
    ] {
        assert_eq!(classify(bytes), classify_with_evidence(bytes).class);
    }
}
