//! The hub's inbound path over the Nym mixnet: receive a `SubmitV1`, admit it,
//! reply with an `AckV1`; receive a `LookupV1`, answer it with a
//! `LookupReplyV1`.
//!
//! The design keeps the Nym SDK out of everything here. A driver task (which
//! lands with the SDK) owns the mixnet client and does nothing but move bytes:
//! it hands each inbound message to this module as a [`Received`] and sends each
//! [`Reply`] this module produces back out. So the listen loop is a plain async
//! function over two channels plus the shared cores, and its whole behaviour is
//! exercised by feeding the channels directly, with no SDK and no fake client.
//!
//! What crosses the channels is BYTES and an opaque [`SenderTag`], never a domain
//! object: the tag is carried from a request to its reply and never
//! interpreted, logged, or stored, because it is a per-session pseudonym for the
//! requesting shim and the whole point of the hop is to hold none of those.
//!
//! Admission is [`crate::server::Hub::admit`] and lookup is
//! [`crate::server::Hub::lookup`], the exact calls the HTTP serving path uses,
//! so the two ingress paths cannot drift. Everything REVIEW.md binds on those
//! calls binds here too: an unparseable transaction is queued and published,
//! never refused (#5); only counts, reasons, and dispositions are logged, never
//! a txid, a queried hash, or a body (#157).

use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::server::{Hub, LookupOutcome};
use crate::wire::{self, AckKind, AckRefusal, LookupReply};

/// How many inbound requests may be served concurrently.
///
/// Sized for the slow arm: a lookup that misses the queue dials the indexer,
/// so this bounds how many of those can be in flight, and with them the hub's
/// memory and its outbound connections. Generous enough that honest shims
/// never queue behind it, small enough that a flood cannot spawn without limit.
const MAX_CONCURRENT_REQUESTS: usize = 64;

/// The mixnet's anonymous sender tag, carried but never interpreted. Sized to the
/// SDK's tag (16 bytes); the driver converts the SDK value to and from this so
/// nothing here depends on the SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderTag(pub [u8; 16]);

/// One inbound request: the frame bytes and the tag to reply to.
pub struct Received {
    pub frame: Vec<u8>,
    pub sender_tag: SenderTag,
}

/// One outbound reply: the reply frame (an `AckV1` or a `LookupReplyV1`) and
/// the tag it goes back to. [`Zeroizing`] because a found lookup reply carries
/// transaction bytes, possibly of a diverted, not-yet-published migration.
pub struct Reply {
    pub sender_tag: SenderTag,
    pub frame: Zeroizing<Vec<u8>>,
}

/// Serve requests until the inbound channel closes.
///
/// Each message is handled on its own task so a slow reply (a lookup awaiting
/// the indexer, most of all) cannot head-of-line block the next admission; the
/// queue is internally locked, and [`Hub`] is cheap to clone (it is `Arc`s and
/// `Copy` params).
pub async fn run_listener(
    mut incoming: mpsc::Receiver<Received>,
    outgoing: mpsc::Sender<Reply>,
    hub: Hub,
) {
    // A ceiling on requests being served at once.
    //
    // Spawning per message keeps a slow reply from head-of-line blocking the
    // next admission, but an unbounded spawn also throws away the inbound
    // channel's backpressure, and since the lookup arm landed a request can now
    // hold a fresh dial to the operator's indexer with a 10 s budget on each of
    // connect, request and body. Acquiring before spawning puts the
    // backpressure back where it belongs: on the channel, and so on the driver,
    // rather than on the hub's memory.
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS));

    while let Some(received) = incoming.recv().await {
        // Empty inbound messages are the SDK's SURB-replenishment artifacts, not
        // requests. Drop them before they reach the codec (they would decode as
        // bad_frame and add noise for nothing).
        if received.frame.is_empty() {
            continue;
        }
        let Ok(permit) = permits.clone().acquire_owned().await else {
            // The semaphore is never closed; this cannot happen short of a bug.
            return;
        };
        let hub = hub.clone();
        let outgoing = outgoing.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Some(frame) = build_reply(&hub, &received.frame).await {
                let _ = outgoing
                    .send(Reply {
                        sender_tag: received.sender_tag,
                        frame,
                    })
                    .await;
            }
        });
    }
}

