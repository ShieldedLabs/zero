//! Byte-vector tests for the turnstile classifier.
//!
//! These run against committed wire bytes, with no test-only zebra-chain
//! features involved: bytes in, verdict out, exactly as the shim sees them on
//! the SendTransaction path. `classify_generated.rs` regenerates equivalent
//! transactions in memory and is the tripwire for these fixtures going stale.
//!
//! The five V6 fixtures were produced by the generator in
//! `classify_generated.rs` (see the `regenerate_fixtures` note there). They are
//! not reproducible byte-for-byte, because the dummy Orchard action inside is
//! entropy-seeded, so they are captured once and committed. What the classifier
//! reads is whether an Orchard bundle is present; the balances are evidence.

use zero_indexer_shim::classify::{classify, classify_with_evidence, Class};

/// V6 with an Orchard bundle, value balance +250_000 (value LEAVING Orchard),
/// and an Ironwood bundle at -240_000 (value ENTERING Ironwood). The shape the
/// classifier was originally written for.
const V6_MIGRATION: &[u8] = include_bytes!("fixtures/v6_migration.bin");

/// V6, the same transaction with both balances negated: value entering Orchard,
/// leaving Ironwood.
///
/// This shape is **consensus-invalid after NU6.3** and cannot appear on chain:
/// a transaction-level rule forbids new value entering the Orchard pool, so
/// `orchard_vb >= 0` holds for every post-activation transaction.
///
/// It is kept as a probe that the predicate has NO directionality left in it.
/// Under the old exit predicate this fixture pinned that the sign of the balance
/// decided the verdict; under Zooko's widened rule the sign decides nothing, and
/// the same bytes now pin the opposite property: Orchard actions are present, so
/// it is diverted.
const V6_REVERSE: &[u8] = include_bytes!("fixtures/v6_reverse.bin");

/// V6 with an Orchard bundle and NO Ironwood bundle: Orchard value balance
/// +250_000, Ironwood 0. An Orchard withdrawal to transparent or Sapling.
const V6_ORCHARD_ONLY: &[u8] = include_bytes!("fixtures/v6_orchard_only.bin");

/// V6 with an Orchard bundle whose value balance is exactly ZERO, alongside an
/// Ironwood bundle at -240_000: the internal shuffle whose fee was paid from
/// another pool. The gap Zooko's ruling closes.
const V6_ORCHARD_ZERO: &[u8] = include_bytes!("fixtures/v6_orchard_zero.bin");

/// V6 with an Ironwood bundle at -240_000 and NO Orchard bundle at all:
/// ordinary commerce in the new pool, shielding value into Ironwood.
const V6_IRONWOOD_ONLY: &[u8] = include_bytes!("fixtures/v6_ironwood_only.bin");

/// A real mainnet V4 coinbase transaction. Transparent only, pre-V6, so it
/// carries no Orchard bundle and is a genuine pass-through.
const V4_COINBASE_HEX: &str = "0400008085202f89010000000000000000000000000000000000000000000000000000000000000000ffffffff0503b0e72100ffffffff04e8bbe60e000000001976a914ba92ff06081d5ff6542af8d3b2d209d29ba6337c88ac40787d010000000017a914931fec54c1fea86e574462cc32013f5400b891298738c94d010000000017a914c7a4285ed7aed78d8c0e28d7f1839ccb4046ab0c87286bee000000000017a914d45cb1adffb5215a42720532a076f02c7c778c908700000000b0e721000000000000000000000000";

#[test]
fn v6_orchard_actions_into_ironwood_is_a_migration() {
    let evidence = classify_with_evidence(V6_MIGRATION);
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::Migration);
    assert_eq!(evidence.version, "V6");
    // The predicate, read off the real parsed bundle.
    assert_eq!(evidence.orchard_actions, 1, "Orchard actions are present");
    // Evidence only. Neither the amount nor where it went gates the verdict;
    // both are logged so an operator can read the line.
    assert_eq!(evidence.orchard_vb, 250_000);
    assert_eq!(evidence.ironwood_vb, -240_000);
    assert_eq!(evidence.len, V6_MIGRATION.len());
    assert!(evidence.error.is_none());
    assert!(evidence.class.treat_as_migration());
}

