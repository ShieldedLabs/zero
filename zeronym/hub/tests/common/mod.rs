//! The in-process mock INDEXER shared by the hub's integration tests: a tiny
//! gRPC responder that records every transaction it was asked to broadcast and
//! answers `GetTransaction` with a configurable verdict. Moved here verbatim
//! from `serve.rs` so the mixnet listener's lookup tests drive the same
//! indexer the HTTP tests do.

// Each integration-test binary compiles its own copy and uses its own subset.
#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use tokio::net::TcpListener;
use zaino_proto::proto::service::{RawTransaction, SendResponse, TxFilter};

const GET_TRANSACTION: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTransaction";

/// How the mock indexer answers a `GetTransaction`.
#[derive(Clone)]
pub enum GetTx {
    /// gRPC NOT_FOUND, the lightwalletd unknown-txid answer.
    NotFound,
    /// A framed `RawTransaction` with these bytes and height.
    Found { data: Vec<u8>, height: u64 },
}

/// A mock `CompactTxStreamer` that records broadcasts and answers
/// `GetTransaction` with NOT_FOUND. `seen` collects the raw bytes of every
/// transaction it was asked to broadcast; the shape the batching tests assert on.
pub async fn spawn_mock_indexer(
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
pub async fn spawn_mock_indexer_full(
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

/// An indexer that completes the TCP handshake and then never answers, holding
/// every request for the hub's full per-call budget.
///
/// This is what a half-dead operator indexer looks like from the hub, and it is
/// the condition under which the listener must still admit migrations: a
/// refused connection fails fast and proves nothing.
pub async fn spawn_hanging_indexer() -> SocketAddr {
    spawn_hanging_indexer_counting(Arc::new(AtomicUsize::new(0))).await
}

/// [`spawn_hanging_indexer`], plus a count of how many connections it has
/// accepted. Because every accepted socket is HELD, that count is exactly how
/// many lookups the hub currently has in flight against the indexer -- which
/// makes the listener's concurrency bound observable from outside the crate
/// without exposing the constant.
pub async fn spawn_hanging_indexer_counting(accepted: Arc<AtomicUsize>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Accepted sockets are parked here so the peer sees an open connection
        // rather than a reset.
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            accepted.fetch_add(1, Ordering::SeqCst);
            held.push(stream);
        }
    });
    addr
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
