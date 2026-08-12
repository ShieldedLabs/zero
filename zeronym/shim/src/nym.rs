//! The shim's outbound path over the Nym mixnet: send a `SubmitV1` and await
//! its `AckV1`; send a `LookupV1` and await its `LookupReplyV1`.
//!
//! The design keeps the Nym SDK out of everything here, mirroring the hub's
//! listener. A driver task (which lands with the SDK) owns the mixnet client
//! and does nothing but move bytes: it takes each [`OutFrame`] this module
//! produces and puts it on the mixnet, and hands every inbound mixnet message
//! back as raw bytes. So the transport is a plain async function over three
//! channels — requests in, frames out, mixnet messages in — and its whole
//! behaviour is exercised by holding the driver ends and feeding bytes, with no
//! SDK and no fake client.
//!
//! Correlation is the one job here (D5): every request carries a random nonce,
//! the hub echoes it in the reply, and [`run_transport`] owns the
//! nonce-to-waiter map as its private state — single owner, no lock. A reply
//! for an unknown nonce is dropped (a duplicate, or one that raced its caller's
//! timeout); a reply of the WRONG KIND for a known nonce is ignored and its
//! waiter left pending, so a confused or hostile hub cannot answer a lookup
//! with an ack; an empty inbound message is an SDK SURB-replenishment artifact
//! and is filtered before it reaches the codec (D12), exactly as the hub's
//! listener filters them.
//!
//! The per-request timeout lives at the call site in [`NymHandle`], around the
//! waiter: a dead mixnet, a lost reply, or a gone driver all end in a typed
//! error the intercept path maps onto its existing fail-closed arms
//! (UNAVAILABLE to the wallet, never the operator's indexer). A submit's
//! wallet-level retry resends identical bytes and the hub's queue dedups, so no
//! retry state is kept here.
//!
//! How many reply SURBs to attach is carried on each [`OutFrame`] as data, not
//! decided by the driver: the count is a fixed function of the frame type
//! (D3/D4), and putting it here keeps the driver a pure byte mover and keeps
//! the measured numbers next to the frames they were measured for.

use std::collections::HashMap;
use std::time::Duration;

use rand::RngCore;
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use crate::wire::{self, AckKind, LookupReply, Nonce, WireError};

/// Ceiling on one request, submit or lookup. A frame takes about a second to
/// emit at the client's Poisson rate (more under backpressure) plus a measured
/// ~10 s mixnet round trip; a lookup that misses the hub's queue additionally
/// waits on the hub's own 10 s indexer timeout. 25 s covers both with margin
/// and sits under typical wallet gRPC deadlines, so a slow-but-alive mixnet
/// succeeds and a dead one fails closed before the wallet gives up.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

/// Reply SURBs attached to a `SubmitV1` (D3). The ack is a single 64-byte
/// frame, so a small fixed count carries it with no re-request round trip;
/// measured in the nymnet harness, where 13 acked with no re-request at all.
/// Fixed, because the on-wire packet count is a function of frame size PLUS
/// attached-SURB count (D4).
pub const SUBMIT_REPLY_SURBS: u32 = 13;

/// Reply SURBs attached to a `LookupV1` (D3 as corrected). The reply is a FULL
/// frame, which the nymnet harness measured at exactly 41 reply packets, and
/// the SDK holds back `minimum_reply_surb_storage_threshold` (10) before it
/// will spend any: below 51 the hub must fire a blocking re-request round,
/// costing a full mixnet round trip per lookup (measured). 60 clears the
/// threshold with margin while staying a fixed, bounded count.
pub const LOOKUP_REPLY_SURBS: u32 = 60;

/// What a pending request is waiting for. The variants mirror the two reply
/// frames the hub can send, so a reply that decodes as the wrong kind for its
/// nonce can be recognised as no answer at all.
enum Waiter {
    Ack(oneshot::Sender<AckKind>),
    Lookup(oneshot::Sender<LookupReply>),
}

impl Waiter {
    /// Whether the caller has gone away (timed out, or its task was dropped),
    /// so this entry can be swept rather than held until a reply that may
    /// never come.
    fn is_abandoned(&self) -> bool {
        match self {
            Waiter::Ack(tx) => tx.is_closed(),
            Waiter::Lookup(tx) => tx.is_closed(),
        }
    }
}

/// One request awaiting its reply: the encoded frame, the nonce inside it, how
/// many reply SURBs the driver must attach, and the waiter to fire when the
/// matching reply arrives.
pub struct Request {
    nonce: Nonce,
    frame: Zeroizing<Vec<u8>>,
    reply_surbs: u32,
    waiter: Waiter,
}

/// One outbound frame for the driver to put on the mixnet, with the fixed
/// number of reply SURBs to attach to it (D3/D4). [`Zeroizing`] because a
/// submit frame holds the transaction bytes.
pub struct OutFrame {
    pub frame: Zeroizing<Vec<u8>>,
    pub reply_surbs: u32,
}

