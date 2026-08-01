//! Transparency, proved against a REAL gRPC stack on both ends of the shim.
//!
//! The backing indexer here is a tonic server built from the same vendored
//! `CompactTxStreamer` codegen that Zaino serves, and the wallet is the
//! generated tonic client. Neither one knows the shim exists. Every assertion
//! is made twice, once against the mock indexer directly and once through the
//! shim, and the two results must be identical: that is what "transparent"
//! means, stated as a test.
//!
//! `tests/proxy_transparency.rs` is the companion harness. It works at the raw
//! HTTP/2 frame level and asserts things a tonic client hides from you (byte
//! exact request frames, trailers as frames, trailers-only responses). This
//! file asserts the other half: that a real gRPC implementation is satisfied
//! end to end.
//!
//! What is deliberately NOT asserted here: any behavioural difference for a
//! migration. There is none. The proof of concept is non-destructive, so a
//! migration is classified, logged, and then forwarded exactly like everything
//! else. `tests/classify_logging.rs` is where the classification itself is
//! observed.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use tokio::net::TcpListener;
use tokio_stream::Stream;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Server};
use tonic::{Request, Response, Status, Streaming};
use zaino_proto::proto::compact_formats::{CompactBlock, CompactTx};
use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
use zaino_proto::proto::service::compact_tx_streamer_server::{
    CompactTxStreamer, CompactTxStreamerServer,
};
use zaino_proto::proto::service::{
    Address, AddressList, Balance, BlockId, BlockRange, ChainSpec, Duration as PingDuration, Empty,
    GetAddressUtxosArg, GetAddressUtxosReply, GetAddressUtxosReplyList, GetMempoolTxRequest,
    GetSubtreeRootsArg, LightdInfo, PingResponse, RawTransaction, SendResponse, SubtreeRoot,
    TransparentAddressBlockFilter, TreeState, TxFilter,
};
use zero_indexer_shim::classify::{classify, Class};

/// A real V6 Orchard exit, Orchard(+250_000) with Ironwood(-240_000): the
/// privacy-critical case. Same fixture the classifier's own vector tests use.
const V6_MIGRATION: &[u8] = include_bytes!("fixtures/v6_migration.bin");

/// The same shape reversed, Orchard(-250_000) with Ironwood(+240_000). Value
/// enters Orchard rather than leaving it, so the predicate does not fire.
const V6_REVERSE: &[u8] = include_bytes!("fixtures/v6_reverse.bin");

/// How many blocks `GetBlockRange` yields. Enough that a proxy which reordered
/// or dropped frames would be caught, small enough to stay instant.
const RANGE: std::ops::RangeInclusive<u64> = 1..=64;

/// Every await is bounded. A hang here is a real failure mode (a buffered
/// response, a missing connection task), so it should read as a failure and not
/// as a stuck test run.
const LIMIT: StdDuration = StdDuration::from_secs(10);

async fn bounded<F: Future>(fut: F) -> F::Output {
    tokio::time::timeout(LIMIT, fut)
        .await
        .expect("timed out: the shim is buffering, or a connection task is missing")
}

// ---------------------------------------------------------------- mock indexer

/// The unimplemented server-streaming methods still need a concrete type.
type Stub<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

/// A `CompactTxStreamer` with just enough behaviour to be interesting: one
/// unary method, one server-streaming method, one method that fails, and
/// `SendTransaction`, which records exactly what it was handed.
#[derive(Default)]
struct MockIndexer {
    sent: Arc<Mutex<Vec<RawTransaction>>>,
}

fn lightd_info() -> LightdInfo {
    LightdInfo {
        version: "mock-indexer-0.1".to_owned(),
        vendor: "zeronym shim test".to_owned(),
        chain_name: "regtest".to_owned(),
        block_height: 3_141_592,
        consensus_branch_id: "37a5165b".to_owned(),
        ..Default::default()
    }
}

