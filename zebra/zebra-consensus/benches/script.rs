//! Block-path transaction verification with transparent-script cache hits and misses.
//!
//! BENCH_INPUTS selects the input count (default: 1001).
//! Fixture construction and cache preparation are outside the measured intervals.
//! Benchmarks use in-memory UTXOs and identical per-iteration batching.
//! These measure isolated hits and misses, not eviction or contention.
//!
//! From the zebra workspace, save measurements before changing the implementation:
//!
//! ```sh
//! (for n in 1 1001; do BENCH_INPUTS=$n RAYON_NUM_THREADS=4 TOKIO_WORKER_THREADS=1 cargo bench -p zebra-consensus --features bench-internals --bench script -- --save-baseline before || exit $?; done)
//! ```
//!
//! After changing the implementation, compare using the same checkout's saved results:
//!
//! ```sh
//! (for n in 1 1001; do BENCH_INPUTS=$n RAYON_NUM_THREADS=4 TOKIO_WORKER_THREADS=1 cargo bench -p zebra-consensus --features bench-internals --bench script -- --baseline before || exit $?; done)
//! ```
//!
//! Defaults: 1-second warmup, 3-second measurement, 30 samples per benchmark.
//! Use the same machine, compiler, build settings, and worker counts.
//! Repeat comparisons before drawing conclusions about small differences.

// Disabled due to warnings in criterion macros
#![allow(missing_docs)]

