//! The inbound serving path: receive a diverted migration, broadcast it now.
//!
//! This is the CONTENT-PRIVACY hub, not the batching hub. It broadcasts each
//! migration the moment it arrives, so the operator's indexer never sees the
//! transaction, but nothing here hides timing and there is no anonymity set. The
//! queue, the flush and the batch that the design's anonymity rests on are
//! deliberately not here yet (see `lib.rs`).
//!
//! Three safety rails from `REVIEW.md` bind even without batching:
//!
//! * **Re-parse is telemetry, never a refusal (#5).** A transaction the hub
//!   cannot parse is precisely one the shim deliberately diverted because it
//!   could not read it either, so refusing it would invert the shim's fail-safe
//!   into a leak. `sendrawtransaction` at the node is the only authority on
//!   validity; the hub broadcasts what it is given.
//! * **Never log a txid or a transaction body (#157).** In an enclave the log
//!   reaches the parent host, and the txid is the one fact this system exists to
//!   withhold. Only counts and dispositions are logged.
//! * **Zeroize the decrypted bytes (#161).**

use std::convert::Infallible;
use std::io::Cursor;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use zebra_chain::{serialization::ZcashDeserialize, transaction::Transaction};
use zeroize::Zeroizing;

use crate::chain::{ChainClient, Publish};
use crate::BoxError;

/// Ceiling on a submitted transaction. Real Orchard migrations are 2 to 16 KB;
/// 64 KiB is the frame the batching design pads to (`REVIEW.md`). It is a
/// deliberate, tight bound, NOT the shim's 4 MiB HTTP-body limit, which bounds a
/// wallet's request into a shim and is unrelated.
const MAX_TX_BYTES: usize = 64 * 1024;

/// Accept and serve submissions on an already-bound listener until it errors.
/// Taking the listener rather than an address lets the caller (and tests) choose
/// and observe the bound port.
pub async fn serve(listener: TcpListener, chain: Arc<ChainClient>) -> Result<(), BoxError> {
    tracing::info!(
        local = ?listener.local_addr().ok(),
        nodes = chain.node_count(),
        "hub listening: immediate broadcast, no batching"
    );

    loop {
        let (stream, _peer) = listener.accept().await?;
        let chain = chain.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(move |req| handle(req, chain.clone())))
                .await
            {
                tracing::debug!(%err, "connection closed with error");
            }
        });
    }
}

/// Handle one submission. Never returns `Err`: a bad request is a response, not
/// a connection fault.
async fn handle(
    req: Request<Incoming>,
    chain: Arc<ChainClient>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if req.method() != Method::POST {
        return Ok(text(StatusCode::METHOD_NOT_ALLOWED, "POST a raw transaction"));
    }

    // Buffer the body under a hard cap, refused strictly before anything else.
    let collected = match Limited::new(req.into_body(), MAX_TX_BYTES).collect().await {
        Ok(collected) => collected,
        Err(_) => return Ok(text(StatusCode::PAYLOAD_TOO_LARGE, "transaction exceeds the frame size")),
    };
    let tx_bytes = Zeroizing::new(collected.to_bytes().to_vec());

    if tx_bytes.is_empty() {
        return Ok(text(StatusCode::BAD_REQUEST, "empty body"));
    }

    // Re-parse for TELEMETRY ONLY. A parse failure is never a refusal (#5); the
    // computed txid is returned to the shim so it can answer follow-up queries
    // for this migration without ever touching the operator's indexer.
    let (parseable, orchard_touching, computed_txid) =
        match Transaction::zcash_deserialize(&mut Cursor::new(tx_bytes.as_slice())) {
            Ok(tx) => (true, tx.orchard_shielded_data().is_some(), Some(tx.hash().to_string())),
            Err(_) => (false, false, None),
        };

    // Broadcast now, to every node.
    let published = chain.broadcast(tx_bytes.as_slice()).await;

    let (disposition, node_txid, reason) = match &published {
        Publish::Accepted { txid } => ("accepted", Some(txid.clone()), None),
        Publish::AlreadyKnown => ("already_known", None, None),
        Publish::Rejected { reason } => ("rejected", None, Some(reason.clone())),
    };

    // Counts and disposition only: no txid, no body reaches the log (#157).
    tracing::info!(parseable, orchard_touching, disposition, "migration broadcast");

    let body = serde_json::json!({
        "disposition": disposition,
        "txid": node_txid.or(computed_txid),
        "reason": reason,
    });
    Ok(json(StatusCode::OK, &body))
}

/// A `text/plain` response, built without a fallible builder so no serving path
/// can panic.
fn text(code: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(msg.to_owned())));
    *resp.status_mut() = code;
    resp
}

/// A `application/json` response, likewise panic-free.
fn json(code: StatusCode, value: &serde_json::Value) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(value.to_string())));
    *resp.status_mut() = code;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}
