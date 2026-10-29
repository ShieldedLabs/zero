//! Builds the hub's proven fixtures: V6 migrations whose Halo2 proofs and
//! signatures verify under the post-NU6.3 key.
//!
//! The shim's fixtures carry zero-filled proofs, which is all a classifier
//! needs. Admission now verifies proofs, so the hub needs transactions that pass
//! it. Each one spends a fake Orchard note into Ironwood and one transparent
//! output, the shape of a TEX payment's first leg:
//!
//! * Orchard: one spend of 100 000 zat, against the anchor of a one-leaf tree
//!   the generator builds itself. The proof verifies; the anchor is on no
//!   chain, which is the node's to judge.
//! * Ironwood: one output of 50 000 zat.
//! * Transparent: one P2PKH output of 40 000 zat, so a second leg can spend
//!   `(txid, 0)`.
//! * No expiry, no transparent inputs, so the sighash needs no chain state.
//!
//! `v6_proven_a_resigned` carries the same proven bundles as `v6_proven_a` under
//! fresh signatures: different bytes, the same proofs. `v6_proven_a_bad_sig`
//! carries them signed over the wrong sighash. `v6_proven_b` is independent.
//!
//! To regenerate the committed fixtures:
//!
//! ```text
//! ZIH_WRITE_FIXTURES=1 cargo test --test proven_fixtures -- --ignored
//! ```

use std::io::Cursor;
use std::sync::Arc;

use incrementalmerkletree::{Hashable, Level};
use orchard::{
    builder::{Builder, BundleType},
    bundle::{BundleVersion, Flags},
    circuit::{OrchardCircuitVersion, ProvingKey},
    keys::{FullViewingKey, Scope, SpendAuthorizingKey, SpendingKey},
    note::ExtractedNoteCommitment,
    tree::{MerkleHashOrchard, MerklePath},
    value::NoteValue,
    Anchor, Bundle,
};
use rand08::rngs::OsRng;
use zcash_primitives::transaction::{Authorized, TransactionData};
use zcash_protocol::consensus::{BlockHeight, BranchId};
use zcash_protocol::value::{ZatBalance, Zatoshis};
use zebra_chain::serialization::ZcashDeserialize;
use zebra_chain::transaction::{HashType, Transaction};

type Signed = Bundle<orchard::bundle::Authorized, ZatBalance>;

/// The serialized V6 transaction around two signed bundles.
fn v6_bytes(orchard: Signed, ironwood: Signed) -> Vec<u8> {
    let mut p2pkh = vec![0x76, 0xa9, 0x14];
    p2pkh.extend_from_slice(&[9; 20]);
    p2pkh.extend_from_slice(&[0x88, 0xac]);
    let transparent = zcash_transparent::bundle::Bundle {
        vin: vec![],
        vout: vec![zcash_transparent::bundle::TxOut::new(
            Zatoshis::const_from_u64(40_000),
            zebra_chain::transparent::Script::new(&p2pkh).into(),
        )],
        authorization: zcash_transparent::bundle::Authorized,
    };
    let data = TransactionData::<Authorized>::from_parts_v6(
        BranchId::Nu6_3,
        0,
        BlockHeight::from_u32(0),
        Some(transparent),
        None,
        Some(orchard),
        Some(ironwood),
    );
    let mut bytes = Vec::new();
    data.freeze()
        .expect("a V6 transaction freezes")
        .write(&mut bytes)
        .expect("writing to a Vec cannot fail");
    bytes
}

/// The shielded sighash, as zebra-chain computes it for the hub.
fn sighash(bytes: &[u8]) -> [u8; 32] {
    let tx = Transaction::zcash_deserialize(&mut Cursor::new(bytes)).expect("zebra parses it");
    let nu = tx.network_upgrade().expect("V6 carries its branch id");
    tx.sighash(nu, HashType::ALL, Arc::new(Vec::new()), None)
        .expect("no transparent inputs, so no prevouts are needed")
        .0
}

