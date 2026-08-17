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
//!
//! Every [`Reply`] carries the instant its request was pulled off the inbound
//! channel, and [`REPLY_DEADLINE`] says how long after that it is still worth
//! sending. The driver drains replies one at a time at the mixnet's throttled
//! emission rate, so under a burst the queue behind it can hold answers older
//! than the shim's whole request budget; those are dropped rather than emitted.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::server::{Hub, LookupOutcome};
use crate::wire::{self, AckKind, AckRefusal, LookupReply};

/// How many lookups may be in flight at once: dialling the operator's indexer,
/// framing the reply, or waiting to hand it to the driver.
///
/// Bounds the lookup arm and ONLY the lookup arm. Admission never waits on
/// this: it does no I/O, so a ceiling on it would buy nothing and cost the one
/// property this listener must hold, that a migration is admitted while
/// lookups are stuck. Generous enough that honest polling never queues behind
/// it, small enough that a flood cannot open unbounded connections or park an
/// unbounded pile of 64 KiB reply frames behind a slow driver.
///
/// A slot is held from before the lookup's task is spawned until its reply has
/// been ACCEPTED by the driver's channel, not merely through the indexer dial.
/// Releasing it after the dial, as this used to, let every lookup past the
/// dial become an unbounded spawned task holding a full reply frame and waiting
/// on the driver, which is exactly the queue that then emitted dead answers
/// for minutes while fresh lookups waited behind them.
const MAX_CONCURRENT_LOOKUPS: usize = 64;

/// How long after a request is received its reply is still worth emitting.
///
/// The shim gives up on a lookup after its `REQUEST_TIMEOUT` (90 s by default,
/// `ZIS_LOOKUP_TIMEOUT_SECS` per deployment), measured from the moment IT started
/// sending. This deadline is measured from the moment the hub pulled the request
/// off its inbound channel, which on a backpressured gateway is already ~10 s
/// into the shim's clock (60 reply-SURB packets out at ~8 packets/s, plus the mix
/// delay), and the reply itself then costs ~5 s of emission plus the mix delay
/// on the way back. So a reply started at hub-age 60 s lands at roughly the
/// shim's 75 s mark, inside its budget with margin for a slower gateway; one
/// started much later than that is a full 41-packet emission spent on an answer
/// no one is waiting for, which starves the next live lookup of exactly that
/// emission time. Under a burst that is the whole failure: a FIFO of
/// dead-on-arrival replies eating the send budget while fresh lookups age
/// behind them.
///
/// MUST stay well under the shim's `REQUEST_TIMEOUT`, by at least the round-trip
/// emission arithmetic above; if a deployment lowers the shim's budget, lower
/// this with it or the hub goes back to paying for answers that arrive late.
pub const REPLY_DEADLINE: Duration = Duration::from_secs(60);

/// The mixnet's anonymous sender tag, carried but never interpreted. Sized to the
/// SDK's tag (16 bytes); the driver converts the SDK value to and from this so
/// nothing here depends on the SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderTag(pub [u8; 16]);

/// One inbound request: the frame bytes and the tag to reply to.
///
/// [`Zeroizing`], like everything outbound. Inbound is in fact where the
/// cleartext matters MOST on this side: a `SubmitV1` arriving here carries a
/// diverted migration that no node has seen yet, and the frame outlives the
/// copy `decode_submit` takes. Freeing it un-wiped is the one thing an
/// attestation cannot excuse in a diskless enclave.
pub struct Received {
    pub frame: Zeroizing<Vec<u8>>,
    pub sender_tag: SenderTag,
}

/// One outbound reply: the reply frame (an `AckV1` or a `LookupReplyV1`) and
/// the tag it goes back to. [`Zeroizing`] because a found lookup reply carries
/// transaction bytes, possibly of a diverted, not-yet-published migration.
pub struct Reply {
    pub sender_tag: SenderTag,
    pub frame: Zeroizing<Vec<u8>>,
    /// When the request this answers was pulled off the inbound channel. The
    /// driver reads it through [`Reply::is_dead`] before spending an emission
    /// on the frame.
    pub received_at: Instant,
}

impl Reply {
    /// How long ago the request this answers was received.
    pub fn age(&self) -> Duration {
        self.received_at.elapsed()
    }

