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
        self,
        arbitrary::{fake_v6_orchard_shielded_data, fake_v6_transaction},
        LockTime, Transaction,
    },
    transparent,
};
use zero_indexer_shim::classify::{classify, classify_with_evidence, Class};

/// Real V6 wire bytes carrying an Orchard bundle with the given value balance,
/// and optionally an Ironwood bundle.
///
/// Sign convention, which the predicate no longer reads but the evidence still
/// reports: a POSITIVE balance is value LEAVING that pool, a NEGATIVE balance is
/// value ENTERING it.
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

    fake_v6_transaction(
        NetworkUpgrade::Nu6_3,
        Some(orchard),
        ironwood_zats.map(ironwood_bundle),
    )
    .zcash_serialize_to_vec()
    .expect("v6 transaction serializes")
}

/// Real V6 wire bytes with an Ironwood bundle and NO Orchard bundle at all.
///
/// This is the boundary the rule must not cross. Ironwood is the new pool where
/// ordinary time-sensitive commerce lives, so a transaction that touches only
/// Ironwood has to keep passing through.
fn v6_ironwood_only_bytes(ironwood_zats: i64) -> Vec<u8> {
    fake_v6_transaction(
        NetworkUpgrade::Nu6_3,
        None,
        Some(ironwood_bundle(ironwood_zats)),
    )
    .zcash_serialize_to_vec()
    .expect("v6 transaction serializes")
}

/// One Ironwood bundle with the given value balance. Ironwood reuses the Orchard
/// bundle shape, so it is built from the same helper.
fn ironwood_bundle(ironwood_zats: i64) -> ironwood::ShieldedData {
    let vb: Amount<NegativeAllowed> = ironwood_zats.try_into().expect("valid amount");

    ironwood::ShieldedData::new(ShieldedDataV6::new(fake_v6_orchard_shielded_data(
        Flags::ENABLE_SPENDS | Flags::ENABLE_OUTPUTS,
        vb,
        1,
    )))
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

/// A transparent input spending output 0 of another transaction: how the second
/// leg of a TEX payment spends the first leg's single-use output.
fn prevout() -> transparent::Input {
    transparent::Input::PrevOut {
        outpoint: transparent::OutPoint {
            hash: transaction::Hash::from([7; 32]),
            index: 0,
        },
        unlock_script: transparent::Script::new(&[0x51]),
        sequence: u32::MAX,
    }
}

/// A P2PKH output, which is what a TEX address pays on chain.
fn p2pkh_output() -> transparent::Output {
    let mut script = vec![0x76, 0xa9, 0x14];
    script.extend_from_slice(&[9; 20]);
    script.extend_from_slice(&[0x88, 0xac]);
    transparent::Output {
        value: 100_000.try_into().expect("valid amount"),
        lock_script: transparent::Script::new(&script),
    }
}

/// Real V6 wire bytes with one P2PKH output, the given transparent inputs, and
/// optionally an Ironwood bundle. No Orchard or Sapling bundle.
fn v6_transparent_bytes(inputs: Vec<transparent::Input>, ironwood_zats: Option<i64>) -> Vec<u8> {
    Transaction::V6 {
        network_upgrade: NetworkUpgrade::Nu6_3,
        lock_time: LockTime::unlocked(),
        expiry_height: block::Height(0),
        inputs,
        outputs: vec![p2pkh_output()],
        sapling_shielded_data: None,
        orchard_shielded_data: None,
        ironwood_shielded_data: ironwood_zats.map(ironwood_bundle),
    }
    .zcash_serialize_to_vec()
    .expect("v6 transaction serializes")
}

/// Real V4 wire bytes spending one prevout to one P2PKH output.
fn v4_transparent_bytes() -> Vec<u8> {
    Transaction::V4 {
        inputs: vec![prevout()],
        outputs: vec![p2pkh_output()],
        lock_time: LockTime::unlocked(),
        expiry_height: block::Height(0),
        joinsplit_data: None,
        sapling_shielded_data: None,
    }
    .zcash_serialize_to_vec()
    .expect("v4 transaction serializes")
}

#[test]
fn generated_orchard_actions_into_ironwood_are_a_migration() {
    let bytes = v6_bytes(250_000, Some(-240_000));
    let evidence = classify_with_evidence(&bytes);
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::Migration);
    assert_eq!(evidence.version, "V6");
    // The deciding fact.
    assert_eq!(evidence.orchard_actions, 1);
    // Evidence, reported and not read by the predicate.
    assert_eq!(evidence.orchard_vb, 250_000);
    assert_eq!(evidence.ironwood_vb, -240_000);
}

