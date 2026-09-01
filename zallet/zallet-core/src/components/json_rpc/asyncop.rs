use std::collections::HashSet;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use documented::Documented;
use futures::FutureExt;
use jsonrpsee::{
    core::{JsonValue, RpcResult},
    types::ErrorObjectOwned,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{RwLock, RwLockReadGuard};
use uuid::Uuid;

use super::server::LegacyCode;

/// An async operation ID.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize, Documented, JsonSchema)]
#[serde(try_from = "String")]
pub(crate) struct OperationId(String);

impl OperationId {
    fn new() -> Self {
        Self(format!("opid-{}", Uuid::new_v4()))
    }
}

impl TryFrom<String> for OperationId {
    type Error = ErrorObjectOwned;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let uuid = value
            .strip_prefix("opid-")
            .ok_or_else(|| LegacyCode::InvalidParameter.with_static("Invalid operation ID"))?;
        Uuid::try_parse(uuid)
            .map_err(|_| LegacyCode::InvalidParameter.with_static("Invalid operation ID"))?;
        Ok(Self(value))
    }
}

pub(super) struct ContextInfo {
    method: &'static str,
    params: JsonValue,
}

impl ContextInfo {
    pub(super) fn new(method: &'static str, params: JsonValue) -> Self {
        Self { method, params }
    }
}

/// The possible states that an async operation can be in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(into = "&'static str")]
pub(super) enum OperationState {
    Ready,
    Executing,
    Cancelled,
    Failed,
    Success,
}

impl OperationState {
    pub(super) fn parse(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Ready),
            "executing" => Some(Self::Executing),
            "cancelled" => Some(Self::Cancelled),
            "failed" => Some(Self::Failed),
            "success" => Some(Self::Success),
            _ => None,
        }
    }

    /// Returns whether an operation in this state has finished executing.
    pub(super) fn is_finished(self) -> bool {
        matches!(self, Self::Cancelled | Self::Failed | Self::Success)
    }
}

impl From<OperationState> for &'static str {
    fn from(value: OperationState) -> Self {
        match value {
            OperationState::Ready => "queued",
            OperationState::Executing => "executing",
            OperationState::Cancelled => "cancelled",
            OperationState::Failed => "failed",
            OperationState::Success => "success",
        }
    }
}

/// Data associated with an async operation.
pub(super) struct OperationData {
    state: OperationState,
    start_time: Option<SystemTime>,
    end_time: Option<SystemTime>,
    result: Option<RpcResult<Value>>,
}

/// An async operation launched by an RPC call.
pub(super) struct AsyncOperation {
    operation_id: OperationId,
    context: Option<ContextInfo>,
    creation_time: SystemTime,
    data: Arc<RwLock<OperationData>>,
}

