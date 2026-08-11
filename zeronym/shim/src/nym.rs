//! The shim's outbound path over the Nym mixnet: send a `SubmitV1`, await its
//! `AckV1`.
//!
//! The design keeps the Nym SDK out of everything here, mirroring the hub's
//! listener. A driver task (which lands with the SDK) owns the mixnet client
//! and does nothing but move bytes: it takes each [`OutFrame`] this module
//! produces and puts it on the mixnet, and hands every inbound mixnet message
//! back as raw bytes. So the transport is a plain async function over three
//! channels — submit requests in, frames out, mixnet messages in — and its
//! whole behaviour is exercised by holding the driver ends and feeding bytes,
//! with no SDK and no fake client.
//!
//! Correlation is the one job here (D5): every submit carries a random nonce,
//! the hub echoes it in the ack, and [`run_transport`] owns the nonce-to-waiter
//! map as its private state — single owner, no lock. An ack for an unknown
//! nonce is dropped (a duplicate, or a reply that raced its caller's timeout);
//! an empty inbound message is an SDK SURB-replenishment artifact and is
//! filtered before it reaches the codec (D12), exactly as the hub's listener
//! filters them.
//!
//! The per-submit timeout lives at the call site in [`NymHandle::submit`],
//! around the waiter: a dead mixnet, a lost ack, or a gone driver all end in a
//! typed error the intercept path maps onto its existing fail-closed arm
//! (UNAVAILABLE to the wallet, never the operator's indexer). The wallet's
//! retry resends identical bytes and the hub's queue dedups, so no retry state
//! is kept here.

use std::collections::HashMap;
use std::time::Duration;

use rand::RngCore;
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use crate::wire::{self, AckKind, Nonce, WireError};

/// Ceiling on one submit: the frame takes about a second to emit at the
/// client's Poisson rate (more under backpressure) plus a measured ~10 s mixnet
/// round trip, and typical wallet gRPC deadlines sit above 30 s. Sized so a
/// slow-but-alive mixnet succeeds and a dead one fails closed before the wallet
/// gives up.
pub const SUBMIT_TIMEOUT: Duration = Duration::from_secs(25);

/// One submission awaiting its ack: the encoded frame, the nonce inside it, and
/// the waiter to fire when the matching ack arrives.
pub struct SubmitRequest {
    pub nonce: Nonce,
    pub frame: Zeroizing<Vec<u8>>,
    pub ack: oneshot::Sender<AckKind>,
}

/// One outbound frame for the driver to put on the mixnet. [`Zeroizing`]
/// because it holds the transaction bytes.
pub struct OutFrame {
    pub frame: Zeroizing<Vec<u8>>,
}

/// Why a submit produced no verdict. Every variant fails closed at the caller.
#[derive(Debug, PartialEq, Eq)]
pub enum NymError {
    /// The frame could not be built; in practice [`WireError::TxTooLarge`], the
    /// size gate the wallet must hear about as its own error rather than a
    /// generic unavailability.
    Encode(WireError),
    /// No ack within [`NymHandle`]'s timeout. The transaction may still be
    /// admitted; the wallet's retry is idempotent at the hub.
    Timeout,
    /// The driver or the transport loop is gone; nothing can be sent.
    TransportGone,
}

impl std::fmt::Display for NymError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NymError::Encode(err) => write!(f, "could not frame the transaction: {err}"),
            NymError::Timeout => f.write_str("no acknowledgement from the hub within the timeout"),
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
    requests: mpsc::Sender<SubmitRequest>,
    timeout: Duration,
}

impl NymHandle {
    pub fn new(requests: mpsc::Sender<SubmitRequest>, timeout: Duration) -> Self {
        NymHandle { requests, timeout }
    }

    /// Frame `tx_bytes` under a fresh nonce, hand it to the transport, and wait
    /// for the correlated ack.
    pub async fn submit(&self, tx_bytes: &[u8]) -> Result<AckKind, NymError> {
        let mut nonce: Nonce = [0u8; wire::NONCE_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let frame = wire::encode_submit(&nonce, tx_bytes).map_err(NymError::Encode)?;
        let (ack_tx, ack_rx) = oneshot::channel();
        self.requests
            .send(SubmitRequest {
                nonce,
                frame,
                ack: ack_tx,
            })
            .await
            .map_err(|_| NymError::TransportGone)?;
        match tokio::time::timeout(self.timeout, ack_rx).await {
            Err(_) => Err(NymError::Timeout),
            // The transport dropped the waiter without firing it: it is exiting.
            Ok(Err(_)) => Err(NymError::TransportGone),
            Ok(Ok(kind)) => Ok(kind),
        }
    }
}

/// Correlate submits with acks until the driver goes away.
///
/// Runs until the inbound mixnet channel closes (the driver is gone; every
/// waiter still pending is dropped, which surfaces as [`NymError::TransportGone`]
/// at its caller), or until every handle is dropped and the last pending ack is
/// resolved. A frame is only considered in flight once the driver has accepted
/// it, so a request that cannot even be handed over drops its waiter
/// immediately rather than waiting out the timeout.
pub async fn run_transport(
    mut requests: mpsc::Receiver<SubmitRequest>,
    to_mixnet: mpsc::Sender<OutFrame>,
    mut from_mixnet: mpsc::Receiver<Vec<u8>>,
) {
    let mut pending: HashMap<Nonce, oneshot::Sender<AckKind>> = HashMap::new();
    let mut requests_open = true;
    loop {
        tokio::select! {
            request = requests.recv(), if requests_open => match request {
                Some(SubmitRequest { nonce, frame, ack }) => {
                    if to_mixnet.send(OutFrame { frame }).await.is_err() {
                        // The driver is gone. Dropping `ack` and every pending
                        // waiter unblocks all callers with TransportGone.
                        return;
                    }
                    pending.insert(nonce, ack);
                }
                None => requests_open = false,
            },
            message = from_mixnet.recv() => match message {
                Some(bytes) => {
                    // Empty inbound messages are the SDK's SURB-replenishment
                    // artifacts, not acks (D12).
                    if bytes.is_empty() {
                        continue;
                    }
                    match wire::decode_ack(&bytes) {
                        Ok((nonce, kind)) => {
                            // An unknown nonce is dropped: a duplicate ack, or
                            // one that arrived after its caller timed out.
                            if let Some(ack) = pending.remove(&nonce) {
                                let _ = ack.send(kind);
                            }
                        }
                        // No nonce, no body: the log reaches the parent host,
                        // which is exactly who is withheld those.
                        Err(err) => tracing::warn!(reason = %err, "inbound message could not be decoded as an ack"),
                    }
                }
                None => return,
            },
        }
        if !requests_open && pending.is_empty() {
            return;
        }
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
