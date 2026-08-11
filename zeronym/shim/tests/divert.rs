//! The divert and lookup paths, end to end.
//!
//! With a hub configured, an Orchard-touching `SendTransaction` goes to the hub
//! and EVERY `GetTransaction` is answered by the hub, while the operator's
//! indexer is never even connected on either path. The connection-COUNTING
//! backend is what turns that from a claim into an assertion: it must stay at
//! zero. Forward-only mode (no hub) still passes everything through.
//!
//! The shim keeps no state, so there is nothing to "divert first" and then look
//! up: the hub is the source of truth for a lookup, and these tests drive the
//! hub's answer directly.

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
use zaino_proto::proto::service::{BlockId, RawTransaction, SendResponse, TxFilter};

use zero_indexer_shim::hub::HubClient;
use zero_indexer_shim::intercept::Diversion;
use zero_indexer_shim::proxy::{GET_TRANSACTION, SEND_TRANSACTION};

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

/// How the mock hub answers a `POST /transaction` lookup.
#[derive(Clone)]
enum HubLookup {
    /// `200` + raw bytes + `x-tx-height`, the normal hub reply.
    Found { data: Vec<u8>, height: u64 },
    /// `404`, the "no such transaction" answer.
    NotFound,
    /// `200` carrying a submission's JSON and NO `x-tx-height`: what an OLD hub
    /// (which treats every POST as a submission) would return. The shim must fail
    /// closed rather than frame this as a transaction.
    OldHubJson,
}

/// A hub that records submit bodies and replies `accepted` with `submit_txid`,
/// answering lookups with NOT_FOUND. The two-argument shape the divert tests use.
async fn spawn_mock_hub(
    submit_txid: &'static str,
    submit_seen: Arc<Mutex<Option<Vec<u8>>>>,
) -> SocketAddr {
    spawn_mock_hub_full(
        submit_txid,
        HubLookup::NotFound,
        submit_seen,
        Arc::new(Mutex::new(None)),
    )
    .await
}