impl AsyncOperation {
    /// Launches a new async operation.
    pub(super) async fn new<T: Serialize + Send + 'static>(
        context: Option<ContextInfo>,
        f: impl Future<Output = RpcResult<T>> + Send + 'static,
    ) -> Self {
        let creation_time = SystemTime::now();

        let data = Arc::new(RwLock::new(OperationData {
            state: OperationState::Ready,
            start_time: None,
            end_time: None,
            result: None,
        }));

        let handle = data.clone();

        crate::spawn!(
            context
                .as_ref()
                .map(|context| context.method)
                .unwrap_or("AsyncOp"),
            async move {
                // Record that the task has started.
                {
                    let mut data = handle.write().await;
                    if matches!(data.state, OperationState::Cancelled) {
                        return;
                    }
                    data.state = OperationState::Executing;
                    data.start_time = Some(SystemTime::now());
                }

                // Run the async task, mapping the concrete result into a generic JSON
                // blob. A panic is recorded as a failure of the operation, so it does
                // not remain in the `Executing` state forever.
                let res = AssertUnwindSafe(async {
                    f.await.and_then(|ret| {
                        serde_json::to_value(ret).map_err(|e| {
                            LegacyCode::Misc
                                .with_message(format!("Failed to serialize operation result: {e}"))
                        })
                    })
                })
                .catch_unwind()
                .await
                .unwrap_or_else(|panic| {
                    let msg = panic
                        .downcast_ref::<&str>()
                        .copied()
                        .map(String::from)
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".into());
                    Err(LegacyCode::Misc.with_message(format!("Async operation panicked: {msg}")))
                });
                let end_time = SystemTime::now();

                // Record the result.
                let mut data = handle.write().await;
                data.state = if res.is_ok() {
                    OperationState::Success
                } else {
                    OperationState::Failed
                };
                data.end_time = Some(end_time);
                data.result = Some(res);
            }
        );

        Self {
            operation_id: OperationId::new(),
            context,
            creation_time,
            data,
        }
    }

    /// Returns the ID of this operation.
    pub(super) fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    /// Returns the current state of this operation.
    pub(super) async fn state(&self) -> OperationState {
        self.data.read().await.state
    }

    /// Builds the current status of this operation.
    pub(super) async fn to_status(&self) -> OperationStatus {
        let data = self.data.read().await;

        let (method, params) = self
            .context
            .as_ref()
            .map(|context| (context.method, context.params.clone()))
            .unzip();

        let creation_time = self
            .creation_time
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let (error, result, execution_secs) = match &data.result {
            None => (None, None, None),
            Some(Err(e)) => (
                Some(OperationError {
                    code: e.code(),
                    message: e.message().to_string(),
                    data: e.data().map(|data| data.get().to_string()),
                }),
                None,
                None,
            ),
            Some(Ok(v)) => (
                None,
                Some(v.clone()),
                data.end_time.zip(data.start_time).map(|(end, start)| {
                    end.duration_since(start)
                        .ok()
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                }),
            ),
        };

        OperationStatus {
            id: self.operation_id.clone(),
            method,
            params,
            status: data.state,
            creation_time,
            error,
            result,
            execution_secs,
        }
    }
}

/// A bounded queue of async operations.
///
/// Operations are retained in the queue after they finish so that their results can be
/// collected with `z_getoperationresult` (matching `zcashd` behaviour). To bound the
/// memory used by the queue, inserting an operation into a full queue evicts the oldest
/// finished operations; if the queue is full of unfinished operations, the insertion is
/// rejected instead.
pub(super) struct OperationQueue {
    ops: RwLock<Vec<AsyncOperation>>,
    limit: usize,
}

