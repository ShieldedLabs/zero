//! Benchmarks the block-path spent-UTXO fetch under state-service latency.
//!
//! Every `AwaitUtxo` response is delayed ~1ms (the tokio timer resolution),
//! standing in for a state service under load. Run this bench on the base
//! commit for the serial baseline: one awaited round trip per input, against
//! this branch's overlapped lookups.

// Disabled due to warnings in criterion macros
#![allow(missing_docs)]

use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use tower::{service_fn, ServiceExt};

use zebra_chain::{
    amount::Amount,
    parameters::{Network, NetworkUpgrade},
    transaction::{LockTime, Transaction},
    transparent,
};
use zebra_consensus::transaction::{BlockRequest, BlockTxVerifier};

const INPUTS: usize = 1001;
const LOOKUP_LATENCY: Duration = Duration::from_millis(1);

/// A signature-free 1001-input standard P2SH consolidation whose spent UTXOs
/// all come from the state service.
fn consolidation() -> (Arc<Transaction>, transparent::Output) {
    let network = Network::new_default_testnet();
    let block_height = (NetworkUpgrade::Nu5
        .activation_height(&network)
        .expect("NU5 activation height is specified")
        + 10)
        .expect("height in range");

    const OP_TRUE: u8 = 0x51;
    let unlock_script = transparent::Script::new(&[0x01, OP_TRUE]);
    // OP_HASH160 <RIPEMD160(SHA256([OP_TRUE]))> OP_EQUAL
    let mut p2sh_lock_bytes = vec![0xa9, 0x14];
    p2sh_lock_bytes.extend_from_slice(&[
        0xda, 0x17, 0x45, 0xe9, 0xb5, 0x49, 0xbd, 0x0b, 0xfa, 0x1a, 0x56, 0x99, 0x71, 0xc7, 0x7e,
        0xba, 0x30, 0xcd, 0x5a, 0x4b,
    ]);
    p2sh_lock_bytes.push(0x87);
    let lock_script = transparent::Script::new(&p2sh_lock_bytes);

    let spent_output = transparent::Output {
        value: Amount::try_from(10_000).expect("valid amount"),
        lock_script,
    };

    let source_hash = zebra_chain::transaction::Hash([7u8; 32]);
    let inputs: Vec<transparent::Input> = (0..INPUTS)
        .map(|index| transparent::Input::PrevOut {
            outpoint: transparent::OutPoint {
                hash: source_hash,
                // Bounded by INPUTS, so the cast cannot truncate.
                index: index as u32,
            },
            unlock_script: unlock_script.clone(),
            sequence: 0,
        })
        .collect();

    let output = transparent::Output {
        value: Amount::try_from(5_000).expect("valid amount"),
        lock_script: transparent::Script::new(&[0]),
    };

    let transaction = Arc::new(Transaction::V5 {
        inputs,
        outputs: vec![output],
        lock_time: LockTime::unlocked(),
        expiry_height: (block_height + 1).expect("height in range"),
        sapling_shielded_data: None,
        orchard_shielded_data: None,
        network_upgrade: NetworkUpgrade::Nu5,
    });

    (transaction, spent_output)
}

fn utxo_fetch(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let network = Network::new_default_testnet();
    let (transaction, spent_output) = consolidation();

    let block_height = (NetworkUpgrade::Nu5
        .activation_height(&network)
        .expect("NU5 activation height is specified")
        + 10)
        .expect("height in range");
    let fund_height = (block_height - 1).expect("height in range");

    let state = move || {
        let spent_output = spent_output.clone();
        service_fn(move |request: zebra_state::Request| {
            let spent_output = spent_output.clone();
            async move {
                match request {
                    zebra_state::Request::AwaitUtxo(_) => {
                        tokio::time::sleep(LOOKUP_LATENCY).await;
                        Ok::<_, zebra_consensus::BoxError>(zebra_state::Response::Utxo(
                            transparent::Utxo::new(spent_output, fund_height, false),
                        ))
                    }
                    other => unreachable!("unexpected state request: {other:?}"),
                }
            }
        })
    };

    let mut group = c.benchmark_group("block_verification_1001_inputs");
    group.sample_size(10);

    group.bench_function("state_latency_1ms_per_utxo", |b| {
        b.iter_batched(
            || {
                (
                    BlockTxVerifier::new(&network, state()),
                    BlockRequest {
                        transaction_hash: transaction.hash(),
                        transaction: transaction.clone(),
                        known_utxos: Arc::new(HashMap::new()),
                        height: block_height,
                        time: DateTime::<Utc>::MAX_UTC,
                    },
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

criterion_group!(benches, utxo_fetch);
criterion_main!(benches);