#[test]
fn every_orchard_touching_transaction_is_a_migration_whatever_the_destination() {
    // The destination pool does not gate the verdict. Each of these carries
    // Orchard actions, which is the identifying event on a pool NU6.3 closed to
    // new value, so each is diverted.

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
    // There is no version guard, and this is why it can be absent.
    // zebra-chain's orchard_shielded_data() is version-agnostic, so a
    // pre-Ironwood V5 Orchard spend is caught by exactly the same line.
    let evidence = classify_with_evidence(&v5_orchard_bytes(250_000));
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::Migration);
    assert_eq!(evidence.version, "V5");
    assert_eq!(evidence.orchard_actions, 1);
    assert_eq!(evidence.ironwood_vb, 0, "a V5 has no Ironwood bundle");
}

#[test]
fn the_direction_of_the_orchard_balance_does_not_change_the_verdict() {
    // Value ENTERING Orchard is consensus-invalid post-NU6.3 and cannot appear
    // on chain. It is generated here as a probe that the predicate has no
    // directionality left in it at all: same Orchard actions, opposite sign,
    // same verdict. Under the old exit predicate all three of these passed
    // through.
    assert_eq!(
        classify(&v6_bytes(-250_000, Some(240_000))),
        Class::Migration
    );
    assert_eq!(
        classify(&v6_bytes(-250_000, Some(-240_000))),
        Class::Migration
    );
    assert_eq!(classify(&v5_orchard_bytes(-250_000)), Class::Migration);
}

#[test]
fn the_magnitude_of_the_orchard_balance_does_not_change_the_verdict() {
    // Not sign-based and not magnitude-based: a one-zatoshi movement in a
    // bundle is as identifying as a large one, because what identifies is the
    // bundle.
    assert_eq!(classify(&v6_bytes(1, Some(-1))), Class::Migration);
    assert_eq!(classify(&v6_bytes(1, None)), Class::Migration);
}

#[test]
fn zero_orchard_value_balance_with_orchard_actions_is_a_migration() {
    // THE GAP ZOOKO'S RULING CLOSES, stated as a test.
    //
    // A transaction must pay a fee, and unless it is paid from another pool the
    // fee comes out of Orchard, so most internal shuffling already showed
    // orchard_vb > 0 and the old exit predicate caught it. The one that got
    // through was the shuffle whose fee is paid from a DIFFERENT pool: Orchard
    // actions present, legacy notes spent, nullifiers published, and
    // orchard_vb == 0. It used to be handed to the operator's indexer in the
    // clear. Now the actions alone divert it.
    let evidence = classify_with_evidence(&v6_bytes(0, Some(-240_000)));
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::Migration);
    assert_eq!(evidence.orchard_vb, 0, "no value left the Orchard pool");
    assert_eq!(evidence.orchard_actions, 1, "and it is diverted anyway");

    assert_eq!(classify(&v6_bytes(0, None)), Class::Migration);
    assert_eq!(classify(&v5_orchard_bytes(0)), Class::Migration);
}

#[test]
fn an_ironwood_only_transaction_is_a_pass_through() {
    // The boundary that keeps the rule from swallowing ordinary commerce. Value
    // ENTERS Ironwood (a shield into the new pool) and there is no Orchard
    // bundle, so nothing about legacy Orchard holdings is on the wire and the
    // transaction is forwarded. Zooko's time-insensitivity rationale is what
    // stops here: Ironwood is where time-sensitive payments will live.
    let evidence = classify_with_evidence(&v6_ironwood_only_bytes(-240_000));
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::PassThrough);
    assert_eq!(evidence.version, "V6");
    assert_eq!(evidence.orchard_actions, 0, "no Orchard bundle at all");
    assert_eq!(evidence.orchard_vb, 0);
    assert_eq!(evidence.ironwood_vb, -240_000, "value entering Ironwood");
    assert!(!evidence.class.treat_as_migration());

    // Whichever way the Ironwood value points. Ironwood is not the rule.
    assert_eq!(
        classify(&v6_ironwood_only_bytes(240_000)),
        Class::PassThrough
    );
}

