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