use std::{collections::HashMap, hint::black_box, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use tower::{service_fn, ServiceExt};

use ripemd::Ripemd160;
use sha2::{Digest, Sha256};

use zebra_chain::{
    amount::Amount,
    block::Height,
    parameters::{Network, NetworkUpgrade},
    transaction::{HashType, LockTime, Transaction},
    transparent,
};
use zebra_consensus::transaction::{BlockRequest, BlockTxVerifier};

const INPUTS: usize = 1001;
const INPUT_VALUE: i64 = 10_000;

fn testnet_nu5_height() -> Height {
    (NetworkUpgrade::Nu5
        .activation_height(&Network::new_default_testnet())
        .expect("NU5 activation height is specified")
        + 10)
        .expect("height in range")
}

/// Builds a signed P2SH consolidation and its spent UTXOs.
fn consolidation() -> (
    Arc<Transaction>,
    Arc<HashMap<transparent::OutPoint, transparent::OrderedUtxo>>,
) {
    let input_count = std::env::var("BENCH_INPUTS")
        .map(|s| s.parse::<usize>().expect("BENCH_INPUTS must be an integer"))
        .unwrap_or(INPUTS);

    assert!(input_count > 0);

    let block_height = testnet_nu5_height();
    let fund_height = (block_height - 1).expect("height in range");

    let secp = secp256k1::Secp256k1::signing_only();
    let secret_key = secp256k1::SecretKey::from_slice(&[0x42; 32]).expect("valid secret key");
    let public_key = secret_key.public_key(&secp);

    // Redeem script: <33-byte pubkey> OP_CHECKSIG
    let mut redeem = vec![0x21];
    redeem.extend_from_slice(&public_key.serialize());
    redeem.push(0xac);

    // Lock script: OP_HASH160 <HASH160(redeem)> OP_EQUAL
    let redeem_hash = Ripemd160::digest(Sha256::digest(&redeem));
    let mut p2sh_lock_bytes = vec![0xa9, 0x14];
    p2sh_lock_bytes.extend_from_slice(&redeem_hash);
    p2sh_lock_bytes.push(0x87);
    let lock_script = transparent::Script::new(&p2sh_lock_bytes);

    let spent_output = transparent::Output {
        value: Amount::try_from(INPUT_VALUE).expect("valid amount"),
        lock_script,
    };

    let source_hash = zebra_chain::transaction::Hash([7u8; 32]);
    let mut known_utxos = HashMap::new();
    let unsigned_inputs: Vec<transparent::Input> = (0..input_count)
        .map(|index| {
            let outpoint = transparent::OutPoint {
                hash: source_hash,
                index: u32::try_from(index).expect("input index fits in u32"),
            };
            known_utxos.insert(
                outpoint,
                transparent::OrderedUtxo::new(spent_output.clone(), fund_height, index),
            );
            transparent::Input::PrevOut {
                outpoint,
                unlock_script: transparent::Script::new(&[]),
                sequence: 0,
            }
        })
        .collect();

    let output = transparent::Output {
        value: Amount::try_from(5_000).expect("valid amount"),
        lock_script: transparent::Script::new(&[0]),
    };

    let unsigned = Transaction::V5 {
        inputs: unsigned_inputs.clone(),
        outputs: vec![output],
        lock_time: LockTime::unlocked(),
        expiry_height: (block_height + 1).expect("height in range"),
        sapling_shielded_data: None,
        orchard_shielded_data: None,
        network_upgrade: NetworkUpgrade::Nu5,
    };

    let spent_outputs = vec![spent_output; input_count];

    // The ZIP-244 signature digest excludes the unlock scripts, so the unsigned
    // transaction produces the same sighashes as the signed one.
    let sighasher = unsigned
        .sighasher(NetworkUpgrade::Nu5, Arc::new(spent_outputs))
        .expect("supported transaction version");

    let inputs = unsigned_inputs
        .into_iter()
        .enumerate()
        .map(|(index, input)| {
            let sighash = sighasher.sighash(HashType::ALL, Some((index, redeem.clone())));
            let message = secp256k1::Message::from_digest(sighash.into());
            let mut sig_bytes = secp
                .sign_ecdsa(&message, &secret_key)
                .serialize_der()
                .to_vec();
            // The SIGHASH_ALL type byte.
            sig_bytes.push(1);

            // Unlock script: <sig> <redeem>. Both pushes are under 76 bytes,
            // so the push opcode is the bare length byte.
            let mut unlock = Vec::with_capacity(sig_bytes.len() + redeem.len() + 2);
            unlock.push(u8::try_from(sig_bytes.len()).expect("a DER signature fits one push byte"));
            unlock.extend_from_slice(&sig_bytes);
            unlock.push(u8::try_from(redeem.len()).expect("the redeem script fits one push byte"));
            unlock.extend_from_slice(&redeem);

            let transparent::Input::PrevOut {
                outpoint, sequence, ..
            } = input
            else {
                unreachable!("all inputs are PrevOut")
            };
            transparent::Input::PrevOut {
                outpoint,
                unlock_script: transparent::Script::new(&unlock),
                sequence,
            }
        })
        .collect();

    let Transaction::V5 {
        outputs,
        lock_time,
        expiry_height,
        sapling_shielded_data,
        orchard_shielded_data,
        network_upgrade,
        ..
    } = unsigned
    else {
        unreachable!("the transaction is V5 by construction")
    };
    let transaction = Arc::new(Transaction::V5 {
        inputs,
        outputs,
        lock_time,
        expiry_height,
        sapling_shielded_data,
        orchard_shielded_data,
        network_upgrade,
    });

    (transaction, Arc::new(known_utxos))
}

fn block_request(
    transaction: &Arc<Transaction>,
    known_utxos: &Arc<HashMap<transparent::OutPoint, transparent::OrderedUtxo>>,
) -> BlockRequest {
    BlockRequest {
        transaction_hash: transaction.hash(),
        transaction: transaction.clone(),
        known_utxos: known_utxos.clone(),
        height: testnet_nu5_height(),
        time: DateTime::<Utc>::MAX_UTC,
    }
}

fn benchmarks(c: &mut Criterion) {
    debug_assert!(false, "benchmarks require debug assertions disabled");

    let (transaction, known_utxos) = consolidation();
    let request = block_request(&transaction, &known_utxos);

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let network = Network::new_default_testnet();
    let make_verifier = || {
        BlockTxVerifier::new(
            &network,
            service_fn(|_| async {
                unreachable!("all UTXOs come from known_utxos")
            }),
        )
    };

    let mut group =
        c.benchmark_group(format!("script/{}inputs", transaction.inputs().len()));

    group.bench_function("block_path/cache_disabled", |b| {
        b.iter_batched(
            || (make_verifier(), request.clone()),
            |(verifier, request)| {
                black_box(
                    rt.block_on(verifier.oneshot(request))
                        .expect("transaction verifies"),
                );
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
        .sample_size(30);
    targets = benchmarks
}
criterion_main!(benches);
