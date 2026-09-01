//! fatcraft: craft and sign transparent-only v5 regtest transactions with
//! arbitrary input/output counts, using zebra-chain's own serialization and
//! sighash code so the bytes always match what zebrad expects.
//!
//! Reads one JSON job on stdin, writes one JSON result on stdout.
//!
//! Modes:
//!   address      derive the bench P2PKH address/script from privkey_hex
//!   fanout       spend `inputs` into `fanout_count` outputs of
//!                `fanout_value_zat` each plus a change output
//!   consolidate  spend all `inputs` into a single output (sum - fee)
//!
//! All inputs and outputs use the single bench P2PKH key. Regtest only.

use std::{error::Error, io::Read, sync::Arc};

use ripemd::Ripemd160;
use secp256k1::{Message, PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zebra_chain::{
    amount::{Amount, NonNegative},
    block::Height,
    parameters::{NetworkKind, NetworkUpgrade},
    serialization::ZcashSerialize,
    transaction::{HashType, LockTime, Transaction},
    transparent::{self, OutPoint, Script},
};

#[derive(Deserialize)]
struct Job {
    mode: String,
    privkey_hex: String,
    #[serde(default = "default_nu")]
    nu: String,
    #[serde(default)]
    inputs: Vec<JobInput>,
    #[serde(default)]
    fanout_count: u32,
    #[serde(default)]
    fanout_value_zat: i64,
    #[serde(default)]
    fee_zat: i64,
    #[serde(default)]
    expiry_height: u32,
}

fn default_nu() -> String {
    "Nu5".into()
}

#[derive(Deserialize)]
struct JobInput {
    txid: String,
    vout: u32,
    value_zat: i64,
}

#[derive(Serialize)]
struct OutRef {
    txid: String,
    vout: u32,
    value_zat: i64,
}

#[derive(Serialize, Default)]
struct Reply {
    address: String,
    pubkey_hex: String,
    lock_script_hex: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    txid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(default)]
    outputs: Vec<OutRef>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("fatcraft error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let job: Job = serde_json::from_str(&input)?;

    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(&hex::decode(&job.privkey_hex)?)?;
    let pk = PublicKey::from_secret_key(&secp, &sk);
    let pk_bytes = pk.serialize();

    let mut h160 = [0u8; 20];
    h160.copy_from_slice(&Ripemd160::digest(Sha256::digest(pk_bytes)));
    let addr = transparent::Address::from_pub_key_hash(NetworkKind::Regtest, h160);
    let lock_script = addr.script();

    let mut reply = Reply {
        address: addr.to_string(),
        pubkey_hex: hex::encode(pk_bytes),
        lock_script_hex: hex::encode(lock_script.as_raw_bytes()),
        ..Default::default()
    };

    if job.mode == "address" {
        println!("{}", serde_json::to_string(&reply)?);
        return Ok(());
    }

    let nu = match job.nu.as_str() {
        "Nu5" => NetworkUpgrade::Nu5,
        "Nu6" => NetworkUpgrade::Nu6,
        other => return Err(format!("unsupported nu: {other}").into()),
    };

    if job.inputs.is_empty() {
        return Err("no inputs".into());
    }
    let total_in: i64 = job.inputs.iter().map(|i| i.value_zat).sum();

    let amount = |v: i64| -> Result<Amount<NonNegative>, Box<dyn Error>> {
        Amount::try_from(v).map_err(|e| format!("bad amount {v}: {e:?}").into())
    };

    let mut outputs: Vec<transparent::Output> = Vec::new();
    match job.mode.as_str() {
        "fanout" => {
            let bench_total = job.fanout_count as i64 * job.fanout_value_zat;
            let change = total_in - bench_total - job.fee_zat;
            if change < 10_000 {
                return Err(format!(
                    "fanout underfunded: in={total_in} bench={bench_total} fee={} change={change}",
                    job.fee_zat
                )
                .into());
            }
            for _ in 0..job.fanout_count {
                outputs.push(transparent::Output {
                    value: amount(job.fanout_value_zat)?,
                    lock_script: lock_script.clone(),
                });
            }
            outputs.push(transparent::Output {
                value: amount(change)?,
                lock_script: lock_script.clone(),
            });
        }
        "consolidate" => {
            let out_value = total_in - job.fee_zat;
            if out_value < 10_000 {
                return Err(format!(
                    "consolidate underfunded: in={total_in} fee={}",
                    job.fee_zat
                )
                .into());
            }
            outputs.push(transparent::Output {
                value: amount(out_value)?,
                lock_script: lock_script.clone(),
            });
        }
        other => return Err(format!("unknown mode: {other}").into()),
    }

    // Unsigned inputs first: ZIP-244 sighashes do not cover scriptSigs, so we
    // can compute every sighash once and then splice the signatures in.
    let unsigned_inputs: Vec<transparent::Input> = job
        .inputs
        .iter()
        .map(|i| -> Result<transparent::Input, Box<dyn Error>> {
            Ok(transparent::Input::PrevOut {
                outpoint: OutPoint {
                    hash: i.txid.parse()?,
                    index: i.vout,
                },
                unlock_script: Script::new(&[]),
                sequence: 0xffff_ffff,
            })
        })
        .collect::<Result<_, _>>()?;

    let prev_outputs: Arc<Vec<transparent::Output>> = Arc::new(
        job.inputs
            .iter()
            .map(|i| {
                Ok(transparent::Output {
                    value: amount(i.value_zat)?,
                    lock_script: lock_script.clone(),
                })
            })
            .collect::<Result<_, Box<dyn Error>>>()?,
    );

    let mut tx = Transaction::V5 {
        network_upgrade: nu,
        lock_time: LockTime::unlocked(),
        expiry_height: Height(job.expiry_height),
        inputs: unsigned_inputs,
        outputs: outputs.clone(),
        sapling_shielded_data: None,
        orchard_shielded_data: None,
    };

    let script_code: Vec<u8> = lock_script.as_raw_bytes().to_vec();
    let sighasher = tx
        .sighasher(nu, prev_outputs.clone())
        .map_err(|e| format!("sighasher: {e:?}"))?;

    let mut first_sighash = None;
    let mut script_sigs: Vec<Vec<u8>> = Vec::with_capacity(job.inputs.len());
    for idx in 0..job.inputs.len() {
        let sighash = sighasher.sighash(HashType::ALL, Some((idx, script_code.clone())));
        if idx == 0 {
            first_sighash = Some(sighash.0);
        }
        let msg = Message::from_digest(sighash.0);
        let sig = secp.sign_ecdsa(&msg, &sk);
        let mut der = sig.serialize_der().to_vec();
        der.push(0x01); // SIGHASH_ALL

        let mut script_sig = Vec::with_capacity(der.len() + 35);
        script_sig.push(der.len() as u8);
        script_sig.extend_from_slice(&der);
        script_sig.push(33);
        script_sig.extend_from_slice(&pk_bytes);
        script_sigs.push(script_sig);
    }

    let signed_inputs: Vec<transparent::Input> = job
        .inputs
        .iter()
        .zip(script_sigs)
        .map(|(i, script_sig)| {
            Ok(transparent::Input::PrevOut {
                outpoint: OutPoint {
                    hash: i.txid.parse()?,
                    index: i.vout,
                },
                unlock_script: Script::new(&script_sig),
                sequence: 0xffff_ffff,
            })
        })
        .collect::<Result<_, Box<dyn Error>>>()?;

    if let Transaction::V5 { inputs, .. } = &mut tx {
        *inputs = signed_inputs;
    }

    // Guard the "sighash ignores scriptSigs" assumption: recompute input 0's
    // sighash on the signed transaction and require it unchanged.
    let check_hasher = tx
        .sighasher(nu, prev_outputs)
        .map_err(|e| format!("sighasher (signed): {e:?}"))?;
    let check = check_hasher.sighash(HashType::ALL, Some((0, script_code)));
    if Some(check.0) != first_sighash {
        return Err("sighash changed after signing; scriptSig is covered?".into());
    }

    let bytes = tx.zcash_serialize_to_vec()?;
    let txid = tx.hash().to_string();

    reply.outputs = match job.mode.as_str() {
        "fanout" => (0..job.fanout_count)
            .map(|vout| OutRef {
                txid: txid.clone(),
                vout,
                value_zat: job.fanout_value_zat,
            })
            .collect(),
        _ => vec![OutRef {
            txid: txid.clone(),
            vout: 0,
            value_zat: total_in - job.fee_zat,
        }],
    };
    reply.size = Some(bytes.len());
    reply.hex = Some(hex::encode(bytes));
    reply.txid = Some(txid);

    println!("{}", serde_json::to_string(&reply)?);
    Ok(())
}
