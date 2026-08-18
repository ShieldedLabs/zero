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
        for forbidden in [
            "queue", "batch", "depth", "pending", "txid", "size", "count",
        ] {
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

/// A dead mixnet client must be VISIBLE, even though its address is kept.
///
/// This is the regression this endpoint exists for. Measured 2026-08-14: the
/// attested hub answered `/nym-address` 200 and `/healthz` 200 for hours while
/// answering no mixnet traffic at all, because `NymAddress` had no way to say
/// "connected" separately from "has ever connected". An afternoon went into
/// suspecting the mixnet, the gateways and the send rate; a local shim and hub
/// then round-tripped a lookup over the real public mixnet in 5.6 s, which is
/// what finally pointed back here.
#[tokio::test]
async fn nym_status_distinguishes_a_dead_client_from_a_live_one() {
    let nym_address = NymAddress::unknown();
    let addr = spawn(ServeOptions {
        nym_address: nym_address.clone(),
        ..Default::default()
    })
    .await;

    // Before any client has connected: nothing published, nothing connected.
    let (status, body) = get(&addr, "/nym-status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"mixnet_connected\":false"), "{body}");
    assert!(body.contains("\"address_published\":false"), "{body}");

    // Connected: the address is published and the client is live.
    nym_address.set("ident.enc@gateway".to_owned());
    let (_, body) = get(&addr, "/nym-status").await;
    assert!(body.contains("\"mixnet_connected\":true"), "{body}");
    assert!(body.contains("\"address_published\":true"), "{body}");

    // THE CASE THAT WAS INVISIBLE. The client dies: the address is deliberately
    // KEPT (shims are baked against it and it returns on rebuild), so
    // /nym-address still answers 200 with the same value...
    nym_address.set_died();
    let (status, address_body) = get(&addr, "/nym-address").await;
    assert_eq!(status, StatusCode::OK, "the address survives the client");
    assert_eq!(address_body.trim(), "ident.enc@gateway");

    // ...but the hub is no longer reachable over the mixnet, and now says so.
    let (_, body) = get(&addr, "/nym-status").await;
    assert!(
        body.contains("\"mixnet_connected\":false"),
        "a dead client must not read as healthy: {body}"
    );
    assert!(body.contains("\"address_published\":true"), "{body}");
    assert!(body.contains("\"client_deaths\":1"), "{body}");

    // Never an oracle for how much is in flight: that is the anonymity-set size.
    for forbidden in ["queue", "depth", "pending", "batch", "txid", "admitted"] {
        assert!(
            !body.contains(forbidden),
            "must not expose '{forbidden}': {body}"
        );
    }
}

/// A dead mixnet client and a dead mixnet DRIVER are different states, and the
/// endpoint must not report them the same way.
///
/// `set_died` describes a recoverable condition: the client is down and the
/// driver is actively rebuilding it, so an operator should wait. A driver that
/// has returned or panicked is not recoverable, and nothing else in the process
/// notices: `tokio::spawn` hands back a `JoinHandle` that carries the panic, and
/// dropping that handle discards it. The hub then stays up with `/healthz`
/// answering 200 forever while accepting nothing over the mixnet -- the exact
/// shape of the 2026-08-14 afternoon, one level further up.
///
/// The hub deliberately does NOT exit when this happens: its clearnet ingress
/// and its flush cadence are independent of the mixnet, and dying would drop a
/// queue of migrations that shims believe are on their way. Staying up is
/// correct; staying up while claiming to be reachable is not.
#[tokio::test]
async fn nym_status_separates_a_client_being_rebuilt_from_a_driver_that_is_gone() {
    let nym_address = NymAddress::unknown();
    let addr = spawn(ServeOptions {
        nym_address: nym_address.clone(),
        ..Default::default()
    })
    .await;

    nym_address.set("ident.enc@gateway".to_owned());
    let (_, body) = get(&addr, "/nym-status").await;
    assert!(body.contains("\"driver_running\":true"), "{body}");

    // A client death alone leaves the driver running: somebody is working on it.
    nym_address.set_died();
    let (_, body) = get(&addr, "/nym-status").await;
    assert!(body.contains("\"mixnet_connected\":false"), "{body}");
    assert!(
        body.contains("\"driver_running\":true"),
        "a client death is recoverable and must not read as a dead driver: {body}"
    );

    // A rebuild failure likewise: still down, still being worked on.
    nym_address.set_rebuild_failed();
    let (_, body) = get(&addr, "/nym-status").await;
    assert!(
        body.contains("\"driver_running\":true"),
        "a failed rebuild attempt is the driver WORKING: {body}"
    );

    // The driver task itself exiting is the terminal state, and the only one
    // that tells an operator to restart the hub rather than wait.
    nym_address.set_driver_exited();
    let (_, body) = get(&addr, "/nym-status").await;
    assert!(
        body.contains("\"driver_running\":false"),
        "a driver that returned or panicked must be visible: {body}"
    );
    assert!(
        body.contains("\"mixnet_connected\":false"),
        "a gone driver is definitionally not connected: {body}"
    );

    // Still no oracle for how much is in flight.
    for forbidden in ["queue", "depth", "pending", "batch", "txid", "admitted"] {
        assert!(
            !body.contains(forbidden),
            "must not expose '{forbidden}': {body}"
        );
    }
}

/// A client that sends headers and then stops must not hold a connection open
/// forever.
///
/// The body caps bound how MUCH a client may send. They say nothing about how
/// LONG it may take, and hyper's `header_read_timeout` stops applying the moment
/// the head is complete -- so before this deadline existed, "Content-Length: 64"
/// followed by silence held a connection and a task until the peer gave up.
/// Doing that a few thousand times is the whole attack, and it needs no valid
/// transaction and no credentials.
///
/// Time is paused, so this asserts the deadline EXISTS and fires rather than
/// spending its wall-clock duration proving it.
#[tokio::test(start_paused = true)]
async fn a_body_that_never_arrives_is_timed_out_rather_than_held() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let addr = spawn(ServeOptions::default()).await;
    let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();

    // A complete, well-formed head promising a body that will never come. The
    // lookup path is used because it is open unconditionally; the deadline is
    // the same one on both paths.
    sock.write_all(b"POST /transaction HTTP/1.1\r\nHost: hub\r\nContent-Length: 64\r\n\r\n")
        .await
        .unwrap();
    sock.flush().await.unwrap();

    let mut response = Vec::new();
    // Returns when the hub answers and closes, which only happens if something
    // gave up on the body. Without the deadline this read never completes.
    sock.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8_lossy(&response);

    assert!(
        response.starts_with("HTTP/1.1 408"),
        "a stalled body must be refused as a timeout, got:\n{response}"
    );
}