impl OperationQueue {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            ops: RwLock::new(Vec::new()),
            limit,
        }
    }

    /// Launches a new async operation and inserts it into the queue.
    ///
    /// Returns an error (and does not launch the operation) if the queue is full of
    /// unfinished operations.
    pub(super) async fn insert<T: Serialize + Send + 'static>(
        &self,
        context: Option<ContextInfo>,
        f: impl Future<Output = RpcResult<T>> + Send + 'static,
    ) -> RpcResult<OperationId> {
        // Determine which operations are evictable before taking the write lock, so
        // that the queue remains readable while per-operation states are inspected.
        let finished = self.finished_ids().await;

        let mut ops = self.ops.write().await;
        if ops.len() >= self.limit {
            // Evict the oldest finished operations to make room. An operation that
            // finished after the snapshot above remains evictable by later insertions.
            let mut excess = (ops.len() + 1).saturating_sub(self.limit);
            ops.retain(|op| {
                if excess > 0 && finished.contains(op.operation_id()) {
                    excess -= 1;
                    false
                } else {
                    true
                }
            });
            if ops.len() >= self.limit {
                return Err(Self::full_error());
            }
        }

        // `AsyncOperation::new` spawns the operation's task, so it must only be called
        // once the operation is guaranteed a slot in the queue.
        let op = AsyncOperation::new(context, f).await;
        let operation_id = op.operation_id().clone();
        ops.push(op);
        Ok(operation_id)
    }

    /// Errors if a subsequent [`Self::insert`] would be rejected.
    ///
    /// Callers that do expensive work to construct an operation should check this first,
    /// so that a request rejected by the queue is rejected cheaply. The answer is
    /// necessarily advisory: the queue may fill up or drain between this check and the
    /// insertion.
    pub(super) async fn check_capacity(&self) -> RpcResult<()> {
        if self.ops.read().await.len() < self.limit || self.any_finished().await {
            Ok(())
        } else {
            Err(Self::full_error())
        }
    }

    /// Returns a read guard over the operations in the queue.
    pub(super) async fn read(&self) -> RwLockReadGuard<'_, Vec<AsyncOperation>> {
        self.ops.read().await
    }

    /// Removes the finished operations matching `filter` from the queue, returning
    /// their statuses.
    ///
    /// The queue's write lock is held for the duration, so concurrent calls cannot
    /// collect the same result twice. Insertions must go through [`Self::insert`] to
    /// maintain the queue bound.
    pub(super) async fn collect_finished(
        &self,
        filter: impl Fn(&AsyncOperation) -> bool,
    ) -> Vec<OperationStatus> {
        let mut ops = self.ops.write().await;

        let mut ret = vec![];
        let mut remove = HashSet::new();
        for op in ops.iter().filter(|op| filter(op)) {
            if op.state().await.is_finished() {
                ret.push(op.to_status().await);
                remove.insert(op.operation_id().clone());
            }
        }
        ops.retain(|op| !remove.contains(op.operation_id()));

        ret
    }

    /// Returns whether any operation in the queue has finished executing.
    ///
    /// The queue lock is not held while individual operation states are read.
    async fn any_finished(&self) -> bool {
        let snapshot = self
            .ops
            .read()
            .await
            .iter()
            .map(|op| op.data.clone())
            .collect::<Vec<_>>();

        for data in snapshot {
            if data.read().await.state.is_finished() {
                return true;
            }
        }
        false
    }

    /// Returns the IDs of the operations that have finished executing.
    ///
    /// The queue lock is not held while individual operation states are read.
    async fn finished_ids(&self) -> HashSet<OperationId> {
        let snapshot = self
            .ops
            .read()
            .await
            .iter()
            .map(|op| (op.operation_id().clone(), op.data.clone()))
            .collect::<Vec<_>>();

        let mut finished = HashSet::new();
        for (operation_id, data) in snapshot {
            if data.read().await.state.is_finished() {
                finished.insert(operation_id);
            }
        }
        finished
    }

    fn full_error() -> ErrorObjectOwned {
        LegacyCode::OutOfMemory.with_static(
            "Too many pending asynchronous operations; \
             collect finished operations with z_getoperationresult",
        )
    }
}

/// The status of an async operation.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct OperationStatus {
    id: OperationId,

    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<&'static str>,

    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<JsonValue>,

    status: OperationState,

    // The creation time, in seconds since the Unix epoch.
    creation_time: u64,

    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<OperationError>,

    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,

    /// Execution time for successful operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_secs: Option<u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
struct OperationError {
    /// Code
    code: i32,

    /// Message
    message: String,

