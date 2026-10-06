//! [zero] @claude Block scanning under wasm.
//!
//! `scan_cached_blocks` is the only caller of the batch trial-decryption runner, and so
//! the only place that runner's threading assumptions meet a single-threaded target.

#![cfg(all(target_family = "wasm", target_os = "unknown"))]

use rand_chacha::{rand_core::SeedableRng, ChaChaRng};
use sapling::{
    note_encryption::{sapling_note_encryption, SaplingDomain, COMPACT_NOTE_SIZE},
    util::generate_random_rseed,
    value::NoteValue,
};
use secrecy::SecretVec;
use wasm_bindgen_test::wasm_bindgen_test;
use zcash_client_backend::{
    data_api::{
        chain::{scan_cached_blocks, BlockCache, ChainState},
        AccountBirthday, WalletWrite,
    },
    proto::compact_formats::{ChainMetadata, CompactBlock, CompactSaplingOutput, CompactTx},
};
use zcash_client_sqlite::wallet::init::init_wallet_db;
use zcash_note_encryption::Domain;
use zcash_primitives::{block::BlockHash, transaction::components::sapling::zip212_enforcement};
use zcash_protocol::{
    consensus::{BlockHeight, Network},
    memo::MemoBytes,
};

use zero_wasm_smoke::{open_wallet, MemoryBlockCache};

/// A testnet height well past Sapling activation and the ZIP 212 grace period.
const SCAN_HEIGHT: u32 = 2_000_000;

/// The value of the note the test pays to the wallet, in zatoshis.
const NOTE_VALUE: u64 = 50_000;

/// A note paid to the wallet is found by `scan_cached_blocks`.
///
/// Before the batch runner ran its work inline on single-threaded wasm, this never
/// returned: the decryption batch was queued on a rayon pool with no thread to run it,
/// and the scan waited on its result indefinitely.
#[wasm_bindgen_test]
async fn scan_finds_a_received_note() {
    let network = Network::TestNetwork;
    let mut rng = ChaChaRng::seed_from_u64(0);

    let mut db = open_wallet();
    init_wallet_db(&mut db, None).expect("migrations apply");

    let prior = ChainState::empty(BlockHeight::from_u32(SCAN_HEIGHT - 1), BlockHash([0; 32]));
    let birthday = AccountBirthday::from_parts(prior.clone(), None);
    let (_, usk) = db
        .create_account("scan", &SecretVec::new(vec![1u8; 32]), &birthday, None)
        .expect("creates an account");
    let dfvk = usk.sapling().to_diversifiable_full_viewing_key();

    let height = BlockHeight::from_u32(SCAN_HEIGHT);
    let note = sapling::Note::from_parts(
        dfvk.default_address().1,
        NoteValue::from_raw(NOTE_VALUE),
        generate_random_rseed(zip212_enforcement(&network, height), &mut rng),
    );
    let encryptor = sapling_note_encryption(
        Some(dfvk.fvk().ovk),
        note.clone(),
        MemoBytes::empty().into_bytes(),
        &mut rng,
    );

    let tx = CompactTx {
        txid: vec![1; 32],
        outputs: vec![CompactSaplingOutput {
            cmu: note.cmu().to_bytes().to_vec(),
            ephemeral_key: SaplingDomain::epk_bytes(encryptor.epk()).0.to_vec(),
            ciphertext: encryptor.encrypt_note_plaintext().0[..COMPACT_NOTE_SIZE].to_vec(),
        }],
        ..Default::default()
    };
    let block = CompactBlock {
        height: SCAN_HEIGHT.into(),
        hash: vec![1; 32],
        prev_hash: prior.block_hash().0.to_vec(),
        vtx: vec![tx],
        chain_metadata: Some(ChainMetadata {
            sapling_commitment_tree_size: 1,
            ..Default::default()
        }),
        ..Default::default()
    };

    let cache = MemoryBlockCache::new();
    cache.insert(vec![block]).await.expect("caches the block");

    let summary =
        scan_cached_blocks(&network, &cache, &mut db, height, &prior, 1).expect("scan completes");
    assert_eq!(summary.received_sapling_note_count(), 1);
}
