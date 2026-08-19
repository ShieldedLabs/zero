//! A stalled-but-alive upstream must terminate, not hang the wallet.
//!
//! The failure this guards is not a dead indexer -- a refused connection fails
//! fast and always did. It is an indexer that is UP: it completes the TCP and h2
//! handshakes, accepts the stream, and answers PINGs, so the shim's connection
//! keepalive is satisfied and never tears it down. It simply never sends
//! response headers. Before the deadline in `forward()`, the wallet sat on that
//! for its own full timeout with no explanation, and every retry opened another
//! stalled stream against the same upstream.
//!
//! Note what is asserted: the DISPOSITION, not the duration. The property is
//! that a stall ends in a clear typed error the wallet can act on, rather than
//! in silence.
//!
//! Time is paused so the suite does not spend the 30 s head deadline proving
//! it -- but only once the upstream is holding the request. Under a paused
//! clock, every time the runtime parks it may leap straight to the next timer
//! deadline, so any real I/O await racing a live timer can lose to the leap.
//! Connection establishment (the shim's dial under `DIAL_TIMEOUT`, both h2
//! handshakes, the request write) is exactly such a race, and which side wins
//! is a scheduling accident that a rebuild can flip: an unrelated dependency
//! bump once flipped it, surfacing the dial timeout instead of the stall
//! error. So establishment runs in real time (microseconds against loopback),
//! and the clock pauses only for the wait this test exists to skip.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::client::conn::http2 as client_h2;
use hyper::server::conn::http2 as server_h2;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// An indexer that is fully alive at every layer below the application: it
/// accepts, handshakes, reads the request, keeps answering PINGs -- and never
/// produces a response. Notifies the returned [`Notify`] once it has read a
/// request to the end, i.e. once the stall is the only thing left to observe.
async fn spawn_stalled_backend() -> (SocketAddr, Arc<Notify>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let got_request = Arc::new(Notify::new());
    let notify = got_request.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let notify = notify.clone();
            tokio::spawn(async move {
                let _ = server_h2::Builder::new(TokioExecutor::new())
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |req: Request<Incoming>| {
                            let notify = notify.clone();
                            async move {
                                let _ = req.into_body().collect().await;
                                notify.notify_one();
                                std::future::pending::<()>().await;
                                Ok::<_, Infallible>(Response::new(Empty::<Bytes>::new()))
                            }
                        }),
                    )
                    .await;
            });
        }
    });
    (addr, got_request)
}

async fn spawn_shim(backend: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = zero_indexer_shim::serve_with_shutdown(
            listener,
            backend,
            None,
            None,
            zero_indexer_shim::CautionRelay::default(),
            zero_indexer_shim::nym::MixnetStatus::default(),
            std::future::pending::<()>(),
        )
        .await;
    });
    addr
}

#[tokio::test]
async fn a_stalled_upstream_becomes_an_error_the_wallet_can_read() {
    let (backend, upstream_holds_request) = spawn_stalled_backend().await;
    let shim = spawn_shim(backend).await;

    let stream = TcpStream::connect(shim).await.unwrap();
    let (mut sender, conn) = client_h2::handshake::<_, _, BoxBody<Bytes, Infallible>>(
        TokioExecutor::new(),
        TokioIo::new(stream),
    )
    .await
    .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });

    // A plain pass-through call: nothing the shim intercepts, so it goes
    // straight at the upstream that will never answer.
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "http://{shim}/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo"
        ))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(BoxBody::new(Empty::<Bytes>::new()))
        .unwrap();

    sender.ready().await.unwrap();
    // If the deadline is missing, this task never resolves and the test hangs
    // rather than failing -- which is exactly what the wallet experienced.
    let response = tokio::spawn(async move { sender.send_request(request).await });

    // Real time until the upstream is provably holding the whole request: from
    // here on, the only live await in the shim is the response-head deadline.
    upstream_holds_request.notified().await;
    tokio::time::pause();

    let response = response.await.unwrap().unwrap();

    let status = response
        .headers()
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let message = response
        .headers()
        .get("grpc-message")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_default();

    assert_eq!(
        status.as_deref(),
        Some("14"),
        "a stalled upstream is UNAVAILABLE to the wallet, not a hang"
    );
    assert!(
        message.contains("no response headers"),
        "the wallet must be told WHY, so a retry is an informed choice: got {message:?}"
    );
}
