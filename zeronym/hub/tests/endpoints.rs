//! The hub's read-only endpoints, and the closed clearnet submit path.
//!
//! Both exist because of the same fact: an ATTESTED enclave has no console.
//! `GET /nym-address` publishes the address the hub minted (previously
//! log-only, so reading it required debug mode, which disables attestation —
//! reading it and proving it were mutually exclusive). `GET /healthz` says the
//! process is serving.
//!
//! The third property here is a subtraction. `POST /` used to accept an
//! unauthenticated submission from anyone on the internet, on an enclave that
//! declares `ingress 0.0.0.0/0` and has no submitter ACL by design. With the
//! mixnet carrying submissions, nothing legitimate posts there, so it is off
//! unless `ServeOptions::http_submit` asks for it (NYM_PLAN M7).
//!
//! What every assertion below also guards: these responses carry the address
//! and liveness and NOTHING else. A queue depth or batch size returned here
//! would be a live anonymity-set-size oracle for anyone who can reach the hub,
//! which `server.rs`'s header forbids — and unlike the shim-facing paths, these
//! are reachable by everyone.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tokio::net::TcpListener;

use zero_indexer_hub::batcher::{BatchParams, TipTracker};
use zero_indexer_hub::chain::ChainClient;
use zero_indexer_hub::queue::Queue;
use zero_indexer_hub::server::{self, Hub, NymAddress, ServeOptions};

/// An address in the shape the SDK prints, `identity.encryption@gateway`.
const ADDRESS: &str = "41pY2atgQD2iQkvC2TnKaxkUDGkqfxU9XBtQEk2GmJqD.\
HdjEPfnu2u5DV3jbZqZ6NJEhr1U5u9r41Gt8qncoByHx@tUiLPjz5nkPn5ZJT5ZXLPGDcZ3caQsfkMAp1epoAuSQ";

/// Start a hub with the given options and return its address.
async fn spawn(options: ServeOptions) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let tip = Arc::new(TipTracker::new());
    tip.observe(100);
    tokio::spawn(server::serve(
        listener,
        Hub {
            queue: Arc::new(Queue::new()),
            tip,
            params: BatchParams::default(),
            // No test here reaches an indexer: constructed, never connected.
            chain: Arc::new(ChainClient::new(vec!["127.0.0.1:9".parse().unwrap()], None).unwrap()),
        },
        options,
    ));
    addr
}

async fn get(hub_addr: &str, path: &str) -> (StatusCode, String) {
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let req = Request::builder()
        .method("GET")
        .uri(format!("http://{hub_addr}{path}"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = client.request(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn post(hub_addr: &str, path: &str, body: Vec<u8>) -> StatusCode {
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let req = Request::builder()
        .method("POST")
        .uri(format!("http://{hub_addr}{path}"))
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    client.request(req).await.unwrap().status()
}

#[tokio::test]
async fn nym_address_is_published_once_the_driver_has_one() {
    let nym_address = NymAddress::unknown();
    let addr = spawn(ServeOptions {
        nym_address: nym_address.clone(),
        ..Default::default()
    })
    .await;

    // Before the mixnet client connects there is no address. Answered as such,
    // never as an empty 200: an operator pasting "" into a shim's --hub-nym
    // would get a deployment that fails at its first divert instead of at
    // assemble time.
    let (status, body) = get(&addr, "/nym-address").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!body.is_empty(), "the reason must be stated, not implied");

    nym_address.set(ADDRESS.to_owned());

    // The body is the BARE address, so `curl` output pastes straight into a
    // shim's --hub-nym with no unwrapping.
    let (status, body) = get(&addr, "/nym-address").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, ADDRESS);
}

#[tokio::test]
async fn healthz_answers_while_serving() {
    let addr = spawn(ServeOptions::default()).await;
    let (status, _) = get(&addr, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
}

/// Neither read-only endpoint may leak anything about what the hub is holding.
/// A queue depth here is a real-time "the fleet is unprotected right now" feed
/// for an adversary choosing when to correlate.
#[tokio::test]
async fn the_read_only_endpoints_reveal_nothing_about_the_queue() {
    let nym_address = NymAddress::unknown();
    nym_address.set(ADDRESS.to_owned());
    let addr = spawn(ServeOptions {
        nym_address,
        ..Default::default()
    })
    .await;

    for path in ["/nym-address", "/healthz"] {
        let (_, body) = get(&addr, path).await;
        let lowered = body.to_lowercase();
        for forbidden in ["queue", "batch", "depth", "pending", "txid", "size", "count"] {
            assert!(
                !lowered.contains(forbidden),
                "{path} response mentions {forbidden:?}: {body}"
            );
        }
    }
}

/// The clearnet submit path is absent by default, and absent means 404 — the
/// same answer an unknown path gets, so a scanner cannot tell whether this hub
/// could have accepted a submission.
#[tokio::test]
async fn clearnet_submit_is_closed_by_default() {
    let addr = spawn(ServeOptions::default()).await;
    let tx = include_bytes!("../../shim/tests/fixtures/v6_orchard_only.bin").to_vec();

    assert_eq!(post(&addr, "/", tx).await, StatusCode::NOT_FOUND);
    assert_eq!(
        post(&addr, "/nonsense", vec![1, 2, 3]).await,
        StatusCode::NOT_FOUND,
        "a closed submit path must be indistinguishable from a path that never existed"
    );
}

/// Turning it on restores the transitional hop, and does not disturb the
/// lookup path, which was never gated.
#[tokio::test]
async fn clearnet_submit_works_when_explicitly_enabled() {
    let addr = spawn(ServeOptions {
        http_submit: true,
        ..Default::default()
    })
    .await;
    let tx = include_bytes!("../../shim/tests/fixtures/v6_orchard_only.bin").to_vec();

    assert_eq!(post(&addr, "/", tx).await, StatusCode::OK);
}

/// The lookup path stays open regardless of the submit gate: a shim that
/// diverted over the mixnet still has to answer its wallet's `GetTransaction`.
#[tokio::test]
async fn the_lookup_path_is_not_gated_by_the_submit_flag() {
    let addr = spawn(ServeOptions::default()).await;
    // An empty key is a malformed query, and a 400 proves the handler ran at
    // all — which is the point here, versus the 404 a gated path returns.
    assert_eq!(
        post(&addr, "/transaction", Vec::new()).await,
        StatusCode::BAD_REQUEST
    );
}

/// Method discipline: the read-only endpoints are GET, the write paths are
/// POST, and a wrong method never falls through to another handler. A path that
/// exists answers `405`, which is what distinguishes it from the `404` a
/// disabled or unknown path gives.
#[tokio::test]
async fn methods_do_not_cross_over() {
    let addr = spawn(ServeOptions {
        http_submit: true,
        ..Default::default()
    })
    .await;

    assert_eq!(
        post(&addr, "/nym-address", Vec::new()).await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        post(&addr, "/healthz", Vec::new()).await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let (status, _) = get(&addr, "/").await;
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "GET / must not submit — but the path exists here, so it is 405 not 404"
    );
    let (status, _) = get(&addr, "/transaction").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

/// And with the submit path gated OFF, `/` stops being a path at all: even a
/// wrong-method probe gets the unknown-path answer, so the two states cannot be
/// told apart from outside.
#[tokio::test]
async fn a_gated_submit_path_looks_like_no_path_at_all() {
    let addr = spawn(ServeOptions::default()).await;

    let (status, _) = get(&addr, "/").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (unknown, _) = get(&addr, "/nonsense").await;
    assert_eq!(status, unknown, "the two must be indistinguishable");
}
