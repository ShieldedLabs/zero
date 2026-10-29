//! Integration check against a real indexer. Ignored by default: it needs a
//! reachable `CompactTxStreamer`, which CI does not have.
//!
//! Plaintext h2c against a local indexer:
//!   ZIH_TEST_INDEXER=127.0.0.1:9067 cargo test --test live_chain -- --ignored --nocapture
//!
//! Or over TLS, which is what a deployed enclave does (dial the address, verify
//! the NAME, resolve nothing):
//!   ZIH_TEST_INDEXER=66.42.124.202:443 ZIH_TEST_INDEXER_TLS=lwd.shieldedinfra.net \
//!     cargo test --test live_chain -- --ignored --nocapture

use std::net::SocketAddr;

use zero_indexer_hub::chain::ChainClient;
use zero_indexer_hub::tip_guard;
use zero_indexer_hub::tls::IndexerTls;

#[tokio::test]
#[ignore = "needs a reachable indexer; set ZIH_TEST_INDEXER"]
async fn reads_the_tip_from_a_real_indexer() {
    let addr: SocketAddr = std::env::var("ZIH_TEST_INDEXER")
        .expect("set ZIH_TEST_INDEXER=ip:port")
        .parse()
        .expect("a literal IPv4:port, since an enclave resolves no DNS");

    let tls = std::env::var("ZIH_TEST_INDEXER_TLS")
        .ok()
        .map(|name| IndexerTls::new(&name).expect("a valid DNS name"));

    let client = ChainClient::new(vec![addr], tls).expect("client");

    let heights = client.tip_heights().await.expect("tip query failed");
    let height = heights[0];
    println!("tip height from {addr}: {height}");

    // A real mainnet tip must sit inside the band the hub judges it by, or a
    // mainnet hub would drop every answer this indexer gives.
    let (low, high) = tip_guard::unix_now()
        .and_then(tip_guard::mainnet_band)
        .expect("a clock past the mainnet anchor");
    assert!(
        (low..=high).contains(&height),
        "height {height} outside the mainnet band {low}..={high}"
    );
}

#[tokio::test]
#[ignore = "needs a reachable mainnet indexer; set ZIH_TEST_INDEXER"]
async fn syncs_the_orchard_nullifiers_since_nu6_3_from_a_real_indexer() {
    use zero_indexer_hub::config::Network;
    use zero_indexer_hub::spent::{sync_from, sync_once, ChainNullifiers};

    let addr: SocketAddr = std::env::var("ZIH_TEST_INDEXER")
        .expect("set ZIH_TEST_INDEXER=ip:port")
        .parse()
        .expect("a literal IPv4:port, since an enclave resolves no DNS");
    let tls = std::env::var("ZIH_TEST_INDEXER_TLS")
        .ok()
        .map(|name| IndexerTls::new(&name).expect("a valid DNS name"));
    let client = ChainClient::new(vec![addr], tls).expect("client");

    // A read that fails ends the round where it stopped, and the next round
    // resumes there, as the hub's own loop does.
    let revealed = ChainNullifiers::new(1);
    let started = std::time::Instant::now();
    let mut rounds = 1;
    while sync_once(&revealed, &client, sync_from(Network::Mainnet)).await == 0 {
        assert!(rounds < 5, "the read never reached the tip");
        rounds += 1;
    }
    println!(
        "synced {:?} from {} in {:?} over {rounds} round(s), holding {:?} Orchard nullifiers",
        revealed.synced_to(),
        sync_from(Network::Mainnet),
        started.elapsed(),
        revealed.held()
    );
    assert!(
        revealed.held()[0] > 0,
        "mainnet has spent Orchard notes since NU6.3"
    );

    // Optionally, a raw mined NU6.3 Orchard transaction with an expiry still
    // ahead: its real proofs verify, and the synced set refuses the copy.
    if let Ok(path) = std::env::var("ZIH_TEST_MINED_TX") {
        use std::sync::Arc;
        use zero_indexer_hub::queue::{Admission, Queue, Refusal};

        let mined = std::fs::read(path).expect("a raw transaction file");
        // Judged at height 0, so any expiry past the first flush is admissible.
        assert!(
            matches!(
                Queue::new().admit(&mined, 0, 20, 4),
                Admission::Admitted { .. }
            ),
            "a mined transaction's proofs and signatures verify"
        );
        let watching = Queue::watching(Arc::new(revealed));
        assert_eq!(
            watching.admit(&mined, 0, 20, 4),
            Admission::Refused(Refusal::Rejected),
            "the copy is refused once its nullifiers are on chain"
        );
        println!("mined transaction verified, and refused as a copy");
    }
}
