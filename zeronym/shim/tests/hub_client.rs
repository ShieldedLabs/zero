//! `HubClient`'s clearnet verdict mapping.
//!
//! M2' (M4 review): the `413` arm reported the mixnet's `MAX_NYM_TX_BYTES`
//! budget, but a clearnet submission is bounded by the hub's HTTP body cap,
//! which is the frame size (`FRAME_BYTES`), nine bytes wider. A clearnet hub and
//! a mixnet hub have different caps, and each must report its own.

use std::convert::Infallible;
use std::net::SocketAddr;

use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use zero_indexer_shim::hub::{HubClient, Submit};
use zero_indexer_shim::wire::FRAME_BYTES;

/// A hub that answers every POST with `413 Payload Too Large` and a plain-text
/// body, exactly as the real hub does when a submission exceeds its frame.
async fn spawn_413_hub() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(|_req| async {
                            let mut resp =
                                Response::new(Full::new(Bytes::from_static(b"too large")));
                            *resp.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
                            Ok::<_, Infallible>(resp)
                        }),
                    )
                    .await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn a_clearnet_413_reports_the_hubs_frame_cap_not_the_mixnet_budget() {
    let addr = spawn_413_hub().await;
    let verdict = HubClient::new(addr, None)
        .submit(&[0u8; 128])
        .await
        .expect("a 413 is a typed verdict, not a transport error");
    assert_eq!(
        verdict,
        Submit::TooLarge { limit: FRAME_BYTES },
        "the clearnet cap is the hub's frame, not the mixnet tx budget"
    );
}

/// A hub that completes the TCP handshake, reads the request, and then never
/// answers. This is what a half-dead hub looks like from the shim, and it is
/// strictly worse than a refused connection: a refusal fails fast and the shim
/// falls through to its own verdict.
async fn spawn_silent_hub() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(|_req| async {
                            // Hold the request open forever without answering.
                            std::future::pending::<()>().await;
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::new())))
                        }),
                    )
                    .await;
            });
        }
    });
    addr
}

/// A submission must be bounded in time, not merely in size.
///
/// `get_transaction` has carried a deadline since it was written; `submit` did
/// not, and the asymmetry is easy to miss because both call the same
/// `round_trip`. The consequence is not confined to the shim: a wallet's
/// `SendTransaction` is blocked on this call, so a hub that accepts a connection
/// and goes quiet holds the WALLET open indefinitely, and the shim never reaches
/// the point where it would decide what to tell it.
///
/// The deadline must also cover the connect and the TLS handshake, not just the
/// read -- each can stall on its own, and a deadline around one of them bounds
/// nothing. Time is paused, so this proves the bound exists without waiting it
/// out.
#[tokio::test(start_paused = true)]
async fn a_hub_that_never_answers_is_given_up_on_rather_than_waited_for() {
    let addr = spawn_silent_hub().await;
    let err = HubClient::new(addr, None)
        .submit(&[0u8; 128])
        .await
        .expect_err("a hub that never answers must not hang the wallet");
    assert!(
        err.to_string().contains("timed out"),
        "the failure must name the deadline that fired, got: {err}"
    );
}
