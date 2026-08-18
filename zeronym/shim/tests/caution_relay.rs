//! The Caution control-plane relay gate ([`CautionRelay`]).
//!
//! `handle` owns `/attestation` and `/.well-known/caution/health` ONLY when the
//! relay is enabled; disabled, the shim is a pure proxy and both paths are
//! forwarded to the operator's indexer like any other request. The
//! connection-counting backend from `common` is the assertion, exactly as it is
//! for the divert tests: an enabled relay MUST keep these paths off the operator,
//! a disabled one MUST forward them. Regression guard for the `a6063ef` h2c
//! workaround being made configurable rather than unconditional.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use http::{Request, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http2 as client_h2;

use zero_indexer_shim::proxy::{CAUTION_ATTESTATION, CAUTION_HEALTH};
use zero_indexer_shim::CautionRelay;

use common::{bounded, connect_h2, dead_addr, spawn_counting_backend};

/// Start a shim in front of `backend` with a chosen relay config. Mirrors
/// `common::spawn_forward_only_shim`, but the relay is the variable under test.
async fn spawn_shim(backend: SocketAddr, caution: CautionRelay) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = zero_indexer_shim::serve_with_shutdown(
            listener,
            backend,
            None,
            None,
            caution,
            zero_indexer_shim::nym::MixnetStatus::default(),
            std::future::pending::<()>(),
        )
        .await;
    });
    addr
}

/// Send one request to `path` and return its HTTP status, draining the body.
async fn request_path(
    sender: &mut client_h2::SendRequest<BoxBody<Bytes, Infallible>>,
    shim: SocketAddr,
    method: &str,
    path: &str,
) -> StatusCode {
    let request = Request::builder()
        .method(method)
        .uri(format!("http://{shim}{path}"))
        .body(Full::new(Bytes::new()).boxed())
        .unwrap();
    sender.ready().await.unwrap();
    let response = bounded(sender.send_request(request)).await.unwrap();
    let status = response.status();
    let _ = bounded(response.into_body().collect()).await;
    status
}

/// Send one request and return its body as a string.
async fn body_of(
    sender: &mut client_h2::SendRequest<BoxBody<Bytes, Infallible>>,
    shim: SocketAddr,
    method: &str,
    path: &str,
) -> String {
    let request = Request::builder()
        .method(method)
        .uri(format!("http://{shim}{path}"))
        .body(Full::new(Bytes::new()).boxed())
        .unwrap();
    sender.ready().await.unwrap();
    let response = bounded(sender.send_request(request)).await.unwrap();
    let body = bounded(response.into_body().collect()).await.unwrap();
    String::from_utf8_lossy(&body.to_bytes()).into_owned()
}

