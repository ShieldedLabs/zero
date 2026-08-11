//! The hub's inbound path over the Nym mixnet: receive a `SubmitV1`, admit it,
//! reply with an `AckV1`.
//!
//! The design keeps the Nym SDK out of everything here. A driver task (which
//! lands with the SDK) owns the mixnet client and does nothing but move bytes:
//! it hands each inbound message to this module as a [`Received`] and sends each
//! [`Reply`] this module produces back out. So the listen loop is a plain async
//! function over two channels plus the admission core, and its whole behaviour is
//! exercised by feeding the channels directly, with no SDK and no fake client.
//!
//! What crosses the channels is BYTES and an opaque [`SenderTag`], never a domain
//! object: the tag is carried from a submission to its reply and never
//! interpreted, logged, or stored, because it is a per-session pseudonym for the
//! submitting shim and the whole point of the hop is to hold none of those.
//!
//! Admission is [`crate::server::Hub::admit`], the exact call the HTTP serving
//! path uses, so the two ingress paths cannot drift. Everything REVIEW.md binds
//! on that call binds here too: an unparseable transaction is queued and
//! published, never refused (#5); only counts and reasons are logged, never a
//! txid or a body (#157).

use tokio::sync::mpsc;

use crate::server::Hub;
use crate::wire::{self, AckKind, AckRefusal};

/// The mixnet's anonymous sender tag, carried but never interpreted. Sized to the
/// SDK's tag (16 bytes); the driver converts the SDK value to and from this so
/// nothing here depends on the SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderTag(pub [u8; 16]);

/// One inbound submission: the frame bytes and the tag to reply to.
pub struct Received {
    pub frame: Vec<u8>,
    pub sender_tag: SenderTag,
}

/// One outbound reply: the ack frame and the tag it goes back to.
pub struct Reply {
    pub sender_tag: SenderTag,
    pub ack: Vec<u8>,
}

/// Serve submissions until the inbound channel closes.
///
/// Each message is handled on its own task so a slow reply cannot head-of-line
/// block the next admission; the queue is internally locked, and [`Hub`] is cheap
/// to clone (it is `Arc`s and `Copy` params).
pub async fn run_listener(
    mut incoming: mpsc::Receiver<Received>,
    outgoing: mpsc::Sender<Reply>,
    hub: Hub,
) {
    while let Some(received) = incoming.recv().await {
        // Empty inbound messages are the SDK's SURB-replenishment artifacts, not
        // submissions. Drop them before they reach the codec (they would decode
        // as bad_frame and add noise for nothing).
        if received.frame.is_empty() {
            continue;
        }
        let hub = hub.clone();
        let outgoing = outgoing.clone();
        tokio::spawn(async move {
            if let Some(ack) = build_ack(&hub, &received.frame) {
                let _ = outgoing
                    .send(Reply {
                        sender_tag: received.sender_tag,
                        ack,
                    })
                    .await;
            }
        });
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
