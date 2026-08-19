//! The two intercepted methods: `SendTransaction` and `GetTransaction`.
//!
//! **`SendTransaction`** is buffered, the 5-byte gRPC length prefix stripped, the
//! `RawTransaction` decoded, and its `data` field handed to [`crate::classify`].
//! An Orchard-touching transaction is DIVERTED to the hub (never dialling the
//! operator), and the wallet gets a synthesized `SendResponse` carrying the hub's
//! txid, indistinguishable from a real indexer's reply. A pass-through, or a
//! migration when no hub is configured, is replayed to the operator's indexer
//! unchanged.
//!
//! **`GetTransaction`**, when a hub is configured, is answered by the hub (from
//! its queue, or its own indexer), never by the operator's. The shim keeps NO
//! record of what it diverted, so it cannot tell a migration's txid from any
//! other; routing every `GetTransaction` to the hub is what keeps a migration's
//! follow-up query off the operator's indexer. With no hub it passes through.
//!
//! Fail-safe for privacy binds at every layer above the classifier. A
//! `SendTransaction` the shim cannot read cleanly is treated as a migration and
//! diverted, never handed to the operator; these cases are decided here rather
//! than in the classifier:
//!
//! * a body shorter than the 5-byte prefix,
//! * the gRPC compression flag set, or a `grpc-encoding` other than `identity`,
//! * a declared message length that overruns or underruns the body,
//! * a `RawTransaction` that does not decode,
//! * a body over [`MAX_SEND_TX_BYTES`], or a client body stream that errored.
//!
//! And every failure on the divert or lookup path fails CLOSED: an unreachable
//! hub is a gRPC error to the wallet, never a fall-back to the operator, because
//! answering there is the exact leak this component exists to prevent.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Request, Response};
use http_body::{Body, Frame};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::body::Incoming;
use prost::Message;
use zaino_proto::proto::service::{RawTransaction, SendResponse, TxFilter};

use crate::classify::{classify_with_evidence, Class, Evidence};
use crate::hub::{HubTransport, Lookup, Submit};
use crate::proxy::{
    forward, grpc_error, pass_through, ProxyBody, UpstreamPool, GRPC_CANCELLED,
    GRPC_DEADLINE_EXCEEDED, GRPC_INVALID_ARGUMENT, GRPC_NOT_FOUND, GRPC_RESOURCE_EXHAUSTED,
    GRPC_UNAVAILABLE,
};
use crate::BoxError;

/// The context that turns the classifying proof of concept into a diverting
/// shim: just where to send migrations and look them back up. Present only when
/// `--hub` is configured; absent means forward-only, the merged proof-of-concept
/// behaviour.
///
/// Deliberately holds NO state about what it diverted. A stateless shim survives
/// a restart and can run as more than one instance without a follow-up query
/// leaking to the operator, because it recognises nothing: every
/// `GetTransaction` goes to the hub regardless.
pub struct Diversion {
    pub hub: HubTransport,
}

/// gRPC length-prefixed message header: 1 flag byte + 4 big-endian length bytes.
const GRPC_PREFIX_LEN: usize = 5;

/// Cap on a buffered `SendTransaction` body. Well above the 2 MB Zcash
/// transaction limit, so a legitimate wallet never reaches it, while a hostile
/// client cannot make the shim buffer unbounded memory.
const MAX_SEND_TX_BYTES: usize = 4 * 1024 * 1024;

/// Cap on a buffered `GetTransaction` body, which is a `TxFilter`: a block id, an
/// index and a 32-byte hash, roughly 100 bytes at the very most. This used to
/// share `MAX_SEND_TX_BYTES`, which was 4000x looser than the request can ever
/// legitimately be, and the looseness had a price: hyper allows ~200 streams per
/// connection and connections are uncapped, so a hostile client trickling
/// near-4 MiB bodies on many streams could pin gigabytes in an enclave whose
/// memory is mostly EnclaveOS. A kilobyte refuses nothing a wallet sends and
/// takes that lever away.
const MAX_TX_FILTER_BYTES: usize = 1024;

