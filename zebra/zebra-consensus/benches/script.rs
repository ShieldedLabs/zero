//! Paired script-cache measurements, driven by bench-script-cache.sh.
//!
//! BENCH_INPUTS selects the signed transaction's transparent input count.
//! After initialization, the program prints READY.
//! Each stdin line supplies a positive batch size.
//! Each response reports average verification times in nanoseconds:
//! cache miss and hit when cache_enabled; ordinary verification and zero otherwise.
//!
//! Fixture construction, verifier construction, and cache preparation are untimed.

use std::{
    collections::HashMap,
    hint::black_box,
    io::{self, BufRead, Write},
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
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

#[cfg(cache_enabled)]
use zebra_consensus::transaction::bench_support;

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

fn main() {
    debug_assert!(false, "benchmarks require debug assertions disabled");

    let (transaction, known_utxos) = consolidation();
    let request = block_request(&transaction, &known_utxos);

    #[cfg(cache_enabled)]
    let key = match transaction.unmined_id() {
        zebra_chain::transaction::UnminedTxId::Witnessed(key) => key,
        zebra_chain::transaction::UnminedTxId::Legacy(_) => {
            panic!("the fixture must be witnessed")
        }
    };

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let network = Network::new_default_testnet();

    let measure = || {
        let verifier = BlockTxVerifier::new(
            &network,
            service_fn(|_| async {
                unreachable!("all UTXOs come from known_utxos")
            }),
        );
        let request = request.clone();

        let start = Instant::now();
        black_box(
            rt.block_on(verifier.oneshot(request))
                .expect("transaction verifies"),
        );
        start.elapsed()
    };

    // Initialize the verifier's workers before reporting readiness.
    let _ = measure();
    println!("READY");
    io::stdout().flush().expect("flush readiness");

    for line in io::stdin().lock().lines() {
        let batch: usize = line
            .expect("read batch size")
            .parse()
            .expect("batch size must be an integer");
        assert!(batch > 0);

        let mut first = Duration::ZERO;

        #[cfg(cache_enabled)]
        let mut second = Duration::ZERO;

        for _ in 0..batch {
            #[cfg(cache_enabled)]
            {
                bench_support::forget(&key);
                assert!(!bench_support::contains(&key));
            }

            first += measure();

            #[cfg(cache_enabled)]
            {
                assert!(bench_support::contains(&key));
                second += measure();
            }
        }

        #[cfg(not(cache_enabled))]
        let second = Duration::ZERO;

        println!(
            "{} {}",
            first.as_nanos() as f64 / batch as f64,
            second.as_nanos() as f64 / batch as f64,
        );
        io::stdout().flush().expect("flush measurements");
    }
}
