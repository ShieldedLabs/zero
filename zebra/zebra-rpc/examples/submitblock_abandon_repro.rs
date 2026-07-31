//! Local reproduction of the `submitblock` disconnect-cancel block loss.
//!
//! Recreates the incident shape at laptop scale: a block containing fat consolidation
//! transactions that each spend many transparent inputs, so block verification takes long
//! enough for a client timeout to land in the middle of it.
//!
//! The node is driven over raw TCP so the client connection can be killed at a chosen moment
//! during `submitblock`. Two equivalent blocks are built:
//!
//! 1. a calibration block, submitted with the connection held open, to measure how long
//!    verification of this block shape actually takes, and
//! 2. a test block, submitted first with the connection abandoned well inside that window, and
//!    then — the exact same bytes — with the connection held open.
//!
//! If the abandoned submissions do not commit the block but the held-open submission does, the
//! block was valid the whole time and the disconnect is what discarded it.
//!
//! Run with no arguments to print the miner address to configure zebrad with:
//!     cargo run -p zebra-rpc --example submitblock_abandon_repro
//! Run with the RPC address to perform the reproduction:
//!     cargo run -p zebra-rpc --example submitblock_abandon_repro -- 127.0.0.1:18991

// This example reports its results on stdout, which is the whole point of running it.
#![allow(clippy::print_stdout)]

use std::{env, error::Error, net::SocketAddr, sync::Arc, time::Duration, time::Instant};

