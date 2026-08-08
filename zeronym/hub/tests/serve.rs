//! Integration tests for the batching serving path.
//!
//! Each test stands up an in-process mock full node (a tiny JSON-RPC responder
//! that records every hex transaction it was asked to broadcast) and the hub
//! server, both on ephemeral ports, then drives real HTTP round-trips.
//!
//! The property under test is the one the whole design rests on: **a submission
//! does not reach a node when it arrives.** It is held, and it reaches the
//! network only when a flush publishes the whole batch at once. A test that only
//! checked "the hub accepted it" would pass just as happily against the
//! immediate-broadcast hub this replaced, so the node's view is what is asserted
//! throughout.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;

use zero_indexer_hub::batcher::{self, BatchParams, TipTracker};
use zero_indexer_hub::chain::{ChainClient, NodeEndpoint};
use zero_indexer_hub::queue::Queue;
use zero_indexer_hub::server::{self, Hub};

/// A height low enough that any realistic fixture expiry clears the admission
/// deadline, so these tests exercise batching rather than expiry arithmetic.
const TIP: u32 = 100;

/// A mock `sendrawtransaction` node. It records EVERY hex tx it is asked to
/// broadcast, in order, and replies with either a `result` (accepted, echoing a
/// txid) or an `error` (e.g. a duplicate). Returns its `host:port`.
async fn spawn_mock_node(
    result: Option<&'static str>,
    error: Option<&'static str>,
    seen: Arc<Mutex<Vec<String>>>,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let seen = seen.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |req: Request<Incoming>| {
                            let seen = seen.clone();
                            async move {
                                let body = req.into_body().collect().await.unwrap().to_bytes();
                                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                                if let Some(hex) = v["params"][0].as_str() {
                                    seen.lock().unwrap().push(hex.to_string());
                                }
                                let reply = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": 1,
                                    "result": result,
                                    "error": error.map(|e| serde_json::json!({"code": -1, "message": e})),
                                });
                                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(reply.to_string()))))
                            }
                        }),
                    )
                    .await;
            });
        }
    });
    addr.to_string()
}

/// A running hub, plus the handles a test needs to drive a flush itself rather
/// than waiting out a real block cadence.
struct Harness {
    addr: String,
    queue: Arc<Queue>,
    chain: Arc<ChainClient>,
}

impl Harness {
    /// Publish everything held, exactly as the cadence would at a flush
    /// boundary. Returns the achieved batch size.
    async fn flush(&self) -> usize {
        batcher::flush(&self.queue, &self.chain).await
    }
}

/// Start the hub against `node_addr`, with a known tip so admission is
/// deterministic.
async fn spawn_hub(node_addr: String) -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let chain = Arc::new(
        ChainClient::new(vec![NodeEndpoint {
            addr: node_addr,
            user: None,
            password: None,
        }])
        .unwrap(),
    );
    let queue = Arc::new(Queue::new());
    let tip = Arc::new(TipTracker::new());
    tip.observe(TIP);

    tokio::spawn(server::serve(
        listener,
        Hub {
            queue: queue.clone(),
            tip,
            params: BatchParams::default(),
        },
    ));

    Harness {
        addr: addr.to_string(),
        queue,
        chain,
    }
}

/// POST a body to the hub, returning the response status and body bytes.
async fn post(hub_addr: &str, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let req = Request::builder()
        .method("POST")
        .uri(format!("http://{hub_addr}/"))
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    let resp = client.request(req).await.unwrap();
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes)
}

async fn post_json(hub_addr: &str, body: Vec<u8>) -> serde_json::Value {
    let (status, bytes) = post(hub_addr, body).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice(&bytes).unwrap()
}

// ------------------------------------------------------------------- tests

#[tokio::test]
async fn a_submission_is_held_and_does_not_reach_a_node_until_the_flush() {
    // THE central property. If this test ever passes trivially, batching is not
    // happening and the anonymity claim is false.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let txid = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let node = spawn_mock_node(Some(txid), None, seen.clone()).await;
    let hub = spawn_hub(node).await;

    // Arbitrary bytes: the re-parse is telemetry only, so an unparseable body is
    // still queued and published (REVIEW #5).
    let v = post_json(&hub.addr, vec![0x01, 0x02, 0x03]).await;
    assert_eq!(v["disposition"], "accepted");

    // Accepted by the hub, and NOT on the network.
    assert!(
        seen.lock().unwrap().is_empty(),
        "an admitted migration must not reach a node before its flush"
    );

    // Only the flush publishes it.
    assert_eq!(hub.flush().await, 1);
    assert_eq!(seen.lock().unwrap().as_slice(), &["010203".to_string()]);
}

#[tokio::test]
async fn a_whole_batch_is_published_together_on_one_flush() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let node = spawn_mock_node(Some("aa"), None, seen.clone()).await;
    let hub = spawn_hub(node).await;

    for i in 1u8..=5 {
        let v = post_json(&hub.addr, vec![i; 8]).await;
        assert_eq!(v["disposition"], "accepted");
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing publishes before the flush"
    );

    assert_eq!(hub.flush().await, 5);
    assert_eq!(
        seen.lock().unwrap().len(),
        5,
        "every member of the batch reaches the network on the same flush"
    );
}

