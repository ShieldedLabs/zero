//! Broadcast-only mode (`ZIS_BROADCAST_ONLY`).
//!
//! With the mode on the shim relays `SendTransaction` and `GetTransaction` and
//! answers every other gRPC method with UNIMPLEMENTED (12). The
//! connection-counting backend from `common` is the assertion throughout: a
//! served method reaches the operator's indexer exactly as it does with the
//! mode off, and a rejected one leaves the count at zero. `divert.rs` covers
//! the same mode with a hub configured.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, Request, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::Frame;
use hyper::client::conn::http2 as client_h2;
use prost::Message;
use tokio_stream::StreamExt;
use zaino_proto::proto::service::{RawTransaction, TxFilter};

use zero_indexer_shim::nym::MixnetStatus;
use zero_indexer_shim::proxy::{
    CAUTION_ATTESTATION, CAUTION_HEALTH, GET_TRANSACTION, SEND_TRANSACTION, SHIM_HEALTH,
    SHIM_NYM_DIAG, SHIM_NYM_STATUS,
};
use zero_indexer_shim::CautionRelay;

use common::{
    bounded, connect_h2, dead_addr, decode_send_response, get_transaction, grpc_call, grpc_frame,
    grpc_web_call, send_tx_reply, spawn_counting_backend, spawn_forward_only_shim,
    V6_IRONWOOD_ONLY,
};

const SERVICE: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer";
const LATEST_BLOCK: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLatestBlock";
const BLOCK_RANGE: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRange";

/// The `grpc-message` every rejected request carries, whatever its path.
const REJECTED: &str = "zero-indexer-shim: method not served (broadcast-only)";

const WEB_CONTENT_TYPES: [&str; 2] = ["application/grpc-web+proto", "application/grpc-web-text"];

/// Start a shim in front of `backend` with broadcast-only mode ON and no hub.
/// The relay and the status handle are parameters because both decide what the
/// mode still serves.
async fn spawn_broadcast_only_shim(
    backend: SocketAddr,
    caution: CautionRelay,
    status: MixnetStatus,
) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = zero_indexer_shim::serve_with_shutdown(
            listener,
            backend,
            None,
            None,
            caution,
            true,
            status,
            std::future::pending::<()>(),
        )
        .await;
    });
    addr
}

/// A counting backend and a broadcast-only shim in front of it, with the
/// default relay and a status handle nothing has written to.
async fn default_shim() -> (SocketAddr, Arc<AtomicUsize>) {
    let connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(connections.clone()).await;
    let shim =
        spawn_broadcast_only_shim(backend, CautionRelay::default(), MixnetStatus::default()).await;
    (shim, connections)
}

fn raw_transaction(tx: &[u8]) -> Vec<u8> {
    RawTransaction {
        data: tx.to_vec().into(),
        height: 0,
    }
    .encode_to_vec()
}

/// What a non-gRPC client sees: the HTTP status, the response headers and the
/// collected body.
struct PlainReply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