#[test]
fn the_second_leg_of_a_tex_payment_is_a_migration() {
    // ZIP 320: the second leg spends the first leg's single-use output and has
    // transparent parts only. Its prevout names the first leg, so forwarding it
    // names a diverted transaction to the operator.
    let evidence = classify_with_evidence(&v6_transparent_bytes(vec![prevout()], None));
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::Migration);
    assert!(evidence.transparent_only, "the deciding fact for this arm");
    assert_eq!(evidence.orchard_actions, 0, "no Orchard bundle at all");
    assert_eq!(evidence.inputs, 1);

    // The shape decides, not the version.
    let v4 = classify_with_evidence(&v4_transparent_bytes());
    assert_eq!(v4.class, Class::Migration);
    assert_eq!(v4.version, "V4");
    assert!(v4.transparent_only);
}

#[test]
fn a_transparent_spend_with_a_shielded_bundle_is_a_pass_through() {
    // Auto-shielding into Ironwood: a transparent input and value entering
    // Ironwood. Ironwood keeps its boundary.
    let evidence = classify_with_evidence(&v6_transparent_bytes(vec![prevout()], Some(-90_000)));
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::PassThrough);
    assert!(!evidence.transparent_only);
}

#[test]
fn an_ironwood_funded_first_leg_is_a_pass_through() {
    // The first leg of a TEX payment funded from Ironwood: an Ironwood spend
    // into a transparent output, with no transparent input to name a parent.
    let evidence = classify_with_evidence(&v6_transparent_bytes(Vec::new(), Some(110_000)));
    println!("{evidence:?}");

    assert_eq!(evidence.class, Class::PassThrough);
    assert!(!evidence.transparent_only);
    assert_eq!(evidence.inputs, 0);
}

#[test]
fn orchard_actions_with_a_transparent_input_take_the_orchard_arm() {
    let mut tx = fake_v6_transaction(
        NetworkUpgrade::Nu6_3,
        Some(ShieldedDataV6::new(fake_v6_orchard_shielded_data(
            Flags::ENABLE_SPENDS | Flags::ENABLE_OUTPUTS,
            Amount::<NegativeAllowed>::try_from(250_000).expect("valid amount"),
            1,
        ))),
        None,
    );
    if let Transaction::V6 { inputs, .. } = &mut tx {
        inputs.push(prevout());
    }
    let evidence = classify_with_evidence(
        &tx.zcash_serialize_to_vec()
            .expect("v6 transaction serializes"),
    );

    assert_eq!(evidence.class, Class::Migration);
    assert_eq!(evidence.orchard_actions, 1);
    assert!(
        !evidence.transparent_only,
        "an Orchard bundle is not transparent"
    );
}

#[test]
fn generated_bytes_survive_the_full_consumption_check() {
    // A freshly serialized transaction must consume its bytes exactly, or the
    // classifier's trailing-bytes guard would reject every real transaction.
    let bytes = v6_bytes(250_000, Some(-240_000));
    assert!(classify_with_evidence(&bytes).error.is_none());

    let ironwood_only = v6_ironwood_only_bytes(-240_000);
    assert!(classify_with_evidence(&ironwood_only).error.is_none());

    let transparent_only = v6_transparent_bytes(vec![prevout()], None);
    assert!(classify_with_evidence(&transparent_only).error.is_none());
    assert!(classify_with_evidence(&v4_transparent_bytes())
        .error
        .is_none());
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
        ("v6_orchard_zero", v6_bytes(0, Some(-240_000))),
        ("v6_ironwood_only", v6_ironwood_only_bytes(-240_000)),
        (
            "v6_transparent_only",
            v6_transparent_bytes(vec![prevout()], None),
        ),
    ] {
        std::fs::write(dir.join(format!("{name}.bin")), &bytes).expect("fixture written");
        println!("{name}: {} bytes", bytes.len());
    }
}