/// Why a request produced no verdict. Every variant fails closed at the caller.
#[derive(Debug, PartialEq, Eq)]
pub enum NymError {
    /// The frame could not be built; in practice [`WireError::TxTooLarge`], the
    /// size gate the wallet must hear about as its own error rather than a
    /// generic unavailability.
    Encode(WireError),
    /// No reply within [`NymHandle`]'s timeout. A submitted transaction may
    /// still be admitted; the wallet's retry is idempotent at the hub.
    Timeout,
    /// The driver or the transport loop is gone; nothing can be sent.
    TransportGone,
}

impl std::fmt::Display for NymError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NymError::Encode(err) => write!(f, "could not frame the request: {err}"),
            NymError::Timeout => f.write_str("no reply from the hub within the timeout"),
            NymError::TransportGone => f.write_str("the mixnet transport is not running"),
        }
    }
}

impl std::error::Error for NymError {}

/// The sender side of the mixnet transport, held by [`crate::hub::HubTransport`].
/// Cheap to clone; every clone submits through the same transport loop and the
/// same persistent client (D2).
#[derive(Clone)]
pub struct NymHandle {
    requests: mpsc::Sender<Request>,
    timeout: Duration,
}

impl NymHandle {
    pub fn new(requests: mpsc::Sender<Request>, timeout: Duration) -> Self {
        NymHandle { requests, timeout }
    }

    /// Frame `tx_bytes` under a fresh nonce, hand it to the transport, and wait
    /// for the correlated ack.
    pub async fn submit(&self, tx_bytes: &[u8]) -> Result<AckKind, NymError> {
        let nonce = fresh_nonce();
        let frame = wire::encode_submit(&nonce, tx_bytes).map_err(NymError::Encode)?;
        let (ack_tx, ack_rx) = oneshot::channel();
        self.dispatch(Request {
            nonce,
            frame,
            reply_surbs: SUBMIT_REPLY_SURBS,
            waiter: Waiter::Ack(ack_tx),
        })
        .await?;
        self.await_reply(ack_rx).await
    }

    /// Frame `wire_hash` as a lookup under a fresh nonce, hand it to the
    /// transport, and wait for the correlated reply. The hash is the wallet's
    /// `TxFilter.hash` in wire order, passed through unmodified exactly as the
    /// HTTP transport posts it.
    pub async fn get_transaction(&self, wire_hash: &[u8]) -> Result<LookupReply, NymError> {
        let nonce = fresh_nonce();
        // The frame is small and holds no transaction bytes, but the request
        // channel carries one type, so it travels in the same Zeroizing buffer.
        let frame = Zeroizing::new(
            wire::encode_lookup(&nonce, wire_hash)
                .map_err(NymError::Encode)?
                .to_vec(),
        );
        let (reply_tx, reply_rx) = oneshot::channel();
        self.dispatch(Request {
            nonce,
            frame,
            reply_surbs: LOOKUP_REPLY_SURBS,
            waiter: Waiter::Lookup(reply_tx),
        })
        .await?;
        self.await_reply(reply_rx).await
    }

    async fn dispatch(&self, request: Request) -> Result<(), NymError> {
        self.requests
            .send(request)
            .await
            .map_err(|_| NymError::TransportGone)
    }

    async fn await_reply<T>(&self, rx: oneshot::Receiver<T>) -> Result<T, NymError> {
        match tokio::time::timeout(self.timeout, rx).await {
            Err(_) => Err(NymError::Timeout),
            // The transport dropped the waiter without firing it: it is exiting.
            Ok(Err(_)) => Err(NymError::TransportGone),
            Ok(Ok(reply)) => Ok(reply),
        }
    }
}