/// One migration: `[well signed, re-signed, signed over the wrong sighash]`.
fn migration(pk: &ProvingKey, seed: u8) -> [Vec<u8>; 3] {
    let sk = SpendingKey::from_bytes([seed; 32]).expect("a valid spending key");
    let fvk = FullViewingKey::from(&sk);
    let recipient = fvk.address_at(0u32, Scope::External);
    let empty_tree: Anchor = MerkleHashOrchard::empty_root(Level::from(32)).into();

    // A shielding bundle exists only to mint a note to spend: its output,
    // decrypted, is the note. It needs no proof.
    let mut shielding = Builder::new(
        BundleType::DEFAULT,
        BundleVersion::orchard_v2(),
        Flags::SPENDS_DISABLED,
        empty_tree,
    )
    .expect("output-only flags suit the Orchard v2 pool");
    shielding
        .add_output(None, recipient, NoteValue::from_raw(100_000), [0; 512])
        .expect("outputs are enabled");
    let (shielding, meta) = shielding
        .build::<i64>(OsRng)
        .expect("the bundle builds")
        .expect("the bundle is non-empty");
    let (note, _, _) = shielding
        .decrypt_output_with_key(
            meta.output_action_index(0).expect("output 0 exists"),
            &fvk.to_ivk(Scope::External),
        )
        .expect("the recipient decrypts its own output");

    // The note at position 0 of an otherwise empty tree.
    let cmx: ExtractedNoteCommitment = note.commitment().into();
    let path = MerklePath::from_parts(
        0,
        std::array::from_fn(|level| {
            MerkleHashOrchard::empty_root(Level::from(u8::try_from(level).expect("depth 32")))
        }),
    );
    let anchor = path.root(cmx);

    let mut orchard = Builder::new(
        BundleType::DEFAULT,
        BundleVersion::orchard_v3(),
        BundleVersion::orchard_v3().default_flags(),
        anchor,
    )
    .expect("default flags suit the Orchard v3 pool");
    orchard
        .add_spend(fvk.clone(), note, path)
        .expect("the path matches the anchor");
    let (orchard, _) = orchard
        .build::<ZatBalance>(OsRng)
        .expect("the bundle builds")
        .expect("the bundle is non-empty");
    let orchard = orchard.create_proof(pk, OsRng).expect("the proof builds");

    let mut ironwood = Builder::new(
        BundleType::DEFAULT,
        BundleVersion::ironwood_v3(),
        BundleVersion::ironwood_v3().default_flags(),
        empty_tree,
    )
    .expect("default flags suit the Ironwood pool");
    ironwood
        .add_output(None, recipient, NoteValue::from_raw(50_000), [0; 512])
        .expect("outputs are enabled");
    let (ironwood, _) = ironwood
        .build::<ZatBalance>(OsRng)
        .expect("the bundle builds")
        .expect("the bundle is non-empty");
    let ironwood = ironwood.create_proof(pk, OsRng).expect("the proof builds");

    let keys = [SpendAuthorizingKey::from(&sk)];
    let sign = |sighash: [u8; 32]| {
        v6_bytes(
            orchard
                .clone()
                .apply_signatures(OsRng, sighash, &keys)
                .expect("the spend's key signs"),
            ironwood
                .clone()
                .apply_signatures(OsRng, sighash, &[])
                .expect("dummy spends sign themselves"),
        )
    };

    // The sighash excludes proofs and signatures (ZIP 244), so a copy signed
    // over a placeholder yields it.
    let real = sighash(&sign([0; 32]));
    let proven = sign(real);
    let resigned = sign(real);
    assert_eq!(sighash(&proven), real);
    assert_ne!(proven, resigned, "fresh signatures change the bytes");
    [proven, resigned, sign([0; 32])]
}

#[test]
#[ignore = "writes tests/fixtures/, run explicitly with ZIH_WRITE_FIXTURES=1"]
fn regenerate_fixtures() {
    assert!(
        std::env::var_os("ZIH_WRITE_FIXTURES").is_some(),
        "set ZIH_WRITE_FIXTURES=1 to confirm overwriting the committed fixtures"
    );
    let pk = ProvingKey::build(OrchardCircuitVersion::PostNu6_3);
    let [a, a_resigned, a_bad_sig] = migration(&pk, 7);
    let [b, _, _] = migration(&pk, 8);

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for (name, bytes) in [
        ("v6_proven_a", a),
        ("v6_proven_a_resigned", a_resigned),
        ("v6_proven_a_bad_sig", a_bad_sig),
        ("v6_proven_b", b),
    ] {
        std::fs::write(dir.join(format!("{name}.bin")), bytes).expect("fixture written");
    }
}
