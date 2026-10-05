//! [zero] @claude The wasm build with a rayon thread pool.
//!
//! Runs in a dedicated Worker: rayon's callers block while the pool works, and a browser's
//! main thread is not allowed to block.

#![cfg(all(target_family = "wasm", target_os = "unknown"))]

use std::{cell::Cell, collections::HashSet};

use orchard::{
    builder::{Builder, BundleType},
    bundle::{BundleVersion, Flags},
    circuit::{OrchardCircuitVersion, ProvingKey},
    keys::{FullViewingKey, Scope, SpendingKey},
    tree::Anchor,
    value::NoteValue as OrchardNoteValue,
};
use rand_chacha::{rand_core::SeedableRng, ChaChaRng};
use rayon::prelude::*;
use sapling::{
    note_encryption::{sapling_note_encryption, SaplingDomain},
    util::generate_random_rseed,
    value::NoteValue,
};
use secrecy::SecretVec;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::{console_log, wasm_bindgen_test, wasm_bindgen_test_configure};
use zcash_client_backend::{
    data_api::{
        chain::{scan_cached_blocks, BlockCache, ChainState},
        AccountBirthday, WalletWrite,
    },
    proto::compact_formats::{ChainMetadata, CompactBlock, CompactSaplingOutput, CompactTx},
};
use zcash_client_sqlite::wallet::init::init_wallet_db;
use zcash_note_encryption::{Domain, COMPACT_NOTE_SIZE};
use zcash_primitives::{block::BlockHash, transaction::components::sapling::zip212_enforcement};
use zcash_protocol::{
    consensus::{BlockHeight, Network},
    memo::MemoBytes,
};
use zero_wasm_smoke::{open_wallet, MemoryBlockCache};
use zero_wasm_threads::init_thread_pool;

wasm_bindgen_test_configure!(run_in_dedicated_worker);

/// A testnet height well past Sapling activation and the ZIP 212 grace period.
const SCAN_HEIGHT: u32 = 2_000_000;

/// The value of the note the scan test pays to the wallet, in zatoshis.
const NOTE_VALUE: u64 = 50_000;

/// The value of the Orchard output the proving test creates, in zatoshis.
const OUTPUT_VALUE: u64 = 100_000;

/// Enough parallel items for every worker to take some.
const PARALLEL_ITEMS: u64 = 10_000;

thread_local! {
    static POOL_THREADS: Cell<usize> = const { Cell::new(0) };
}

/// Starts the rayon pool once per module and returns its size: `ZERO_WASM_THREADS` at
/// build time if set, otherwise the machine's `hardwareConcurrency`.
async fn pool() -> usize {
    let started = POOL_THREADS.with(Cell::get);
    if started != 0 {
        return started;
    }
    let navigator = js_sys::Reflect::get(&js_sys::global(), &"navigator".into())
        .expect("a worker has a navigator");
    let threads = option_env!("ZERO_WASM_THREADS")
        .map(|n| n.parse().expect("ZERO_WASM_THREADS is a number"))
        .unwrap_or_else(|| {
            js_sys::Reflect::get(&navigator, &"hardwareConcurrency".into())
                .ok()
                .and_then(|n| n.as_f64())
                .map_or(1, |n| n as usize)
        })
        .max(2);
    JsFuture::from(init_thread_pool(threads))
        .await
        .expect("the thread pool starts");
    POOL_THREADS.with(|p| p.set(threads));
    threads
}

/// Times `f`, returning its result and the elapsed milliseconds.
fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let started = js_sys::Date::now();
    let value = f();
    (value, js_sys::Date::now() - started)
}

/// The pool starts, and parallel work actually lands on more than one worker.
#[wasm_bindgen_test]
async fn pool_runs_work_on_several_workers() {
    let threads = pool().await;
    assert_eq!(rayon::current_num_threads(), threads);

    let workers: HashSet<usize> = (0..PARALLEL_ITEMS)
        .into_par_iter()
        .filter_map(|i| {
            // Enough work per item that one worker cannot drain the queue alone.
            let mut x = i;
            for _ in 0..10_000 {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            }
            std::hint::black_box(x);
            rayon::current_thread_index()
        })
        .collect();
    console_log!("pool of {threads}; work ran on {} workers", workers.len());
    assert!(workers.len() > 1, "all work ran on one worker");
}

/// Orchard proving with the pool. Compare `zero-wasm-smoke`'s `tests/proving.rs` for the
/// single-threaded numbers on the same machine.
#[wasm_bindgen_test]
async fn orchard_proving_with_threads() {
    let threads = pool().await;
    let mut rng = ChaChaRng::seed_from_u64(0);
    let sk = SpendingKey::from_bytes([1u8; 32]).unwrap();
    let recipient = FullViewingKey::from(&sk).address_at(0u32, Scope::External);

    let (pk, pk_ms) = timed(|| ProvingKey::build(OrchardCircuitVersion::FixedPostNu6_2));

    let mut builder = Builder::new(
        BundleType::Transactional {
            bundle_required: false,
            pad_to_minimum: None,
        },
        BundleVersion::orchard_v2(),
        Flags::SPENDS_DISABLED,
        Anchor::empty_tree(),
    )
    .expect("the bundle type and flags are consistent");
    builder
        .add_output(
            None,
            recipient,
            OrchardNoteValue::from_raw(OUTPUT_VALUE),
            [0u8; 512],
        )
        .expect("outputs are enabled");
    let (unauthorized, _) = builder
        .build::<i64>(&mut rng)
        .expect("the bundle builds")
        .expect("the bundle is not empty");

    let (_proved, proof_ms) = timed(|| {
        unauthorized
            .create_proof(&pk, &mut rng)
            .expect("the proof is created")
    });
    console_log!(
        "{threads} threads: orchard ProvingKey::build {pk_ms:.0} ms, 2-action proof {proof_ms:.0} ms"
    );
}

/// A note paid to the wallet is found by `scan_cached_blocks` when the batch decryptor
/// runs on the pool, as it does on every threaded target.
#[wasm_bindgen_test]
async fn scan_finds_a_received_note_on_the_pool() {
    pool().await;
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
    let block = CompactBlock {
        height: SCAN_HEIGHT.into(),
        hash: vec![1; 32],
        prev_hash: prior.block_hash().0.to_vec(),
        vtx: vec![CompactTx {
            txid: vec![1; 32],
            outputs: vec![CompactSaplingOutput {
                cmu: note.cmu().to_bytes().to_vec(),
                ephemeral_key: SaplingDomain::epk_bytes(encryptor.epk()).0.to_vec(),
                ciphertext: encryptor.encrypt_note_plaintext()[..COMPACT_NOTE_SIZE].to_vec(),
            }],
            ..Default::default()
        }],
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