fn fresh_nonce() -> Nonce {
    let mut nonce: Nonce = [0u8; wire::NONCE_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    nonce
}

/// Correlate requests with their replies until the driver goes away.
///
/// Runs until the inbound mixnet channel closes (the driver is gone; every
/// waiter still pending is dropped, which surfaces as [`NymError::TransportGone`]
/// at its caller), or until every handle is dropped and the last pending reply
/// is resolved. A frame is only considered in flight once the driver has
/// accepted it, so a request that cannot even be handed over drops its waiter
/// immediately rather than waiting out the timeout.
///
/// Which reply frame arrived is read from its LENGTH, the one thing every
/// transport layer already knows: an `AckV1` is [`wire::ACK_BYTES`] and a
/// `LookupReplyV1` is [`wire::FRAME_BYTES`]. The decoders still verify the
/// magic, so a frame of the right size and the wrong type is rejected there.
pub async fn run_transport(
    mut requests: mpsc::Receiver<Request>,
    to_mixnet: mpsc::Sender<OutFrame>,
    mut from_mixnet: mpsc::Receiver<Vec<u8>>,
) {
    let mut pending: HashMap<Nonce, Waiter> = HashMap::new();
    let mut requests_open = true;
    loop {
        tokio::select! {
            request = requests.recv(), if requests_open => match request {
                Some(Request { nonce, frame, reply_surbs, waiter }) => {
                    if to_mixnet
                        .send(OutFrame { frame, reply_surbs })
                        .await
                        .is_err()
                    {
                        // The driver is gone. Dropping `waiter` and every
                        // pending one unblocks all callers with TransportGone.
                        return;
                    }
                    pending.insert(nonce, waiter);
                }
                None => requests_open = false,
            },
            message = from_mixnet.recv() => match message {
                Some(bytes) => {
                    // Empty inbound messages are the SDK's SURB-replenishment
                    // artifacts, not replies (D12).
                    if bytes.is_empty() {
                        continue;
                    }
                    deliver(&mut pending, &bytes);
                }
                None => return,
            },
        }
        // Callers that timed out (or were cancelled) have dropped their
        // receivers; without this sweep their entries would accumulate for the
        // life of the process, since the reply that would remove them is
        // exactly the one that never came.
        pending.retain(|_, waiter| !waiter.is_abandoned());
        if !requests_open && pending.is_empty() {
            return;
        }
    }
}

/// Match one inbound reply frame to its waiter and fire it.
///
/// A reply for an unknown nonce is dropped (a duplicate, or one that raced its
/// caller's timeout). A reply of the wrong KIND for a known nonce is not an
/// answer: the waiter stays pending, so the caller fails closed on its timeout
/// instead of a hostile or confused hub answering a lookup with an ack.
fn deliver(pending: &mut HashMap<Nonce, Waiter>, bytes: &[u8]) {
    match bytes.len() {
        wire::ACK_BYTES => match wire::decode_ack(bytes) {
            Ok((nonce, kind)) => match pending.remove(&nonce) {
                Some(Waiter::Ack(waiter)) => {
                    let _ = waiter.send(kind);
                }
                Some(other) => {
                    pending.insert(nonce, other);
                    tracing::warn!("an ack arrived for a lookup's nonce; ignoring it");
                }
                None => {}
            },
            // No nonce, no body: the log reaches the parent host, which is
            // exactly who is withheld those.
            Err(err) => {
                tracing::warn!(reason = %err, "inbound message could not be decoded as an ack")
            }
        },
        wire::FRAME_BYTES => match wire::decode_lookup_reply(bytes) {
            Ok((nonce, reply)) => match pending.remove(&nonce) {
                Some(Waiter::Lookup(waiter)) => {
                    let _ = waiter.send(reply);
                }
                Some(other) => {
                    pending.insert(nonce, other);
                    tracing::warn!("a lookup reply arrived for a submit's nonce; ignoring it");
                }
                None => {}
            },
            Err(err) => tracing::warn!(
                reason = %err,
                "inbound message could not be decoded as a lookup reply"
            ),
        },
        other => tracing::warn!(bytes = other, "inbound message is not a reply frame size"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The abandoned-waiter sweep, tested directly because the map is private
    /// state inside [`run_transport`]: an integration test can only observe
    /// that correlation still works, not that the map actually shrank, and the
    /// whole point of the sweep is the entries nobody will ever ask about
    /// again.
    #[test]
    fn the_sweep_drops_abandoned_waiters_and_keeps_live_ones() {
        let mut pending: HashMap<Nonce, Waiter> = HashMap::new();

        // A caller that timed out: its receiver is gone.
        let (abandoned_tx, abandoned_rx) = oneshot::channel::<AckKind>();
        drop(abandoned_rx);
        pending.insert([1u8; 16], Waiter::Ack(abandoned_tx));

        // A caller still waiting, of each kind.
        let (live_ack_tx, _live_ack_rx) = oneshot::channel::<AckKind>();
        pending.insert([2u8; 16], Waiter::Ack(live_ack_tx));
        let (live_lookup_tx, _live_lookup_rx) = oneshot::channel::<LookupReply>();
        pending.insert([3u8; 16], Waiter::Lookup(live_lookup_tx));

        pending.retain(|_, waiter| !waiter.is_abandoned());

        assert_eq!(pending.len(), 2);
        assert!(!pending.contains_key(&[1u8; 16]));
        assert!(pending.contains_key(&[2u8; 16]));
        assert!(pending.contains_key(&[3u8; 16]));
    }
}

/// The display-order txid for the wallet's `SendResponse`, computed locally
/// from the diverted bytes: the ack deliberately carries none (D5), and this is
/// `Transaction::hash().to_string()`, the exact computation the hub applies to
/// the same bytes, so the wallet reads the identical txid either way. For a
/// fail-safe divert whose bytes do not parse there is no txid and the wallet
/// gets an accepted response with an empty message, matching the HTTP path's
/// behaviour for the same case.
pub fn local_txid(tx_bytes: &[u8]) -> String {
    use zebra_chain::serialization::ZcashDeserialize;
    match zebra_chain::transaction::Transaction::zcash_deserialize(&mut std::io::Cursor::new(
        tx_bytes,
    )) {
        Ok(tx) => tx.hash().to_string(),
        Err(_) => String::new(),
    }
}