/// Ceiling on reading a request body off the wire.
///
/// The size caps above bound how MUCH a client may send; they do not bound how
/// LONG it may take. A client that sends one byte a minute stays under every
/// size limit forever while holding a connection and a task. hyper's
/// `header_read_timeout` does not help: it covers the head only, and stops
/// applying the moment the body starts.
///
/// Thirty seconds is far above any honest client on the slowest path measured
/// (an enclave sustains ~220 KB/s outbound, so even a maximum-size body is ~10 s)
/// and far below the forever a slow-loris wants.
const BODY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Handle a request routed to the `SendTransaction` method.
///
/// The HTTP method is not checked here or by the caller, on purpose: see rule 3
/// in [`crate::proxy`]. A backend that acts on a `GET` must not be handed one
/// the classifier never saw.
pub(crate) async fn send_transaction(
    req: Request<Incoming>,
    pool: Arc<UpstreamPool>,
    diversion: Option<Arc<Diversion>>,
) -> Result<Response<ProxyBody>, BoxError> {
    let (parts, body) = req.into_parts();

    // The only buffering in the entire shim, and it is bounded.
    let read = Limited::new(body, MAX_SEND_TX_BYTES).collect();
    let collected = match tokio::time::timeout(BODY_READ_TIMEOUT, read).await {
        Ok(Ok(collected)) => collected,
        Ok(Err(err)) => return Ok(body_read_failed(err)),
        // Fail closed, exactly as an oversized body does: an unclassifiable
        // transaction is never forwarded to the operator, because forwarding it
        // is the one outcome the whole component exists to prevent.
        Err(_) => {
            tracing::warn!(
                target: "zis::classify",
                "MIGRATION-FAILSAFE: SendTransaction body read timed out,                  refusing to forward a body that could not be classified"
            );
            return Ok(grpc_error(
                GRPC_DEADLINE_EXCEEDED,
                "zero-indexer-shim: SendTransaction body read timed out",
            ));
        }
    };

    let trailers = collected.trailers().cloned();
    let frame = collected.to_bytes();

    let (inspection, tx_data) = inspect(&parts.headers, &frame);
    log_verdict(&inspection, &frame);

    // A migration bound for the hub is diverted here, and ONLY here does the
    // operator's indexer stay undialled: this function holds the pool and dials
    // it (below) solely on a pass-through verdict, or on a migration when no hub
    // is configured. Moving the dial out of `handle` is what makes that true.
    if inspection.treat_as_migration() {
        if let Some(diversion) = diversion {
            return divert(&diversion, tx_data).await;
        }
        // Forward-only: no hub configured, so behave exactly like the merged
        // proof of concept and forward the migration to the operator. No
        // privacy, but no behaviour change until an operator sets `--hub`.
    }

    // Pass-through, or a migration with no hub: replay the ORIGINAL bytes to the
    // backing indexer, which sees exactly what the wallet sent, trailers and all.
    let upstream = pool.get().await?;
    let replay = ReplayBody::new(frame, trailers).boxed();
    let resp = forward(upstream, Request::from_parts(parts, replay)).await?;
    Ok(resp.map(|body| body.map_err(BoxError::from).boxed()))
}

/// Send a migration to the hub instead of the operator's indexer, then answer
/// the wallet with a synthesized `SendResponse`. The operator's indexer is never
/// contacted on this path.
async fn divert(
    diversion: &Diversion,
    tx_data: Option<Bytes>,
) -> Result<Response<ProxyBody>, BoxError> {
    // A fail-safe verdict with no clean transaction bytes (compression flag,
    // truncated frame, undecodable RawTransaction): the shim cannot broadcast
    // what it could not read, and must not forward it to the operator. Fail
    // closed, per REVIEW #11: the wallet retries, and a migration is never leaked
    // to recover an availability failure.
    let Some(tx_data) = tx_data else {
        tracing::warn!(
            target: "zis::classify",
            "MIGRATION: body could not be read cleanly; failing closed rather than diverting or forwarding"
        );
        return Ok(grpc_error(
            GRPC_UNAVAILABLE,
            "zero-indexer-shim: could not divert transaction",
        ));
    };

    match diversion.hub.submit(&tx_data).await {
        // Too large for the transport's fixed frame. RESOURCE_EXHAUSTED, not
        // UNAVAILABLE: this can never succeed, and UNAVAILABLE is the status
        // that tells a wallet to retry. It is never forwarded to the operator
        // and never broadcast another way; not fitting the frame is the price
        // of leaking zero bits of length.
        //
        // The log line carries the LIMIT, never the transaction's own size:
        // that number would otherwise reach the parent host, which is the one
        // reader D4 exists to keep it from.
        Ok(Submit::TooLarge { limit }) => {
            tracing::warn!(
                target: "zis::classify",
                limit,
                "MIGRATION: too large for the hub frame; refusing rather than diverting or forwarding"
            );
            Ok(grpc_error(
                GRPC_RESOURCE_EXHAUSTED,
                &format!("zero-indexer-shim: transaction exceeds the {limit}-byte hub frame limit"),
            ))
        }
        Ok(submit) => {
            // The SendTransaction reply is a `SendResponse { error_code,
            // error_message }`: on success `error_code` is 0 and `error_message`
            // carries the txid, exactly as lightwalletd answers, so an unmodified
            // wallet reads the txid it expects.
            let (error_code, message) = match submit {
                Submit::Accepted { txid } => (0, txid),
                Submit::AlreadyKnown { txid } => (0, txid.unwrap_or_default()),
                Submit::Rejected { reason } => (-1, reason),
                // Answered above, as a gRPC status rather than a SendResponse.
                Submit::TooLarge { .. } => unreachable!("handled in its own arm"),
            };

            // Nothing is recorded: the shim keeps no map of what it diverted. A
            // follow-up GetTransaction is answered by the hub, not from local
            // state, which is what makes this shim safe to restart or replicate.

            tracing::info!(
                target: "zis::classify",
                accepted = error_code == 0,
                "migration diverted to the hub"
            );
            Ok(grpc_send_response(error_code, &message))
        }
        Err(err) => {
            // Hub unreachable after the client's own attempt. Fail closed; do NOT
            // fall back to the operator's indexer (that is the leak) or to direct
            // broadcast (off by default, REVIEW #11).
            tracing::warn!(target: "zis::classify", %err, "hub unreachable; failing closed");
            Ok(grpc_error(
                GRPC_UNAVAILABLE,
                "zero-indexer-shim: hub unreachable",
            ))
        }
    }
}