fn compact_block(height: u64) -> CompactBlock {
    CompactBlock {
        height,
        hash: vec![height as u8; 32],
        vtx: vec![CompactTx {
            index: height,
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[tonic::async_trait]
impl CompactTxStreamer for MockIndexer {
    // --- the methods this test actually exercises ---

    async fn get_lightd_info(&self, _req: Request<Empty>) -> Result<Response<LightdInfo>, Status> {
        Ok(Response::new(lightd_info()))
    }

    type GetBlockRangeStream = Stub<CompactBlock>;

    async fn get_block_range(
        &self,
        req: Request<BlockRange>,
    ) -> Result<Response<Self::GetBlockRangeStream>, Status> {
        let range = req.into_inner();
        let start = range.start.map(|id| id.height).unwrap_or_default();
        let end = range.end.map(|id| id.height).unwrap_or_default();
        let blocks: Vec<Result<CompactBlock, Status>> = (start..=end)
            .map(|height| Ok(compact_block(height)))
            .collect();
        Ok(Response::new(Box::pin(tokio_stream::iter(blocks))))
    }

    /// Deliberately fails, so the error path has something to carry.
    async fn get_block(&self, req: Request<BlockId>) -> Result<Response<CompactBlock>, Status> {
        Err(Status::not_found(format!(
            "no block at height {}",
            req.into_inner().height
        )))
    }

    async fn send_transaction(
        &self,
        req: Request<RawTransaction>,
    ) -> Result<Response<SendResponse>, Status> {
        let raw = req.into_inner();
        let len = raw.data.len();
        self.sent.lock().unwrap().push(raw);
        Ok(Response::new(SendResponse {
            error_code: 0,
            error_message: format!("accepted {len} bytes"),
        }))
    }

    // --- everything else: present so the trait is satisfied, never called ---

    async fn get_latest_block(&self, _r: Request<ChainSpec>) -> Result<Response<BlockId>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn get_block_nullifiers(
        &self,
        _r: Request<BlockId>,
    ) -> Result<Response<CompactBlock>, Status> {
        Err(Status::unimplemented("mock"))
    }

    type GetBlockRangeNullifiersStream = Stub<CompactBlock>;

    async fn get_block_range_nullifiers(
        &self,
        _r: Request<BlockRange>,
    ) -> Result<Response<Self::GetBlockRangeNullifiersStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn get_transaction(
        &self,
        _r: Request<TxFilter>,
    ) -> Result<Response<RawTransaction>, Status> {
        Err(Status::unimplemented("mock"))
    }

    type GetTaddressTxidsStream = Stub<RawTransaction>;

    async fn get_taddress_txids(
        &self,
        _r: Request<TransparentAddressBlockFilter>,
    ) -> Result<Response<Self::GetTaddressTxidsStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    type GetTaddressTransactionsStream = Stub<RawTransaction>;

    async fn get_taddress_transactions(
        &self,
        _r: Request<TransparentAddressBlockFilter>,
    ) -> Result<Response<Self::GetTaddressTransactionsStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn get_taddress_balance(
        &self,
        _r: Request<AddressList>,
    ) -> Result<Response<Balance>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn get_taddress_balance_stream(
        &self,
        _r: Request<Streaming<Address>>,
    ) -> Result<Response<Balance>, Status> {
        Err(Status::unimplemented("mock"))
    }

    type GetMempoolTxStream = Stub<CompactTx>;

    async fn get_mempool_tx(
        &self,
        _r: Request<GetMempoolTxRequest>,
    ) -> Result<Response<Self::GetMempoolTxStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    type GetMempoolStreamStream = Stub<RawTransaction>;

    async fn get_mempool_stream(
        &self,
        _r: Request<Empty>,
    ) -> Result<Response<Self::GetMempoolStreamStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn get_tree_state(&self, _r: Request<BlockId>) -> Result<Response<TreeState>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn get_latest_tree_state(
        &self,
        _r: Request<Empty>,
    ) -> Result<Response<TreeState>, Status> {
        Err(Status::unimplemented("mock"))
    }

    type GetSubtreeRootsStream = Stub<SubtreeRoot>;

    async fn get_subtree_roots(
        &self,
        _r: Request<GetSubtreeRootsArg>,
    ) -> Result<Response<Self::GetSubtreeRootsStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn get_address_utxos(
        &self,
        _r: Request<GetAddressUtxosArg>,
    ) -> Result<Response<GetAddressUtxosReplyList>, Status> {
        Err(Status::unimplemented("mock"))
    }

    type GetAddressUtxosStreamStream = Stub<GetAddressUtxosReply>;

    async fn get_address_utxos_stream(
        &self,
        _r: Request<GetAddressUtxosArg>,
    ) -> Result<Response<Self::GetAddressUtxosStreamStream>, Status> {
        Err(Status::unimplemented("mock"))
    }

    async fn ping(&self, _r: Request<PingDuration>) -> Result<Response<PingResponse>, Status> {
        Err(Status::unimplemented("mock"))
    }
}

// ------------------------------------------------------------------ harness

/// Mock indexer plus a shim in front of it, both on ephemeral ports, plus a
/// client for each so every call can be made twice.
struct Stack {
    direct: CompactTxStreamerClient<Channel>,
    through_shim: CompactTxStreamerClient<Channel>,
    sent: Arc<Mutex<Vec<RawTransaction>>>,
}

impl Stack {
    async fn up() -> Stack {
        let sent = Arc::new(Mutex::new(Vec::new()));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = listener.local_addr().unwrap();
        let indexer = MockIndexer { sent: sent.clone() };
        tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(CompactTxStreamerServer::new(indexer))
                .serve_with_incoming(TcpIncoming::from(listener))
                .await;
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let shim = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = zero_indexer_shim::serve(listener, backend).await;
        });

        Stack {
            direct: connect(backend).await,
            through_shim: connect(shim).await,
            sent,
        }
    }

    /// What the backing indexer was handed, decoded from the wire by tonic.
    fn sent(&self) -> Vec<RawTransaction> {
        self.sent.lock().unwrap().clone()
    }
}

async fn connect(addr: SocketAddr) -> CompactTxStreamerClient<Channel> {
    // Plaintext h2c with prior knowledge, which is what the shim speaks.
    bounded(CompactTxStreamerClient::connect(format!("http://{addr}")))
        .await
        .expect("client connects")
}

// -------------------------------------------------------------------- tests

#[tokio::test]
async fn a_unary_call_round_trips_identically() {
    let mut stack = Stack::up().await;

    let direct = bounded(stack.direct.get_lightd_info(Empty {}))
        .await
        .expect("direct call succeeds")
        .into_inner();
    let proxied = bounded(stack.through_shim.get_lightd_info(Empty {}))
        .await
        .expect("proxied call succeeds")
        .into_inner();

    assert_eq!(direct, proxied);
    assert_eq!(proxied, lightd_info());
}

#[tokio::test]
async fn an_error_status_round_trips_identically() {
    let mut stack = Stack::up().await;
    let request = || BlockId {
        height: 99,
        hash: Vec::new(),
    };

    let direct = bounded(stack.direct.get_block(request()))
        .await
        .expect_err("the mock always fails this method");
    let proxied = bounded(stack.through_shim.get_block(request()))
        .await
        .expect_err("the mock always fails this method");

    // gRPC carries its status in the trailers. A proxy that drops them turns
    // this into "server closed the stream without sending a status" instead.
    assert_eq!(direct.code(), proxied.code());
    assert_eq!(direct.message(), proxied.message());
    assert_eq!(proxied.code(), tonic::Code::NotFound);
    assert_eq!(proxied.message(), "no block at height 99");
}

#[tokio::test]
async fn a_server_streaming_call_delivers_every_message_in_order() {
    let mut stack = Stack::up().await;
    let request = || BlockRange {
        start: Some(BlockId {
            height: *RANGE.start(),
            hash: Vec::new(),
        }),
        end: Some(BlockId {
            height: *RANGE.end(),
            hash: Vec::new(),
        }),
        pool_types: Vec::new(),
    };

    async fn drain(
        client: &mut CompactTxStreamerClient<Channel>,
        request: BlockRange,
    ) -> Vec<CompactBlock> {
        let mut stream = bounded(client.get_block_range(request))
            .await
            .expect("stream opens")
            .into_inner();

        let mut blocks = Vec::new();
        // tonic surfaces a missing `grpc-status` trailer as a final
        // `Some(Err(..))`, so draining to a clean `None` is itself the
        // assertion that the trailers survived the proxy.
        while let Some(block) = bounded(stream.message()).await.expect("no stream error") {
            blocks.push(block);
        }
        blocks
    }

    let direct = drain(&mut stack.direct, request()).await;
    let proxied = drain(&mut stack.through_shim, request()).await;

    assert_eq!(direct, proxied);
    assert_eq!(proxied.len(), RANGE.count());
    assert_eq!(
        proxied.iter().map(|b| b.height).collect::<Vec<_>>(),
        RANGE.collect::<Vec<_>>()
    );
    // Not just the right heights in the right order: the whole message.
    assert_eq!(proxied, RANGE.map(compact_block).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_non_migration_send_transaction_is_forwarded_unchanged() {
    // The shim reaches this verdict through the same function.
    assert_eq!(classify(V6_REVERSE), Class::PassThrough);

    let mut stack = Stack::up().await;
    let raw = RawTransaction {
        data: V6_REVERSE.to_vec(),
        height: 0,
    };

    let proxied = bounded(stack.through_shim.send_transaction(raw.clone()))
        .await
        .expect("proxied send succeeds")
        .into_inner();

    // The backing indexer's own reply, relayed rather than synthesized.
    assert_eq!(proxied.error_code, 0);
    assert_eq!(
        proxied.error_message,
        format!("accepted {} bytes", V6_REVERSE.len())
    );

    assert_eq!(stack.sent(), vec![raw]);
}

#[tokio::test]
async fn a_migration_send_transaction_is_also_forwarded_unchanged() {
    // The privacy-critical case, and the whole reason this component exists.
    // The shim classifies it (see tests/classify_logging.rs for the verdict
    // itself) and then, because this proof of concept is NON-DESTRUCTIVE,
    // forwards it anyway. Production diverts it instead.
    assert_eq!(classify(V6_MIGRATION), Class::Migration);

    let mut stack = Stack::up().await;
    let raw = RawTransaction {
        data: V6_MIGRATION.to_vec(),
        height: 0,
    };

    let direct = bounded(stack.direct.send_transaction(raw.clone()))
        .await
        .expect("direct send succeeds")
        .into_inner();
    let proxied = bounded(stack.through_shim.send_transaction(raw.clone()))
        .await
        .expect("proxied send succeeds")
        .into_inner();

    assert_eq!(direct, proxied);
    assert_eq!(
        proxied.error_message,
        format!("accepted {} bytes", V6_MIGRATION.len())
    );

    // Byte for byte, and indistinguishable from the direct call that preceded
    // it. Intercepting a migration changed nothing the indexer can see.
    assert_eq!(stack.sent(), vec![raw.clone(), raw]);
}
