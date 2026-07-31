//! The one intercepted method: `SendTransaction`.
//!
//! The request is unary and small, so the body is buffered, the 5-byte gRPC
//! length prefix is stripped, the `RawTransaction` message is decoded, and its
//! `data` field (the serialized Zcash transaction) is handed to
//! [`crate::classify`]. The verdict is logged with the evidence it rests on.
//!
//! **This proof of concept is non-destructive.** After logging, the ORIGINAL
//! bytes are forwarded to the backing indexer through the same
//! [`crate::proxy::forward`] the pass-through path uses, and the backing
//! indexer's real response is relayed back. Nothing is diverted. Diversion,
//! the hub, and Nym are out of scope here.
//!
//! Fail-safe for privacy applies at every layer above the classifier too. The
//! classifier never sees a malformed gRPC frame, so these cases are decided
//! here, and all of them mean "in production this would be diverted, not handed
//! to the operator's indexer":
//!
//! * a body shorter than the 5-byte prefix,
//! * the gRPC compression flag set, or a `grpc-encoding` other than `identity`,
//! * a declared message length that overruns or underruns the body,
//! * a `RawTransaction` that does not decode,
//! * a body over [`MAX_SEND_TX_BYTES`], or a client body stream that errored.
//!
//! The last one is the only case the proof of concept refuses to forward: once
//! [`Limited`] has errored mid-collect the original bytes are gone, so they
//! cannot be replayed byte-for-byte, and forwarding a body we could neither
//! read nor reproduce is the exact leak this component exists to prevent.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{HeaderMap, Request, Response};
use http_body::{Body, Frame};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::body::Incoming;
use prost::Message;
use zaino_proto::proto::service::RawTransaction;

use crate::classify::{classify_with_evidence, Class, Evidence};
use crate::proxy::{
    forward, grpc_error, ProxyBody, Upstream, GRPC_CANCELLED, GRPC_RESOURCE_EXHAUSTED,
};
use crate::BoxError;

/// gRPC length-prefixed message header: 1 flag byte + 4 big-endian length bytes.
const GRPC_PREFIX_LEN: usize = 5;

/// Cap on a buffered `SendTransaction` body. Well above the 2 MB Zcash
/// transaction limit, so a legitimate wallet never reaches it, while a hostile
/// client cannot make the shim buffer unbounded memory.
const MAX_SEND_TX_BYTES: usize = 4 * 1024 * 1024;

/// How many leading bytes of the transaction to log on a fail-safe. The first
/// eight carry the version and version group id, which is what distinguishes a
/// truncated frame from a genuinely new transaction format. A V6 always begins
/// `06 00 00 80 98 b6 84 d8`.
const PREFIX_LOG_BYTES: usize = 8;

/// Handle a request routed to the `SendTransaction` method.
///
/// The HTTP method is not checked here or by the caller, on purpose: see rule 3
/// in [`crate::proxy`]. A backend that acts on a `GET` must not be handed one
/// the classifier never saw.
pub async fn send_transaction(
    req: Request<Incoming>,
    upstream: Upstream,
) -> Result<Response<ProxyBody>, BoxError> {
    let (parts, body) = req.into_parts();

    // The only buffering in the entire shim, and it is bounded.
    let collected = match Limited::new(body, MAX_SEND_TX_BYTES).collect().await {
        Ok(collected) => collected,
        Err(err) => return Ok(body_read_failed(err)),
    };

    let trailers = collected.trailers().cloned();
    let frame = collected.to_bytes();

    let inspection = inspect(&parts.headers, &frame);
    log_verdict(&inspection, &frame);

    // Non-destructive: replay the ORIGINAL bytes, so the backing indexer sees
    // exactly what the wallet sent, down to the request trailers.
    let replay = ReplayBody::new(frame, trailers).boxed();
    let resp = forward(upstream, Request::from_parts(parts, replay)).await?;
    Ok(resp.map(|body| body.map_err(BoxError::from).boxed()))
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
fn inspect(headers: &HeaderMap, frame: &[u8]) -> Inspection {
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
            return Inspection::failsafe_with(
                "grpc-encoding is not identity",
                String::from_utf8_lossy(encoding.as_bytes()).into_owned(),
            );
        }
    }

    if frame.len() < GRPC_PREFIX_LEN {
        return Inspection::failsafe("gRPC frame shorter than its 5-byte prefix");
    }
    if frame[0] != 0 {
        return Inspection::failsafe("gRPC compression flag set");
    }

    let declared = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
    // `checked_add`, because `declared` is attacker-controlled and 32-bit
    // targets exist: `GRPC_PREFIX_LEN + declared` can wrap, which in a debug
    // build panics inside the proxy instead of landing in this fail-safe.
    let Some(message) = GRPC_PREFIX_LEN
        .checked_add(declared)
        .and_then(|end| frame.get(GRPC_PREFIX_LEN..end))
    else {
        return Inspection::failsafe_with(
            "gRPC message truncated",
            format!(
                "declared {declared} bytes, body carries {}",
                frame.len() - GRPC_PREFIX_LEN
            ),
        );
    };
    // A unary request carries exactly one message and nothing after it.
    if message.len() != frame.len() - GRPC_PREFIX_LEN {
        return Inspection::failsafe_with(
            "trailing bytes after the unary gRPC message",
            format!(
                "declared {declared} bytes, body carries {}",
                frame.len() - GRPC_PREFIX_LEN
            ),
        );
    }

    match RawTransaction::decode(message) {
        // `data` is the serialized Zcash transaction. This is the only value
        // the classifier ever sees.
        Ok(raw) => Inspection::Classified(classify_with_evidence(&raw.data)),
        Err(err) => Inspection::failsafe_with("RawTransaction decode failed", err.to_string()),
    }
}

