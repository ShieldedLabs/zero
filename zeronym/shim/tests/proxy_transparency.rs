//! End-to-end tests for the properties that make the shim invisible to a gRPC
//! client: streaming stays incremental, trailers survive, unknown methods pass
//! through, and the intercepted `SendTransaction` reaches the backing indexer
//! byte for byte.
//!
//! The backing indexer here is a hand-rolled h2c mock rather than a real
//! lightwalletd or Zaino. That is deliberate, and it is stronger evidence than a
//! live node: the mock records the exact bytes it received, and it can withhold
//! the second message of a stream until the test says otherwise, which turns
//! "is the response really streamed?" from an eyeball judgement into a test that
//! times out on regression.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Request, Response};
use http_body::{Body, Frame};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::client::conn::http2 as client_h2;
use hyper::server::conn::http2 as server_h2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};
use zaino_proto::proto::service::{RawTransaction, SendResponse};
use zero_indexer_shim::proxy::SEND_TRANSACTION;

const GET_BLOCK_RANGE: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRange";
const UNKNOWN_METHOD: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/NoSuchMethodYet";

/// The only client-streaming method in `CompactTxStreamer`. Used here purely as
/// a path the mock reads incrementally.
const CLIENT_STREAM: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTaddressBalanceStream";

/// One byte over the intercept path's 4 MiB buffer cap.
const OVERSIZED: usize = 4 * 1024 * 1024 + 1;

/// A real V6 Orchard(+250_000) -> Ironwood(-240_000) transaction.
const V6_MIGRATION: &[u8] = include_bytes!("fixtures/v6_migration.bin");

/// Every await in these tests is bounded. A hang here is a real failure mode
/// (a buffered response, or a body whose connection task was never spawned),
/// so it should read as a failure and not as a stuck test run.
const LIMIT: Duration = Duration::from_secs(10);

async fn bounded<F: Future>(fut: F) -> F::Output {
    tokio::time::timeout(LIMIT, fut)
        .await
        .expect("timed out: the shim is buffering, or a connection task is missing")
}

// ---------------------------------------------------------------- mock indexer

/// One request as the backing indexer saw it.
#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    authority: Option<String>,
    path: String,
    headers: HeaderMap,
    body: Bytes,
    trailers: Option<HeaderMap>,
}

#[derive(Default)]
struct MockState {
    requests: Mutex<Vec<Recorded>>,
    /// Held closed until the test has proved it received the first streamed
    /// message.
    gate: Notify,
    /// Fired when the mock has read the FIRST frame of a client-streaming
    /// request body, before the client has finished sending.
    first_request_frame: Notify,
}