/// A synthesized unary `SendTransaction` response: the framed `SendResponse`
/// message with a `grpc-status: 0` trailer, the exact shape a real indexer's
/// reply has, so the wallet cannot tell the transaction was diverted.
fn grpc_send_response(error_code: i32, error_message: &str) -> Response<ProxyBody> {
    let message = SendResponse {
        error_code,
        error_message: error_message.to_owned(),
    }
    .encode_to_vec();
    grpc_unary(&message)
}

/// Handle `GetTransaction`.
///
/// With a hub configured, EVERY `GetTransaction` is answered via the hub and NONE
/// reaches the operator's indexer. A stateless shim cannot tell a migration's
/// txid from any other, so the only way to keep a migration's follow-up query
/// off the operator is to route them all to the hub, which answers from its
/// queue (a diverted, unflushed migration) or from its own indexer. Forward-only
/// mode (no hub) passes through to the operator unchanged.
pub(crate) async fn get_transaction(
    req: Request<Incoming>,
    pool: Arc<UpstreamPool>,
    diversion: Option<Arc<Diversion>>,
) -> Result<Response<ProxyBody>, BoxError> {
    // No hub: nothing is diverted and there is nothing to hide, so relay
    // untouched, without buffering.
    let Some(diversion) = diversion else {
        return pass_through(req, pool).await;
    };

    let (parts, body) = req.into_parts();
    let read = Limited::new(body, MAX_TX_FILTER_BYTES).collect();
    let collected = match tokio::time::timeout(BODY_READ_TIMEOUT, read).await {
        Ok(Ok(collected)) => collected,
        Ok(Err(_)) => {
            return Ok(grpc_error(
                GRPC_CANCELLED,
                "zero-indexer-shim: GetTransaction body could not be read",
            ))
        }
        Err(_) => {
            return Ok(grpc_error(
                GRPC_DEADLINE_EXCEEDED,
                "zero-indexer-shim: GetTransaction body read timed out",
            ))
        }
    };
    let frame = collected.to_bytes();

    let filter = match decode_tx_filter(&parts.headers, &frame) {
        Ok(filter) => filter,
        // With a hub configured this path may never dial the operator, so an
        // undecodable request is a terminal INVALID_ARGUMENT, not a forward.
        Err(reason) => {
            return Ok(grpc_error(
                GRPC_INVALID_ARGUMENT,
                &format!("zero-indexer-shim: {reason}"),
            ))
        }
    };

    // Validate the filter locally, matching lightwalletd and Zaino, so a bad
    // filter never becomes a hub round trip.
    if filter.hash.is_empty() {
        let message = if filter.block.is_some() {
            "GetTransaction: specify a txid, not a blockhash+num"
        } else {
            "GetTransaction: specify a txid"
        };
        return Ok(grpc_error(
            GRPC_INVALID_ARGUMENT,
            &format!("zero-indexer-shim: {message}"),
        ));
    }
    if filter.hash.len() != 32 {
        return Ok(grpc_error(
            GRPC_INVALID_ARGUMENT,
            &format!(
                "zero-indexer-shim: GetTransaction: transaction ID has invalid length: {}",
                filter.hash.len()
            ),
        ));
    }

    match diversion.hub.get_transaction(&filter.hash).await {
        Ok(Lookup::Found { data, height }) => {
            // L4: verify the hub returned the transaction that was ASKED for. A
            // hub, buggy or hostile, that answers a query with a DIFFERENT
            // transaction must not have it served to the wallet as the answer:
            // the wallet asked for one txid and would take whatever bytes came
            // back as that txid. On a mismatch, fail closed as not_found rather
            // than serve the wrong transaction. This binds both transports, since
            // both arrive here as `Lookup::Found`.
            if lookup_is_for_query(&data, &filter.hash) {
                Ok(get_transaction_response(&data, height))
            } else {
                tracing::warn!(
                    target: "zis::classify",
                    "the hub returned a transaction whose txid does not match the query; refusing it"
                );
                Ok(grpc_error(GRPC_NOT_FOUND, &not_found_message(&filter.hash)))
            }
        }
        Ok(Lookup::NotFound) => Ok(grpc_error(GRPC_NOT_FOUND, &not_found_message(&filter.hash))),
        Err(err) => {
            // Fail closed. Never fall back to the operator's indexer: answering a
            // migration's follow-up query there is the exact leak this prevents.
            tracing::warn!(target: "zis::classify", %err, "hub lookup failed; failing closed");
            Ok(grpc_error(
                GRPC_UNAVAILABLE,
                "zero-indexer-shim: hub unreachable",
            ))
        }
    }
}