use ripemd::Ripemd160;
use secp256k1::{Message, PublicKey, Secp256k1, SecretKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use zebra_chain::{
    amount::Amount,
    block::{Block, ChainHistoryBlockTxAuthCommitmentHash, Height},
    parameters::{testnet::ConfiguredActivationHeights, Network, NetworkKind, NetworkUpgrade},
    serialization::{ZcashDeserializeInto, ZcashSerialize},
    transaction::{HashType, LockTime, Transaction},
    transparent::{self, Script},
};
use zebra_rpc::{
    client::{BlockTemplateResponse, BlockTemplateTimeSource},
    proposal_block_from_template,
};

type BoxError = Box<dyn Error + Send + Sync>;

/// Early coinbase outputs spent to fund the fan-out transaction.
///
/// Regtest halves the block subsidy every 144 blocks, so only early coinbases hold
/// meaningful value.
const FANOUT_SOURCES: u32 = 40;

/// Transparent inputs per fat consolidation transaction.
///
/// The incident transactions had 1001 inputs each.
const INPUTS_PER_TX: usize = 500;

/// Fat consolidation transactions per test block.
const TXS_PER_BLOCK: usize = 4;

/// Coinbase outputs cannot be spent until this many blocks later.
const COINBASE_MATURITY: u32 = 100;

/// Fee paid by each crafted transaction.
const FEE_ZATS: i64 = 5_000_000;

/// Fixed test-only spending key, so runs are reproducible.
const SECRET_KEY: [u8; 32] = [0x42; 32];

/// How long to wait for an abandoned block to show up before declaring it lost.
const GRACE: Duration = Duration::from_secs(5);

/// An output this program can spend.
#[derive(Clone)]
struct Utxo {
    outpoint: transparent::OutPoint,
    output: transparent::Output,
}

fn request_bytes(addr: SocketAddr, body: &str) -> String {
    format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn rpc_body(method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string()
}

/// Makes a JSON-RPC call, holding the connection open until the response arrives.
async fn rpc(addr: SocketAddr, method: &str, params: Value) -> Result<Value, BoxError> {
    let body = rpc_body(method, params);

    let mut stream = TcpStream::connect(addr).await?;
    stream
        .write_all(request_bytes(addr, &body).as_bytes())
        .await?;

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;

    let text = String::from_utf8_lossy(&buf).into_owned();
    let body_start = text.find("\r\n\r\n").ok_or("response has no body")?;
    let response: Value = serde_json::from_str(&text[body_start + 4..])
        .map_err(|err| format!("{method}: bad JSON response: {err}"))?;

    match response.get("error") {
        Some(error) if !error.is_null() => Err(format!("{method} failed: {error}").into()),
        _ => Ok(response["result"].clone()),
    }
}

/// Sends a JSON-RPC call, then kills the connection `delay` later without reading the response.
///
/// `SO_LINGER = 0` makes the close send a RST, so the server sees the connection die
/// immediately rather than waiting on a half-closed socket.
async fn rpc_abandon(
    addr: SocketAddr,
    method: &str,
    params: Value,
    delay: Duration,
) -> Result<(), BoxError> {
    let body = rpc_body(method, params);

    let mut stream = TcpStream::connect(addr).await?;
    #[allow(deprecated)]
    stream.set_linger(Some(Duration::ZERO))?;
    stream
        .write_all(request_bytes(addr, &body).as_bytes())
        .await?;
    stream.flush().await?;

    tokio::time::sleep(delay).await;
    drop(stream);

    Ok(())
}

async fn tip_height(addr: SocketAddr) -> Result<u32, BoxError> {
    let info = rpc(addr, "getblockchaininfo", json!([])).await?;

    info["blocks"]
        .as_u64()
        .map(|height| height as u32)
        .ok_or_else(|| "getblockchaininfo has no block height".into())
}

async fn wait_for_rpc(addr: SocketAddr) -> Result<(), BoxError> {
    for _ in 0..60 {
        if tip_height(addr).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    Err("zebrad RPC did not come up".into())
}

async fn get_block(addr: SocketAddr, height: u32) -> Result<Block, BoxError> {
    let block = rpc(addr, "getblock", json!([height.to_string(), 0])).await?;
    let block = block.as_str().ok_or("getblock did not return hex")?;

    Ok(hex::decode(block)?.zcash_deserialize_into::<Block>()?)
}

/// Derives the P2PKH spending key, public key, and address used for mining and spending.
fn miner_keys() -> Result<(SecretKey, [u8; 33], transparent::Address), BoxError> {
    let secp = Secp256k1::new();
    let secret_key = SecretKey::from_slice(&SECRET_KEY)?;
    let public_key = PublicKey::from_secret_key(&secp, &secret_key).serialize();

    let pub_key_hash: [u8; 20] = Ripemd160::digest(Sha256::digest(public_key)).into();
    let address = transparent::Address::from_pub_key_hash(NetworkKind::Testnet, pub_key_hash);

    Ok((secret_key, public_key, address))
}

/// Collects coinbase outputs paying to `lock_script` from the first `count` blocks.
async fn collect_coinbase_utxos(
    addr: SocketAddr,
    lock_script: &Script,
    count: u32,
) -> Result<Vec<Utxo>, BoxError> {
    let mut utxos = Vec::new();

    for height in 1..=count {
        let block = get_block(addr, height).await?;
        let coinbase = block.transactions.first().ok_or("block has no coinbase")?;
        let hash = coinbase.hash();

        for (index, output) in coinbase.outputs().iter().enumerate() {
            if &output.lock_script == lock_script && i64::from(output.value) > 0 {
                utxos.push(Utxo {
                    outpoint: transparent::OutPoint {
                        hash,
                        index: index as u32,
                    },
                    output: output.clone(),
                });
                break;
            }
        }
    }

    Ok(utxos)
}

/// Builds and signs a transaction spending every UTXO in `utxos` into `num_outputs` equal outputs.
fn build_transaction(
    utxos: &[Utxo],
    num_outputs: usize,
    secret_key: &SecretKey,
    public_key: &[u8; 33],
    lock_script: &Script,
) -> Result<Transaction, BoxError> {
    let total: i64 = utxos.iter().map(|utxo| i64::from(utxo.output.value)).sum();
    let per_output = (total - FEE_ZATS) / num_outputs as i64;
    if per_output <= 0 {
        return Err("inputs do not cover the fee".into());
    }

    let outputs: Vec<_> = (0..num_outputs)
        .map(|_| {
            Ok(transparent::Output {
                value: Amount::try_from(per_output)?,
                lock_script: lock_script.clone(),
            })
        })
        .collect::<Result<_, BoxError>>()?;

    let unsigned_inputs = utxos
        .iter()
        .map(|utxo| transparent::Input::PrevOut {
            outpoint: utxo.outpoint,
            unlock_script: Script::new(&[]),
            sequence: u32::MAX,
        })
        .collect();

    let unsigned = Transaction::V5 {
        network_upgrade: NetworkUpgrade::Nu5,
        lock_time: LockTime::unlocked(),
        expiry_height: Height(0),
        inputs: unsigned_inputs,
        outputs: outputs.clone(),
        sapling_shielded_data: None,
        orchard_shielded_data: None,
    };

    // ZIP-244 transparent signature digests do not commit to any input's unlock script, so
    // every input can be signed against the unsigned transaction.
    let all_previous_outputs: Arc<Vec<transparent::Output>> =
        Arc::new(utxos.iter().map(|utxo| utxo.output.clone()).collect());
    let sighasher = unsigned.sighasher(NetworkUpgrade::Nu5, all_previous_outputs)?;

    let secp = Secp256k1::new();
    let mut inputs = Vec::with_capacity(utxos.len());

    for (index, utxo) in utxos.iter().enumerate() {
        let script_code = utxo.output.lock_script.as_raw_bytes().to_vec();
        let sighash = sighasher.sighash(HashType::ALL, Some((index, script_code)));
        let signature = secp.sign_ecdsa(&Message::from_digest(sighash.0), secret_key);

        // scriptSig: <DER signature ‖ SIGHASH_ALL> <compressed public key>
        let mut signature = signature.serialize_der().to_vec();
        signature.push(0x01);

        let mut unlock_script = Vec::with_capacity(signature.len() + public_key.len() + 2);
        unlock_script.push(signature.len() as u8);
        unlock_script.extend_from_slice(&signature);
        unlock_script.push(public_key.len() as u8);
        unlock_script.extend_from_slice(public_key);

        inputs.push(transparent::Input::PrevOut {
            outpoint: utxo.outpoint,
            unlock_script: Script::new(&unlock_script),
            sequence: u32::MAX,
        });
    }

    Ok(Transaction::V5 {
        network_upgrade: NetworkUpgrade::Nu5,
        lock_time: LockTime::unlocked(),
        expiry_height: Height(0),
        inputs,
        outputs,
        sapling_shielded_data: None,
        orchard_shielded_data: None,
    })
}

/// Returns the spendable outputs of `transaction` paying to `lock_script`.
fn outputs_of(transaction: &Transaction, lock_script: &Script) -> Vec<Utxo> {
    let hash = transaction.hash();

    transaction
        .outputs()
        .iter()
        .enumerate()
        .filter(|(_, output)| &output.lock_script == lock_script)
        .map(|(index, output)| Utxo {
            outpoint: transparent::OutPoint {
                hash,
                index: index as u32,
            },
            output: output.clone(),
        })
        .collect()
}

/// Builds a block extending the current tip that contains `extra_txs` after the template's
/// coinbase, recomputing the roots the header commits to.
async fn assemble_block(
    addr: SocketAddr,
    network: &Network,
    extra_txs: Vec<Arc<Transaction>>,
) -> Result<Block, BoxError> {
    let template = rpc(addr, "getblocktemplate", json!([])).await?;
    let template: BlockTemplateResponse = serde_json::from_value(template)?;
    let history_root = template.default_roots().chain_history_root();

    let mut block =
        proposal_block_from_template(&template, BlockTemplateTimeSource::CurTime, network)?;

    // Keep only the coinbase: the template may already carry mempool transactions, and this
    // function is for blocks whose contents are chosen by the caller.
    block.transactions.truncate(1);
    block.transactions.extend(extra_txs);

    let mut header = *block.header;
    header.merkle_root = block.transactions.iter().collect();
    header.commitment_bytes =
        <[u8; 32]>::from(ChainHistoryBlockTxAuthCommitmentHash::from_commitments(
            &history_root,
            &block.auth_data_root(),
        ))
        .into();
    block.header = Arc::new(header);

    Ok(block)
}

/// Submits a block with the connection held open, returning the response and how long it took.
async fn submit_held_open(addr: SocketAddr, block: &Block) -> Result<(Value, Duration), BoxError> {
    let block_hex = hex::encode(block.zcash_serialize_to_vec()?);

    let start = Instant::now();
    let response = rpc(addr, "submitblock", json!([block_hex])).await?;

    Ok((response, start.elapsed()))
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let (secret_key, public_key, address) = miner_keys()?;
    let lock_script = address.script();

    let Some(addr) = env::args().nth(1) else {
        println!("miner_address = '{address}'");
        return Ok(());
    };
    let addr: SocketAddr = addr.parse()?;

    // Must match the node's configured Regtest activation heights, because the block
    // commitment field depends on the network upgrade active at the block's height.
    let network = Network::new_regtest(
        ConfiguredActivationHeights {
            nu5: Some(1),
            ..Default::default()
        }
        .into(),
    );

    wait_for_rpc(addr).await?;

    // Mine enough blocks that the funding coinbase outputs are mature.
    let needed = FANOUT_SOURCES + COINBASE_MATURITY + 1;
    let start = tip_height(addr).await?;
    if start < needed {
        println!("mining {} blocks...", needed - start);
        rpc(addr, "generate", json!([needed - start])).await?;
    }
    println!("chain built, tip height {}", tip_height(addr).await?);

    // Fan out the early coinbase value into enough outputs to feed every fat transaction.
    let fanout_outputs = INPUTS_PER_TX * TXS_PER_BLOCK * 5;
    let funding = collect_coinbase_utxos(addr, &lock_script, FANOUT_SOURCES).await?;
    if funding.is_empty() {
        return Err(format!(
            "no spendable coinbase outputs; is zebrad's miner_address set to {address}?"
        )
        .into());
    }

    println!("building fan-out transaction with {fanout_outputs} outputs...");
    let fanout = Arc::new(build_transaction(
        &funding,
        fanout_outputs,
        &secret_key,
        &public_key,
        &lock_script,
    )?);
    let fanout_utxos = outputs_of(&fanout, &lock_script);

    let fanout_block = assemble_block(addr, &network, vec![fanout.clone()]).await?;
    let (response, elapsed) = submit_held_open(addr, &fanout_block).await?;
    if !response.is_null() {
        return Err(format!("fan-out block rejected: {response}").into());
    }
    println!(
        "fan-out block committed in {}ms, tip {}\n",
        elapsed.as_millis(),
        tip_height(addr).await?
    );

    // Build two equivalent fat blocks from disjoint halves of the fan-out outputs.
    let per_block = INPUTS_PER_TX * TXS_PER_BLOCK;
    let mut fat_txs = Vec::new();
    for chunk in fanout_utxos.chunks(INPUTS_PER_TX).take(TXS_PER_BLOCK * 5) {
        fat_txs.push(Arc::new(build_transaction(
            chunk,
            1,
            &secret_key,
            &public_key,
            &lock_script,
        )?));
    }
    let calibration_txs = &fat_txs[..TXS_PER_BLOCK];
    let test_txs = &fat_txs[TXS_PER_BLOCK..TXS_PER_BLOCK * 2];
    let warm_txs = &fat_txs[TXS_PER_BLOCK * 2..TXS_PER_BLOCK * 3];
    let cold_txs = &fat_txs[TXS_PER_BLOCK * 3..TXS_PER_BLOCK * 4];
    let concurrent_txs = &fat_txs[TXS_PER_BLOCK * 4..];
    println!(
        "built {} consolidation transactions of {INPUTS_PER_TX} inputs each ({per_block} inputs per block)",
        fat_txs.len()
    );

    // Calibration: how long does verifying this block shape actually take?
    let calibration_block = assemble_block(addr, &network, calibration_txs.to_vec()).await?;
    let (response, verify_time) = submit_held_open(addr, &calibration_block).await?;
    if !response.is_null() {
        return Err(format!("calibration block rejected: {response}").into());
    }
    println!(
        "calibration block ({} bytes) verified and committed in {}ms\n",
        calibration_block.zcash_serialize_to_vec()?.len(),
        verify_time.as_millis()
    );

    // Control for the abandonment itself: a trivial block, abandoned at 0ms. Verification of a
    // coinbase-only block finishes before the connection reset is processed, so it commits.
    // This proves an abandoned request still reaches the node and is verified normally, which
    // isolates verification *duration* as the only variable in the test below.
    let trivial_block = assemble_block(addr, &network, vec![]).await?;
    let trivial_height = tip_height(addr).await? + 1;
    rpc_abandon(
        addr,
        "submitblock",
        json!([hex::encode(trivial_block.zcash_serialize_to_vec()?)]),
        Duration::ZERO,
    )
    .await?;
    tokio::time::sleep(GRACE).await;

    let trivial_committed = tip_height(addr).await? >= trivial_height;
    println!(
        "control: trivial coinbase-only block abandoned at 0ms was {}\n",
        if trivial_committed {
            "COMMITTED (abandoned requests are received and verified)"
        } else {
            "LOST"
        }
    );

    // The test block, submitted with the connection killed well inside the verification window.
    let test_block = assemble_block(addr, &network, test_txs.to_vec()).await?;
    let block_hash = test_block.hash();
    let block_hex = hex::encode(test_block.zcash_serialize_to_vec()?);
    let target_height = tip_height(addr).await? + 1;
    println!("test block {block_hash} for height {target_height}");

    let delays = [verify_time / 8, verify_time / 4, verify_time / 2];
    let mut lost = 0;
    let mut landed_early = false;

    for delay in delays {
        rpc_abandon(addr, "submitblock", json!([block_hex]), delay).await?;

        tokio::time::sleep(GRACE).await;
        let height = tip_height(addr).await?;

        if height >= target_height {
            println!(
                "abandoned submit (drop after {}ms): block COMMITTED, tip {height}",
                delay.as_millis()
            );
            landed_early = true;
            break;
        }

        println!(
            "abandoned submit (drop after {}ms): block LOST, tip still {height}",
            delay.as_millis()
        );
        lost += 1;
    }

    // Control: the same bytes, with the connection held open.
    println!();
    let (response, elapsed) = submit_held_open(addr, &test_block).await?;
    let final_height = tip_height(addr).await?;
    println!(
        "held-open submit: response {response}, took {}ms, tip {final_height}",
        elapsed.as_millis()
    );

    // Does the block path reuse mempool verification? Submit two blocks of identical shape:
    // one whose transactions were fully verified at mempool admission, one whose transactions
    // the node has never seen. If both take the same time, there is no reuse.
    println!("\n=== mempool verification reuse ===");

    let admission_start = Instant::now();
    for tx in warm_txs {
        rpc(
            addr,
            "sendrawtransaction",
            json!([hex::encode(tx.zcash_serialize_to_vec()?)]),
        )
        .await?;
    }
    let admission_time = admission_start.elapsed();
    println!(
        "admitted {TXS_PER_BLOCK} transactions to the mempool (full verification) in {}ms",
        admission_time.as_millis()
    );

    // Let the template pick them all up, then mine it the way a miner would.
    let mut warm_block = None;
    for _ in 0..30 {
        let template = rpc(addr, "getblocktemplate", json!([])).await?;
        let included = template["transactions"]
            .as_array()
            .map_or(0, |txs| txs.len());

        if included == TXS_PER_BLOCK {
            let template: BlockTemplateResponse = serde_json::from_value(template)?;
            warm_block = Some(proposal_block_from_template(
                &template,
                BlockTemplateTimeSource::CurTime,
                &network,
            )?);
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let warm_time = match warm_block {
        Some(warm_block) => {
            let (response, elapsed) = submit_held_open(addr, &warm_block).await?;
            if !response.is_null() {
                return Err(format!("mempool-verified block rejected: {response}").into());
            }
            println!(
                "block whose transactions were ALREADY verified in the mempool: {}ms",
                elapsed.as_millis()
            );
            Some(elapsed)
        }
        None => {
            println!("(skipped: the block template never included all mempool transactions)");
            None
        }
    };

    let cold_block = assemble_block(addr, &network, cold_txs.to_vec()).await?;
    let (response, cold_time) = submit_held_open(addr, &cold_block).await?;
    if !response.is_null() {
        return Err(format!("cold block rejected: {response}").into());
    }
    println!(
        "block whose transactions the node has NEVER seen:                 {}ms",
        cold_time.as_millis()
    );

    if let Some(warm_time) = warm_time {
        let ratio = warm_time.as_secs_f64() / cold_time.as_secs_f64();
        println!(
            "\nratio {ratio:.2}x — {}",
            if ratio > 0.8 {
                "NO REUSE: prior mempool verification saves nothing at block time"
            } else {
                "the block path appears to reuse mempool verification"
            }
        );
    }

    // Does a resubmission while the first is still verifying start a second verification?
    // The second submission is sent once the first is in flight; if it comes back far sooner
    // than a real verification takes, the node recognised it as already being verified.
    println!("\n=== concurrent submissions of the same block ===");

    let concurrent_block = assemble_block(addr, &network, concurrent_txs.to_vec()).await?;
    let concurrent_hex = hex::encode(concurrent_block.zcash_serialize_to_vec()?);

    let first = tokio::spawn({
        let concurrent_hex = concurrent_hex.clone();
        async move {
            let start = Instant::now();
            (
                rpc(addr, "submitblock", json!([concurrent_hex])).await,
                start.elapsed(),
            )
        }
    });

    tokio::time::sleep(Duration::from_millis(20)).await;
    let second_start = Instant::now();
    let second_response = rpc(addr, "submitblock", json!([concurrent_hex])).await?;
    let second_elapsed = second_start.elapsed();

    let (first_response, first_elapsed) = first.await?;
    let first_response = first_response?;

    println!(
        "first submission:  response {first_response}, {}ms",
        first_elapsed.as_millis()
    );
    println!(
        "second submission: response {second_response}, {}ms",
        second_elapsed.as_millis()
    );

    // A block that is still being verified is not a validated duplicate yet, so the node
    // answers `duplicate-inconclusive`.
    if second_elapsed * 4 < first_elapsed
        && second_response.as_str() == Some("duplicate-inconclusive")
    {
        println!("DEDUPED: the resubmission returned immediately instead of verifying again");
    } else {
        println!("NOT DEDUPED: the resubmission paid full verification cost");
    }

    println!("\n=== verdict ===");
    if landed_early {
        println!("NOT REPRODUCED: an abandoned submission committed the block.");
    } else if final_height >= target_height && response.is_null() {
        println!(
            "REPRODUCED: {lost}/{} abandoned submissions silently discarded block {block_hash}; \
             the identical bytes committed in {}ms once the connection was held open.",
            delays.len(),
            elapsed.as_millis()
        );
    } else if matches!(
        response.as_str(),
        Some("duplicate") | Some("duplicate-inconclusive")
    ) {
        println!(
            "INCONCLUSIVE: an abandoned submission committed the block after the grace period."
        );
    } else {
        println!("INCONCLUSIVE: control submission did not commit the block: {response}");
    }

    Ok(())
}
