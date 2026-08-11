//! Integration tests for the batching serving path.
//!
//! Each test stands up an in-process mock INDEXER (a tiny gRPC responder that
//! records every transaction it was asked to broadcast) and the hub server, both
//! on ephemeral ports, then drives real HTTP round-trips.
//!
//! The property under test is the one the whole design rests on: **a submission
//! does not reach a node when it arrives.** It is held, and it reaches the
//! network only when a flush publishes the whole batch at once. A test that only
//! checked "the hub accepted it" would pass just as happily against the
//! immediate-broadcast hub this replaced, so the INDEXER's view is what is
//! asserted throughout.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use tokio::net::TcpListener;
use zaino_proto::proto::service::{RawTransaction, SendResponse, TxFilter};

use zero_indexer_hub::batcher::{self, BatchParams, TipTracker};
use zero_indexer_hub::chain::ChainClient;
use zero_indexer_hub::queue::Queue;
use zero_indexer_hub::server::{self, Hub};

const GET_TRANSACTION: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTransaction";

/// A height low enough that any realistic fixture expiry clears the admission
/// deadline, so these tests exercise batching rather than expiry arithmetic.
const TIP: u32 = 100;

/// How the mock indexer answers a `GetTransaction`.
#[derive(Clone)]
enum GetTx {
    /// gRPC NOT_FOUND, the lightwalletd unknown-txid answer.
    NotFound,
    /// A framed `RawTransaction` with these bytes and height.
    Found { data: Vec<u8>, height: u64 },
}

/// A mock `CompactTxStreamer` that records broadcasts and answers
/// `GetTransaction` with NOT_FOUND. `seen` collects the raw bytes of every
/// transaction it was asked to broadcast; the shape the batching tests assert on.
async fn spawn_mock_indexer(
    code: i32,
    message: &'static str,
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
) -> SocketAddr {
    spawn_mock_indexer_full(
        code,
        message,
        GetTx::NotFound,
        seen,
        Arc::new(Mutex::new(Vec::new())),
    )
    .await
}

/// The path-aware mock, with a configurable `GetTransaction` answer and a
/// separate record of the `TxFilter.hash` bytes of every lookup it received.
/// `SendTransaction` records the tx and replies `SendResponse { code, message }`.
async fn spawn_mock_indexer_full(
    code: i32,
    message: &'static str,
    get_tx: GetTx,
    broadcast_seen: Arc<Mutex<Vec<Vec<u8>>>>,
    lookup_seen: Arc<Mutex<Vec<Vec<u8>>>>,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let broadcast_seen = broadcast_seen.clone();
            let lookup_seen = lookup_seen.clone();
            let get_tx = get_tx.clone();
            tokio::spawn(async move {
                let _ = http2::Builder::new(TokioExecutor::new())
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |req: Request<Incoming>| {
                            let broadcast_seen = broadcast_seen.clone();
                            let lookup_seen = lookup_seen.clone();
                            let get_tx = get_tx.clone();
                            async move {
                                let path = req.uri().path().to_owned();
                                let body = req.into_body().collect().await.unwrap().to_bytes();
                                let message_bytes =
                                    if body.len() > 5 { &body[5..] } else { &[][..] };
                                match path.as_str() {
                                    GET_TRANSACTION => {
                                        if let Ok(filter) = TxFilter::decode(message_bytes) {
                                            lookup_seen.lock().unwrap().push(filter.hash);
                                        }
                                        Ok::<_, Infallible>(match &get_tx {
                                            GetTx::NotFound => grpc_not_found(),
                                            GetTx::Found { data, height } => {
                                                grpc_raw_tx(data, *height)
                                            }
                                        })
                                    }
                                    _ => {
                                        if let Ok(raw) = RawTransaction::decode(message_bytes) {
                                            if !raw.data.is_empty() {
                                                broadcast_seen.lock().unwrap().push(raw.data);
                                            }
                                        }
                                        Ok(grpc_send_response(code, message))
                                    }
                                }
                            }
                        }),
                    )
                    .await;
            });
        }
    });
    addr
}

/// Frame `payload` as a gRPC unary body with a `grpc-status: 0` trailer.
fn grpc_ok(payload: Vec<u8>) -> Response<BoxBody<Bytes, Infallible>> {
    let mut framed = Vec::with_capacity(5 + payload.len());
    framed.push(0);
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(&payload);

    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", "0".parse().unwrap());

    Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .body(
            Full::new(Bytes::from(framed))
                .with_trailers(async move { Some(Ok(trailers)) })
                .boxed(),
        )
        .unwrap()
}

fn grpc_send_response(code: i32, message: &str) -> Response<BoxBody<Bytes, Infallible>> {
    grpc_ok(
        SendResponse {
            error_code: code,
            error_message: message.to_owned(),
        }
        .encode_to_vec(),
    )
}