impl MockState {
    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

/// A response body fed frame by frame from a channel, so the mock can emit data
/// frames and a trailers frame on its own schedule.
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

type MockBody = BoxBody<Bytes, Infallible>;

async fn mock_service(
    req: Request<Incoming>,
    state: Arc<MockState>,
) -> Result<Response<MockBody>, Infallible> {
    let (parts, mut body) = req.into_parts();

    // On the client-streaming path the mock reads ONE frame and announces it,
    // before the client has finished sending. If the shim buffered the request
    // body, that announcement cannot happen until the client sends EOS, and the
    // test waiting on it fails by timeout. Every other path collects the whole
    // body first, which is why this half of the streaming property needs its
    // own arm here.
    let mut head = Vec::new();
    let mut early_trailers = None;
    if parts.uri.path() == CLIENT_STREAM {
        if let Some(Ok(frame)) = body.frame().await {
            match frame.into_data() {
                Ok(data) => head.extend_from_slice(&data),
                Err(frame) => early_trailers = frame.into_trailers().ok(),
            }
        }
        state.first_request_frame.notify_one();
    }

    let collected = body.collect().await.expect("mock reads the request body");
    let trailers = collected.trailers().cloned().or(early_trailers);
    head.extend_from_slice(&collected.to_bytes());

    state.requests.lock().unwrap().push(Recorded {
        method: parts.method.to_string(),
        authority: parts.uri.authority().map(ToString::to_string),
        path: parts.uri.path().to_owned(),
        headers: parts.headers.clone(),
        body: Bytes::from(head),
        trailers,
    });

    let response = match parts.uri.path() {
        GET_BLOCK_RANGE => {
            let (tx, rx) = mpsc::channel(1);
            let gate = state.clone();
            tokio::spawn(async move {
                let _ = tx.send(Frame::data(grpc_frame(b"block-1"))).await;
                // The test opens this gate only after it has received block 1
                // through the shim. If the shim buffered the response, block 1
                // never arrives, the gate never opens, and the test times out.
                gate.gate.notified().await;
                let _ = tx.send(Frame::data(grpc_frame(b"block-2"))).await;
                let _ = tx.send(Frame::trailers(grpc_trailers(0))).await;
            });
            grpc_head().body(ChannelBody { rx }.boxed()).unwrap()
        }
        SEND_TRANSACTION | CLIENT_STREAM => {
            let message = SendResponse {
                error_code: 0,
                error_message: "ok".to_owned(),
            }
            .encode_to_vec();
            let (tx, rx) = mpsc::channel(2);
            let _ = tx.send(Frame::data(grpc_frame(&message))).await;
            let _ = tx.send(Frame::trailers(grpc_trailers(0))).await;
            drop(tx);
            grpc_head().body(ChannelBody { rx }.boxed()).unwrap()
        }
        // Anything else: a trailers-only UNIMPLEMENTED, which is what a real
        // indexer returns for a method it does not know. There is no body and
        // no trailers frame at all; the status rides in the response headers.
        _ => {
            let mut response = grpc_head().body(Empty::<Bytes>::new().boxed()).unwrap();
            response.headers_mut().extend(grpc_trailers(12));
            response
        }
    };

    Ok(response)
}

fn grpc_head() -> http::response::Builder {
    Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
}

fn grpc_trailers(status: u16) -> HeaderMap {
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", status.to_string().parse().unwrap());
    trailers.insert("grpc-message", "".parse().unwrap());
    trailers
}

/// Wrap a protobuf message in the 5-byte gRPC length prefix.
fn grpc_frame(message: &[u8]) -> Bytes {
    let mut frame = Vec::with_capacity(5 + message.len());
    frame.push(0);
    frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
    frame.extend_from_slice(message);
    Bytes::from(frame)
}

// ------------------------------------------------------------------ harness

/// Every task one mock indexer owns: its accept loop, and one per established
/// connection. Aborting all of them is what a process restart looks like from
/// the shim's side, which is stronger than just closing the listener: the
/// established connection dies too, which is what makes the shim's cached
/// upstream go stale.
#[derive(Clone, Default)]
struct MockTasks(Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>);

impl MockTasks {
    fn push(&self, task: tokio::task::JoinHandle<()>) {
        self.0.lock().unwrap().push(task);
    }

    /// Abort every task and WAIT for it to unwind, so the listening socket and
    /// the connection sockets are really closed. `abort()` alone only schedules
    /// the cancellation, and rebinding the port then fails with EADDRINUSE.
    async fn kill(&self) {
        let tasks: Vec<_> = self.0.lock().unwrap().drain(..).collect();
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
    }
}

fn serve_mock(listener: TcpListener, state: Arc<MockState>) -> MockTasks {
    let tasks = MockTasks::default();
    let accepted = tasks.clone();

    let accept = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let served = state.clone();
            accepted.push(tokio::spawn(async move {
                let service = service_fn(move |req| mock_service(req, served.clone()));
                let _ = server_h2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            }));
        }
    });

    tasks.push(accept);
    tasks
}

async fn spawn_mock() -> (SocketAddr, Arc<MockState>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(MockState::default());
    serve_mock(listener, state.clone());
    (addr, state)
}

async fn spawn_shim(backend: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = zero_indexer_shim::serve(listener, backend).await;
    });
    addr
}

/// Mock backend plus a shim in front of it, both on ephemeral ports.
async fn spawn_stack() -> (SocketAddr, SocketAddr, Arc<MockState>) {
    let (backend, state) = spawn_mock().await;
    let shim = spawn_shim(backend).await;
    (shim, backend, state)
}

type ClientBody = BoxBody<Bytes, Infallible>;

async fn connect(addr: SocketAddr) -> client_h2::SendRequest<ClientBody> {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (sender, conn) = client_h2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    sender
}

