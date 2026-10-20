//! gRPC-web, the dialect browser wallets speak because a browser cannot read
//! HTTP/2 trailers. The shim translates it at the edge, so these tests pin the
//! three properties that matter: a browser gets a working reply, the CORS
//! preflight never reaches the operator, and the browser's identifying headers
//! (`Origin`, `Referer`) never reach the operator either. The divert test in
//! `divert.rs` pins the fourth: a gRPC-web migration is still classified.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http::{HeaderMap, Request, Response};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::Frame;
use hyper::body::Incoming;
use hyper::server::conn::http2 as server_h2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use tokio::net::TcpListener;
use zaino_proto::proto::service::SendResponse;

use common::{
    bounded, connect_h2, grpc_frame, grpc_web_call, spawn_counting_backend,
    spawn_forward_only_shim, V6_IRONWOOD_ONLY,
};
use zero_indexer_shim::proxy::SEND_TRANSACTION;

const LATEST_BLOCK: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLatestBlock";
const BLOCK_RANGE: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRange";

/// A stub indexer that answers any call as a server stream: `count` messages,
/// each in its own DATA frame, then a `grpc-status: 0` trailer.
async fn spawn_streaming_backend(count: u8) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| async move {
                    let _ = req.into_body().collect().await;
                    let mut trailers = HeaderMap::new();
                    trailers.insert("grpc-status", "0".parse().unwrap());
                    let mut frames: Vec<Result<Frame<Bytes>, Infallible>> = (0..count)
                        .map(|i| Ok(Frame::data(grpc_frame(&[i; 3]))))
                        .collect();
                    frames.push(Ok(Frame::trailers(trailers)));
                    Ok::<_, Infallible>(
                        Response::builder()
                            .header("content-type", "application/grpc")
                            .body(StreamBody::new(tokio_stream::iter(frames)))
                            .unwrap(),
                    )
                });
                let _ = server_h2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    addr
}

/// A stub indexer that records the headers of the last request it served and
/// answers with a framed `SendResponse` and a `grpc-status: 0` trailer.
async fn spawn_recording_backend(seen: Arc<Mutex<Option<HeaderMap>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let seen = seen.clone();
                    async move {
                        *seen.lock().unwrap() = Some(req.headers().clone());
                        let _ = req.into_body().collect().await;
                        let message = SendResponse {
                            error_code: 0,
                            error_message: "operator-answered".to_owned(),
                        }
                        .encode_to_vec();
                        let mut trailers = HeaderMap::new();
                        trailers.insert("grpc-status", "0".parse().unwrap());
                        Ok::<_, Infallible>(
                            Response::builder()
                                .header("content-type", "application/grpc")
                                .body(
                                    Full::new(grpc_frame(&message))
                                        .with_trailers(async move { Some(Ok(trailers)) })
                                        .boxed(),
                                )
                                .unwrap(),
                        )
                    }
                });
                let _ = server_h2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    addr
}

fn assert_operator_answered(reply: &common::GrpcWebReply) {
    assert!(
        reply.trailers.contains("grpc-status:0"),
        "the status rides in the trailer frame: {:?}",
        reply.trailers
    );
    assert_eq!(reply.messages.len(), 1);
    let resp = SendResponse::decode(reply.messages[0].as_ref()).expect("a SendResponse");
    assert_eq!(resp.error_message, "operator-answered");
}

#[tokio::test]
async fn a_binary_grpc_web_call_gets_its_status_in_the_body() {
    let seen = Arc::new(Mutex::new(None));
    let backend = spawn_recording_backend(seen.clone()).await;
    let shim = spawn_forward_only_shim(backend).await;

    let mut sender = connect_h2(shim).await;
    let reply = grpc_web_call(
        &mut sender,
        shim,
        LATEST_BLOCK,
        "application/grpc-web+proto",
        &[],
    )
    .await;

    assert_eq!(
        reply.headers.get("content-type").unwrap(),
        "application/grpc-web+proto"
    );
    assert_eq!(
        reply.headers.get("access-control-allow-origin").unwrap(),
        "*"
    );
    assert_operator_answered(&reply);

    // The operator saw plain gRPC, and nothing that names the browser wallet.
    let headers = seen
        .lock()
        .unwrap()
        .clone()
        .expect("the backend was called");
    assert_eq!(headers.get("content-type").unwrap(), "application/grpc");
    assert!(
        headers.get("origin").is_none(),
        "Origin reached the operator"
    );
    assert!(
        headers.get("referer").is_none(),
        "Referer reached the operator"
    );
}

#[tokio::test]
async fn a_text_grpc_web_call_round_trips_through_base64() {
    let seen = Arc::new(Mutex::new(None));
    let backend = spawn_recording_backend(seen.clone()).await;
    let shim = spawn_forward_only_shim(backend).await;

    let mut sender = connect_h2(shim).await;
    let reply = grpc_web_call(
        &mut sender,
        shim,
        LATEST_BLOCK,
        "application/grpc-web-text",
        &[],
    )
    .await;

    assert_eq!(
        reply.headers.get("content-type").unwrap(),
        "application/grpc-web-text+proto"
    );
    assert_operator_answered(&reply);
}

#[tokio::test]
async fn a_server_stream_arrives_whole_in_both_encodings() {
    // GetBlockRange is how a wallet syncs, so the stream must survive the
    // translation message for message, with the status after the last one.
    let backend = spawn_streaming_backend(3).await;
    let shim = spawn_forward_only_shim(backend).await;

    for content_type in ["application/grpc-web+proto", "application/grpc-web-text"] {
        let mut sender = connect_h2(shim).await;
        let reply = grpc_web_call(&mut sender, shim, BLOCK_RANGE, content_type, &[]).await;
        let expected: Vec<Bytes> = (0..3u8).map(|i| Bytes::from(vec![i; 3])).collect();
        assert_eq!(reply.messages, expected, "{content_type}");
        assert!(reply.trailers.contains("grpc-status:0"), "{content_type}");
    }
}

#[tokio::test]
async fn a_pass_through_send_over_grpc_web_reaches_the_operator() {
    let seen = Arc::new(Mutex::new(None));
    let backend = spawn_recording_backend(seen.clone()).await;
    let shim = spawn_forward_only_shim(backend).await;

    let message = zaino_proto::proto::service::RawTransaction {
        data: V6_IRONWOOD_ONLY.to_vec().into(),
        height: 0,
    }
    .encode_to_vec();
    let mut sender = connect_h2(shim).await;
    let reply = grpc_web_call(
        &mut sender,
        shim,
        SEND_TRANSACTION,
        "application/grpc-web+proto",
        &message,
    )
    .await;

    assert_operator_answered(&reply);
}

#[tokio::test]
async fn the_cors_preflight_is_answered_without_dialling_the_operator() {
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let shim = spawn_forward_only_shim(backend).await;

    let mut sender = connect_h2(shim).await;
    let request = Request::builder()
        .method("OPTIONS")
        .uri(format!("http://{shim}{SEND_TRANSACTION}"))
        .header("origin", "chrome-extension://abcdefghijklmnop")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "content-type,x-grpc-web")
        .body(BoxBody::new(Empty::<Bytes>::new()))
        .unwrap();
    sender.ready().await.unwrap();
    let response = bounded(sender.send_request(request)).await.unwrap();

    assert_eq!(response.status(), 200);
    let headers = response.headers();
    assert_eq!(headers.get("access-control-allow-origin").unwrap(), "*");
    let allowed = headers
        .get("access-control-allow-headers")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(allowed.contains("x-grpc-web"), "allow-headers: {allowed}");
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "a preflight must never reach the operator"
    );
}