#[tokio::test]
async fn a_second_flush_republishes_nothing() {
    // The queue is drained by the flush, so a migration is published once. A
    // standalone republish would be a singleton event tied to one transaction,
    // which is a fresh timing signal for exactly the transaction being protected.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let node = spawn_mock_node(Some("aa"), None, seen.clone()).await;
    let hub = spawn_hub(node).await;

    let _ = post_json(&hub.addr, vec![0x42; 8]).await;
    assert_eq!(hub.flush().await, 1);
    assert_eq!(seen.lock().unwrap().len(), 1);

    assert_eq!(hub.flush().await, 0, "an empty flush publishes nothing");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_resubmission_of_identical_bytes_does_not_inflate_the_batch() {
    // Cross-hub submission and honest retries are designed behaviour, so the
    // same bytes arriving twice must collapse rather than double-publish.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let node = spawn_mock_node(Some("aa"), None, seen.clone()).await;
    let hub = spawn_hub(node).await;

    let first = post_json(&hub.addr, vec![0x07; 8]).await;
    let second = post_json(&hub.addr, vec![0x07; 8]).await;
    assert_eq!(first["disposition"], "accepted");
    assert_eq!(
        second["disposition"], "accepted",
        "a duplicate is a success, not an error the shim should retry"
    );

    assert_eq!(hub.flush().await, 1);
    assert_eq!(seen.lock().unwrap().len(), 1, "published once, not twice");
}

#[tokio::test]
async fn already_known_at_the_node_counts_toward_the_achieved_batch_size() {
    // With every shim submitting to every hub, the second hub's publish is
    // already-known by construction. Counting only Accepted would report zero on
    // one side of every honest batch.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let node = spawn_mock_node(None, Some("txn-already-known"), seen.clone()).await;
    let hub = spawn_hub(node).await;

    let _ = post_json(&hub.addr, vec![0xde, 0xad, 0xbe, 0xef]).await;
    assert_eq!(
        hub.flush().await,
        1,
        "already-known is a success: the network has it"
    );
}

#[tokio::test]
async fn a_real_orchard_transaction_is_admitted_and_gets_a_computed_txid() {
    // Shared corpus with the shim. The txid comes back at ADMISSION, before the
    // transaction has been anywhere near a node, because the hub computes it
    // from the bytes. That is what lets the shim answer the wallet immediately
    // while publication is still a flush away.
    let bytes = include_bytes!("../../shim/tests/fixtures/v6_orchard_only.bin").to_vec();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let node = spawn_mock_node(Some("unused"), None, seen.clone()).await;
    let hub = spawn_hub(node).await;

    let v = post_json(&hub.addr, bytes).await;
    assert_eq!(v["disposition"], "accepted");
    let txid = v["txid"].as_str().expect("a computed txid");
    assert_eq!(txid.len(), 64, "txid is 32 bytes of hex");
    assert!(txid.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(
        seen.lock().unwrap().is_empty(),
        "the txid is known before the transaction is published, not because of it"
    );
}

#[tokio::test]
async fn a_no_expiry_transaction_is_admissible_at_any_height() {
    // The shared fixtures carry no expiry (`expiry_height() == None`), which
    // under ZIP 203 means the transaction never expires. Such a transaction must
    // be admissible at ANY tip: folding "no expiry" to height zero would refuse
    // every one of them forever. The expiry arithmetic itself is covered
    // exhaustively by the `survives_next_flush` unit tests, which can vary the
    // expiry directly instead of needing a fixture per case.
    // No mock node: nothing is flushed here, so nothing should reach one.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let queue = Arc::new(Queue::new());
    let tip = Arc::new(TipTracker::new());
    // A height far beyond any plausible expiry.
    tip.observe(u32::MAX - 1000);
    tokio::spawn(server::serve(
        listener,
        Hub {
            queue: queue.clone(),
            tip,
            params: BatchParams::default(),
        },
    ));

    let bytes = include_bytes!("../../shim/tests/fixtures/v6_orchard_only.bin").to_vec();
    let v = post_json(&addr, bytes).await;
    assert_eq!(v["disposition"], "accepted");
    assert_eq!(queue.len(), 1);
}

#[tokio::test]
async fn a_stale_tip_stops_admission_rather_than_forcing_a_flush() {
    // Fail closed. A tip the hub cannot trust means the flush schedule and the
    // expiry check are both unreliable, and flushing on a stale tip would hand
    // an adversary the trigger: brief interference against the hub's node would
    // force a near-empty batch containing the targeted transaction.
    // No mock node: a refused submission must never reach one.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    // A tracker that has never observed a height is stale by definition.
    let tip = Arc::new(TipTracker::new());
    let queue = Arc::new(Queue::new());
    tokio::spawn(server::serve(
        listener,
        Hub {
            queue: queue.clone(),
            tip,
            params: BatchParams::default(),
        },
    ));

    let v = post_json(&addr, vec![0x01, 0x02, 0x03]).await;
    assert_eq!(v["disposition"], "rejected");
    assert_eq!(v["reason"], "tip_stale");
    assert_eq!(queue.len(), 0);
}

#[tokio::test]
async fn an_oversize_body_is_refused_and_never_queued() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let node = spawn_mock_node(Some("aa"), None, seen.clone()).await;
    let hub = spawn_hub(node).await;

    let (status, _) = post(&hub.addr, vec![0u8; 64 * 1024 + 1]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(hub.queue.len(), 0);
    assert_eq!(hub.flush().await, 0);
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_get_is_rejected() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let node = spawn_mock_node(Some("aa"), None, seen.clone()).await;
    let hub = spawn_hub(node).await;

    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let req = Request::builder()
        .method("GET")
        .uri(format!("http://{}/", hub.addr))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}