/// The proof of concept's visible output.
fn log_verdict(inspection: &Inspection, frame: &[u8]) {
    let diverted_in_production = inspection.treat_as_migration();

    match inspection {
        Inspection::Classified(evidence) => match evidence.class {
            Class::Migration => tracing::info!(
                target: "zis::classify",
                version = %evidence.version,
                orchard_vb = %format!("{:+}", evidence.orchard_vb),
                ironwood_vb = %format!("{:+}", evidence.ironwood_vb),
                sapling_vb = %format!("{:+}", evidence.sapling_vb),
                expiry = ?evidence.expiry_height,
                inputs = evidence.inputs,
                outputs = evidence.outputs,
                tx_len = evidence.len,
                diverted_in_production,
                "MIGRATION detected: value leaving Orchard and entering Ironwood \
                 (this PoC still forwards it; production diverts it to the hub)"
            ),
            Class::PassThrough => tracing::info!(
                target: "zis::classify",
                version = %evidence.version,
                orchard_vb = %format!("{:+}", evidence.orchard_vb),
                ironwood_vb = %format!("{:+}", evidence.ironwood_vb),
                sapling_vb = %format!("{:+}", evidence.sapling_vb),
                expiry = ?evidence.expiry_height,
                inputs = evidence.inputs,
                outputs = evidence.outputs,
                tx_len = evidence.len,
                diverted_in_production,
                "passthrough: SendTransaction non-migration"
            ),
            Class::Unparseable => tracing::warn!(
                target: "zis::classify",
                error = evidence.error.as_deref().unwrap_or("(none)"),
                tx_len = evidence.len,
                frame_len = frame.len(),
                body_prefix = %hex_prefix(frame, GRPC_PREFIX_LEN + PREFIX_LOG_BYTES),
                diverted_in_production,
                "MIGRATION-FAILSAFE: unparseable SendTransaction body, treating as migration"
            ),
        },
        Inspection::Failsafe { reason, detail } => tracing::warn!(
            target: "zis::classify",
            reason,
            detail = detail.as_deref().unwrap_or("(none)"),
            frame_len = frame.len(),
            body_prefix = %hex_prefix(frame, GRPC_PREFIX_LEN + PREFIX_LOG_BYTES),
            diverted_in_production,
            "MIGRATION-FAILSAFE: SendTransaction body could not be classified, \
             treating as migration"
        ),
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

    /// A real V6 Orchard(+250_000) -> Ironwood(-240_000) transaction. Same
    /// fixture the classifier's own vector tests use.
    const V6_MIGRATION: &[u8] = include_bytes!("../tests/fixtures/v6_migration.bin");

    /// The same shape reversed: Orchard(-250_000) -> Ironwood(+240_000), which
    /// is not a migration.
    const V6_REVERSE: &[u8] = include_bytes!("../tests/fixtures/v6_reverse.bin");

    /// Wrap transaction bytes in a `RawTransaction` inside a gRPC length prefix,
    /// the way a wallet's gRPC client does.
    fn framed(tx: &[u8]) -> Vec<u8> {
        let message = RawTransaction {
            data: tx.to_vec(),
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
        let inspection = inspect(&HeaderMap::new(), &framed(V6_MIGRATION));
        assert_eq!(classified(&inspection), Class::Migration);
        assert!(inspection.treat_as_migration());
    }

    #[test]
    fn a_framed_non_migration_is_a_pass_through() {
        // The same transaction shape in reverse: value entering Orchard and
        // leaving Ironwood, which is not a migration.
        let inspection = inspect(&HeaderMap::new(), &framed(V6_REVERSE));
        assert_eq!(classified(&inspection), Class::PassThrough);
        assert!(!inspection.treat_as_migration());
    }

    #[test]
    fn compression_flag_fails_safe() {
        let mut frame = framed(V6_MIGRATION);
        frame[0] = 1;
        let inspection = inspect(&HeaderMap::new(), &frame);
        assert!(matches!(inspection, Inspection::Failsafe { .. }));
        assert!(inspection.treat_as_migration());
    }

    #[test]
    fn grpc_encoding_header_fails_safe() {
        let mut headers = HeaderMap::new();
        headers.insert("grpc-encoding", "gzip".parse().unwrap());
        let inspection = inspect(&headers, &framed(V6_MIGRATION));
        assert!(matches!(inspection, Inspection::Failsafe { .. }));
        assert!(inspection.treat_as_migration());
    }

    #[test]
    fn identity_encoding_is_not_treated_as_compression() {
        let mut headers = HeaderMap::new();
        headers.insert("grpc-encoding", "identity".parse().unwrap());
        assert_eq!(
            classified(&inspect(&headers, &framed(V6_MIGRATION))),
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
            let inspection = inspect(&HeaderMap::new(), body);
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
        let inspection = inspect(&HeaderMap::new(), &trailing);
        assert!(matches!(inspection, Inspection::Failsafe { .. }));
        assert!(inspection.treat_as_migration());
    }

    #[test]
    fn a_declared_length_that_would_overflow_fails_safe() {
        // u32::MAX declared on a 5-byte body. `GRPC_PREFIX_LEN + declared`
        // wraps on a 32-bit target, which panicked in a debug build (a denial
        // of service in the proxy) instead of landing here.
        let frame = [0u8, 0xff, 0xff, 0xff, 0xff];
        let inspection = inspect(&HeaderMap::new(), &frame);
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

        let inspection = inspect(&HeaderMap::new(), &frame);
        assert!(matches!(inspection, Inspection::Failsafe { .. }));
        assert!(inspection.treat_as_migration());
    }

    #[test]
    fn an_empty_transaction_is_unparseable_not_a_pass_through() {
        let inspection = inspect(&HeaderMap::new(), &framed(&[]));
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
