//! Integration check against a real node. Ignored by default: it needs a
//! reachable zebrad, which CI does not have.
//!
//!   ZIH_TEST_NODE=127.0.0.1:18232 cargo test --test live_chain -- --ignored --nocapture

use zero_indexer_hub::chain::{ChainClient, NodeEndpoint};

#[tokio::test]
#[ignore = "needs a reachable zebrad; set ZIH_TEST_NODE"]
async fn reads_the_tip_from_a_real_node() {
    let addr = std::env::var("ZIH_TEST_NODE").expect("set ZIH_TEST_NODE=host:port");
    let client = ChainClient::new(vec![NodeEndpoint {
        addr: addr.clone(),
        user: std::env::var("ZIH_TEST_USER").ok(),
        password: std::env::var("ZIH_TEST_PASS").ok(),
    }])
    .expect("client");

    let height = client.tip_height().await.expect("tip query failed");
    println!("tip height from {addr}: {height}");

    // Sanity rather than a fixed value: mainnet is far past this and it will
    // not regress, so a plausible height proves we parsed a real answer rather
    // than a default.
    assert!(height > 3_000_000, "implausible height {height}");
}