    /// Whether the shim that asked has, by [`REPLY_DEADLINE`], already given up:
    /// a reply that is dead is not worth the 41 packets it costs to emit.
    pub fn is_dead(&self) -> bool {
        self.age() >= REPLY_DEADLINE
    }
}

/// Serve requests until the inbound channel closes.
///
/// Admission is handled INLINE: [`build_ack`] is pure queue work with no I/O in
/// it at all (it is not even `async`), so the loop admits a migration the
/// moment it pulls the frame, and nothing a lookup does can delay that. The ack
/// is then offered to the driver without waiting: if the driver's channel is
/// full, the ack is dropped, because the migration is ALREADY admitted, the
/// shim never awaits an ack (its submit path is dispatch-only), and an ack
/// queued behind a full channel of 64 KiB lookup replies would emit minutes
/// late to no one.
///
/// A lookup is the arm that can afford to wait and the only one that must be
/// bounded: it may dial the operator's indexer with a 10 s budget on each of
/// connect, request and body, and its reply is a full 64 KiB frame that the
/// driver emits at the mixnet's throttled rate. Each runs on its own task so a
/// slow indexer cannot head-of-line block the next admission, and each takes a
/// slot from [`MAX_CONCURRENT_LOOKUPS`] BEFORE it is spawned and holds it until
/// the driver has accepted the reply. When every slot is held the lookup is
/// dropped, not parked: parking it in a task is the unbounded pile this bound
/// exists to prevent, and blocking the loop on a slot would starve admission
/// behind it. A dropped lookup costs the shim its timeout and fails closed
/// there, which is the same disposition it would have got from a reply that
/// aged out in the queue. Under a backpressured gateway a 65th reply behind
/// 64 in flight would emit past [`REPLY_DEADLINE`] anyway, so nothing that
/// could have been answered in time is lost.
///
/// The queue is internally locked, and [`Hub`] is cheap to clone (it is `Arc`s
/// and `Copy` params).
pub async fn run_listener(
    mut incoming: mpsc::Receiver<Received>,
    outgoing: mpsc::Sender<Reply>,
    hub: Hub,
) {
    let lookups = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_LOOKUPS));

    while let Some(received) = incoming.recv().await {
        // Empty inbound messages are the SDK's SURB-replenishment artifacts, not
        // requests. Drop them before they reach the codec (they would decode as
        // bad_frame and add noise for nothing).
        if received.frame.is_empty() {
            continue;
        }
        // Stamped when the frame is pulled, not when the reply is built: the
        // deadline is about how long the SHIM has been waiting, and everything
        // after this point (the dial, the framing, the driver's queue) is time
        // the shim has already spent.
        let received_at = Instant::now();

        if is_lookup(&received.frame) {
            // The slot is taken here, before the spawn, and released by the task
            // only once the reply is in the driver's channel; see the constant.
            let permit = match lookups.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    // Count only, no hash, no tag (#157).
                    tracing::warn!(
                        in_flight = MAX_CONCURRENT_LOOKUPS,
                        "lookup dropped: every lookup slot is held; the shim fails closed on its timeout"
                    );
                    continue;
                }
            };
            let hub = hub.clone();
            let outgoing = outgoing.clone();
            tokio::spawn(async move {
                if let Some(frame) = build_lookup_reply(&hub, &received.frame, received_at).await {
                    let _ = outgoing
                        .send(Reply {
                            sender_tag: received.sender_tag,
                            frame,
                            received_at,
                        })
                        .await;
                }
                // Explicitly after the send: the slot covers the hand-off, and a
                // driver slow to drain is exactly the pressure the bound must feel.
                drop(permit);
            });
        } else if let Some(frame) = build_ack(&hub, &received.frame) {
            let reply = Reply {
                sender_tag: received.sender_tag,
                frame: Zeroizing::new(frame),
                received_at,
            };
            match outgoing.try_send(reply) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(
                        "ack dropped: the driver's reply queue is full; the migration is admitted regardless"
                    );
                }
                // The driver is gone; there is no one to carry the ack.
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }
}