    /// Optional data
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_for_terminal(op: &AsyncOperation) {
        for _ in 0..100 {
            if !matches!(
                op.state().await,
                OperationState::Ready | OperationState::Executing
            ) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("operation did not reach a terminal state");
    }

    #[tokio::test]
    async fn task_panic_marks_operation_failed() {
        let op = AsyncOperation::new(None, async {
            panic!("something went wrong");
            #[allow(unreachable_code)]
            Ok(0_u32)
        })
        .await;

        wait_for_terminal(&op).await;
        assert_eq!(op.state().await, OperationState::Failed);

        let status = op.to_status().await;
        let error = status.error.expect("panic should be recorded as an error");
        assert_eq!(error.code, LegacyCode::Misc as i32);
        assert_eq!(
            error.message,
            "Async operation panicked: something went wrong",
        );
    }

    #[tokio::test]
    async fn unserializable_result_marks_operation_failed() {
        let op = AsyncOperation::new(None, async {
            Ok(std::collections::HashMap::from([(vec![0_u8], 0_u32)]))
        })
        .await;

        wait_for_terminal(&op).await;
        assert_eq!(op.state().await, OperationState::Failed);

        let status = op.to_status().await;
        let error = status
            .error
            .expect("serialization failure should be recorded as an error");
        assert_eq!(error.code, LegacyCode::Misc as i32);
        assert!(error.message.starts_with("Failed to serialize"));
    }

    #[tokio::test]
    async fn successful_task_records_result() {
        let op = AsyncOperation::new(None, async { Ok(42_u32) }).await;

        wait_for_terminal(&op).await;
        assert_eq!(op.state().await, OperationState::Success);

        let status = op.to_status().await;
        assert!(status.error.is_none());
        assert_eq!(status.result, Some(Value::from(42_u32)));
    }

    use tokio::sync::oneshot;

    /// Inserts an operation that finishes once the returned sender is dropped.
    async fn insert_op(queue: &OperationQueue) -> (OperationId, oneshot::Sender<()>) {
        let (tx, rx) = oneshot::channel::<()>();
        let operation_id = queue
            .insert(None, async move {
                let _ = rx.await;
                Ok(())
            })
            .await
            .expect("queue has capacity");
        (operation_id, tx)
    }

    /// Finishes the given operation and waits for its state to reflect that.
    async fn finish_op(
        queue: &OperationQueue,
        operation_id: &OperationId,
        tx: oneshot::Sender<()>,
    ) {
        drop(tx);
        for _ in 0..100 {
            let ops = queue.read().await;
            let op = ops
                .iter()
                .find(|op| op.operation_id() == operation_id)
                .expect("operation is in the queue");
            if op.state().await.is_finished() {
                return;
            }
            drop(ops);
            tokio::task::yield_now().await;
        }
        panic!("operation did not finish");
    }

    async fn queued_ids(queue: &OperationQueue) -> Vec<OperationId> {
        queue
            .read()
            .await
            .iter()
            .map(|op| op.operation_id().clone())
            .collect()
    }

    #[tokio::test]
    async fn finished_operations_are_retained_below_the_limit() {
        let queue = OperationQueue::new(4);

        let (op_a, tx_a) = insert_op(&queue).await;
        finish_op(&queue, &op_a, tx_a).await;

        let (op_b, _tx_b) = insert_op(&queue).await;

        // Inserting a new operation must not discard an uncollected result.
        assert_eq!(queued_ids(&queue).await, vec![op_a, op_b]);
    }

    #[tokio::test]
    async fn oldest_finished_operations_are_evicted_at_the_limit() {
        let queue = OperationQueue::new(2);

        let (op_a, tx_a) = insert_op(&queue).await;
        finish_op(&queue, &op_a, tx_a).await;
        let (op_b, tx_b) = insert_op(&queue).await;
        finish_op(&queue, &op_b, tx_b).await;

        let (op_c, _tx_c) = insert_op(&queue).await;

        // Only the oldest finished operation is evicted to make room.
        assert_eq!(queued_ids(&queue).await, vec![op_b, op_c]);
    }

    #[tokio::test]
    async fn eviction_skips_unfinished_operations() {
        let queue = OperationQueue::new(3);

        let (op_a, _tx_a) = insert_op(&queue).await;
        let (op_b, tx_b) = insert_op(&queue).await;
        finish_op(&queue, &op_b, tx_b).await;
        let (op_c, _tx_c) = insert_op(&queue).await;

        let (op_d, _tx_d) = insert_op(&queue).await;

        // The unfinished head of the queue is skipped; the oldest finished operation
        // is evicted.
        assert_eq!(queued_ids(&queue).await, vec![op_a, op_c, op_d]);
    }

    #[tokio::test]
    async fn a_zero_limit_rejects_all_insertions() {
        let queue = OperationQueue::new(0);

        assert!(queue.check_capacity().await.is_err());
        assert!(queue.insert(None, async { Ok(()) }).await.is_err());
        assert!(queued_ids(&queue).await.is_empty());
    }

    #[tokio::test]
    async fn insertions_are_rejected_while_full_of_unfinished_operations() {
        let queue = OperationQueue::new(2);

        let (op_a, tx_a) = insert_op(&queue).await;
        let (op_b, _tx_b) = insert_op(&queue).await;

        assert!(queue.check_capacity().await.is_err());
        assert!(queue.insert(None, async { Ok(()) }).await.is_err());
        assert_eq!(queued_ids(&queue).await, vec![op_a.clone(), op_b.clone()]);

        // Capacity is reclaimed once an operation finishes.
        finish_op(&queue, &op_a, tx_a).await;
        assert!(queue.check_capacity().await.is_ok());
        let (op_c, _tx_c) = insert_op(&queue).await;
        assert_eq!(queued_ids(&queue).await, vec![op_b, op_c]);
    }
}
