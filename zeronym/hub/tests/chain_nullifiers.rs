//! The chain nullifier set end to end: mock indexers serve `GetLightdInfo` and
//! `GetBlockRangeNullifiers`, one sync round reads them, and the queue refuses a
//! copy of a transaction they report mined.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

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
use zaino_proto::proto::compact_formats::{CompactBlock, CompactOrchardAction, CompactTx};
use zaino_proto::proto::service::{BlockRange, LightdInfo};
use zero_indexer_hub::admission;
use zero_indexer_hub::chain::ChainClient;
use zero_indexer_hub::queue::{Admission, Queue, Refusal};
use zero_indexer_hub::spent::{sync_from, sync_once, ChainNullifiers, RESYNC_DEPTH};

const PROVEN_A: &[u8] = include_bytes!("fixtures/v6_proven_a.bin");
const PROVEN_B: &[u8] = include_bytes!("fixtures/v6_proven_b.bin");

/// Regtest reads from height 1, which keeps the chunk arithmetic readable.
const FROM: u32 = 1;
const TIP: u32 = 2500;

struct MockIndexer {
    addr: SocketAddr,
    /// Every `start..=end` it was asked for, in order.
    ranges: Arc<Mutex<Vec<(u64, u64)>>>,
}

/// An indexer at `TIP` whose chain carries `orchard` nullifiers at the given
/// heights, and nothing else.
async fn spawn_indexer(orchard: BTreeMap<u64, Vec<[u8; 32]>>) -> MockIndexer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let (orchard, seen) = (Arc::new(orchard), ranges.clone());
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (orchard, seen) = (orchard.clone(), seen.clone());
            tokio::spawn(async move {
                let _ = http2::Builder::new(TokioExecutor::new())
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |req: Request<Incoming>| {
                            let (orchard, seen) = (orchard.clone(), seen.clone());
                            async move {
                                let path = req.uri().path().to_owned();
                                let body = req.into_body().collect().await.unwrap().to_bytes();
                                let message = if body.len() > 5 { &body[5..] } else { &[][..] };
                                let messages = if path.ends_with("/GetBlockRangeNullifiers") {
                                    let range = BlockRange::decode(message).unwrap();
                                    let (start, end) =
                                        (range.start.unwrap().height, range.end.unwrap().height);
                                    seen.lock().unwrap().push((start, end));
                                    (start..=end)
                                        .map(|height| compact_block(height, &orchard))
                                        .collect()
                                } else {
                                    vec![LightdInfo {
                                        block_height: u64::from(TIP),
                                        ..Default::default()
                                    }
                                    .encode_to_vec()]
                                };
                                Ok::<_, Infallible>(grpc_ok(&messages))
                            }
                        }),
                    )
                    .await;
            });
        }
    });
    MockIndexer { addr, ranges }
}

fn compact_block(height: u64, orchard: &BTreeMap<u64, Vec<[u8; 32]>>) -> Vec<u8> {
    let actions = orchard
        .get(&height)
        .into_iter()
        .flatten()
        .map(|nf| CompactOrchardAction {
            nullifier: nf.to_vec(),
            ..Default::default()
        })
        .collect();
    CompactBlock {
        height,
        vtx: vec![CompactTx {
            actions,
            ..Default::default()
        }],
        ..Default::default()
    }
    .encode_to_vec()
}

/// Every message framed in one body, with a `grpc-status: 0` trailer.
fn grpc_ok(messages: &[Vec<u8>]) -> Response<BoxBody<Bytes, Infallible>> {
    let mut framed = Vec::new();
    for message in messages {
        framed.push(0);
        framed.extend_from_slice(&(message.len() as u32).to_be_bytes());
        framed.extend_from_slice(message);
    }
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

fn mined(height: u64, tx: &[u8]) -> BTreeMap<u64, Vec<[u8; 32]>> {
    BTreeMap::from([(height, admission::inspect(tx).orchard_nullifiers)])
}

fn admit(queue: &Queue, tx: &[u8]) -> Admission {
    queue.admit(tx, 100, 20, 4)
}

#[tokio::test]
async fn a_copy_of_a_transaction_every_indexer_reports_mined_is_refused() {
    let a = spawn_indexer(mined(1500, PROVEN_A)).await;
    let b = spawn_indexer(mined(1500, PROVEN_A)).await;
    let chain = ChainClient::new(vec![a.addr, b.addr], None).unwrap();
    let revealed = Arc::new(ChainNullifiers::new(2));
    let queue = Queue::watching(revealed.clone());

    assert_eq!(
        sync_once(&revealed, &chain, FROM).await,
        2,
        "both views current"
    );
    assert_eq!(revealed.synced_to(), vec![Some(TIP), Some(TIP)]);

    assert_eq!(
        admit(&queue, PROVEN_A),
        Admission::Refused(Refusal::Rejected)
    );
    assert!(matches!(
        admit(&queue, PROVEN_B),
        Admission::Admitted { .. }
    ));
}

#[tokio::test]
async fn one_indexer_alone_blocks_nothing() {
    // It sees every migration the hub publishes, so one that lies could invent
    // a published migration's nullifier and block the wallet's resend.
    let liar = spawn_indexer(mined(1500, PROVEN_A)).await;
    let honest = spawn_indexer(BTreeMap::new()).await;
    let chain = ChainClient::new(vec![liar.addr, honest.addr], None).unwrap();
    let revealed = Arc::new(ChainNullifiers::new(2));
    let queue = Queue::watching(revealed.clone());

    sync_once(&revealed, &chain, FROM).await;
    assert!(matches!(
        admit(&queue, PROVEN_A),
        Admission::Admitted { .. }
    ));
}

#[tokio::test]
async fn a_sync_reads_in_chunks_and_resumes_inside_the_reorg_window() {
    let indexer = spawn_indexer(BTreeMap::new()).await;
    let chain = ChainClient::new(vec![indexer.addr], None).unwrap();
    let revealed = ChainNullifiers::new(1);

    sync_once(&revealed, &chain, FROM).await;
    sync_once(&revealed, &chain, FROM).await;
    let resume = u64::from(TIP - RESYNC_DEPTH + 1);
    assert_eq!(
        *indexer.ranges.lock().unwrap(),
        vec![(1, 1000), (1001, 2000), (2001, 2500), (resume, 2500)]
    );
}

#[test]
fn mainnet_starts_where_the_proofs_do() {
    assert_eq!(
        sync_from(zero_indexer_hub::config::Network::Mainnet),
        3_428_143
    );
}