/// Unwrap a single gRPC-framed `TxFilter` out of a `GetTransaction` request.
///
/// Strict, like `inspect`: identity encoding only, the 5-byte length prefix must
/// exactly account for the rest of the body, and the message must decode. A
/// request that fails any of these is INVALID_ARGUMENT, never forwarded.
fn decode_tx_filter(headers: &HeaderMap, frame: &[u8]) -> Result<TxFilter, &'static str> {
    if let Some(encoding) = headers.get("grpc-encoding") {
        if encoding.as_bytes() != b"identity" {
            return Err("GetTransaction: a compressed request is not supported");
        }
    }
    if frame.len() < GRPC_PREFIX_LEN || frame[0] != 0 {
        return Err("GetTransaction: request is not a single identity-coded frame");
    }
    let declared = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
    if GRPC_PREFIX_LEN.checked_add(declared) != Some(frame.len()) {
        return Err("GetTransaction: request frame length does not match its body");
    }
    TxFilter::decode(&frame[GRPC_PREFIX_LEN..])
        .map_err(|_| "GetTransaction: could not decode TxFilter")
}

/// Whether the transaction the hub returned is actually the one that was queried
/// (L4). Compute the returned transaction's txid and compare it to the queried
/// hash. Bytes that do not parse cannot be verified and do not match. Both byte
/// orders are accepted, because a txid's wire (internal) order is the reverse of
/// its display order and the hub checks both against its queue; matching either
/// confirms identity, and a different transaction matches neither.
fn lookup_is_for_query(tx_bytes: &[u8], wire_hash: &[u8]) -> bool {
    use zebra_chain::serialization::ZcashDeserialize;
    let Ok(tx) = zebra_chain::transaction::Transaction::zcash_deserialize(
        &mut std::io::Cursor::new(tx_bytes),
    ) else {
        return false;
    };
    let txid = tx.hash().to_string();
    let direct = hex_prefix(wire_hash, wire_hash.len());
    let mut reversed = wire_hash.to_vec();
    reversed.reverse();
    let reversed = hex_prefix(&reversed, reversed.len());
    txid.eq_ignore_ascii_case(&direct) || txid.eq_ignore_ascii_case(&reversed)
}

/// The message lightwalletd returns for an unknown txid, naming the DISPLAY txid
/// (the byte-reverse of the wire hash). The wallet already knows its own txid, so
/// echoing it here reveals nothing, and it goes only to the wallet over its TLS
/// link, never to a log.
fn not_found_message(wire_hash: &[u8]) -> String {
    let mut display = wire_hash.to_vec();
    display.reverse();
    format!(
        "zero-indexer-shim: GetTransaction: getrawtransaction {} failed: -5: No such mempool or main chain transaction",
        hex_prefix(&display, display.len())
    )
}