fn grpc_raw_tx(data: &[u8], height: u64) -> Response<BoxBody<Bytes, Infallible>> {
    grpc_ok(
        RawTransaction {
            data: data.to_vec(),
            height,
        }
        .encode_to_vec(),
    )
}

/// A trailers-only gRPC NOT_FOUND (status in the headers, empty body), the shape
/// lightwalletd returns for an unknown txid.
fn grpc_not_found() -> Response<BoxBody<Bytes, Infallible>> {
    Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .header("grpc-status", "5")
        .header("grpc-message", "No such mempool or main chain transaction")
        .body(Full::new(Bytes::new()).boxed())
        .unwrap()
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
async fn spawn_hub(indexer: SocketAddr) -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // No TLS in tests: the mock indexer speaks plaintext h2c.
    let chain = Arc::new(ChainClient::new(vec![indexer], None).unwrap());
    let queue = Arc::new(Queue::new());
    let tip = Arc::new(TipTracker::new());
    tip.observe(TIP);

    tokio::spawn(server::serve(
        listener,
        Hub {
            queue: queue.clone(),
            tip,
            params: BatchParams::default(),
            chain: chain.clone(),
        },
    ));

    Harness {
        addr: addr.to_string(),
        queue,
        chain,
    }
}

/// POST a lookup body to `/transaction`, returning status, the parsed
/// `x-tx-height` header (if present) and the response body.
async fn lookup(hub_addr: &str, wire_hash: Vec<u8>) -> (StatusCode, Option<u64>, Vec<u8>) {
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let req = Request::builder()
        .method("POST")
        .uri(format!("http://{hub_addr}/transaction"))
        .body(Full::new(Bytes::from(wire_hash)))
        .unwrap();
    let resp = client.request(req).await.unwrap();
    let status = resp.status();
    let height = resp
        .headers()
        .get("x-tx-height")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, height, body)
}

/// The wire-order (internal, little-endian) bytes for a display-order txid hex:
/// decode, then reverse. This is what a wallet's `TxFilter.hash` carries.
fn wire_hash(display_txid: &str) -> Vec<u8> {
    let mut bytes = hex::decode(display_txid).unwrap();
    bytes.reverse();
    bytes
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
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let txid = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let indexer = spawn_mock_indexer(0, txid, seen.clone()).await;
    let hub = spawn_hub(indexer).await;

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
    assert_eq!(seen.lock().unwrap().as_slice(), &[vec![0x01u8, 0x02, 0x03]]);
}

#[tokio::test]
async fn a_whole_batch_is_published_together_on_one_flush() {
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer(0, "aa", seen.clone()).await;
    let hub = spawn_hub(indexer).await;

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
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer(0, "aa", seen.clone()).await;
    let hub = spawn_hub(indexer).await;

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
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer(0, "aa", seen.clone()).await;
    let hub = spawn_hub(indexer).await;

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
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer(-25, "txn-already-known", seen.clone()).await;
    let hub = spawn_hub(indexer).await;

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
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer(0, "unused", seen.clone()).await;
    let hub = spawn_hub(indexer).await;

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
            // Never reached in this test (no flush, no lookup), but the type
            // needs one. Constructed, not connected.
            chain: Arc::new(ChainClient::new(vec!["127.0.0.1:9".parse().unwrap()], None).unwrap()),
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
            // Never reached in this test (no flush, no lookup), but the type
            // needs one. Constructed, not connected.
            chain: Arc::new(ChainClient::new(vec!["127.0.0.1:9".parse().unwrap()], None).unwrap()),
        },
    ));

    let v = post_json(&addr, vec![0x01, 0x02, 0x03]).await;
    assert_eq!(v["disposition"], "rejected");
    assert_eq!(v["reason"], "tip_stale");
    assert_eq!(queue.len(), 0);
}

