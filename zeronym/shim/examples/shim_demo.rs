//! A self-contained demo of the shim's visible output.
//!
//! ```text
//! cargo run --example shim_demo
//! ```
//!
//! Starts a stub backing indexer, puts a shim in front of it, and sends five
//! calls through: a migration, a non-migration, an unparseable body, a
//! compressed body, and one ordinary proxied method. Watch the `zis::classify`
//! and `zis::proxy` lines. Every one of the five is forwarded to the stub
//! indexer, because this proof of concept is non-destructive.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{HeaderMap, Request, Response};
use http_body::{Body, Frame};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::client::conn::http2 as client_h2;
use hyper::server::conn::http2 as server_h2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use zaino_proto::proto::service::{RawTransaction, SendResponse};
use zero_indexer_shim::proxy::SEND_TRANSACTION;
use zero_indexer_shim::BoxError;

const GET_LIGHTD_INFO: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo";

const V6_MIGRATION: &[u8] = include_bytes!("../tests/fixtures/v6_migration.bin");
const V6_REVERSE: &[u8] = include_bytes!("../tests/fixtures/v6_reverse.bin");

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    // The demo wants the per-request `zis::proxy` line, which the shipped
    // binary keeps below its default level on purpose (it is an access log on
    // the operator's box). Here it is turned on explicitly.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zis::proxy=debug,info".into()),
        )
        .init();

    let backend = spawn_stub_indexer().await?;
    let shim = spawn_shim(backend).await?;
    tracing::info!(%shim, %backend, "demo stack up");

    let mut sender = connect(shim).await?;

    // 1. A real V6 Orchard -> Ironwood migration. The privacy-critical case.
    send_tx(&mut sender, shim, V6_MIGRATION, false).await?;

    // 2. The same shape reversed, which is not a migration.
    send_tx(&mut sender, shim, V6_REVERSE, false).await?;

    // 3. Bytes that are not a transaction at all. Fail-safe for privacy.
    send_tx(&mut sender, shim, &[0xff; 64], false).await?;

    // 4. A compressed body. Not parseable, so also fail-safe.
    send_tx(&mut sender, shim, V6_MIGRATION, true).await?;

    // 5. An ordinary proxied method: opaque, never decoded.
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{shim}{GET_LIGHTD_INFO}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(Full::new(grpc_frame(&[])).boxed())?;
    sender.ready().await?;
    sender
        .send_request(request)
        .await?
        .into_body()
        .collect()
        .await?;

    tracing::info!("demo complete: all five calls were forwarded to the stub indexer");
    Ok(())
}

/// Send one `SendTransaction` carrying `tx` as the `RawTransaction.data` field.
async fn send_tx(
    sender: &mut client_h2::SendRequest<BoxBody<Bytes, Infallible>>,
    shim: SocketAddr,
    tx: &[u8],
    compressed: bool,
) -> Result<(), BoxError> {
    let message = RawTransaction {
        data: tx.to_vec(),
        height: 0,
    }
    .encode_to_vec();

    let mut frame = grpc_frame(&message).to_vec();
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("http://{shim}{SEND_TRANSACTION}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers");
    if compressed {
        // The flag byte says the message payload is compressed. The shim must
        // not try to parse it.
        frame[0] = 1;
        request = request.header("grpc-encoding", "gzip");
    }

    let request = request.body(Full::new(Bytes::from(frame)).boxed())?;
    sender.ready().await?;
    sender
        .send_request(request)
        .await?
        .into_body()
        .collect()
        .await?;
    Ok(())
}

/// A stub backing indexer: answers every method with `SendResponse { 0, "ok" }`
/// and a `grpc-status: 0` trailer. Enough to make the shim's forward leg real.
async fn spawn_stub_indexer() -> Result<SocketAddr, BoxError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = server_h2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service_fn(stub_service))
                    .await;
            });
        }
    });

    Ok(addr)
}

async fn stub_service(
    req: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, Infallible>>, Infallible> {
    let (parts, body) = req.into_parts();
    let received = body
        .collect()
        .await
        .map(|b| b.to_bytes().len())
        .unwrap_or(0);
    tracing::info!(
        target: "stub-indexer",
        path = %parts.uri.path(),
        bytes = received,
        "backing indexer received the forwarded request"
    );

    let message = SendResponse {
        error_code: 0,
        error_message: "ok".to_owned(),
    }
    .encode_to_vec();

    let (tx, rx) = mpsc::channel(2);
    let _ = tx.send(Frame::data(grpc_frame(&message))).await;
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", "0".parse().unwrap());
    let _ = tx.send(Frame::trailers(trailers)).await;
    drop(tx);

    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .body(ChannelBody { rx }.boxed())
        .expect("response builds"))
}

async fn spawn_shim(backend: SocketAddr) -> Result<SocketAddr, BoxError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        if let Err(err) = zero_indexer_shim::serve(listener, backend).await {
            tracing::error!(%err, "shim stopped");
        }
    });
    Ok(addr)
}

async fn connect(
    addr: SocketAddr,
) -> Result<client_h2::SendRequest<BoxBody<Bytes, Infallible>>, BoxError> {
    let stream = TcpStream::connect(addr).await?;
    let (sender, conn) = client_h2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(sender)
}

/// Wrap a protobuf message in the 5-byte gRPC length prefix.
fn grpc_frame(message: &[u8]) -> Bytes {
    let mut frame = Vec::with_capacity(5 + message.len());
    frame.push(0);
    frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
    frame.extend_from_slice(message);
    Bytes::from(frame)
}

/// A response body fed frame by frame, so the stub can emit real trailers.
struct ChannelBody {
    rx: mpsc::Receiver<Frame<Bytes>>,
}

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.get_mut().rx.poll_recv(cx).map(|frame| frame.map(Ok))
    }
}