/// A synthesized `GetTransaction` reply carrying the transaction the hub
/// returned. Height 0 (from a queue hit) is the mempool sentinel; a mined
/// transaction relays the indexer's height.
fn get_transaction_response(tx_bytes: &[u8], height: u64) -> Response<ProxyBody> {
    let message = RawTransaction {
        data: tx_bytes.to_vec().into(),
        height,
    }
    .encode_to_vec();
    grpc_unary(&message)
}

/// Frame one unary protobuf message into a gRPC response with a `grpc-status: 0`
/// trailer, the shape a real indexer's unary reply has.
fn grpc_unary(message: &[u8]) -> Response<ProxyBody> {
    let mut framed = Vec::with_capacity(GRPC_PREFIX_LEN + message.len());
    framed.push(0);
    framed.extend_from_slice(&(message.len() as u32).to_be_bytes());
    framed.extend_from_slice(message);

    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", HeaderValue::from_static("0"));

    let body = ReplayBody::new(Bytes::from(framed), Some(trailers)).boxed();
    let mut resp = Response::new(body);
    resp.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    resp
}

/// The one case the proof of concept refuses to forward, split by cause.
///
/// `Limited::collect()` fails for two quite different reasons and telling an
/// operator the wrong one costs them an afternoon: either the body really did
/// exceed [`MAX_SEND_TX_BYTES`], or the CLIENT's body stream errored (a reset
/// mid-upload, a broken connection, a content-length mismatch). Both are
/// fail-safes and neither is classifiable, so both get the fail-safe log line,
/// but they get different reasons and different gRPC statuses.
fn body_read_failed(err: BoxError) -> Response<ProxyBody> {
    if err.is::<LengthLimitError>() {
        tracing::warn!(
            target: "zis::classify",
            limit = MAX_SEND_TX_BYTES,
            %err,
            "MIGRATION-FAILSAFE: SendTransaction body exceeded the buffer limit, \
             refusing to forward a body that could not be classified"
        );
        return grpc_error(
            GRPC_RESOURCE_EXHAUSTED,
            "zero-indexer-shim: SendTransaction body too large to classify",
        );
    }

    tracing::warn!(
        target: "zis::classify",
        %err,
        "MIGRATION-FAILSAFE: SendTransaction body could not be read from the client, \
         refusing to forward a body that could not be classified"
    );
    grpc_error(
        GRPC_CANCELLED,
        "zero-indexer-shim: SendTransaction body could not be read",
    )
}

/// What the shim was able to learn about one `SendTransaction` body.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Inspection {
    /// The transaction bytes reached the classifier. Carries its verdict.
    Classified(Evidence),
    /// The body could not be unwrapped far enough to classify it. Fail-safe:
    /// production treats this as a migration.
    Failsafe {
        reason: &'static str,
        detail: Option<String>,
    },
}

impl Inspection {
    fn failsafe(reason: &'static str) -> Self {
        Inspection::Failsafe {
            reason,
            detail: None,
        }
    }

    fn failsafe_with(reason: &'static str, detail: impl Into<String>) -> Self {
        Inspection::Failsafe {
            reason,
            detail: Some(detail.into()),
        }
    }

    /// The routing decision. `true` means "do not hand this to the backing
    /// indexer" (in production: divert to the hub). The proof of concept
    /// forwards regardless and only logs this.
    fn treat_as_migration(&self) -> bool {
        match self {
            // Note the call: branching on `treat_as_migration()` rather than on
            // `== Class::Migration` is what folds `Unparseable` into the
            // migration arm. A match that let `Unparseable` fall through to
            // pass-through would be the leak.
            Inspection::Classified(evidence) => evidence.class.treat_as_migration(),
            Inspection::Failsafe { .. } => true,
        }
    }
}