/// Whether one inbound frame is a lookup, deciding which arm it takes.
///
/// On SIZE first, then magic. A lookup is a fixed [`wire::LOOKUP_BYTES`] frame,
/// so anything else is not one however it starts, and only a frame of exactly
/// that size can reach the lookup arm's full-frame `error` reply. Dispatching
/// on the magic alone was an amplifier: `peek_lookup_nonce` needs just 21 bytes
/// and the lookup magic, so a 21-byte message got a 65 536-byte answer, 41
/// sphinx packets of the hub's own metered egress, from anyone at all, since
/// the hub's Nym address is public by design and has no operator ACL in front
/// of it.
///
/// A frame that is lookup-shaped but the wrong size falls through to the submit
/// arm, finds no submit magic, and is dropped with no reply. That mirrors the
/// shim's own `deliver`, which has always dispatched on length first.
fn is_lookup(frame: &[u8]) -> bool {
    frame.len() == wire::LOOKUP_BYTES && wire::peek_lookup_nonce(frame).is_some()
}

/// Decode one lookup frame, answer it through the shared core, and build the
/// reply. Always correlatable: this arm is only entered when the nonce is
/// recoverable, and every failure inside it (a malformed frame, an empty key,
/// an unanswerable indexer, a transaction too large for the reply frame) is an
/// `error` disposition, which fails CLOSED at the shim.
///
/// The one thing that yields NO frame is a lookup that has already outlived
/// [`REPLY_DEADLINE`] by the time the indexer answers: framing 64 KiB that the
/// driver would only drop is work for nothing, so the drop happens here,
/// before the encode.
///
/// The caller holds this lookup's concurrency slot for the whole call and past
/// it, so nothing here waits on the bound.
async fn build_lookup_reply(
    hub: &Hub,
    frame: &[u8],
    received_at: Instant,
) -> Option<Zeroizing<Vec<u8>>> {
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
            return Some(error_reply(nonce));
        }
    };
    // An empty key is a malformed query, not a miss: answering not_found would
    // dress a shim bug up as a real verdict.
    if hash.is_empty() {
        tracing::warn!(reason = "empty lookup key", "lookup refused");
        return Some(error_reply(nonce));
    }
    // `Hub::lookup` is the only work in this module that leaves the process,
    // dialling the operator's indexer fresh on a queue miss, with three 10 s
    // budgets in it. It is the reason the arm is bounded at all.
    let reply = match hub.lookup(&hash).await {
        LookupOutcome::Found { data, height } => LookupReply::Found { height, tx: data },
        LookupOutcome::NotFound => LookupReply::NotFound,
        LookupOutcome::Unavailable => LookupReply::Error,
    };
    // A slow indexer can by itself use up the shim's whole budget. Checked after
    // the dial and before the encode: past the deadline the driver would drop
    // the frame anyway, and not building it is cheaper than building it to be
    // dropped.
    let age = received_at.elapsed();
    if age >= REPLY_DEADLINE {
        tracing::warn!(
            age_secs = age.as_secs(),
            "lookup answered after the shim's budget; reply not framed"
        );
        return None;
    }
    match wire::encode_lookup_reply(&nonce, &reply) {
        Ok(frame) => Some(frame),
        // The reply budget is nine bytes under the submit cap, so an indexer
        // can return a transaction that fits nowhere in a reply frame. Fail
        // closed rather than truncate.
        Err(err) => {
            tracing::warn!(reason = %err, "lookup reply could not be framed");
            Some(error_reply(nonce))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn reply_received(received_at: Instant) -> Reply {
        Reply {
            sender_tag: SenderTag([0; 16]),
            frame: Zeroizing::new(Vec::new()),
            received_at,
        }
    }

    #[test]
    fn a_fresh_reply_is_live_and_one_past_the_deadline_is_dead() {
        assert!(!reply_received(Instant::now()).is_dead());
        // `checked_sub`: an `Instant` cannot precede the clock's origin, and on
        // a machine up for less than the deadline this would otherwise panic.
        if let Some(stale) = Instant::now().checked_sub(REPLY_DEADLINE) {
            assert!(reply_received(stale).is_dead());
        }
    }

    #[test]
    fn the_deadline_leaves_the_shim_room_to_receive_the_reply() {
        // The shim's REQUEST_TIMEOUT is 90 s. The hub sees a request ~10 s into
        // that clock and the reply costs ~7 s to land, so anything at or over
        // ~73 s of hub-age is dead on arrival; the constant must sit under that
        // with margin, and the arithmetic in its doc comment is what this pins.
        assert!(REPLY_DEADLINE <= Duration::from_secs(70));
        // But not so tight that a healthy lookup on a slow indexer (three 10 s
        // budgets) is thrown away.
        assert!(REPLY_DEADLINE >= Duration::from_secs(30));
    }
}