/// The shim's own operator endpoints are answered LOCALLY and never proxied.
///
/// The point is observability on an attested shim, which has no SSH: without
/// these, a shim whose mixnet client is dead is indistinguishable from a healthy
/// one, because dispatch-only submit answers the wallet before the mixnet is
/// involved. The operator's indexer staying at zero connections is what proves
/// these are ours and not forwarded.
#[tokio::test]
async fn the_shim_serves_its_own_status_and_never_proxies_it() {
    use zero_indexer_shim::nym::MixnetStatus;

    let connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(connections.clone()).await;

    let status = MixnetStatus::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let shim = listener.local_addr().unwrap();
    let served = status.clone();
    tokio::spawn(async move {
        let _ = zero_indexer_shim::serve_with_shutdown(
            listener,
            backend,
            None,
            None,
            CautionRelay::default(),
            served,
            std::future::pending::<()>(),
        )
        .await;
    });

    let mut client = connect_h2(shim).await;

    // Liveness.
    assert_eq!(
        request_path(&mut client, shim, "GET", "/healthz").await,
        StatusCode::OK
    );

    // A forward-only shim reports honestly that diversion is not configured,
    // rather than claiming health it cannot have.
    let body = body_of(&mut client, shim, "GET", "/nym-status").await;
    assert!(
        body.contains("\"diversion_configured\":false"),
        "forward-only shim should not claim diversion: {body}"
    );

    // Once the driver has published its lifecycle, the endpoint reflects it.
    status.set_configured();
    status.set_connected();
    let body = body_of(&mut client, shim, "GET", "/nym-status").await;
    assert!(body.contains("\"diversion_configured\":true"), "{body}");
    assert!(body.contains("\"mixnet_connected\":true"), "{body}");

    // Connected: /healthz agrees. Checked here and at every state below,
    // because /healthz used to answer 200 unconditionally -- an uptime monitor
    // on an attested shim whose client had died saw "ok", the exact blind spot
    // the endpoint was added to close -- and this test passed either way. The
    // property that matters is that the two endpoints cannot DISAGREE with the
    // object they both read.
    assert_eq!(
        request_path(&mut client, shim, "GET", "/healthz").await,
        StatusCode::OK,
        "connected with diversion configured must be healthy"
    );

    // A death is visible, which is the whole point: this is the state that
    // silently swallows migrations. And /healthz must say so too: 503, not 200.
    status.set_died();
    let body = body_of(&mut client, shim, "GET", "/nym-status").await;
    assert!(body.contains("\"mixnet_connected\":false"), "{body}");
    assert!(body.contains("\"client_deaths\":1"), "{body}");
    assert_eq!(
        request_path(&mut client, shim, "GET", "/healthz").await,
        StatusCode::SERVICE_UNAVAILABLE,
        "a dead client with diversion configured must NOT read as healthy"
    );

    // Rebuild failing: still down, and the run of failures is visible.
    status.set_rebuild_failed();
    let body = body_of(&mut client, shim, "GET", "/nym-status").await;
    assert!(body.contains("\"mixnet_connected\":false"), "{body}");
    assert!(
        body.contains("\"consecutive_rebuild_failures\":1"),
        "{body}"
    );
    assert_eq!(
        request_path(&mut client, shim, "GET", "/healthz").await,
        StatusCode::SERVICE_UNAVAILABLE
    );

    // Reconnected: healthy again, the failure run resets, and deaths are
    // CUMULATIVE -- a reconnect does not erase the history an operator reads
    // churn from. If this ever resets to 0 the client_deaths alerting rule in
    // OPERATORS.md is silently defeated.
    status.set_connected();
    let body = body_of(&mut client, shim, "GET", "/nym-status").await;
    assert!(body.contains("\"mixnet_connected\":true"), "{body}");
    assert!(
        body.contains("\"consecutive_rebuild_failures\":0"),
        "{body}"
    );
    assert!(
        body.contains("\"client_deaths\":1"),
        "deaths are cumulative and must survive a reconnect: {body}"
    );
    assert_eq!(
        request_path(&mut client, shim, "GET", "/healthz").await,
        StatusCode::OK,
        "reconnected must be healthy again"
    );

    // NEVER an oracle for user traffic: no send counts, no timestamps, no txids,
    // and never the shim's own address (its sender identity).
    for forbidden in [
        "txid",
        "last",
        "sent",
        "diverted",
        "migration",
        "queue",
        "address",
        "depth",
        "pending",
    ] {
        assert!(
            !body.contains(forbidden),
            "status must not expose '{forbidden}': {body}"
        );
    }

    // And none of it was proxied to the operator's indexer.
    assert_eq!(
        connections.load(Ordering::SeqCst),
        0,
        "the shim's own endpoints must be answered locally"
    );
}

/// With the relay OFF, `/attestation` is not special: it dials the operator like
/// any pass-through path. The connection count going up is the proof.
#[tokio::test]
async fn disabled_relay_forwards_caution_paths_to_the_operator() {
    let connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(connections.clone()).await;
    // bootproofd deliberately unreachable: with the relay OFF it must never be
    // dialled anyway, so the address is immaterial.
    let shim = spawn_shim(
        backend,
        CautionRelay {
            enabled: false,
            bootproofd_addr: Arc::from("127.0.0.1:1"),
        },
    )
    .await;

    let mut client = connect_h2(shim).await;
    // The stub operator answers any path with a 200 gRPC frame.
    let status = request_path(&mut client, shim, "POST", CAUTION_ATTESTATION).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        connections.load(Ordering::SeqCst) >= 1,
        "a disabled relay must forward /attestation to the operator's indexer"
    );
}

/// With the relay ON, neither control-plane path reaches the operator:
/// `/attestation` is relayed to bootproofd (here a dead address, so it fails
/// rather than ever dialling the operator) and `/health` is answered locally.
#[tokio::test]
async fn enabled_relay_keeps_caution_paths_off_the_operator() {
    let connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(connections.clone()).await;
    // A dead bootproofd address: the relay tries it (and fails) instead of ever
    // dialling the operator, which is the property under test.
    let dead = dead_addr().await;
    let shim = spawn_shim(
        backend,
        CautionRelay {
            enabled: true,
            bootproofd_addr: Arc::from(dead.to_string()),
        },
    )
    .await;

    let mut client = connect_h2(shim).await;
    // Relayed to (dead) bootproofd, never the operator.
    let _ = request_path(&mut client, shim, "POST", CAUTION_ATTESTATION).await;
    // Answered locally with 200.
    let health = request_path(&mut client, shim, "GET", CAUTION_HEALTH).await;
    assert_eq!(health, StatusCode::OK, "health is answered locally");

    assert_eq!(
        connections.load(Ordering::SeqCst),
        0,
        "an enabled relay must keep Caution's control-plane paths off the operator"
    );
}
