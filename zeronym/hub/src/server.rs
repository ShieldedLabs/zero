//! The inbound serving path: receive a diverted migration, hold it for the batch.
//!
//! A submission is **admitted, not broadcast**. It joins the queue and is
//! published at the next cadence boundary together with everything else admitted
//! in that window (see [`crate::batcher`]). The acknowledgement carries the txid
//! the hub computes from the bytes, so the wallet gets the txid it expects
//! immediately even though publication is minutes away.
//!
//! Safety rails from `REVIEW.md` that bind on this path:
//!
//! * **Re-parse is telemetry, never a refusal (#5).** A transaction the hub
//!   cannot parse is precisely one the shim deliberately diverted because it
//!   could not read it either, so refusing it would invert the shim's fail-safe
//!   into a leak. `sendrawtransaction` at the node is the only authority on
//!   validity; the hub publishes what it is given.
//! * **Never log a txid or a transaction body (#157).** In an enclave the log
//!   reaches the parent host, and the txid is the one fact this system exists to
//!   withhold. Only counts and dispositions are logged.
//! * **Zeroize the decrypted bytes (#161).** Held bytes live in
//!   [`zeroize::Zeroizing`] for as long as they are queued.
//! * **Never return a queue depth or batch size down this channel.** That would
//!   be a live anonymity-set-size oracle for anyone who can run a shim, and a
//!   real-time "the fleet is unprotected right now" feed for an adversary
//!   choosing when to correlate. The response says admitted or refused, and
//!   nothing about how much company the entry has.

use std::convert::Infallible;
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
use zeroize::Zeroizing;

use crate::batcher::{BatchParams, TipTracker};
use crate::queue::{Admission, Queue, Refusal};
use crate::BoxError;

/// Ceiling on a submitted transaction. Real Orchard migrations are 2 to 16 KB;
/// 64 KiB is the frame the batching design pads to (`REVIEW.md`). It is a
/// deliberate, tight bound, NOT the shim's 4 MiB HTTP-body limit, which bounds a
/// wallet's request into a shim and is unrelated.
const MAX_TX_BYTES: usize = 64 * 1024;

/// Everything the serving path needs to admit a submission.
///
/// Note what is NOT here: any way to reach the network. Admission never touches
/// a node. Calling `testmempoolaccept` (or any per-submission node query) to
/// filter junk at the door would leak each transaction to a node individually at
/// arrival, which is precisely the per-transaction timing signal the batch
/// exists to destroy, handed to the node operator.
#[derive(Clone)]
pub struct Hub {
    pub queue: Arc<Queue>,
    pub tip: Arc<TipTracker>,
    pub params: BatchParams,
}

/// Accept and serve submissions on an already-bound listener until it errors.
/// Taking the listener rather than an address lets the caller (and tests) choose
/// and observe the bound port.
pub async fn serve(listener: TcpListener, hub: Hub) -> Result<(), BoxError> {
    tracing::info!(
        local = ?listener.local_addr().ok(),
        flush_interval = hub.params.flush_interval,
        "hub listening: submissions are queued and published on the flush cadence"
    );

    loop {
        let (stream, _peer) = listener.accept().await?;
        let hub = hub.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(move |req| handle(req, hub.clone())))
                .await
            {
                tracing::debug!(%err, "connection closed with error");
            }
        });
    }
}

/// Handle one submission. Never returns `Err`: a bad request is a response, not
/// a connection fault.
async fn handle(req: Request<Incoming>, hub: Hub) -> Result<Response<Full<Bytes>>, Infallible> {
    if req.method() != Method::POST {
        return Ok(text(
            StatusCode::METHOD_NOT_ALLOWED,
            "POST a raw transaction",
        ));
    }

    // Buffer the body under a hard cap, refused strictly before anything else.
    let collected = match Limited::new(req.into_body(), MAX_TX_BYTES).collect().await {
        Ok(collected) => collected,
        Err(_) => {
            return Ok(text(
                StatusCode::PAYLOAD_TOO_LARGE,
                "transaction exceeds the frame size",
            ))
        }
    };
    let tx_bytes = Zeroizing::new(collected.to_bytes().to_vec());

    if tx_bytes.is_empty() {
        return Ok(text(StatusCode::BAD_REQUEST, "empty body"));
    }

    // A stale tip means neither the flush schedule nor the expiry check can be
    // trusted, so admission stops. Refusing is fail-closed: the shim holds and
    // retries, or tries another hub. It must never answer this by handing the
    // migration to the operator.
    if hub.tip.is_stale() {
        return Ok(refused(Refusal::TipStale));
    }

    let admission = hub.queue.admit(
        tx_bytes.as_slice(),
        hub.tip.observed_height(),
        hub.params.flush_interval,
        hub.params.mining_margin,
    );

    match admission {
        // Both are success: the hub holds these bytes and will publish them.
        // Duplicate is not an error, because honest resends and cross-hub
        // submission are the designed behaviour, and identical bytes collapse.
        Admission::Admitted { txid } | Admission::Duplicate { txid } => {
            // Counts and disposition only: no txid, no body reaches the log
            // (#157). Whether it parsed is the one telemetry bit worth keeping,
            // since an unparseable payload is queued and published regardless.
            tracing::info!(
                parseable = txid.is_some(),
                "migration admitted to the batch"
            );

            // `accepted` because the hub has taken responsibility for it, which
            // is what the shim needs in order to answer the wallet. The txid is
            // computed from the bytes, so it is correct now even though the
            // transaction will not reach a node until the next flush.
            Ok(json(
                StatusCode::OK,
                &serde_json::json!({
                    "disposition": "accepted",
                    "txid": txid,
                    "reason": serde_json::Value::Null,
                }),
            ))
        }
        Admission::Refused(refusal) => {
            tracing::info!(reason = refusal.as_str(), "submission refused at admission");
            Ok(refused(refusal))
        }
    }
}

/// A typed refusal. The reason is a stable machine-readable token so the shim
/// can tell "hold and retry" from "try another hub", and it carries nothing
/// about the entry or the queue.
fn refused(refusal: Refusal) -> Response<Full<Bytes>> {
    json(
        StatusCode::OK,
        &serde_json::json!({
            "disposition": "rejected",
            "txid": serde_json::Value::Null,
            "reason": refusal.as_str(),
        }),
    )
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
