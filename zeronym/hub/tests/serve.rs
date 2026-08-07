//! Integration tests for the immediate-broadcast serving path.
//!
//! Each test stands up an in-process mock full node (a tiny JSON-RPC responder
//! that records the hex transaction it was asked to broadcast) and the hub
//! server, both on ephemeral ports, then drives real HTTP round-trips through
//! the hub.

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

use zero_indexer_hub::chain::{ChainClient, NodeEndpoint};
use zero_indexer_hub::server;

/// A mock `sendrawtransaction` node. It records the hex tx from `params[0]` and
/// replies with either a `result` (accepted, echoing a txid) or an `error`
/// (e.g. a duplicate). Returns its `host:port`.
async fn spawn_mock_node(
    result: Option<&'static str>,
    error: Option<&'static str>,
    seen: Arc<Mutex<Option<String>>>,
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
                                    *seen.lock().unwrap() = Some(hex.to_string());
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

/// Start the hub against `node_addr`. Returns its `host:port`.
async fn spawn_hub(node_addr: String) -> String {
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
    tokio::spawn(server::serve(listener, chain));
    addr.to_string()
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
    let bytes = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, bytes)
}

#[tokio::test]
async fn accepts_and_broadcasts_to_the_node() {
    let seen = Arc::new(Mutex::new(None));
    let txid = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let node = spawn_mock_node(Some(txid), None, seen.clone()).await;
    let hub = spawn_hub(node).await;

    // Arbitrary bytes: the re-parse is telemetry only, so an unparseable body is
    // still broadcast (REVIEW #5).
    let (status, body) = post(&hub, vec![0x01, 0x02, 0x03]).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["disposition"], "accepted");
    assert_eq!(v["txid"], txid);

    // The node was asked to broadcast exactly our bytes, hex-encoded.
    assert_eq!(seen.lock().unwrap().as_deref(), Some("010203"));
}

#[tokio::test]
async fn already_known_is_a_success_not_a_rejection() {
    let seen = Arc::new(Mutex::new(None));
    let node = spawn_mock_node(None, Some("txn-already-known"), seen.clone()).await;
    let hub = spawn_hub(node).await;

    let (status, body) = post(&hub, vec![0xde, 0xad, 0xbe, 0xef]).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["disposition"], "already_known");
}

#[tokio::test]
async fn a_real_orchard_transaction_is_parsed_and_gets_a_computed_txid() {
    // Shared corpus with the shim (REVIEW #169). On an already-known reply the
    // node returns no txid, so the response txid is the one the HUB computed by
    // parsing the bytes: a proof the hub's parser ran and agreed there is a tx.
    let bytes = include_bytes!("../../shim/tests/fixtures/v6_orchard_only.bin").to_vec();
    let seen = Arc::new(Mutex::new(None));
    let node = spawn_mock_node(None, Some("txn-already-in-mempool"), seen.clone()).await;
    let hub = spawn_hub(node).await;

    let (status, body) = post(&hub, bytes).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["disposition"], "already_known");
    let txid = v["txid"].as_str().expect("a computed txid");
    assert_eq!(txid.len(), 64, "txid is 32 bytes of hex");
    assert!(txid.chars().all(|c| c.is_ascii_hexdigit()));
}

#[tokio::test]
async fn an_oversize_body_is_refused() {
    let seen = Arc::new(Mutex::new(None));
    let node = spawn_mock_node(Some("aa"), None, seen.clone()).await;
    let hub = spawn_hub(node).await;

    let (status, _) = post(&hub, vec![0u8; 64 * 1024 + 1]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    // Nothing reached the node.
    assert!(seen.lock().unwrap().is_none());
}

#[tokio::test]
async fn a_get_is_rejected() {
    let seen = Arc::new(Mutex::new(None));
    let node = spawn_mock_node(Some("aa"), None, seen.clone()).await;
    let hub = spawn_hub(node).await;

    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let req = Request::builder()
        .method("GET")
        .uri(format!("http://{hub}/"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}