fn grpc_request(addr: SocketAddr, path: &str, body: Bytes) -> Request<ClientBody> {
    Request::builder()
        .method("POST")
        .uri(format!("http://{addr}{path}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(Full::new(body).boxed())
        .unwrap()
}

// -------------------------------------------------------------------- tests

#[tokio::test]
async fn server_streaming_stays_incremental_and_delivers_trailers() {
    let (shim, _backend, state) = spawn_stack().await;
    let mut sender = connect(shim).await;

    let request = grpc_request(shim, GET_BLOCK_RANGE, grpc_frame(b"range"));
    let response = bounded(sender.send_request(request)).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/grpc"
    );

    let first = grpc_frame(b"block-1");
    let second = grpc_frame(b"block-2");

    let mut body = response.into_body();
    let mut data = Vec::new();
    let mut trailers = None;
    let mut opened_gate = false;

    while let Some(frame) = bounded(body.frame()).await {
        let frame = frame.unwrap();
        if let Some(chunk) = frame.data_ref() {
            data.extend_from_slice(chunk);
            if !opened_gate && data.len() >= first.len() {
                // The first message arrived while the backing indexer was still
                // blocked from producing the second one. That is the proof the
                // response is relayed frame by frame, not collected.
                opened_gate = true;
                state.gate.notify_one();
            }
        } else if let Some(received) = frame.trailers_ref() {
            trailers = Some(received.clone());
        }
    }

    assert!(opened_gate, "no data arrived before the gate was opened");
    assert_eq!(data, [first.as_ref(), second.as_ref()].concat());
    assert_eq!(
        trailers
            .expect("grpc-status must arrive as an HTTP/2 trailer")
            .get("grpc-status")
            .unwrap(),
        "0"
    );
}

#[tokio::test]
async fn unknown_method_paths_pass_through_with_the_backend_status() {
    let (shim, _backend, state) = spawn_stack().await;
    let mut sender = connect(shim).await;

    let request = grpc_request(shim, UNKNOWN_METHOD, grpc_frame(b"whatever"));
    let response = bounded(sender.send_request(request)).await.unwrap();

    // A trailers-only response: HTTP 200, status in the response headers, no
    // body. A proxy that only knows how to relay trailer frames drops these.
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers().get("grpc-status").unwrap(), "12");

    let recorded = state.requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].path, UNKNOWN_METHOD);
    assert_eq!(recorded[0].method, "POST");
}

#[tokio::test]
async fn send_transaction_reaches_the_indexer_byte_for_byte() {
    let (shim, backend, state) = spawn_stack().await;
    let mut sender = connect(shim).await;

    let message = RawTransaction {
        data: V6_MIGRATION.to_vec().into(),
        height: 0,
    }
    .encode_to_vec();
    let body = grpc_frame(&message);

    // Metadata a wallet might set. All of it must survive the intercept.
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{shim}{SEND_TRANSACTION}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .header("grpc-timeout", "10S")
        .header("authorization", "Bearer test-token")
        .header("x-zeronym-test", "metadata")
        .body(Full::new(body.clone()).boxed())
        .unwrap();

    let response = bounded(sender.send_request(request)).await.unwrap();
    assert_eq!(response.status(), 200);

    let collected = bounded(response.into_body().collect()).await.unwrap();
    let trailers = collected.trailers().cloned();
    let bytes = collected.to_bytes();

    // The backing indexer's real SendResponse is relayed, not synthesized: the
    // proof of concept is non-destructive.
    let send_response = SendResponse::decode(&bytes[5..]).unwrap();
    assert_eq!(send_response.error_code, 0);
    assert_eq!(send_response.error_message, "ok");
    assert_eq!(
        trailers
            .expect("grpc-status must arrive as an HTTP/2 trailer")
            .get("grpc-status")
            .unwrap(),
        "0"
    );

    let recorded = state.requests();
    assert_eq!(recorded.len(), 1);
    let recorded = &recorded[0];

    // The whole point: a migration was classified and still arrived unchanged.
    assert_eq!(recorded.body, body);
    assert_eq!(recorded.path, SEND_TRANSACTION);
    assert_eq!(recorded.method, "POST");
    assert_eq!(recorded.headers.get("te").unwrap(), "trailers");
    assert_eq!(recorded.headers.get("grpc-timeout").unwrap(), "10S");
    assert_eq!(
        recorded.headers.get("authorization").unwrap(),
        "Bearer test-token"
    );
    assert_eq!(recorded.headers.get("x-zeronym-test").unwrap(), "metadata");
    assert_eq!(
        recorded.headers.get("content-type").unwrap(),
        "application/grpc"
    );

    // Only the origin is retargeted.
    assert_eq!(
        recorded.authority.as_deref(),
        Some(backend.to_string()).as_deref()
    );
}

