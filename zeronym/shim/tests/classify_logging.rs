//! What the shim actually DOES with a migration in this proof of concept: it
//! logs it.
//!
//! Because the proof of concept is non-destructive, classification has no
//! observable effect on the wire. Every other test in this crate would pass
//! just as well against a shim whose classifier was never called. This one
//! captures the shim's own `tracing` output and asserts the verdicts, which is
//! the only evidence that the intercept path ran.
//!
//! It lives in its own file on purpose: the subscriber it installs is global,
//! and Cargo gives each integration test file its own process, so nothing else
//! writes into the buffer these assertions read.

use std::convert::Infallible;
use std::future::Future;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
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
use tracing_subscriber::fmt::MakeWriter;
use zaino_proto::proto::service::{RawTransaction, SendResponse};
use zero_indexer_shim::proxy::SEND_TRANSACTION;

const V6_MIGRATION: &[u8] = include_bytes!("fixtures/v6_migration.bin");
const V6_REVERSE: &[u8] = include_bytes!("fixtures/v6_reverse.bin");

const LIMIT: StdDuration = StdDuration::from_secs(10);

async fn bounded<F: Future>(fut: F) -> F::Output {
    tokio::time::timeout(LIMIT, fut).await.expect("timed out")
}

// ------------------------------------------------------------ log capture

/// A `tracing` writer that keeps everything in memory.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

// ------------------------------------------------------------ stub indexer

/// Answers every method with a framed `SendResponse` and a `grpc-status: 0`
/// trailer. Just enough to make the shim's forward leg real, so the intercept
/// path runs to completion. The tonic mock in `tests/grpc_transparency.rs` is
/// the one that checks what the indexer received.
async fn spawn_stub_indexer() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
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
        error_message: "ok".to_owned(),
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
        .expect("response builds"))
}

fn grpc_frame(message: &[u8]) -> Bytes {
    let mut frame = Vec::with_capacity(5 + message.len());
    frame.push(0);
    frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
    frame.extend_from_slice(message);
    Bytes::from(frame)
}

// ----------------------------------------------------------------- harness

async fn spawn_shim(backend: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = zero_indexer_shim::serve(listener, backend).await;
    });
    addr
}

/// Send one `SendTransaction` carrying `tx` as `RawTransaction.data`.
async fn send_tx(
    sender: &mut client_h2::SendRequest<BoxBody<Bytes, Infallible>>,
    shim: SocketAddr,
    tx: &[u8],
) {
    send_tx_with_method(sender, shim, tx, "POST").await
}

/// The same, with the HTTP method under the caller's control. The backends this
/// shim fronts route on path alone, so the method must make no difference to
/// whether the classifier runs.
async fn send_tx_with_method(
    sender: &mut client_h2::SendRequest<BoxBody<Bytes, Infallible>>,
    shim: SocketAddr,
    tx: &[u8],
    method: &str,
) {
    let message = RawTransaction {
        data: tx.to_vec(),
        height: 0,
    }
    .encode_to_vec();

    let request = Request::builder()
        .method(method)
        .uri(format!("http://{shim}{SEND_TRANSACTION}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(Full::new(grpc_frame(&message)).boxed())
        .unwrap();

    sender.ready().await.unwrap();
    let response = bounded(sender.send_request(request)).await.unwrap();
    // Draining the response is what guarantees the shim finished the intercept,
    // so the log line is on the page before the assertions read it.
    bounded(response.into_body().collect()).await.unwrap();
}

// ------------------------------------------------------------------- test

#[tokio::test]
async fn the_shim_logs_a_verdict_for_every_send_transaction() {
    let capture = Capture::default();
    tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .expect("this test process owns the global subscriber");

    let backend = spawn_stub_indexer().await;
    let shim = spawn_shim(backend).await;

    let stream = TcpStream::connect(shim).await.unwrap();
    let (mut sender, conn) = client_h2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    send_tx(&mut sender, shim, V6_MIGRATION).await;
    send_tx(&mut sender, shim, V6_REVERSE).await;
    send_tx(&mut sender, shim, &[0xff; 64]).await;
    // A migration sent with a method the shim used to ignore. The tonic server
    // Zaino is built from dispatches on path alone, so this one reaches the
    // indexer's send_transaction handler; if the shim required POST it would
    // reach it unclassified, which is the fail-open direction.
    send_tx_with_method(&mut sender, shim, V6_MIGRATION, "GET").await;

    let log = capture.text();

    // 1. The migration. The verdict, the evidence it rests on, and the routing
    //    decision production would take.
    assert!(log.contains("MIGRATION detected"), "log was:\n{log}");
    assert!(log.contains("orchard_vb=+250000"), "log was:\n{log}");
    assert!(log.contains("ironwood_vb=-240000"), "log was:\n{log}");
    assert!(log.contains("version=V6"), "log was:\n{log}");

    // 2. The reverse shape: same pools, opposite signs, not a migration.
    assert!(
        log.contains("passthrough: SendTransaction non-migration"),
        "log was:\n{log}"
    );
    assert!(log.contains("orchard_vb=-250000"), "log was:\n{log}");
    assert!(
        log.contains("diverted_in_production=false"),
        "log was:\n{log}"
    );

    // 3. Bytes that are not a transaction. Fail-safe for privacy: a body the
    //    shim could not read is treated as a migration, never as a pass-through.
    assert!(log.contains("MIGRATION-FAILSAFE"), "log was:\n{log}");

    // 4. The GET. It must produce a verdict of its own, not silence: two
    //    migrations were detected, not one.
    assert_eq!(
        log.matches("MIGRATION detected").count(),
        2,
        "a SendTransaction with a non-POST method skipped the classifier, log was:\n{log}"
    );

    // Three of the four would be diverted in production: two migrations and
    // the unreadable body.
    assert_eq!(
        log.matches("diverted_in_production=true").count(),
        3,
        "log was:\n{log}"
    );
}