#[tokio::test]
async fn an_oversize_body_is_refused_and_never_queued() {
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer(0, "aa", seen.clone()).await;
    let hub = spawn_hub(indexer).await;

    let (status, _) = post(&hub.addr, vec![0u8; 64 * 1024 + 1]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(hub.queue.len(), 0);
    assert_eq!(hub.flush().await, 0);
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_get_is_rejected() {
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer(0, "aa", seen.clone()).await;
    let hub = spawn_hub(indexer).await;

    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let req = Request::builder()
        .method("GET")
        .uri(format!("http://{}/", hub.addr))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// ----------------------------------------------------------- lookup (/transaction)

const V6_ORCHARD_ONLY: &[u8] = include_bytes!("../../shim/tests/fixtures/v6_orchard_only.bin");

#[tokio::test]
async fn a_queued_transaction_is_served_from_the_queue_with_height_zero() {
    // The whole reason the shim can be stateless: a diverted, not-yet-flushed
    // migration exists only in the hub's queue, and a lookup for it is answered
    // from there with height 0 (mempool), never touching the indexer.
    let broadcast = Arc::new(Mutex::new(Vec::new()));
    let looked_up = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer_full(
        0,
        "unused",
        GetTx::NotFound,
        broadcast.clone(),
        looked_up.clone(),
    )
    .await;
    let hub = spawn_hub(indexer).await;

    let txid = post_json(&hub.addr, V6_ORCHARD_ONLY.to_vec()).await["txid"]
        .as_str()
        .expect("a computed txid")
        .to_owned();

    let (status, height, body) = lookup(&hub.addr, wire_hash(&txid)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(height, Some(0), "a queued tx is mempool: height 0");
    assert_eq!(body, V6_ORCHARD_ONLY);
    assert!(
        looked_up.lock().unwrap().is_empty(),
        "the queue answered, so the indexer was never queried"
    );
    assert!(
        broadcast.lock().unwrap().is_empty(),
        "and nothing was broadcast"
    );
}

#[tokio::test]
async fn both_byte_orders_hit_the_queue() {
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer(0, "unused", seen).await;
    let hub = spawn_hub(indexer).await;

    let txid = post_json(&hub.addr, V6_ORCHARD_ONLY.to_vec()).await["txid"]
        .as_str()
        .unwrap()
        .to_owned();

    // Internal order (what a wallet sends) and display order both resolve.
    let (internal_status, _, _) = lookup(&hub.addr, wire_hash(&txid)).await;
    assert_eq!(internal_status, StatusCode::OK);
    let (display_status, _, _) = lookup(&hub.addr, hex::decode(&txid).unwrap()).await;
    assert_eq!(display_status, StatusCode::OK);
}

#[tokio::test]
async fn after_the_flush_a_lookup_falls_through_to_the_indexer() {
    // Once flushed, the queue no longer holds the tx, so the lookup is served by
    // the hub's indexer, whose height is relayed verbatim and whose TxFilter.hash
    // must be exactly the bytes the wallet posted (unmodified pass-through).
    let broadcast = Arc::new(Mutex::new(Vec::new()));
    let looked_up = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer_full(
        0,
        "unused",
        GetTx::Found {
            data: V6_ORCHARD_ONLY.to_vec(),
            height: 12345,
        },
        broadcast.clone(),
        looked_up.clone(),
    )
    .await;
    let hub = spawn_hub(indexer).await;

    let txid = post_json(&hub.addr, V6_ORCHARD_ONLY.to_vec()).await["txid"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(hub.flush().await, 1);

    let posted = wire_hash(&txid);
    let (status, height, body) = lookup(&hub.addr, posted.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(height, Some(12345), "the indexer's height is relayed");
    assert_eq!(body, V6_ORCHARD_ONLY);
    assert_eq!(
        looked_up.lock().unwrap().as_slice(),
        &[posted],
        "the indexer received the wallet's bytes unmodified"
    );
}

#[tokio::test]
async fn an_unknown_txid_is_404() {
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer(0, "unused", seen).await; // GetTransaction -> NOT_FOUND
    let hub = spawn_hub(indexer).await;

    let (status, _, _) = lookup(&hub.addr, vec![0x55; 32]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(hub.queue.len(), 0, "a lookup never mutates the queue");
}

#[tokio::test]
async fn an_unreachable_indexer_is_502_not_404() {
    // The distinction the shim depends on: 404 means "known-absent" (map to
    // NOT_FOUND), 502 means "could not ask" (map to UNAVAILABLE, fail closed).
    let dead: SocketAddr = "127.0.0.1:9".parse().unwrap(); // discard port, nothing listening
    let hub = spawn_hub(dead).await;

    let (status, _, _) = lookup(&hub.addr, vec![0x55; 32]).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn lookup_bodies_are_never_queued() {
    // Regression guard against the any-POST-is-a-submission past: a lookup body
    // must never become a queue entry, whatever its size.
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer(0, "unused", seen).await;
    let hub = spawn_hub(indexer).await;

    let (empty, _, _) = lookup(&hub.addr, vec![]).await;
    assert_eq!(empty, StatusCode::BAD_REQUEST);
    let (oversize, _, _) = lookup(&hub.addr, vec![0u8; 65]).await;
    assert_eq!(oversize, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(hub.queue.len(), 0);
}

#[tokio::test]
async fn an_unknown_path_is_404_not_a_submission() {
    // The old hub queued any POST regardless of path; a typo silently queued
    // garbage. Now it 404s.
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer(0, "unused", seen).await;
    let hub = spawn_hub(indexer).await;

    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let req = Request::builder()
        .method("POST")
        .uri(format!("http://{}/submitx", hub.addr))
        .body(Full::new(Bytes::from(vec![0u8; 32])))
        .unwrap();
    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(hub.queue.len(), 0);
}