/// Unwrap one buffered unary request body down to the transaction bytes and
/// classify them. Pure: no I/O, no state.
fn inspect(headers: &HeaderMap, frame: &[u8]) -> (Inspection, Option<Bytes>) {
    // Message-level compression is negotiated by header and flagged per
    // message. A compressed body is not the protobuf we would decode, so it
    // fails safe here. Note that this is the SECOND line of defence, not the
    // first: `proxy::normalize_response_encoding` rewrites the indexer's
    // advertised `grpc-accept-encoding` to `identity` on the way back, so a
    // wallet never negotiates message compression through the shim in the first
    // place. Without that, an operator could blind the classifier by turning
    // compression on in their own indexer.
    if let Some(encoding) = headers.get("grpc-encoding") {
        if encoding.as_bytes() != b"identity" {
            return (
                Inspection::failsafe_with(
                    "grpc-encoding is not identity",
                    String::from_utf8_lossy(encoding.as_bytes()).into_owned(),
                ),
                None,
            );
        }
    }

    if frame.len() < GRPC_PREFIX_LEN {
        return (
            Inspection::failsafe("gRPC frame shorter than its 5-byte prefix"),
            None,
        );
    }
    if frame[0] != 0 {
        return (Inspection::failsafe("gRPC compression flag set"), None);
    }

    let declared = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
    // `checked_add`, because `declared` is attacker-controlled and 32-bit
    // targets exist: `GRPC_PREFIX_LEN + declared` can wrap, which in a debug
    // build panics inside the proxy instead of landing in this fail-safe.
    let Some(message) = GRPC_PREFIX_LEN
        .checked_add(declared)
        .and_then(|end| frame.get(GRPC_PREFIX_LEN..end))
    else {
        return (
            Inspection::failsafe_with(
                "gRPC message truncated",
                format!(
                    "declared {declared} bytes, body carries {}",
                    frame.len() - GRPC_PREFIX_LEN
                ),
            ),
            None,
        );
    };
    // A unary request carries exactly one message and nothing after it.
    if message.len() != frame.len() - GRPC_PREFIX_LEN {
        return (
            Inspection::failsafe_with(
                "trailing bytes after the unary gRPC message",
                format!(
                    "declared {declared} bytes, body carries {}",
                    frame.len() - GRPC_PREFIX_LEN
                ),
            ),
            None,
        );
    }

    match RawTransaction::decode(message) {
        // `data` is the serialized Zcash transaction: the only value the
        // classifier ever sees, and the exact bytes the hub broadcasts.
        Ok(raw) => {
            let evidence = classify_with_evidence(&raw.data);
            (
                Inspection::Classified(evidence),
                Some(Bytes::from(raw.data)),
            )
        }
        Err(err) => (
            Inspection::failsafe_with("RawTransaction decode failed", err.to_string()),
            None,
        ),
    }
}

/// Log the classification verdict -- as a COUNT, not a description.
///
/// This used to be "the proof of concept's visible output" and logged, at INFO
/// (the default level), every diverted transaction's expiry height, its
/// Orchard/Ironwood/Sapling value balances, action count, input/output counts
/// and length; the failsafe arms additionally logged a hex prefix of the raw
/// body. In an enclave the log reaches the parent host, so that was a
/// fingerprint the operator could hold for 25 minutes and match against the
/// batch when it published -- exactly the wallet-to-txid link the shim exists to
/// break. The `TooLarge` arm in `send_transaction` already refused to log the
/// size for this reason; the rest of the file had not caught up.
///
/// So: the INFO line carries the class and the disposition and nothing else,
/// which is what an operator needs to see that diversion is happening. Every
/// per-transaction field is on a DEBUG line, which is off by default and which
/// the operator guide already says never to raise in a deployed enclave. The
/// hex body prefix is gone entirely: there is no diagnostic value in raw
/// transaction bytes that justifies writing them where the operator can read
/// them.
fn log_verdict(inspection: &Inspection, frame: &[u8]) {
    let diverted_in_production = inspection.treat_as_migration();

    match inspection {
        Inspection::Classified(evidence) => {
            let class = match evidence.class {
                Class::Migration => "migration",
                Class::PassThrough => "passthrough",
                Class::Unparseable => "unparseable",
            };
            tracing::info!(
                target: "zis::classify",
                class,
                diverted_in_production,
                "SendTransaction classified"
            );
            // The evidence behind the verdict, for local debugging only. Never
            // at info: see the doc comment above.
            tracing::debug!(
                target: "zis::classify",
                class,
                version = %evidence.version,
                orchard_actions = evidence.orchard_actions,
                orchard_vb = %format!("{:+}", evidence.orchard_vb),
                ironwood_vb = %format!("{:+}", evidence.ironwood_vb),
                sapling_vb = %format!("{:+}", evidence.sapling_vb),
                expiry = ?evidence.expiry_height,
                inputs = evidence.inputs,
                outputs = evidence.outputs,
                tx_len = evidence.len,
                error = evidence.error.as_deref().unwrap_or("(none)"),
                frame_len = frame.len(),
                "classification evidence"
            );
        }
        Inspection::Failsafe { reason, detail } => {
            // A failsafe is a real operational signal (the classifier could not
            // run at all), so it stays at warn -- but with the REASON, not the
            // body. `frame_len` alone is not a fingerprint; a body prefix is.
            tracing::warn!(
                target: "zis::classify",
                reason,
                detail = detail.as_deref().unwrap_or("(none)"),
                frame_len = frame.len(),
                diverted_in_production,
                "MIGRATION-FAILSAFE: SendTransaction body could not be classified, \
                 treating as migration"
            );
        }
    }
}