#[tokio::test]
async fn a_compressed_send_transaction_is_still_forwarded_unchanged() {
    let (shim, _backend, state) = spawn_stack().await;
    let mut sender = connect(shim).await;

    // Compression flag set: the shim must not try to parse this. It logs
    // MIGRATION-FAILSAFE and, because this proof of concept is non-destructive,
    // still forwards the original bytes.
    let mut body = grpc_frame(b"not really gzip").to_vec();
    body[0] = 1;
    let body = Bytes::from(body);

    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{shim}{SEND_TRANSACTION}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .header("grpc-encoding", "gzip")
        .body(Full::new(body.clone()).boxed())
        .unwrap();

    let response = bounded(sender.send_request(request)).await.unwrap();
    assert_eq!(response.status(), 200);
    bounded(response.into_body().collect()).await.unwrap();

    let recorded = state.requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].body, body);
    assert_eq!(recorded[0].headers.get("grpc-encoding").unwrap(), "gzip");
}

#[tokio::test]
async fn a_client_streaming_request_body_is_relayed_frame_by_frame() {
    let (shim, _backend, state) = spawn_stack().await;
    let mut sender = connect(shim).await;

    // The mirror image of the server-streaming test. Everything else in this
    // file sends a `Full` request body, which cannot tell a relayed body from a
    // buffered one.
    let (tx, rx) = mpsc::channel(4);
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://{shim}{CLIENT_STREAM}"))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(ChannelBody { rx }.boxed())
        .unwrap();

    let first = grpc_frame(b"chunk-1");
    let second = grpc_frame(b"chunk-2");
    tx.send(Frame::data(first.clone())).await.unwrap();

    // Dispatched, deliberately NOT awaited: the mock does not answer until it
    // has the whole body, so awaiting here would deadlock the test rather than
    // test anything.
    let pending = sender.send_request(request);

    // The backing indexer holds the first frame while the client is still
    // sending. That is the proof the request body is relayed, not collected.
    bounded(state.first_request_frame.notified()).await;

    let mut trailers = HeaderMap::new();
    trailers.insert("x-wallet-trailer", "1".parse().unwrap());
    tx.send(Frame::data(second.clone())).await.unwrap();
    tx.send(Frame::trailers(trailers)).await.unwrap();
    drop(tx);

    let response = bounded(pending).await.unwrap();
    assert_eq!(response.status(), 200);
    bounded(response.into_body().collect()).await.unwrap();

    let recorded = state.requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].body,
        Bytes::from([first.as_ref(), second.as_ref()].concat())
    );
    // Request trailers are as easy to drop as response trailers, and until now
    // they were only unit-tested on `ReplayBody`, never over the wire.
    assert_eq!(
        recorded[0]
            .trailers
            .as_ref()
            .expect("request trailers must survive the proxy")
            .get("x-wallet-trailer")
            .unwrap(),
        "1"
    );
}

#[tokio::test]
async fn an_oversized_send_transaction_is_refused_and_never_forwarded() {
    let (shim, _backend, state) = spawn_stack().await;
    let mut sender = connect(shim).await;

    // The over-limit branch is the ONLY place this non-destructive proof of
    // concept refuses to forward a request, and nothing exercised it.
    let request = grpc_request(shim, SEND_TRANSACTION, Bytes::from(vec![0u8; OVERSIZED]));
    let response = bounded(sender.send_request(request)).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers().get("grpc-status").unwrap(), "8");
    assert!(
        state.requests().is_empty(),
        "a body the shim could not classify must never reach the indexer"
    );
}

