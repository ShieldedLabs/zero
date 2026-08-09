//! The hub's connection to the Zcash network: chain tip in, transactions out.
//!
//! Two methods and nothing else. `getblockchaininfo` gives the height that
//! drives flush scheduling and expiry checks; `sendrawtransaction` publishes a
//! flushed batch. The hub deliberately does NOT run a validator in-enclave:
//! mainnet state is several hundred gigabytes and an enclave is diskless, so it
//! connects out to full nodes that already exist.
//!
//! **Multiple nodes are a correctness requirement, not redundancy theatre.** No
//! tip means no flush scheduling and no expiry checking, and no node means a
//! batch cannot be published at all, so a single node is a single point at
//! which the hub silently stops protecting anyone. Every call therefore tries
//! the configured nodes in order and only fails when all of them do.

use std::time::Duration;

use futures_util::future::join_all;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;

use crate::BoxError;

/// Per-request ceiling. A node that hangs must not stall a flush, because the
/// flush is on a block-cadence deadline: late is not merely slow here, it can
/// push a migration past its expiry height.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// One Zcash full node reachable over JSON-RPC.
#[derive(Debug, Clone)]
pub struct NodeEndpoint {
    /// `host:port`. A literal address is expected in an enclave deployment,
    /// which resolves no DNS.
    pub addr: String,
    /// Optional HTTP basic auth. zebrad with `enable_cookie_auth = false`
    /// ignores credentials entirely, but zcashd and a cookie-auth zebrad do
    /// not, so the field exists rather than assuming our own deployment's
    /// settings hold everywhere.
    pub user: Option<String>,
    pub password: Option<String>,
}

/// The outcome of publishing one transaction.
///
/// `AlreadyKnown` is a success, and saying so explicitly is load-bearing. Hub
/// failover can legitimately deliver the same migration to two hubs, and the
/// design accepts that both may publish it; the second publish is then a
/// duplicate. Treating that as an error would make normal failover look like a
/// fault and, worse, could trigger a re-submission loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Publish {
    Accepted { txid: String },
    AlreadyKnown,
    Rejected { reason: String },
}

#[derive(Deserialize)]
struct RpcEnvelope<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Deserialize, Debug)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct BlockchainInfo {
    blocks: u32,
}

/// A client over one or more full nodes.
pub struct ChainClient {
    nodes: Vec<NodeEndpoint>,
    http: Client<hyper_util::client::legacy::connect::HttpConnector, Full<bytes::Bytes>>,
}

impl ChainClient {
    pub fn new(nodes: Vec<NodeEndpoint>) -> Result<Self, BoxError> {
        if nodes.is_empty() {
            // Refused at construction rather than at the first flush, when the
            // failure would coincide with transactions being at risk of expiry.
            return Err("at least one node endpoint is required".into());
        }
        Ok(Self {
            nodes,
            http: Client::builder(TokioExecutor::new()).build_http(),
        })
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Current best-chain height: the MAX over every node that answers.
    ///
    /// Not "the first node that answers", which is a second, independent lever
    /// on the flush clock: a single lagging or hostile node would stall the
    /// cadence (freezing flushes, so everything falls out to the shim's retry
    /// ladder) or advance it (draining the queue into a near-empty batch).
    /// Taking the max means an adversary must slow down EVERY node to slow the
    /// clock, and cannot speed it up at all without a node that lies upward.
    ///
    /// Requiring agreement between nodes would be the wrong fix: tips
    /// legitimately differ by a block during propagation, so a lagging node
    /// would then stall scheduling.
    pub async fn tip_height(&self) -> Result<u32, BoxError> {
        let queries = self.nodes.iter().map(|node| async move {
            self.call::<BlockchainInfo>(node, "getblockchaininfo", serde_json::json!([]))
                .await
                .map(|info| info.blocks)
                .map_err(|err| {
                    tracing::debug!(node = %node.addr, %err, "tip query failed");
                    err
                })
        });

        let results = join_all(queries).await;
        results
            .into_iter()
            .filter_map(Result::ok)
            .max()
            .ok_or_else(|| "no node answered a tip query".into())
    }

    /// Publish one raw transaction.
    ///
    /// Submitted to EVERY configured node, not just the first that accepts. The
    /// point is to reach as much of the network as quickly as possible: a
    /// transaction that only ever entered one node's mempool is one node
    /// outage away from never being mined, and the batch's whole value depends
    /// on its members actually landing together.
    pub async fn broadcast(&self, tx_bytes: &[u8]) -> Publish {
        let hex_tx = hex::encode(tx_bytes);

        // Concurrent, not sequential. A flush publishes k transactions across n
        // nodes with a 10 s per-call ceiling; done in sequence, one hung node
        // pushes the total past a 75 s block interval, which would reintroduce
        // the very ordering the shuffle exists to remove.
        let calls = self.nodes.iter().map(|node| {
            let hex_tx = hex_tx.clone();
            async move {
                match self
                    .call::<String>(node, "sendrawtransaction", serde_json::json!([hex_tx]))
                    .await
                {
                    Ok(txid) => Publish::Accepted { txid },
                    Err(err) => classify_publish_error(&err.to_string()),
                }
            }
        });

        let outcomes = join_all(calls).await;

        outcomes
            .into_iter()
            .fold(None, |best: Option<Publish>, outcome| {
                Some(match (best, outcome) {
                    // Any acceptance wins: one node taking it is enough for the
                    // transaction to reach the network.
                    (Some(Publish::Accepted { txid }), _) => Publish::Accepted { txid },
                    (_, Publish::Accepted { txid }) => Publish::Accepted { txid },
                    // Already-known beats a rejection: it means the network has it.
                    (Some(Publish::AlreadyKnown), _) | (_, Publish::AlreadyKnown) => {
                        Publish::AlreadyKnown
                    }
                    (_, other) => other,
                })
            })
            .unwrap_or(Publish::Rejected {
                reason: "no nodes configured".into(),
            })
    }

    /// Publish a whole flushed batch, every transaction to every node, all at
    /// once.
    ///
    /// Simultaneity is the property: the batch is the anonymity set only if its
    /// members hit the network together. Publishing them one after another would
    /// re-expose exactly the arrival ordering the shuffle just destroyed, so the
    /// full (transaction x node) product is issued concurrently and the returned
    /// verdicts are positional, one per input transaction.
    pub async fn broadcast_batch(&self, txs: &[Vec<u8>]) -> Vec<Publish> {
        join_all(txs.iter().map(|tx| self.broadcast(tx))).await
    }

    async fn call<T: for<'de> Deserialize<'de>>(
        &self,
        node: &NodeEndpoint,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T, BoxError> {
        let body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        }))?;

