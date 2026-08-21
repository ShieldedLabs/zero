//! Benchmarks of transparent script verification and the script cache.
//!
//! The workload is a 1001-input P2SH consolidation with a real ECDSA
//! signature on every input (P2SH-wrapped pay-to-public-key), so a cache
//! miss pays the interpreter, the ZIP-244 sighash, and the signature
//! verification for every input, and a hit skips all three.

// Disabled due to warnings in criterion macros
#![allow(missing_docs)]

use std::{collections::HashMap, hint::black_box, sync::Arc};

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
use zebra_script::CachedFfiTransaction;

const INPUTS: usize = 1001;
const INPUT_VALUE: i64 = 10_000;

fn testnet_nu5_height() -> Height {
    (NetworkUpgrade::Nu5
        .activation_height(&Network::new_default_testnet())
        .expect("NU5 activation height is specified")
        + 10)
        .expect("height in range")
}

/// Builds the consolidation transaction with a real ECDSA signature on every
/// input, its spent outputs in input order, and the `known_utxos` map serving
/// them to the block verifier.
fn consolidation(
    output_value: i64,
) -> (
    Arc<Transaction>,
    Vec<transparent::Output>,
    Arc<HashMap<transparent::OutPoint, transparent::OrderedUtxo>>,
) {
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
    let unsigned_inputs: Vec<transparent::Input> = (0..INPUTS)
        .map(|index| {
            let outpoint = transparent::OutPoint {
                hash: source_hash,
                // Bounded by INPUTS, so the cast cannot truncate.
                index: index as u32,
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
        value: Amount::try_from(output_value).expect("valid amount"),
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

    let spent_outputs = vec![spent_output; INPUTS];

    // The ZIP-244 signature digest excludes the unlock scripts, so the unsigned
    // transaction produces the same sighashes as the signed one.
    let sighasher = unsigned
        .sighasher(NetworkUpgrade::Nu5, Arc::new(spent_outputs.clone()))
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

    (transaction, spent_outputs, Arc::new(known_utxos))
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

/// The per-input script verification a cache hit skips.
fn script_verification(c: &mut Criterion) {
    let (transaction, spent_outputs, _) = consolidation(5_000);
    let cached = CachedFfiTransaction::new(
        transaction.clone(),
        Arc::new(spent_outputs),
        NetworkUpgrade::Nu5,
    )
    .expect("supported transaction version");

    c.bench_function("verify_1001_input_scripts", |b| {
        b.iter(|| {
            for input_index in 0..INPUTS {
                black_box(&cached)
                    .is_valid(input_index)
                    .expect("script is valid");
            }
        })
    });
}

/// Full block-path transaction verification, miss vs hit.
///
/// Every miss iteration verifies a distinct transaction (unique output value,
/// so a unique cache key); the hit series repeats one transaction after its
/// first verification populated the cache.
fn block_verification(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let network = Network::new_default_testnet();
    let state = || service_fn(|_| async { unreachable!("all UTXOs come from known_utxos") });

    let mut group = c.benchmark_group("block_verification_1001_inputs");
    group.sample_size(20);

    let mut next_value = 5_000;
    group.bench_function("cache_miss", |b| {
        b.iter_batched(
            || {
                next_value += 1;
                let (transaction, _, known_utxos) = consolidation(next_value);
                (
                    BlockTxVerifier::new(&network, state()),
                    block_request(&transaction, &known_utxos),
                )
            },
            |(verifier, request)| {
                rt.block_on(verifier.oneshot(request))
                    .expect("transaction verifies")
            },
            BatchSize::SmallInput,
        )
    });

    let (transaction, _, known_utxos) = consolidation(4_000);
    rt.block_on(
        BlockTxVerifier::new(&network, state()).oneshot(block_request(&transaction, &known_utxos)),
    )
    .expect("the populating verification succeeds");

    group.bench_function("cache_hit", |b| {
        b.iter_batched(
            || {
                (
                    BlockTxVerifier::new(&network, state()),
                    block_request(&transaction, &known_utxos),
                )
            },
            |(verifier, request)| {
                rt.block_on(verifier.oneshot(request))
                    .expect("transaction verifies")
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

criterion_group!(benches, script_verification, block_verification);
criterion_main!(benches);
