//! The divert and lookup paths over the clearnet HTTP transport, end to end.
//!
//! With a hub configured, an Orchard-touching `SendTransaction` goes to the hub
//! and EVERY `GetTransaction` is answered by the hub, while the operator's
//! indexer is never even connected on either path. The connection-COUNTING
//! backend is what turns that from a claim into an assertion: it must stay at
//! zero. Forward-only mode (no hub) still passes everything through.
//!
//! The shim keeps no state, so there is nothing to "divert first" and then look
//! up: the hub is the source of truth for a lookup, and these tests drive the
//! hub's answer directly. `divert_nym.rs` asserts the same properties over the
//! mixnet transport.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1 as server_h1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use zaino_proto::proto::service::{BlockId, TxFilter};

use zero_indexer_shim::hub::HubClient;
use zero_indexer_shim::intercept::Diversion;
use zero_indexer_shim::proxy::GET_TRANSACTION;

mod common;
use common::{
    connect_h2, dead_addr, decode_raw_transaction, decode_send_response, get_transaction,
    get_transaction_filter, send_tx, send_tx_reply, spawn_counting_backend,
    spawn_forward_only_shim, wire_hash, V6_IRONWOOD_ONLY, V6_MIGRATION,
};

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
            zero_indexer_shim::CautionRelay::default(),
            zero_indexer_shim::nym::MixnetStatus::default(),
            std::future::pending::<()>(),
        )
        .await;
    });
    addr
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
    let hash = wire_hash(V6_MIGRATION);
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
    let reply = get_transaction(&mut sender, shim, &wire_hash(V6_MIGRATION)).await;
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
async fn hub_down_send_transaction_fails_closed() {
    // The counterpart of the lookup case below, and the more consequential of
    // the two: a migration whose hub is unreachable must be refused to the
    // wallet, never handed to the operator to get it broadcast somehow.
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let shim = spawn_diverting_shim(backend, dead_addr().await).await;

    let mut sender = connect_h2(shim).await;
    let reply = send_tx_reply(&mut sender, shim, V6_MIGRATION).await;

    assert_eq!(reply.status, 14, "hub unreachable maps to gRPC UNAVAILABLE");
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "failing closed means the operator never sees the migration"
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

/// A `GetTransaction` body over the TxFilter cap is refused before it is
/// buffered, and never reaches the hub or the operator.
///
/// GetTransaction used to buffer its body under the SendTransaction cap of
/// 4 MiB. A TxFilter is a block id, an index and a 32-byte hash -- roughly a
/// hundred bytes at the very most -- so that cap was 4000x looser than the
/// request can ever legitimately be, and the looseness had a price: hyper allows
/// ~200 streams per connection and connections are uncapped, so a hostile
/// client trickling near-4 MiB bodies on many streams could pin gigabytes in an
/// enclave whose memory is mostly EnclaveOS. The cap is now 1 KiB. This test
/// pins it, and pins that SendTransaction still gets its 4 MiB, so the two are
/// never accidentally unified.
#[tokio::test]
async fn an_oversized_get_transaction_body_is_refused_before_it_is_buffered() {
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

    // 8 KiB of framed junk: well over the 1 KiB cap, well under the 4 MiB one,
    // so it discriminates between the two caps.
    let oversized = common::grpc_frame(&vec![0u8; 8 * 1024]);
    let reply = common::grpc_call(
        &mut sender,
        shim,
        GET_TRANSACTION,
        Full::new(oversized).boxed(),
    )
    .await;
    assert_eq!(
        reply.status, 1,
        "an oversized filter body is CANCELLED at the cap, got {} ({:?})",
        reply.status, reply.message
    );
    assert!(
        looked_up.lock().unwrap().is_none(),
        "nothing over the cap may reach the hub"
    );
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "nothing over the cap may reach the operator either"
    );

    // A legitimate hash-only filter (~40 bytes framed) is NOT tripped by the
    // cap: it goes through to the hub and is answered NotFound as configured.
    let hash = [0x42u8; 32];
    let reply = get_transaction(&mut sender, shim, &hash).await;
    assert_eq!(
        reply.status, 5,
        "a legitimate filter must clear the cap and reach the hub (NotFound here)"
    );
    assert!(
        looked_up.lock().unwrap().is_some(),
        "the hub saw the lookup"
    );

    // And SendTransaction keeps its own, larger cap: a ~100 KiB body is not
    // refused for size. It is not a valid transaction, so classification
    // rejects it downstream -- what matters here is that the status is NOT the
    // body-cap CANCELLED, proving the two caps are independent.
    let big_tx = vec![0u8; 100 * 1024];
    let reply = send_tx_reply(&mut sender, shim, &big_tx).await;
    assert_ne!(
        reply.status, 1,
        "a 100 KiB SendTransaction must not trip a body cap; the two caps are separate ({:?})",
        reply.message
    );
}
