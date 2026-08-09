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

    let height = client.tip_height().await.expect("tip query failed");
    println!("tip height from {addr}: {height}");

    // Sanity rather than a fixed value: mainnet is far past this and it will
    // not regress, so a plausible height proves we parsed a real answer rather
    // than a default.
    assert!(height > 3_000_000, "implausible height {height}");
}