#[tokio::test]
async fn a_non_post_or_near_miss_send_transaction_is_still_intercepted() {
    // The vendored tonic server dispatches on `req.uri().path()` alone, with no
    // HTTP-method guard, so a GET or a PUT to the SendTransaction path reaches
    // the indexer's send_transaction handler. A shim whose interception
    // predicate is narrower than the backend's routing predicate fails OPEN:
    // the transaction is broadcast having never been classified. The predicate
    // must be a superset, never a subset.
    //
    // The wire-level tell that interception happened is the buffer limit:
    // refusing an oversized body is something ONLY the intercept path does. A
    // request that fell through to pass-through would be streamed to the mock,
    // which would record it and answer 12.
    for (method, path) in [
        ("GET", SEND_TRANSACTION),
        ("PUT", SEND_TRANSACTION),
        ("PATCH", SEND_TRANSACTION),
        // A near miss: not a path any backend we have checked routes, but
        // guessing wrong about a future one is not recoverable.
        (
            "POST",
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/sendtransaction",
        ),
    ] {
        let (shim, _backend, state) = spawn_stack().await;
        let mut sender = connect(shim).await;

        let request = Request::builder()
            .method(method)
            .uri(format!("http://{shim}{path}"))
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(Full::new(Bytes::from(vec![0u8; OVERSIZED])).boxed())
            .unwrap();

        let response = bounded(sender.send_request(request)).await.unwrap();
        assert_eq!(
            response.headers().get("grpc-status").unwrap(),
            "8",
            "{method} {path} bypassed the classifier"
        );
        assert!(
            state.requests().is_empty(),
            "{method} {path} was handed to the indexer unclassified"
        );
    }
}

#[tokio::test]
async fn the_shim_redials_after_the_backing_indexer_restarts() {
    // A restart of the backing indexer must not strand live wallet connections.
    // The shim answers a dead upstream with gRPC UNAVAILABLE, which is a clean
    // application-level status on a perfectly healthy HTTP/2 connection, so the
    // wallet's transport never errors and its own reconnect logic never fires.
    // If the shim does not redial, nothing does, and the wallet is stuck for as
    // long as it keeps that connection.
    let state = Arc::new(MockState::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = listener.local_addr().unwrap();
    let first = serve_mock(listener, state.clone());

    let shim = spawn_shim(backend).await;
    let mut sender = connect(shim).await;

    // One healthy call, which is what makes the shim cache an upstream.
    assert_eq!(call_status(&mut sender, shim).await, "12");

    // The indexer restarts: listener closed AND every established connection
    // killed.
    first.kill().await;
    let listener = TcpListener::bind(backend).await.unwrap();
    let _second = serve_mock(listener, state.clone());

    // Same wallet, same HTTP/2 connection to the shim, no reconnect.
    let mut statuses = Vec::new();
    for _ in 0..30 {
        let status = call_status(&mut sender, shim).await;
        let recovered = status == "12";
        statuses.push(status);
        if recovered {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(
        statuses.last().map(String::as_str),
        Some("12"),
        "the shim never redialled the restarted indexer: {statuses:?}"
    );
    // And the recovered call really reached the restarted indexer, rather than
    // being answered by the shim itself.
    assert!(
        state.requests().len() >= 2,
        "recorded {} requests",
        state.requests().len()
    );
}

/// One pass-through call, returning the gRPC status the wallet saw. The mock
/// answers `UNKNOWN_METHOD` with a trailers-only 12, so a 12 means the backing
/// indexer was reached and a 14 means the shim answered on its own.
async fn call_status(sender: &mut client_h2::SendRequest<ClientBody>, shim: SocketAddr) -> String {
    bounded(sender.ready()).await.unwrap();
    let request = grpc_request(shim, UNKNOWN_METHOD, grpc_frame(b"ping"));
    let response = bounded(sender.send_request(request)).await.unwrap();
    let status = response
        .headers()
        .get("grpc-status")
        .map(|value| value.to_str().unwrap().to_owned());
    let _ = bounded(response.into_body().collect()).await;
    status.unwrap_or_else(|| "(none)".to_owned())
}

#[tokio::test]
async fn an_unreachable_indexer_answers_unavailable_rather_than_dropping() {
    // Bind and immediately release a port so nothing is listening on it.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = dead.local_addr().unwrap();
    drop(dead);

    let shim = spawn_shim(backend).await;
    let mut sender = connect(shim).await;

    let request = grpc_request(shim, GET_BLOCK_RANGE, grpc_frame(b"range"));
    let response = bounded(sender.send_request(request)).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers().get("grpc-status").unwrap(), "14");
}