        let mut req = hyper::Request::builder()
            .method("POST")
            .uri(format!("http://{}/", node.addr))
            .header("Content-Type", "application/json");

        if let Some(user) = &node.user {
            let raw = format!("{}:{}", user, node.password.clone().unwrap_or_default());
            req = req.header(
                "Authorization",
                format!("Basic {}", base64_encode(raw.as_bytes())),
            );
        }

        let req = req.body(Full::new(bytes::Bytes::from(body)))?;

        let resp = tokio::time::timeout(RPC_TIMEOUT, self.http.request(req))
            .await
            .map_err(|_| -> BoxError {
                format!("{method} timed out after {RPC_TIMEOUT:?}").into()
            })??;

        let bytes = resp.into_body().collect().await?.to_bytes();
        let env: RpcEnvelope<T> = serde_json::from_slice(&bytes)?;

        if let Some(err) = env.error {
            return Err(format!("rpc error {}: {}", err.code, err.message).into());
        }
        env.result
            .ok_or_else(|| "rpc returned neither result nor error".into())
    }
}

/// Map a node's rejection message onto [`Publish`].
///
/// Matched on text because the JSON-RPC error codes for these cases are not
/// consistent between zebrad and zcashd. Kept in one place, and deliberately
/// conservative: anything unrecognised is a rejection, so a message we have not
/// seen before makes the hub retry rather than silently drop a migration.
fn classify_publish_error(message: &str) -> Publish {
    // Hyphens folded to spaces before matching. Bitcoin-derived nodes report
    // these as hyphenated reject reasons (`txn-already-known`,
    // `txn-already-in-mempool`) while the longer prose forms use spaces, and
    // matching only one shape silently misses the other. That mistake is not
    // cosmetic: an already-known transaction classified as a rejection would be
    // re-submitted forever, and the retries would be a fresh timing signal
    // tied to one transaction, which is precisely what this component exists
    // to avoid emitting.
    let m = message.to_ascii_lowercase().replace('-', " ");
    if m.contains("already in block chain")
        || m.contains("already known")
        || m.contains("already in mempool")
        || m.contains("duplicate")
    {
        Publish::AlreadyKnown
    } else {
        Publish::Rejected {
            reason: message.to_string(),
        }
    }
}

/// Minimal base64, to avoid a dependency for one header.
fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_node_list_is_refused_at_construction() {
        assert!(ChainClient::new(vec![]).is_err());
    }

    #[test]
    fn duplicate_submissions_are_success_not_failure() {
        // Hub failover can deliver the same migration to two hubs and the
        // design accepts that both publish it. If this were an error the second
        // publish would look like a fault and could drive a re-submission loop.
        assert_eq!(
            classify_publish_error("transaction already in block chain"),
            Publish::AlreadyKnown
        );
        // The hyphenated forms real nodes actually emit.
        assert_eq!(
            classify_publish_error("txn-already-known"),
            Publish::AlreadyKnown
        );
        assert_eq!(
            classify_publish_error("txn-already-in-mempool"),
            Publish::AlreadyKnown
        );
        assert_eq!(
            classify_publish_error("18: txn-already-known"),
            Publish::AlreadyKnown
        );
    }

    #[test]
    fn unrecognised_errors_are_rejections_so_the_hub_retries() {
        match classify_publish_error("some node we have never seen says no") {
            Publish::Rejected { .. } => {}
            other => panic!("unrecognised errors must not be treated as success: {other:?}"),
        }
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"user:pass"), "dXNlcjpwYXNz");
    }
}