/// Send one request with an empty body and no gRPC headers.
async fn plain_request(
    sender: &mut client_h2::SendRequest<BoxBody<Bytes, Infallible>>,
    shim: SocketAddr,
    method: &str,
    path: &str,
) -> PlainReply {
    let request = Request::builder()
        .method(method)
        .uri(format!("http://{shim}{path}"))
        .body(Full::new(Bytes::new()).boxed())
        .unwrap();
    sender.ready().await.unwrap();
    let response = bounded(sender.send_request(request)).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = bounded(response.into_body().collect())
        .await
        .unwrap()
        .to_bytes();
    PlainReply {
        status,
        headers,
        body,
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

#[tokio::test]
async fn send_transaction_is_relayed_to_the_operator() {
    let (shim, connections) = default_shim().await;

    let mut sender = connect_h2(shim).await;
    let reply = send_tx_reply(&mut sender, shim, V6_IRONWOOD_ONLY).await;

    assert_eq!(reply.status, 0);
    assert_eq!(
        decode_send_response(&reply.body).error_message,
        "operator-answered"
    );
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn get_transaction_is_relayed_to_the_operator_when_there_is_no_hub() {
    let (shim, connections) = default_shim().await;

    let mut sender = connect_h2(shim).await;
    let reply = get_transaction(&mut sender, shim, &[0x66u8; 32]).await;

    // The stub answers every path with the same framed message, so its reply
    // coming back is what shows the call was relayed.
    assert_eq!(reply.status, 0);
    assert_eq!(
        decode_send_response(&reply.body).error_message,
        "operator-answered"
    );
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn both_served_methods_work_over_grpc_web_in_both_encodings() {
    let (shim, connections) = default_shim().await;

    let send = raw_transaction(V6_IRONWOOD_ONLY);
    let lookup = TxFilter {
        block: None,
        index: 0,
        hash: vec![0x66u8; 32],
    }
    .encode_to_vec();

    let mut sender = connect_h2(shim).await;
    for content_type in WEB_CONTENT_TYPES {
        for (path, message) in [(SEND_TRANSACTION, &send), (GET_TRANSACTION, &lookup)] {
            let reply = grpc_web_call(&mut sender, shim, path, content_type, message).await;
            assert_eq!(reply.messages.len(), 1, "{content_type} {path}");
            assert!(
                reply.trailers.contains("grpc-status:0"),
                "{content_type} {path}: {:?}",
                reply.trailers
            );
        }
    }
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn paths_that_spell_send_transaction_are_classified_as_they_are_with_the_mode_off() {
    let (shim, connections) = default_shim().await;
    let off_connections = Arc::new(AtomicUsize::new(0));
    let off_backend = spawn_counting_backend(off_connections.clone()).await;
    let off_shim = spawn_forward_only_shim(off_backend).await;

    let message = raw_transaction(V6_IRONWOOD_ONLY);
    let mut sender = connect_h2(shim).await;
    let mut off_sender = connect_h2(off_shim).await;

    // The paths `near_miss_paths_fail_safe_toward_the_classifier` pins in
    // `proxy.rs`.
    for path in [
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/sendtransaction",
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SENDTRANSACTION",
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction/",
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction//",
        "/some.other.Service/SendTransaction",
    ] {
        let body = || Full::new(grpc_frame(&message)).boxed();
        let on = grpc_call(&mut sender, shim, path, body()).await;
        let off = grpc_call(&mut off_sender, off_shim, path, body()).await;

        assert_ne!(on.status, 12, "{path} must not be rejected");
        assert_eq!(
            decode_send_response(&on.body).error_message,
            "operator-answered",
            "{path} must reach the operator on a pass-through verdict"
        );
        assert_eq!(on.status, off.status, "{path}");
        assert_eq!(on.message, off.message, "{path}");
        assert_eq!(on.body, off.body, "{path}");
    }

    assert_eq!(connections.load(Ordering::SeqCst), 1);
    assert_eq!(off_connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn every_other_method_is_unimplemented_and_the_operator_is_never_connected() {
    let (shim, connections) = default_shim().await;

    let methods = [
        "GetLightdInfo",
        "GetLatestBlock",
        "GetBlock",
        "GetBlockRange",
        "GetTreeState",
        "GetLatestTreeState",
        "GetSubtreeRoots",
        "GetMempoolTx",
        "GetMempoolStream",
        "GetTaddressTxids",
        "GetTaddressBalanceStream",
        "Ping",
        // A trailing slash is a different path from the served method.
        "GetTransaction/",
    ];
    let mut paths: Vec<String> = methods
        .iter()
        .map(|method| format!("{SERVICE}/{method}"))
        .collect();
    paths.extend(
        [
            "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
            "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo",
            "/grpc.health.v1.Health/Check",
            "/",
            "/no/such/path",
        ]
        .map(str::to_owned),
    );

    let mut sender = connect_h2(shim).await;
    for path in &paths {
        let reply = grpc_call(&mut sender, shim, path, Full::new(grpc_frame(&[])).boxed()).await;
        assert_eq!(reply.status, 12, "{path}");
        assert_eq!(reply.message.as_deref(), Some(REJECTED), "{path}");
        assert!(reply.body.is_empty(), "{path}");
    }

    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_rejected_grpc_web_call_carries_its_status_in_the_response_headers() {
    let (shim, connections) = default_shim().await;

    let mut sender = connect_h2(shim).await;
    for content_type in WEB_CONTENT_TYPES {
        for path in [LATEST_BLOCK, BLOCK_RANGE] {
            let reply = grpc_web_call(&mut sender, shim, path, content_type, &[]).await;
            let context = format!("{content_type} {path}");

            // A browser reads the status of a call that produced no message
            // from the headers, so that is where it must be.
            assert_eq!(
                header(&reply.headers, "grpc-status"),
                Some("12"),
                "{context}"
            );
            assert_eq!(
                header(&reply.headers, "grpc-message"),
                Some(REJECTED),
                "{context}"
            );
            assert!(reply.messages.is_empty(), "{context}");
            assert!(reply.trailers.is_empty(), "{context}: {:?}", reply.trailers);

            let response_type = header(&reply.headers, "content-type").unwrap_or_default();
            assert!(
                response_type.starts_with("application/grpc-web"),
                "{context}: content-type {response_type}"
            );
            assert_eq!(
                header(&reply.headers, "access-control-allow-origin"),
                Some("*"),
                "{context}"
            );
            let exposed = header(&reply.headers, "access-control-expose-headers")
                .unwrap_or_default()
                .to_ascii_lowercase();
            assert!(
                exposed.contains("grpc-status"),
                "{context}: expose-headers {exposed}"
            );
        }
    }

    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_rejected_call_is_answered_without_its_body_being_read() {
    let (shim, connections) = default_shim().await;

    // One frame, then a body that never ends: a reply can only arrive if the
    // shim answers without waiting for the end of the request.
    let first = tokio_stream::iter([Ok::<_, Infallible>(Frame::data(grpc_frame(&[0u8; 16])))]);
    let body = BoxBody::new(StreamBody::new(first.chain(tokio_stream::pending())));

    let mut sender = connect_h2(shim).await;
    let reply = grpc_call(&mut sender, shim, BLOCK_RANGE, body).await;

    assert_eq!(reply.status, 12);
    assert!(reply.body.is_empty());
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_local_endpoints_still_answer() {
    let (shim, connections) = default_shim().await;

    let mut sender = connect_h2(shim).await;
    for path in [SHIM_HEALTH, SHIM_NYM_STATUS, CAUTION_HEALTH] {
        let reply = plain_request(&mut sender, shim, "GET", path).await;
        assert_eq!(reply.status, StatusCode::OK, "{path}");
        assert!(
            reply.headers.get("grpc-status").is_none(),
            "{path} is answered by its own handler"
        );
    }

    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_closed_nym_diag_is_rejected_exactly_like_an_unknown_path() {
    let (shim, connections) = default_shim().await;

    let mut sender = connect_h2(shim).await;
    let diag = plain_request(&mut sender, shim, "GET", SHIM_NYM_DIAG).await;
    let unknown = plain_request(&mut sender, shim, "GET", "/no/such/path").await;

    assert_eq!(header(&diag.headers, "grpc-status"), Some("12"));
    assert_eq!(diag.status, unknown.status);
    assert_eq!(
        header(&diag.headers, "grpc-status"),
        header(&unknown.headers, "grpc-status")
    );
    assert_eq!(
        header(&diag.headers, "grpc-message"),
        header(&unknown.headers, "grpc-message")
    );
    assert_eq!(diag.body, unknown.body);
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_open_nym_diag_still_answers() {
    let connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(connections.clone()).await;
    let status = MixnetStatus::default();
    status.enable_diag();
    let shim = spawn_broadcast_only_shim(backend, CautionRelay::default(), status).await;

    let mut sender = connect_h2(shim).await;
    let reply = plain_request(&mut sender, shim, "GET", SHIM_NYM_DIAG).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        header(&reply.headers, "content-type"),
        Some("application/json")
    );
    assert!(reply.headers.get("grpc-status").is_none());
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn caution_paths_are_rejected_when_the_relay_is_off() {
    let connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(connections.clone()).await;
    let shim = spawn_broadcast_only_shim(
        backend,
        CautionRelay {
            enabled: false,
            bootproofd_addr: Arc::from("127.0.0.1:1"),
        },
        MixnetStatus::default(),
    )
    .await;

    let mut sender = connect_h2(shim).await;
    for (method, path) in [("GET", CAUTION_HEALTH), ("POST", CAUTION_ATTESTATION)] {
        let reply = plain_request(&mut sender, shim, method, path).await;
        assert_eq!(header(&reply.headers, "grpc-status"), Some("12"), "{path}");
    }

    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn attestation_is_still_relayed_to_bootproofd_when_the_relay_is_on() {
    let connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(connections.clone()).await;
    // Nothing listens at this address, so a relay that was attempted answers
    // with its own failure.
    let dead = dead_addr().await;
    let shim = spawn_broadcast_only_shim(
        backend,
        CautionRelay {
            enabled: true,
            bootproofd_addr: Arc::from(dead.to_string()),
        },
        MixnetStatus::default(),
    )
    .await;

    let mut sender = connect_h2(shim).await;
    let reply = grpc_call(
        &mut sender,
        shim,
        CAUTION_ATTESTATION,
        Full::new(Bytes::new()).boxed(),
    )
    .await;

    assert_ne!(reply.status, 12, "/attestation must not be rejected");
    assert_eq!(reply.status, 14, "the relay was attempted and failed");
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_cors_preflight_for_a_rejected_method_is_still_answered() {
    let (shim, connections) = default_shim().await;

    let mut sender = connect_h2(shim).await;
    let request = Request::builder()
        .method("OPTIONS")
        .uri(format!("http://{shim}{BLOCK_RANGE}"))
        .header("origin", "chrome-extension://abcdefghijklmnop")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "content-type,x-grpc-web")
        .body(BoxBody::new(Empty::<Bytes>::new()))
        .unwrap();
    sender.ready().await.unwrap();
    let response = bounded(sender.send_request(request)).await.unwrap();

    assert_eq!(response.status(), 200);
    let headers = response.headers();
    assert_eq!(header(headers, "access-control-allow-origin"), Some("*"));
    let allowed = header(headers, "access-control-allow-headers").unwrap_or_default();
    assert!(allowed.contains("x-grpc-web"), "allow-headers: {allowed}");
    assert!(headers.get("grpc-status").is_none());
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}
