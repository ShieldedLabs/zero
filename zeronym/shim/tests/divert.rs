//! The divert path, end to end.
//!
//! With a hub configured, an Orchard-touching `SendTransaction` must go to the
//! hub and the wallet must get its txid back, while the operator's indexer is
//! never even connected. A pass-through must still reach the operator and not
//! the hub. The connection-COUNTING backend is what turns "classify before
//! connect" from a claim into an assertion: a diverted migration leaves it at
//! zero.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use bytes::Bytes;
use http::{HeaderMap, Request, Response};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http2 as client_h2;
use hyper::server::conn::{http1 as server_h1, http2 as server_h2};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use tokio::net::{TcpListener, TcpStream};
use zaino_proto::proto::service::{RawTransaction, SendResponse};

use zero_indexer_shim::hub::HubClient;
use zero_indexer_shim::intercept::Diversion;
use zero_indexer_shim::proxy::SEND_TRANSACTION;
use zero_indexer_shim::state::DivertState;

/// V6 carrying Orchard actions.
const V6_MIGRATION: &[u8] = include_bytes!("fixtures/v6_migration.bin");
/// V6 with an Ironwood bundle and no Orchard bundle: the pass-through case.
const V6_IRONWOOD_ONLY: &[u8] = include_bytes!("fixtures/v6_ironwood_only.bin");

const LIMIT: StdDuration = StdDuration::from_secs(10);
async fn bounded<F: Future>(fut: F) -> F::Output {
    tokio::time::timeout(LIMIT, fut).await.expect("timed out")
}

fn grpc_frame(message: &[u8]) -> Bytes {
    let mut frame = Vec::with_capacity(5 + message.len());
    frame.push(0);
    frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
    frame.extend_from_slice(message);
    Bytes::from(frame)
}

// -------------------------------------------------------------- mock hub

/// A hub that records the exact bytes it was asked to broadcast and replies
/// `accepted` with a fixed txid.
async fn spawn_mock_hub(txid: &'static str, seen: Arc<Mutex<Option<Vec<u8>>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let _ = server_h1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |req: Request<Incoming>| {
                            let seen = seen.clone();
                            async move {
                                let body = req.into_body().collect().await.unwrap().to_bytes();
                                *seen.lock().unwrap() = Some(body.to_vec());
                                let reply =
                                    format!("{{\"disposition\":\"accepted\",\"txid\":\"{txid}\"}}");
                                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(reply))))
                            }
                        }),
                    )
                    .await;
            });
        }
    });
    addr
}

// ---------------------------------------------------- connection-counting backend

/// A stub indexer that counts how many times it is CONNECTED, and answers any
/// request with a framed `SendResponse`. The count is the whole point: a
/// diverted migration must leave it at zero.
async fn spawn_counting_backend(connections: Arc<AtomicUsize>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            connections.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let _ = server_h2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service_fn(stub_service))
                    .await;
            });
        }
    });
    addr
}

async fn stub_service(
    req: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, Infallible>>, Infallible> {
    let _ = req.into_body().collect().await;
    let message = SendResponse {
        error_code: 0,
        error_message: "operator-answered".to_owned(),
    }
    .encode_to_vec();
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", "0".parse().unwrap());
    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .body(
            Full::new(grpc_frame(&message))
                .with_trailers(async move { Some(Ok(trailers)) })
                .boxed(),
        )
        .unwrap())
}

// ------------------------------------------------------------------ harness

async fn spawn_diverting_shim(backend: SocketAddr, hub: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let diversion = Some(Arc::new(Diversion {
        hub: HubClient::new(hub, None),
        state: Arc::new(DivertState::new()),
    }));
    tokio::spawn(async move {
        let _ = zero_indexer_shim::serve_with_shutdown(
            listener,
            backend,
            None,
            diversion,
            std::future::pending::<()>(),
        )
        .await;
    });
    addr
}

async fn connect_h2(shim: SocketAddr) -> client_h2::SendRequest<BoxBody<Bytes, Infallible>> {
    let stream = TcpStream::connect(shim).await.unwrap();
    let (sender, conn) = client_h2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sender
}

/// Send one `SendTransaction` and return the collected response body bytes.
async fn send_tx(
    sender: &mut client_h2::SendRequest<BoxBody<Bytes, Infallible>>,
    shim: SocketAddr,
    tx: &[u8],
) -> Bytes {
    let message = RawTransaction {
        data: tx.to_vec(),
        height: 0,
    }
    .encode_to_vec();
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{shim}{SEND_TRANSACTION}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(Full::new(grpc_frame(&message)).boxed())
        .unwrap();
    sender.ready().await.unwrap();
    let response = bounded(sender.send_request(request)).await.unwrap();
    bounded(response.into_body().collect()).await.unwrap().to_bytes()
}

/// Decode a unary `SendResponse` out of a framed gRPC body.
fn decode_send_response(framed: &[u8]) -> SendResponse {
    assert!(framed.len() >= 5, "response is at least a gRPC frame header");
    SendResponse::decode(&framed[5..]).expect("a SendResponse")
}

// -------------------------------------------------------------------- tests

#[tokio::test]
async fn a_migration_is_diverted_and_the_operator_is_never_connected() {
    let txid = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let hub_seen = Arc::new(Mutex::new(None));
    let hub = spawn_mock_hub(txid, hub_seen.clone()).await;
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let shim = spawn_diverting_shim(backend, hub).await;

    let mut sender = connect_h2(shim).await;
    let body = send_tx(&mut sender, shim, V6_MIGRATION).await;

    // The wallet gets a synthetic success carrying the hub's txid, indistinguishable
    // from a real indexer's reply.
    let resp = decode_send_response(&body);
    assert_eq!(resp.error_code, 0);
    assert_eq!(resp.error_message, txid);

    // The hub received the exact migration bytes.
    assert_eq!(hub_seen.lock().unwrap().as_deref(), Some(V6_MIGRATION));

    // The property this whole reorder exists for: the operator's indexer was
    // never even connected for a diverted migration.
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "classify-before-connect: a diverted migration must not dial the operator"
    );
}

#[tokio::test]
async fn a_pass_through_still_reaches_the_operator_and_not_the_hub() {
    let hub_seen = Arc::new(Mutex::new(None));
    let hub = spawn_mock_hub("unused", hub_seen.clone()).await;
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let shim = spawn_diverting_shim(backend, hub).await;

    let mut sender = connect_h2(shim).await;
    let body = send_tx(&mut sender, shim, V6_IRONWOOD_ONLY).await;

    // The operator answered (its own message, not the hub's), so it was forwarded.
    let resp = decode_send_response(&body);
    assert_eq!(resp.error_message, "operator-answered");
    assert!(
        backend_conns.load(Ordering::SeqCst) >= 1,
        "a pass-through must reach the operator's indexer"
    );
    assert!(
        hub_seen.lock().unwrap().is_none(),
        "a pass-through must never be sent to the hub"
    );
}