/// Direction probe on a consensus-invalid shape, see [`V6_REVERSE`].
#[test]
fn the_direction_of_the_orchard_balance_does_not_change_the_verdict() {
    let evidence = classify_with_evidence(V6_REVERSE);
    println!("{evidence:?}");

    // Same fixture, opposite sign, same verdict. This is the flip Zooko's
    // ruling makes: an implementation that still read the sign, or that read the
    // balance at all, would classify this as a PassThrough and fail here.
    assert_eq!(evidence.class, Class::Migration);
    assert_eq!(evidence.version, "V6");
    assert_eq!(evidence.orchard_actions, 1);
    assert_eq!(evidence.orchard_vb, -250_000, "value entering Orchard");
    assert_eq!(evidence.ironwood_vb, 240_000);
    assert!(evidence.class.treat_as_migration());
}

#[test]
fn orchard_actions_without_an_ironwood_bundle_are_a_migration() {
    let evidence = classify_with_evidence(V6_ORCHARD_ONLY);
    println!("{evidence:?}");

    // The destination is not part of the rule. Value left Orchard and there is
    // no Ironwood bundle at all, so it went to transparent or to Sapling, which
    // leaks exactly the same "this IP controls legacy Orchard funds".
    assert_eq!(evidence.class, Class::Migration);
    assert_eq!(evidence.version, "V6");
    assert_eq!(evidence.orchard_actions, 1);
    assert_eq!(evidence.orchard_vb, 250_000, "value LEFT Orchard");
    assert_eq!(
        evidence.ironwood_vb, 0,
        "no Ironwood bundle, and it does not matter"
    );
    assert!(evidence.class.treat_as_migration());
}

#[test]
fn zero_orchard_value_balance_with_orchard_actions_is_a_migration() {
    let evidence = classify_with_evidence(V6_ORCHARD_ZERO);
    println!("{evidence:?}");

    // THE GAP ZOOKO'S RULING CLOSES, pinned on committed bytes.
    //
    // A transaction must pay a fee, and unless it is paid from another pool the
    // fee comes out of Orchard, so most internal shuffling already showed
    // orchard_vb > 0. The one that slipped through the old exit predicate was
    // the shuffle whose fee is paid from a DIFFERENT pool: Orchard actions
    // present, legacy notes spent, nullifiers published, and orchard_vb == 0.
    // The shim used to hand that to the operator's indexer in the clear.
    assert_eq!(evidence.class, Class::Migration);
    assert_eq!(evidence.version, "V6");
    assert_eq!(evidence.orchard_vb, 0, "no value left the Orchard pool");
    assert_eq!(
        evidence.orchard_actions, 1,
        "and the actions divert it anyway"
    );
    assert!(evidence.class.treat_as_migration());
}

#[test]
fn an_ironwood_only_transaction_is_a_pass_through() {
    let evidence = classify_with_evidence(V6_IRONWOOD_ONLY);
    println!("{evidence:?}");

    // The boundary that keeps the rule from swallowing ordinary commerce. The
    // widening stops at Orchard on purpose: Orchard is the legacy, closing pool
    // whose users already expect slowness, while Ironwood is where ordinary
    // time-sensitive payments will live. No Orchard bundle, so it is forwarded.
    assert_eq!(evidence.class, Class::PassThrough);
    assert_eq!(evidence.version, "V6");
    assert_eq!(evidence.orchard_actions, 0, "no Orchard bundle at all");
    assert_eq!(evidence.orchard_vb, 0);
    assert_eq!(evidence.ironwood_vb, -240_000, "value entering Ironwood");
    assert!(!evidence.class.treat_as_migration());
}

#[test]
fn transparent_pre_v6_is_pass_through() {
    // Real mainnet bytes, and the other realistic pass-through: no Orchard
    // bundle, so the predicate does not fire. Nothing here depends on the
    // version being pre-V6; the absent version guard is not what makes this
    // pass, the absent Orchard bundle is.
    let bytes = hex::decode(V4_COINBASE_HEX).expect("fixture is valid hex");
    let evidence = classify_with_evidence(&bytes);
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::PassThrough);
    assert_eq!(evidence.version, "V4");
    assert_eq!(evidence.orchard_actions, 0);
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
        V6_ORCHARD_ZERO,
        V6_IRONWOOD_ONLY,
        &[][..],
        &[0xff; 32][..],
    ] {
        assert_eq!(classify(bytes), classify_with_evidence(bytes).class);
    }
}