/// Lowercase hex of the first `n` bytes. Local so the shipped binary does not
/// link a hex crate for one log line.
fn hex_prefix(bytes: &[u8], n: usize) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(2 * n);
    for byte in bytes.iter().take(n) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Replays a buffered request body: one DATA frame, then the client's trailers
/// if it sent any. Byte-exact, which is what makes the interception invisible
/// to the backing indexer.
pub struct ReplayBody {
    data: Option<Bytes>,
    trailers: Option<HeaderMap>,
}

impl ReplayBody {
    fn new(data: Bytes, trailers: Option<HeaderMap>) -> Self {
        ReplayBody {
            data: Some(data),
            trailers,
        }
    }
}

impl Body for ReplayBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        if let Some(data) = this.data.take() {
            if !data.is_empty() {
                return Poll::Ready(Some(Ok(Frame::data(data))));
            }
        }
        if let Some(trailers) = this.trailers.take() {
            return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
        }
        Poll::Ready(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real V6 carrying Orchard actions: Orchard(+250_000),
    /// Ironwood(-240_000). Same fixture the classifier's own vector tests use.
    const V6_MIGRATION: &[u8] = include_bytes!("../tests/fixtures/v6_migration.bin");

    /// The same shape reversed: Orchard(-250_000), Ironwood(+240_000). The
    /// Orchard actions are still there, so the verdict is unchanged.
    const V6_REVERSE: &[u8] = include_bytes!("../tests/fixtures/v6_reverse.bin");

    /// V6 with an Ironwood bundle and NO Orchard bundle: ordinary commerce in
    /// the new pool, and the pass-through case at this layer.
    const V6_IRONWOOD_ONLY: &[u8] = include_bytes!("../tests/fixtures/v6_ironwood_only.bin");

    /// Wrap transaction bytes in a `RawTransaction` inside a gRPC length prefix,
    /// the way a wallet's gRPC client does.
    fn framed(tx: &[u8]) -> Vec<u8> {
        let message = RawTransaction {
            data: tx.to_vec().into(),
            height: 0,
        }
        .encode_to_vec();

        let mut frame = Vec::with_capacity(GRPC_PREFIX_LEN + message.len());
        frame.push(0);
        frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
        frame.extend_from_slice(&message);
        frame
    }

    fn classified(inspection: &Inspection) -> Class {
        match inspection {
            Inspection::Classified(evidence) => evidence.class,
            other => panic!("expected a classified body, got {other:?}"),
        }
    }

    #[test]
    fn a_framed_migration_reaches_the_classifier() {
        let (inspection, _) = inspect(&HeaderMap::new(), &framed(V6_MIGRATION));
        assert_eq!(classified(&inspection), Class::Migration);
        assert!(inspection.treat_as_migration());
    }

    #[test]
    fn a_framed_ironwood_only_transaction_is_a_pass_through() {
        // No Orchard bundle, so nothing about legacy Orchard holdings is on the
        // wire and the transaction is forwarded. This is ordinary commerce in
        // the new pool, which the widened rule must not swallow.
        let (inspection, _) = inspect(&HeaderMap::new(), &framed(V6_IRONWOOD_ONLY));
        assert_eq!(classified(&inspection), Class::PassThrough);
        assert!(!inspection.treat_as_migration());
    }

    #[test]
    fn a_framed_orchard_bundle_is_a_migration_whichever_way_its_balance_points() {
        // Same fixture pair, opposite Orchard value balances, one verdict. The
        // sign stopped mattering when the predicate became presence of actions.
        let (inspection, _) = inspect(&HeaderMap::new(), &framed(V6_REVERSE));
        assert_eq!(classified(&inspection), Class::Migration);
        assert!(inspection.treat_as_migration());
    }

    #[test]
    fn compression_flag_fails_safe() {
        let mut frame = framed(V6_MIGRATION);
        frame[0] = 1;
        let (inspection, _) = inspect(&HeaderMap::new(), &frame);
        assert!(matches!(inspection, Inspection::Failsafe { .. }));
        assert!(inspection.treat_as_migration());
    }

    #[test]
    fn grpc_encoding_header_fails_safe() {
        let mut headers = HeaderMap::new();
        headers.insert("grpc-encoding", "gzip".parse().unwrap());
        let (inspection, _) = inspect(&headers, &framed(V6_MIGRATION));
        assert!(matches!(inspection, Inspection::Failsafe { .. }));
        assert!(inspection.treat_as_migration());
    }

    #[test]
    fn identity_encoding_is_not_treated_as_compression() {
        let mut headers = HeaderMap::new();
        headers.insert("grpc-encoding", "identity".parse().unwrap());
        assert_eq!(
            classified(&inspect(&headers, &framed(V6_MIGRATION)).0),
            Class::Migration
        );
    }

    #[test]
    fn short_truncated_and_trailing_frames_fail_safe() {
        let frame = framed(V6_MIGRATION);

        for body in [
            &[][..],
            &[0][..],
            &frame[..GRPC_PREFIX_LEN - 1],
            // Declared length overruns the body.
            &frame[..frame.len() - 1],
        ] {
            let (inspection, _) = inspect(&HeaderMap::new(), body);
            assert!(
                matches!(inspection, Inspection::Failsafe { .. }),
                "expected a fail-safe for a {}-byte body",
                body.len()
            );
            assert!(inspection.treat_as_migration());
        }

        // A second message appended after the unary one.
        let mut trailing = frame.clone();
        trailing.extend_from_slice(&[0, 0, 0, 0, 0]);
        let (inspection, _) = inspect(&HeaderMap::new(), &trailing);
        assert!(matches!(inspection, Inspection::Failsafe { .. }));
        assert!(inspection.treat_as_migration());
    }

    #[test]
    fn a_declared_length_that_would_overflow_fails_safe() {
        // u32::MAX declared on a 5-byte body. `GRPC_PREFIX_LEN + declared`
        // wraps on a 32-bit target, which panicked in a debug build (a denial
        // of service in the proxy) instead of landing here.
        let frame = [0u8, 0xff, 0xff, 0xff, 0xff];
        let (inspection, _) = inspect(&HeaderMap::new(), &frame);
        assert!(matches!(inspection, Inspection::Failsafe { .. }));
        assert!(inspection.treat_as_migration());
    }

    #[tokio::test]
    async fn an_oversized_body_and_a_broken_body_are_reported_differently() {
        use http_body_util::Full;

        // The genuine over-limit case, produced by `Limited` itself rather
        // than hand-built, because `LengthLimitError` cannot be constructed
        // outside its own crate.
        let too_long = Limited::new(Full::new(Bytes::from_static(b"too long")), 1)
            .collect()
            .await
            .expect_err("the body is over the limit");
        assert_eq!(
            body_read_failed(too_long)
                .headers()
                .get("grpc-status")
                .unwrap(),
            "8",
            "an over-limit body is RESOURCE_EXHAUSTED"
        );

        // A client that broke its own upload. Reporting this as "body too
        // large" sends the operator hunting for a 200-byte transaction that
        // was never too large.
        let broken: BoxError = Box::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "client went away",
        ));
        assert_eq!(
            body_read_failed(broken)
                .headers()
                .get("grpc-status")
                .unwrap(),
            "1",
            "a client body error is CANCELLED, not RESOURCE_EXHAUSTED"
        );
    }

    #[test]
    fn undecodable_protobuf_fails_safe() {
        // Field 1, varint wire type, then nothing: a truncated protobuf.
        let message = [0x08u8];
        let mut frame = vec![0, 0, 0, 0, message.len() as u8];
        frame.extend_from_slice(&message);

        let (inspection, _) = inspect(&HeaderMap::new(), &frame);
        assert!(matches!(inspection, Inspection::Failsafe { .. }));
        assert!(inspection.treat_as_migration());
    }

    #[test]
    fn an_empty_transaction_is_unparseable_not_a_pass_through() {
        let (inspection, _) = inspect(&HeaderMap::new(), &framed(&[]));
        assert_eq!(classified(&inspection), Class::Unparseable);
        assert!(inspection.treat_as_migration());
    }

    #[tokio::test]
    async fn replay_body_emits_data_then_trailers() {
        let mut trailers = HeaderMap::new();
        trailers.insert("x-test", "1".parse().unwrap());

        let body = ReplayBody::new(Bytes::from_static(b"abc"), Some(trailers));
        let collected = body.collect().await.unwrap();
        assert_eq!(collected.trailers().unwrap().get("x-test").unwrap(), "1");
        assert_eq!(collected.to_bytes().as_ref(), b"abc");
    }

    #[tokio::test]
    async fn replay_body_without_trailers_is_just_the_bytes() {
        let body = ReplayBody::new(Bytes::from_static(b"abc"), None);
        let collected = body.collect().await.unwrap();
        assert!(collected.trailers().is_none());
        assert_eq!(collected.to_bytes().as_ref(), b"abc");
    }
}