/// The path-aware mock hub: `POST /` is a submission (records the body, replies
/// accepted JSON with `submit_txid`); `POST /transaction` is a lookup (records
/// the posted hash, replies per `lookup`).
async fn spawn_mock_hub_full(
    submit_txid: &'static str,
    lookup: HubLookup,
    submit_seen: Arc<Mutex<Option<Vec<u8>>>>,
    lookup_seen: Arc<Mutex<Option<Vec<u8>>>>,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let submit_seen = submit_seen.clone();
            let lookup_seen = lookup_seen.clone();
            let lookup = lookup.clone();
            tokio::spawn(async move {
                let _ = server_h1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |req: Request<Incoming>| {
                            let submit_seen = submit_seen.clone();
                            let lookup_seen = lookup_seen.clone();
                            let lookup = lookup.clone();
                            async move {
                                let is_lookup = req.uri().path() == "/transaction";
                                let body = req.into_body().collect().await.unwrap().to_bytes();
                                if is_lookup {
                                    *lookup_seen.lock().unwrap() = Some(body.to_vec());
                                    Ok::<_, Infallible>(hub_lookup_reply(&lookup))
                                } else {
                                    *submit_seen.lock().unwrap() = Some(body.to_vec());
                                    let json = format!(
                                        "{{\"disposition\":\"accepted\",\"txid\":\"{submit_txid}\"}}"
                                    );
                                    Ok(Response::new(Full::new(Bytes::from(json))))
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

fn hub_lookup_reply(lookup: &HubLookup) -> Response<Full<Bytes>> {
    match lookup {
        HubLookup::Found { data, height } => Response::builder()
            .status(200)
            .header("content-type", "application/octet-stream")
            .header("x-tx-height", height.to_string())
            .body(Full::new(Bytes::from(data.clone())))
            .unwrap(),
        HubLookup::NotFound => Response::builder()
            .status(404)
            .body(Full::new(Bytes::from("transaction not found")))
            .unwrap(),
        HubLookup::OldHubJson => Response::builder()
            .status(200)
            .body(Full::new(Bytes::from(
                "{\"disposition\":\"accepted\",\"txid\":\"deadbeef\"}",
            )))
            .unwrap(),
    }
}

// ---------------------------------------------------- connection-counting backend

/// A stub indexer that counts how many times it is CONNECTED, and answers any
/// request with a framed `SendResponse`. The count is the whole point: a
/// diverted migration and every hub-served GetTransaction must leave it at zero.
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
        hub: HubClient::new(hub, None).into(),
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

/// A forward-only shim: no hub, so everything (including GetTransaction) passes
/// through to the operator.
async fn spawn_forward_only_shim(backend: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = zero_indexer_shim::serve_with_shutdown(
            listener,
            backend,
            None,
            None,
            std::future::pending::<()>(),
        )
        .await;
    });
    addr
}

/// An address that nothing listens on: bind a port, learn it, drop the listener.
/// Connecting to it is refused, which is how the hub-down test forces failure.
async fn dead_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
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
    bounded(response.into_body().collect())
        .await
        .unwrap()
        .to_bytes()
}

/// A gRPC reply, distilled to what the tests assert: the status code (from the
/// headers on a trailers-only error, or the trailers on a unary success) and the
/// message body.
struct GrpcReply {
    status: i32,
    body: Bytes,
}

/// Send one `GetTransaction` with the given `TxFilter` and return its reply.
async fn get_transaction_filter(
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
async fn get_transaction(
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
fn decode_send_response(framed: &[u8]) -> SendResponse {
    assert!(
        framed.len() >= 5,
        "response is at least a gRPC frame header"
    );
    SendResponse::decode(&framed[5..]).expect("a SendResponse")
}

/// Decode a unary `RawTransaction` out of a framed gRPC body.
fn decode_raw_transaction(framed: &[u8]) -> RawTransaction {
    assert!(
        framed.len() >= 5,
        "response is at least a gRPC frame header"
    );
    RawTransaction::decode(&framed[5..]).expect("a RawTransaction")
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

    // The wallet gets a synthetic success carrying the hub's txid.
    let resp = decode_send_response(&body);
    assert_eq!(resp.error_code, 0);
    assert_eq!(resp.error_message, txid);

    // The hub received the exact migration bytes.
    assert_eq!(hub_seen.lock().unwrap().as_deref(), Some(V6_MIGRATION));

    // The operator's indexer was never even connected for a diverted migration.
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

#[tokio::test]
async fn a_get_transaction_is_answered_by_the_hub_and_the_operator_is_never_dialled() {
    let hub_seen = Arc::new(Mutex::new(None));
    let looked_up = Arc::new(Mutex::new(None));
    let hub = spawn_mock_hub_full(
        "unused",
        HubLookup::Found {
            data: V6_MIGRATION.to_vec(),
            height: 0,
        },
        hub_seen,
        looked_up.clone(),
    )
    .await;
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let shim = spawn_diverting_shim(backend, hub).await;

    let mut sender = connect_h2(shim).await;
    let hash = [0x11u8; 32];
    let reply = get_transaction(&mut sender, shim, &hash).await;

    // The hub's transaction is relayed to the wallet as a normal reply.
    assert_eq!(reply.status, 0);
    let raw = decode_raw_transaction(&reply.body);
    assert_eq!(raw.data, V6_MIGRATION);
    assert_eq!(raw.height, 0);

    // The hub was asked, with the wallet's bytes unmodified.
    assert_eq!(looked_up.lock().unwrap().as_deref(), Some(&hash[..]));
    // And the operator's indexer was never dialled.
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "a hub-served GetTransaction must not dial the operator"
    );
}

#[tokio::test]
async fn get_transaction_height_from_the_hub_is_relayed() {
    let hub = spawn_mock_hub_full(
        "unused",
        HubLookup::Found {
            data: V6_MIGRATION.to_vec(),
            height: 424242,
        },
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(None)),
    )
    .await;
    let backend = spawn_counting_backend(Arc::new(AtomicUsize::new(0))).await;
    let shim = spawn_diverting_shim(backend, hub).await;

    let mut sender = connect_h2(shim).await;
    let reply = get_transaction(&mut sender, shim, &[0x22u8; 32]).await;
    assert_eq!(decode_raw_transaction(&reply.body).height, 424242);
}

#[tokio::test]
async fn an_unknown_txid_is_not_found_and_never_touches_the_operator() {
    let looked_up = Arc::new(Mutex::new(None));
    let hub = spawn_mock_hub_full(
        "unused",
        HubLookup::NotFound,
        Arc::new(Mutex::new(None)),
        looked_up.clone(),
    )
    .await;
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let shim = spawn_diverting_shim(backend, hub).await;

    let mut sender = connect_h2(shim).await;
    let reply = get_transaction(&mut sender, shim, &[0x55u8; 32]).await;

    assert_eq!(reply.status, 5, "unknown txid maps to gRPC NOT_FOUND");
    assert!(
        looked_up.lock().unwrap().is_some(),
        "the hub was asked (and answered not-found)"
    );
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "a not-found lookup must never fall back to the operator"
    );
}

#[tokio::test]
async fn hub_down_get_transaction_fails_closed() {
    // The single most important property: the hub being unreachable must NOT
    // send the query to the operator. It becomes UNAVAILABLE instead.
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let shim = spawn_diverting_shim(backend, dead_addr().await).await;

    let mut sender = connect_h2(shim).await;
    let reply = get_transaction(&mut sender, shim, &[0x33u8; 32]).await;

    assert_eq!(reply.status, 14, "hub unreachable maps to gRPC UNAVAILABLE");
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "failing closed means the operator is never dialled"
    );
}

#[tokio::test]
async fn an_old_hub_shaped_reply_fails_closed() {
    // A hub that answers a lookup with submission JSON (no x-tx-height) is an old
    // hub. The shim must refuse rather than frame that JSON as a transaction.
    let hub = spawn_mock_hub_full(
        "unused",
        HubLookup::OldHubJson,
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(None)),
    )
    .await;
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let shim = spawn_diverting_shim(backend, hub).await;

    let mut sender = connect_h2(shim).await;
    let reply = get_transaction(&mut sender, shim, &[0x44u8; 32]).await;

    assert_eq!(reply.status, 14, "an unrecognised 200 fails closed");
    assert_eq!(backend_conns.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn forward_only_get_transaction_still_passes_through() {
    // No hub: a GetTransaction must reach the operator, exactly as before.
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let shim = spawn_forward_only_shim(backend).await;

    let mut sender = connect_h2(shim).await;
    let _ = get_transaction(&mut sender, shim, &[0x66u8; 32]).await;

    assert!(
        backend_conns.load(Ordering::SeqCst) >= 1,
        "forward-only mode must reach the operator's indexer"
    );
}

#[tokio::test]
async fn a_bad_hash_length_is_invalid_argument_without_dialling_anyone() {
    let looked_up = Arc::new(Mutex::new(None));
    let hub = spawn_mock_hub_full(
        "unused",
        HubLookup::NotFound,
        Arc::new(Mutex::new(None)),
        looked_up.clone(),
    )
    .await;
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let shim = spawn_diverting_shim(backend, hub).await;

    let mut sender = connect_h2(shim).await;
    let reply = get_transaction(&mut sender, shim, &[0x77u8; 17]).await;

    assert_eq!(reply.status, 3, "a wrong-length hash is INVALID_ARGUMENT");
    assert!(
        looked_up.lock().unwrap().is_none(),
        "a bad filter is rejected locally, never sent to the hub"
    );
    assert_eq!(backend_conns.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_block_index_filter_is_invalid_argument() {
    let looked_up = Arc::new(Mutex::new(None));
    let hub = spawn_mock_hub_full(
        "unused",
        HubLookup::NotFound,
        Arc::new(Mutex::new(None)),
        looked_up.clone(),
    )
    .await;
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let shim = spawn_diverting_shim(backend, hub).await;

    let mut sender = connect_h2(shim).await;
    // Empty hash, a block+index filter instead: lightwalletd rejects this, so
    // the shim does too, locally.
    let filter = TxFilter {
        block: Some(BlockId {
            height: 100,
            hash: Vec::new(),
        }),
        index: 3,
        hash: Vec::new(),
    };
    let reply = get_transaction_filter(&mut sender, shim, filter).await;

    assert_eq!(reply.status, 3, "a block+index filter is INVALID_ARGUMENT");
    assert!(looked_up.lock().unwrap().is_none());
    assert_eq!(backend_conns.load(Ordering::SeqCst), 0);
}
