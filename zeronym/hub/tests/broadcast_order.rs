//! The flush's verdicts must line up with the flush's transactions, positionally.
//!
//! `batcher::flush` zips `broadcast_batch`'s output against the drained batch by
//! INDEX: `outcomes.get(i)` decides what happens to `batch[i]`. Nothing in the
//! type system enforces that correspondence, and the consequence of breaking it
//! is not a visible error. It is silent and it costs money: a transaction the
//! network accepted gets counted against a neighbour's `Retryable` verdict and
//! is requeued, while a transaction that genuinely failed is recorded as placed
//! and dropped -- a migration the wallet was told is on its way that no longer
//! exists anywhere.
//!
//! The fan-out is concurrent and bounded (`MAX_PUBLISHES_IN_FLIGHT`), so
//! completions genuinely arrive out of order and the ordering is a property of
//! the combinator, not of the schedule. `buffered` preserves it;
//! `buffer_unordered` does not, and swapping one for the other is a one-word
//! edit that no other test would catch. This is that test.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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
use zaino_proto::proto::service::{RawTransaction, SendResponse};

use zero_indexer_hub::chain::{ChainClient, Publish};

/// More than one bounded window's worth, so completions interleave across
/// windows rather than all landing inside the first.
const BATCH: usize = 200;

/// A transaction whose first byte says how the indexer should answer it and
/// whose next four carry its index, so a verdict can be traced back to exactly
/// one input.
fn tx(i: usize) -> Vec<u8> {
    let mut v = vec![0u8; 64];
    v[0] = if i % 3 == 0 { 1 } else { 0 }; // 1 => the indexer rejects it
    v[1..5].copy_from_slice(&(i as u32).to_le_bytes());
    v
}

fn rejected(i: usize) -> bool {
    i % 3 == 0
}

/// A gRPC OK response carrying one length-prefixed message.
fn grpc_ok(message: Vec<u8>) -> Response<BoxBody<Bytes, Infallible>> {
    let mut framed = Vec::with_capacity(5 + message.len());
    framed.push(0);
    framed.extend_from_slice(&(message.len() as u32).to_be_bytes());
    framed.extend_from_slice(&message);

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

/// An indexer that answers each `SendTransaction` from the CONTENT of that
/// transaction, so its replies are not a script whose order the test controls.
///
/// It also staggers its answers by index, longest first, so the completion order
/// is close to the reverse of the submission order. A positional bug cannot
/// survive that and still look correct.
async fn spawn_content_indexer(calls: Arc<AtomicUsize>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let calls = calls.clone();
            tokio::spawn(async move {
                let _ = http2::Builder::new(TokioExecutor::new())
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |req: Request<Incoming>| {
                            let calls = calls.clone();
                            async move {
                                let body = req.into_body().collect().await.unwrap().to_bytes();
                                // Skip the 5-byte gRPC length prefix.
                                let raw = RawTransaction::decode(&body[5..]).unwrap();
                                calls.fetch_add(1, Ordering::Relaxed);

                                let reject = raw.data[0] == 1;
                                let i = u32::from_le_bytes(raw.data[1..5].try_into().unwrap());

                                // Later indices answer sooner: completion order
                                // is roughly the reverse of submission order.
                                let delay = u64::from(BATCH as u32 - i % BATCH as u32);
                                tokio::time::sleep(std::time::Duration::from_micros(delay * 200))
                                    .await;

                                Ok::<_, Infallible>(grpc_ok(
                                    SendResponse {
                                        error_code: if reject { -26 } else { 0 },
                                        // The message carries the index, so a
                                        // misaligned verdict is legible in the
                                        // failure output rather than just "not
                                        // equal".
                                        error_message: if reject {
                                            format!("bad-txns-rejected-{i}")
                                        } else {
                                            String::new()
                                        },
                                    }
                                    .encode_to_vec(),
                                ))
                            }
                        }),
                    )
                    .await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn every_verdict_belongs_to_the_transaction_at_its_own_index() {
    let calls = Arc::new(AtomicUsize::new(0));
    let addr = spawn_content_indexer(calls.clone()).await;
    let chain = ChainClient::new(vec![addr], None).unwrap();

    let txs: Vec<Vec<u8>> = (0..BATCH).map(tx).collect();
    let outcomes = chain.broadcast_batch(&txs).await;

    assert_eq!(
        outcomes.len(),
        BATCH,
        "one verdict per input transaction, no more and no fewer"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        BATCH,
        "every transaction in the batch must actually be published; a bounded \
         fan-out that drops the tail would still return the right COUNT"
    );

    for (i, outcome) in outcomes.iter().enumerate() {
        if rejected(i) {
            match outcome {
                Publish::Rejected { reason } => assert!(
                    reason.contains(&format!("bad-txns-rejected-{i}")),
                    "verdict at index {i} carries another transaction's reason: {reason}"
                ),
                other => panic!("index {i} was rejected by the indexer, got {other:?}"),
            }
        } else {
            assert!(
                matches!(outcome, Publish::Accepted { .. }),
                "index {i} was accepted by the indexer, got {outcome:?}"
            );
        }
    }
}