/// Dispatch one inbound frame to the submit or the lookup arm.
///
/// On SIZE first, then magic. A lookup is a fixed [`wire::LOOKUP_BYTES`] frame,
/// so anything else is not one however it starts, and only a frame of exactly
/// that size can reach the lookup arm's full-frame `error` reply. Dispatching
/// on the magic alone was an amplifier: `peek_lookup_nonce` needs just 21 bytes
/// and the lookup magic, so a 21-byte message got a 65 536-byte answer — 41
/// sphinx packets of the hub's own metered egress, from anyone at all, since
/// the hub's Nym address is public by design and has no operator ACL in front
/// of it.
///
/// A frame that is lookup-shaped but the wrong size now falls through to the
/// submit arm, finds no submit magic, and is dropped with no reply. That
/// mirrors the shim's own `deliver`, which has always dispatched on length
/// first.
async fn build_reply(hub: &Hub, frame: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    if frame.len() == wire::LOOKUP_BYTES && wire::peek_lookup_nonce(frame).is_some() {
        Some(build_lookup_reply(hub, frame).await)
    } else {
        build_ack(hub, frame).map(Zeroizing::new)
    }
}

/// Decode one lookup frame, answer it through the shared core, and build the
/// reply. Always correlatable: this arm is only entered when the nonce is
/// recoverable, and every failure inside it (a malformed frame, an empty key,
/// an unanswerable indexer, a transaction too large for the reply frame) is an
/// `error` disposition, which fails CLOSED at the shim.
async fn build_lookup_reply(hub: &Hub, frame: &[u8]) -> Zeroizing<Vec<u8>> {
    let error_reply = |nonce| {
        wire::encode_lookup_reply(&nonce, &LookupReply::Error)
            .expect("an error reply carries no transaction and always fits")
    };
    let (nonce, hash) = match wire::decode_lookup(frame) {
        Ok(decoded) => decoded,
        Err(err) => {
            // Reason only: no nonce, no hash, no body (#157).
            tracing::warn!(reason = %err, "lookup frame could not be decoded");
            let nonce = wire::peek_lookup_nonce(frame).expect("dispatch checked the header");
            return error_reply(nonce);
        }
    };
    // An empty key is a malformed query, not a miss: answering not_found would
    // dress a shim bug up as a real verdict.
    if hash.is_empty() {
        tracing::warn!(reason = "empty lookup key", "lookup refused");
        return error_reply(nonce);
    }
    let reply = match hub.lookup(&hash).await {
        LookupOutcome::Found { data, height } => LookupReply::Found { height, tx: data },
        LookupOutcome::NotFound => LookupReply::NotFound,
        LookupOutcome::Unavailable => LookupReply::Error,
    };
    match wire::encode_lookup_reply(&nonce, &reply) {
        Ok(frame) => frame,
        // The reply budget is nine bytes under the submit cap, so an indexer
        // can return a transaction that fits nowhere in a reply frame. Fail
        // closed rather than truncate.
        Err(err) => {
            tracing::warn!(reason = %err, "lookup reply could not be framed");
            error_reply(nonce)
        }
    }
}

/// Decode one submission frame, admit it through the shared core, and build the
/// acknowledgement.
///
/// Returns `None` only when the frame is so malformed that no nonce can be
/// recovered to correlate a reply: there is nothing useful to send, the failure
/// is logged, and the shim falls back to its submit timeout. When the nonce IS
/// recoverable (the frame was ours but its `tx_len` was wrong) a correlatable
/// `bad_frame` ack goes back instead.
fn build_ack(hub: &Hub, frame: &[u8]) -> Option<Vec<u8>> {
    match wire::decode_submit(frame) {
        Ok((nonce, tx)) => {
            // A transaction that does not parse is NOT refused here: it is queued
            // and published like any other, because the shim diverted it for the
            // same reason it could not read it, and the node is the only authority
            // on validity (REVIEW #5). `admit` handles that; a refusal is only the
            // typed admission reasons.
            let kind = match hub.admit(&tx) {
                Ok(_txid) => AckKind::Accepted,
                Err(refusal) => AckKind::Refused(refusal.into()),
            };
            Some(wire::encode_ack(&nonce, kind).to_vec())
        }
        Err(err) => {
            // No nonce, no tag, no body: in an enclave the log reaches the parent
            // host, which is exactly who this system withholds those from.
            tracing::warn!(reason = %err, "submission frame could not be decoded");
            wire::peek_nonce(frame)
                .map(|nonce| wire::encode_ack(&nonce, AckKind::Refused(AckRefusal::BadFrame)).to_vec())
        }
    }
}
