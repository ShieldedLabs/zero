//! zcashd↔Zebra transaction-level differential harness.
//!
//! Feeds raw transactions built by the (fixed) zcashd wallet through Zebra's
//! own `zebra_consensus::transaction::Verifier` — full parse, version/upgrade
//! gating at the real mined height, and every Groth16/Halo2 proof and
//! RedJubjub/RedPallas signature verified under Zebra's independent sighash
//! implementation. A divergent sighash, flag grammar, or proof encoding on
//! either side fails loudly here.
//!
//! Input: zdiff_txs.json produced from the live zcashd regtest chain
//! (NU6.3 @ 210): the pre-activation v5 Orchard shielding, the H-P2-1 Class A
//! (Orchard spend + Sapling change) and Class B (Sapling spend + transparent
//! output) transactions, and the z_shieldtoironwood v6 Ironwood-bundle
//! transaction.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use tower::ServiceExt;

use zebra_chain::{
    amount::Amount,
    block::Height,
    parameters::{
        testnet::{ConfiguredActivationHeights, RegtestParameters},
        Network,
    },
    serialization::ZcashDeserializeInto,
    transaction::{self, Transaction},
    transparent,
};
use zebra_consensus::transaction::{Request, Verifier};
use zebra_node_services::mempool;
use zebra_test::mock_service::{MockService, PanicAssertion};

#[derive(serde::Deserialize)]
struct PrevOut {
    txid: String,
    vout: u32,
    value_zat: i64,
    script: String,
    creating_height: u32,
    is_coinbase: bool,
}

#[derive(serde::Deserialize)]
struct TxCase {
    txid: String,
    hex: String,
    height: u32,
    time: i64,
    version: u32,
    prevouts: Vec<PrevOut>,
}

fn regtest_network() -> Network {
    Network::new_regtest(RegtestParameters {
        activation_heights: ConfiguredActivationHeights {
            overwinter: Some(1),
            sapling: Some(1),
            blossom: Some(1),
            heartwood: Some(1),
            canopy: Some(1),
            nu5: Some(1),
            nu6: Some(1),
            nu6_1: Some(1),
            nu6_2: Some(1),
            nu6_3: Some(210),
            ..Default::default()
        },
        ..Default::default()
    })
}

type StateMock = MockService<zebra_state::Request, zebra_state::Response, PanicAssertion>;
type MempoolMock = MockService<mempool::Request, mempool::Response, PanicAssertion>;

async fn verify_at_height(
    network: &Network,
    state: &StateMock,
    case: &TxCase,
    height: u32,
) -> Result<(), String> {
    let bytes = hex::decode(&case.hex).map_err(|e| format!("hex: {e}"))?;
    let tx: Transaction = bytes
        .zcash_deserialize_into()
        .map_err(|e| format!("Zebra failed to PARSE the transaction: {e}"))?;

    // Supply every transparent prevout so the verifier makes no state calls
    // (the PanicAssertion mock turns any unexpected call into a loud failure).
    let mut known_utxos: HashMap<transparent::OutPoint, transparent::OrderedUtxo> = HashMap::new();
    for po in &case.prevouts {
        let outpoint = transparent::OutPoint {
            hash: po.txid.parse::<transaction::Hash>().map_err(|e| format!("txid: {e}"))?,
            index: po.vout,
        };
        let output = transparent::Output {
            value: Amount::try_from(po.value_zat).map_err(|e| format!("amount: {e:?}"))?,
            lock_script: transparent::Script::new(
                &hex::decode(&po.script).map_err(|e| format!("script hex: {e}"))?,
            ),
        };
        // tx_index_in_block 0 marks the UTXO as coinbase-created.
        let tx_index = if po.is_coinbase { 0 } else { 1 };
        known_utxos.insert(
            outpoint,
            transparent::OrderedUtxo::new(output, Height(po.creating_height), tx_index),
        );
    }

    // A fresh verifier per call (the mempool-setup receiver is single-use).
    let (_mempool_tx, mempool_rx) = tokio::sync::oneshot::channel::<MempoolMock>();
    let verifier = Verifier::new(network, state.clone(), mempool_rx);

    let request = Request::Block {
        transaction_hash: tx.hash(),
        transaction: Arc::new(tx),
        known_outpoint_hashes: Arc::new(HashSet::new()),
        known_utxos: Arc::new(known_utxos),
        height: Height(height),
        time: Utc.timestamp_opt(case.time, 0).single().expect("valid time"),
    };

    verifier
        .oneshot(request)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

#[tokio::main]
async fn main() {
    let path = std::env::args().nth(1).expect("usage: zdiff-harness <zdiff_txs.json>");
    let cases: HashMap<String, TxCase> =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read json"))
            .expect("parse json");

    let network = regtest_network();
    let state: StateMock = MockService::build().for_unit_tests();

    let mut failures = 0;
    let order = ["shieldcoinbase", "classA", "classB", "ironwood"];
    for name in order {
        let case = cases.get(name).expect("case present");
        print!(
            "[{name}] v{} tx {} @ height {} ... ",
            case.version,
            &case.txid[..12],
            case.height
        );
        match verify_at_height(&network, &state, case, case.height).await {
            Ok(()) => println!("ZEBRA ACCEPTS"),
            Err(e) => {
                println!("ZEBRA REJECTS: {e}");
                failures += 1;
            }
        }
    }

    // Negative control: the v6 Class A transaction presented one block BELOW
    // activation must be rejected (proves this harness can detect failures —
    // and pins Zebra's own pre-activation v6 rejection, mirroring zcashd's).
    let case = cases.get("classA").expect("case present");
    print!("[negative-control] classA at pre-activation height 209 ... ");
    match verify_at_height(&network, &state, case, 209).await {
        Ok(()) => {
            println!("UNEXPECTEDLY ACCEPTED — harness cannot detect failures!");
            failures += 1;
        }
        Err(e) => println!("correctly rejected: {e}"),
    }

    if failures == 0 {
        println!("\nRESULT: PARITY — Zebra fully verifies every zcashd-built transaction.");
    } else {
        println!("\nRESULT: {failures} DIVERGENCE(S) — see above.");
        std::process::exit(1);
    }
}
