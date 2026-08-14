//! The wallet-and-operator side of the divert harness, shared by `divert.rs`
//! (the clearnet HTTP transport) and `divert_nym.rs` (the mixnet transport).
//!
//! The connection-COUNTING backend is the load-bearing piece: it is what turns
//! "the operator never sees a migration" from a claim into an assertion, and
//! both transports must hold it at zero on every diverted send and every
//! hub-served lookup.

// Each integration-test binary compiles its own copy and uses its own subset.
#![allow(dead_code)]

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use bytes::Bytes;
use http::{HeaderMap, Request, Response};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http2 as client_h2;
use hyper::server::conn::http2 as server_h2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use tokio::net::{TcpListener, TcpStream};
use zaino_proto::proto::service::{RawTransaction, SendResponse, TxFilter};

use zero_indexer_shim::proxy::{GET_TRANSACTION, SEND_TRANSACTION};

/// V6 carrying Orchard actions: the migration shape that gets diverted.
pub const V6_MIGRATION: &[u8] = include_bytes!("../fixtures/v6_migration.bin");

/// V6 with an Ironwood bundle and no Orchard bundle: the pass-through case.
pub const V6_IRONWOOD_ONLY: &[u8] = include_bytes!("../fixtures/v6_ironwood_only.bin");

/// The display-order txid of a fixture, computed from the bytes with zebra-chain,
/// derived independently of the shim's own code path rather than read back from
/// it.
pub fn expected_txid(tx_bytes: &[u8]) -> String {
    use zebra_chain::serialization::ZcashDeserialize;
    zebra_chain::transaction::Transaction::zcash_deserialize(&mut std::io::Cursor::new(tx_bytes))
        .expect("the fixture parses")
        .hash()
        .to_string()
}

/// The wallet's wire-order `TxFilter.hash` for a fixture: the display txid's
/// bytes reversed, which is the internal (little-endian) order a wallet actually
/// sends. Since the shim verifies a lookup reply against the query (L4), a served
/// fixture must be queried by its real hash, not an arbitrary one.
pub fn wire_hash(tx_bytes: &[u8]) -> Vec<u8> {
    let mut bytes = hex::decode(expected_txid(tx_bytes)).expect("a hex txid");
    bytes.reverse();
    bytes
}

const LIMIT: StdDuration = StdDuration::from_secs(10);

pub async fn bounded<F: Future>(fut: F) -> F::Output {
    tokio::time::timeout(LIMIT, fut).await.expect("timed out")
}

pub fn grpc_frame(message: &[u8]) -> Bytes {
    let mut frame = Vec::with_capacity(5 + message.len());
    frame.push(0);
    frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
    frame.extend_from_slice(message);
    Bytes::from(frame)
}

/// A stub indexer that counts how many times it is CONNECTED, and answers any
/// request with a framed `SendResponse`. The count is the whole point: a
/// diverted migration and every hub-served GetTransaction must leave it at zero.
pub async fn spawn_counting_backend(connections: Arc<AtomicUsize>) -> SocketAddr {
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

/// A forward-only shim: no hub, so everything (including GetTransaction) passes
/// through to the operator.
pub async fn spawn_forward_only_shim(backend: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = zero_indexer_shim::serve_with_shutdown(
            listener,
            backend,
            None,
            None,
            zero_indexer_shim::CautionRelay::default(),
            std::future::pending::<()>(),
        )
        .await;
    });
    addr
}

/// An address that nothing listens on: bind a port, learn it, drop the listener.
/// Connecting to it is refused, which is how the hub-down test forces failure.
pub async fn dead_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

pub async fn connect_h2(shim: SocketAddr) -> client_h2::SendRequest<BoxBody<Bytes, Infallible>> {
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
pub async fn send_tx(
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
    bounded(response.into_body().collect())
        .await
        .unwrap()
        .to_bytes()
}

/// A gRPC reply, distilled to what the tests assert: the status code (from the
/// headers on a trailers-only error, or the trailers on a unary success) and the
/// message body.
pub struct GrpcReply {
    pub status: i32,
    pub body: Bytes,
}

/// Send one `SendTransaction` and return its status as well as its body.
///
/// A successful divert answers with a unary `SendResponse` (status 0, the
/// verdict inside the message), but a failed one answers with a gRPC status
/// error and an EMPTY body, so a test that only reads the body cannot tell the
/// two apart.
pub async fn send_tx_reply(
    sender: &mut client_h2::SendRequest<BoxBody<Bytes, Infallible>>,
    shim: SocketAddr,
    tx: &[u8],
) -> GrpcReply {
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
    let header_status = response
        .headers()
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let collected = bounded(response.into_body().collect()).await.unwrap();
    let trailer_status = collected
        .trailers()
        .and_then(|map| map.get("grpc-status"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let status = trailer_status
        .or(header_status)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    GrpcReply {
        status,
        body: collected.to_bytes(),
    }
}

/// Send one `GetTransaction` with the given `TxFilter` and return its reply.
pub async fn get_transaction_filter(
    sender: &mut client_h2::SendRequest<BoxBody<Bytes, Infallible>>,
    shim: SocketAddr,
    filter: TxFilter,
) -> GrpcReply {
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{shim}{GET_TRANSACTION}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(Full::new(grpc_frame(&filter.encode_to_vec())).boxed())
        .unwrap();
    sender.ready().await.unwrap();
    let response = bounded(sender.send_request(request)).await.unwrap();
    let header_status = response
        .headers()
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let collected = bounded(response.into_body().collect()).await.unwrap();
    let trailer_status = collected
        .trailers()
        .and_then(|map| map.get("grpc-status"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let status = trailer_status
        .or(header_status)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    GrpcReply {
        status,
        body: collected.to_bytes(),
    }
}

/// Send one `GetTransaction` for a 32-byte txid hash (a hash-only `TxFilter`).
pub async fn get_transaction(
    sender: &mut client_h2::SendRequest<BoxBody<Bytes, Infallible>>,
    shim: SocketAddr,
    txid_hash: &[u8],
) -> GrpcReply {
    get_transaction_filter(
        sender,
        shim,
        TxFilter {
            block: None,
            index: 0,
            hash: txid_hash.to_vec(),
        },
    )
    .await
}

/// Decode a unary `SendResponse` out of a framed gRPC body.
pub fn decode_send_response(framed: &[u8]) -> SendResponse {
    assert!(
        framed.len() >= 5,
        "response is at least a gRPC frame header"
    );
    SendResponse::decode(&framed[5..]).expect("a SendResponse")
}

/// Decode a unary `RawTransaction` out of a framed gRPC body.
pub fn decode_raw_transaction(framed: &[u8]) -> RawTransaction {
    assert!(
        framed.len() >= 5,
        "response is at least a gRPC frame header"
    );
    RawTransaction::decode(&framed[5..]).expect("a RawTransaction")
}
