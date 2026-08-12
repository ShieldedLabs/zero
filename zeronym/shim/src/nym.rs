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
    target: usize,
}

/// One outbound frame for the driver to put on the mixnet, with the fixed
/// number of reply SURBs to attach to it (D3/D4) and which configured hub
/// address to send it to. [`Zeroizing`] because a submit frame holds the
/// transaction bytes.
///
/// The target is an INDEX into the driver's configured address list, never an
/// address: nothing in this module knows what a Nym address is, which is the
/// same boundary that keeps the SDK out of the hub's listener.
pub struct OutFrame {
    pub frame: Zeroizing<Vec<u8>>,
    pub reply_surbs: u32,
    pub target: usize,
}

/// How many hub addresses the driver currently holds, shared with the handle.
///
/// A count rather than the addresses themselves, and atomic rather than fixed,
/// because a hub's Nym address changes on every restart of its diskless enclave
/// (D10): the driver can swap its list and update this without the transport or
/// its callers being rebuilt.
pub type TargetCount = std::sync::Arc<std::sync::atomic::AtomicUsize>;

/// How many requests are waiting for a reply, published by [`run_transport`].
///
/// Read by [`run_supervisor`], which will not rotate the client's identity out
/// from under a request that is still expecting an answer.
pub type InflightCount = std::sync::Arc<std::sync::atomic::AtomicUsize>;

/// What the driver reports about its mixnet client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientEvent {
    /// The client is gone and nothing can be sent until it is rebuilt.
    ///
    /// The SDK reaches this on its own: auto-reconnect is only 10 attempts at
    /// 5 s, and after 20 consecutive send failures it declares the gateway dead
    /// and shuts the whole client down with no further reconnect (D12). There
    /// is no recovery inside the SDK past that point, so the driver watches its
    /// cancellation signal and reports here.
    Died,
}

/// What [`run_supervisor`] tells the driver to do with its client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientCommand {
    /// Build a fresh client. A new client means a new identity, a new gateway
    /// registration, and therefore a fresh `AnonymousSenderTag`, which is the
    /// only lever that bounds how long a hub can link one shim's submissions
    /// (D11).
    Rebuild,
    /// Shut the client down cleanly and stop.
    ///
    /// A command rather than a drop because the SDK's `disconnect()` is NOT
    /// cancel-safe and dropping the client leaks its background tasks (D12):
    /// the driver must run it to completion, which it can only do if it is
    /// told rather than dropped.
    Disconnect,
}

/// When the client's identity is rotated, and how patiently.
///
/// The PERIOD is the D11 decision this type exists to make a parameter rather
/// than a redeploy: it is exactly the window within which a hub can link one
/// shim's submissions under one sender tag. Never rotating leaves that window
/// at the whole process uptime; rotating per submission is the condemned
/// connect-burst pattern and drops cover between builds. The period itself is
/// a humans decision (see the plan), so there is no default here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationPolicy {
    /// How often to mint a fresh identity. `None` never rotates.
    pub period: Option<Duration>,
    /// How long a due rotation waits for in-flight requests to drain before
    /// going ahead regardless.
    ///
    /// Rotating under an in-flight request strands it: its reply comes back
    /// through SURBs the old client minted, so the caller waits out its
    /// timeout and the wallet pays a retry. Waiting forever is the opposite
    /// failure, where a busy shim never rotates and the linkage window is
    /// unbounded in practice. This bounds the compromise.
    pub defer_limit: Duration,
    /// How long to wait after asking for a rebuild before acting on anything
    /// else, so a client that cannot be rebuilt is retried steadily rather
    /// than in a hot loop.
    pub rebuild_backoff: Duration,
}

impl RotationPolicy {
    /// Rotate every `period`, with the defaults for the two waits.
    pub fn every(period: Duration) -> Self {
        RotationPolicy {
            period: Some(period),
            ..RotationPolicy::never()
        }
    }

    /// Never rotate: the sender-tag linkage window becomes the process uptime
    /// (D11's residual, stated rather than hidden).
    pub fn never() -> Self {
        RotationPolicy {
            period: None,
            defer_limit: Duration::from_secs(60),
            rebuild_backoff: Duration::from_secs(5),
        }
    }
}

/// How often a deferred rotation re-checks whether the transport has gone idle.
const DEFER_RECHECK: Duration = Duration::from_millis(250);

/// How long an outstanding request can sit before [`run_transport`] re-checks
/// whether its caller is still there.
///
/// Without this the in-flight count only changes when a message arrives, so a
/// request that timed out on a then-quiet transport would keep the count above
/// zero and defer the supervisor's rotation against a caller that has already
/// given up.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

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
    targets: TargetCount,
    /// Where the next request starts its sweep of the address list, so load is
    /// spread across a multi-homed hub's gateways instead of always leaning on
    /// the first.
    cursor: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl NymHandle {
    pub fn new(requests: mpsc::Sender<Request>, timeout: Duration, targets: TargetCount) -> Self {
        NymHandle {
            requests,
            timeout,
            targets,
            cursor: Default::default(),
        }
    }

    /// Frame `tx_bytes` and submit it, trying each configured hub address in
    /// turn until one acknowledges.
    pub async fn submit(&self, tx_bytes: &[u8]) -> Result<AckKind, NymError> {
        self.each_target(|target| {
            let nonce = fresh_nonce();
            let frame = wire::encode_submit(&nonce, tx_bytes)?;
            let (tx, rx) = oneshot::channel();
            Ok((
                Request {
                    nonce,
                    frame,
                    reply_surbs: SUBMIT_REPLY_SURBS,
                    waiter: Waiter::Ack(tx),
                    target,
                },
                rx,
            ))
        })
        .await
    }

    /// Look a transaction up, trying each configured hub address in turn. The
    /// hash is the wallet's `TxFilter.hash` in wire order, passed through
    /// unmodified exactly as the HTTP transport posts it.
    pub async fn get_transaction(&self, wire_hash: &[u8]) -> Result<LookupReply, NymError> {
        self.each_target(|target| {
            let nonce = fresh_nonce();
            // The frame is small and holds no transaction bytes, but the request
            // channel carries one type, so it travels in the same buffer.
            let frame = Zeroizing::new(wire::encode_lookup(&nonce, wire_hash)?.to_vec());
            let (tx, rx) = oneshot::channel();
            Ok((
                Request {
                    nonce,
                    frame,
                    reply_surbs: LOOKUP_REPLY_SURBS,
                    waiter: Waiter::Lookup(tx),
                    target,
                },
                rx,
            ))
        })
        .await
    }

    /// Try `build` against each configured hub address until one answers.
    ///
    /// Only a TIMEOUT moves on to the next address: that is the shape a dead
    /// gateway takes, and a Nym address dies with its gateway (D10). Every
    /// other outcome is an answer or a permanent failure — a refusal is a live
    /// hub's verdict and asking another would not change it, an encode failure
    /// is about the request itself, and a gone transport is gone for all
    /// addresses alike.
    ///
    /// Each attempt mints a FRESH nonce, so a late reply from an address that
    /// was given up on cannot be mistaken for the answer of the one that
    /// followed it. Resending is safe by construction: the hub's queue is keyed
    /// on the payload hash, so a resend collapses to a duplicate (D6).
    ///
    /// The wallet-visible cost of a fully dead mixnet is therefore
    /// `timeout * addresses`, which is the reason to keep the list short: it
    /// bounds how long a wallet waits before hearing UNAVAILABLE.
    async fn each_target<T, F>(&self, mut build: F) -> Result<T, NymError>
    where
        F: FnMut(usize) -> Result<(Request, oneshot::Receiver<T>), WireError>,
    {
        let targets = self.targets.load(std::sync::atomic::Ordering::Relaxed);
        if targets == 0 {
            // No hub address to send to. Fail closed rather than hand the
            // driver an index into an empty list.
            return Err(NymError::TransportGone);
        }
        let start = self
            .cursor
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let mut last = NymError::TransportGone;
        for attempt in 0..targets {
            let target = start.wrapping_add(attempt) % targets;
            let (request, rx) = build(target).map_err(NymError::Encode)?;
            // ONE deadline covers both the wait to be ACCEPTED by the transport
            // and the wait for the reply, so the wallet-visible cost of a dead
            // mixnet stays `timeout * addresses`. Bounding only the reply, and
            // letting the accept `send().await` block unbounded on a
            // backpressured transport (a driver mid-emission holds the channel
            // full for the ~1 s a 64 KiB frame takes), would make the wait
            // unbounded above and falsify the latency claim in the plan.
            let deadline = tokio::time::Instant::now() + self.timeout;
            match tokio::time::timeout_at(deadline, self.requests.send(request)).await {
                Ok(Ok(())) => {}
                // The transport loop is gone; nothing can be sent to any address.
                Ok(Err(_)) => return Err(NymError::TransportGone),
                Err(_) => {
                    tracing::warn!(
                        target_index = target,
                        "the transport did not accept the request in time; trying the next"
                    );
                    last = NymError::Timeout;
                    continue;
                }
            }
            match self.await_reply(deadline, rx).await {
                Err(NymError::Timeout) => {
                    tracing::warn!(
                        target_index = target,
                        "no reply from a hub address; trying the next"
                    );
                    last = NymError::Timeout;
                }
                other => return other,
            }
        }
        Err(last)
    }

    /// Await one reply until `deadline`, the same instant the accept wait shares,
    /// so a single attempt cannot exceed the per-request timeout however the time
    /// is split between being accepted and being answered (M1').
    async fn await_reply<T>(
        &self,
        deadline: tokio::time::Instant,
        rx: oneshot::Receiver<T>,
    ) -> Result<T, NymError> {
        match tokio::time::timeout_at(deadline, rx).await {
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
    requests: mpsc::Receiver<Request>,
    to_mixnet: mpsc::Sender<OutFrame>,
    from_mixnet: mpsc::Receiver<Zeroizing<Vec<u8>>>,
    inflight: InflightCount,
) {
    correlate(requests, to_mixnet, from_mixnet, &inflight).await;
    // However this loop ends, nothing is in flight any more. Leaving the last
    // count behind would have the supervisor defer every future rotation
    // against a transport that no longer exists.
    inflight.store(0, std::sync::atomic::Ordering::Relaxed);
}

async fn correlate(
    mut requests: mpsc::Receiver<Request>,
    to_mixnet: mpsc::Sender<OutFrame>,
    mut from_mixnet: mpsc::Receiver<Zeroizing<Vec<u8>>>,
    inflight: &InflightCount,
) {
    let mut pending: HashMap<Nonce, Waiter> = HashMap::new();
    let mut requests_open = true;
    // Capacity on the driver channel, taken BEFORE a request is accepted.
    //
    // Handing a frame over must never be an awaited step inside a select arm:
    // while it waited, this loop would stop reading inbound messages, so the
    // replies to requests ALREADY in flight would sit undelivered and time out.
    // That is precisely the case the design expects, since a driver mid-emission
    // holds the channel full for the ~1 s a 64 KiB frame takes to emit (more
    // under backpressure). `reserve()` is cancel-safe in `select!` and no
    // capacity is taken unless its branch completes, so the loop keeps serving
    // inbound the whole time and the eventual `Permit::send` cannot block.
    let mut permit: Option<mpsc::Permit<'_, OutFrame>> = None;
    loop {
        tokio::select! {
            reserved = to_mixnet.reserve(), if permit.is_none() && requests_open => {
                match reserved {
                    Ok(reserved) => permit = Some(reserved),
                    // The driver is gone. Dropping every pending waiter
                    // unblocks all callers with TransportGone.
                    Err(_) => return,
                }
            }
            // `requests_open` guards this arm too, not just `reserve`: once the
            // requests channel has closed while a permit is still held, `recv()`
            // returns `None` instantly on every turn, and without this guard that
            // arm stays ready and hot-loops the whole select (pegging a core)
            // while the last replies drain. Guarded, the loop falls through to
            // serving inbound and the sweep until `pending` empties.
            request = requests.recv(), if permit.is_some() && requests_open => match request {
                Some(Request { nonce, frame, reply_surbs, waiter, target }) => {
                    // Non-blocking: the capacity is already ours.
                    permit
                        .take()
                        .expect("the arm is guarded on holding a permit")
                        .send(OutFrame { frame, reply_surbs, target });
                    pending.insert(nonce, waiter);
                }
                None => requests_open = false,
            },
            message = from_mixnet.recv() => match message {
                Some(bytes) => {
                    // Empty inbound messages are the SDK's SURB-replenishment
                    // artifacts, not replies (D12). They are not delivered,
                    // but they still turn the loop, which sweeps below: an
                    // early `continue` here would skip that.
                    if !bytes.is_empty() {
                        deliver(&mut pending, &bytes);
                    }
                }
                None => return,
            },
            // Nothing to do; the loop turns so the sweep below runs while
            // requests are outstanding. Armed only when there is something
            // that could become abandoned, so an idle transport does not wake
            // up at all.
            _ = tokio::time::sleep(SWEEP_INTERVAL), if !pending.is_empty() => {}
        }
        // Callers that timed out (or were cancelled) have dropped their
        // receivers; without this sweep their entries would accumulate for the
        // life of the process, since the reply that would remove them is
        // exactly the one that never came.
        pending.retain(|_, waiter| !waiter.is_abandoned());
        // Published after the sweep, so it counts requests whose caller is
        // still listening: that is what the supervisor must not rotate out
        // from under.
        inflight.store(pending.len(), std::sync::atomic::Ordering::Relaxed);
        if !requests_open && pending.is_empty() {
            return;
        }
    }
}

/// Own the mixnet client's lifecycle: rebuild it when it dies, rotate it on a
/// schedule, and disconnect it cleanly on shutdown.
///
/// Like [`run_transport`], this touches no SDK. It consumes [`ClientEvent`]s
/// the driver reports and emits [`ClientCommand`]s the driver executes, so the
/// whole policy — when to rotate, how long to defer, how hard to retry — is
/// exercised by holding the channel ends, and the driver stays a thin thing
/// that owns a client and does what it is told.
///
/// Two rules the SDK's own behaviour dictates (D12). A dead client is rebuilt
/// IMMEDIATELY and without waiting for in-flight requests, because after the
/// SDK's 20-failure hard stop nothing is deliverable and those requests are
/// already lost to their timeouts. Shutdown sends [`ClientCommand::Disconnect`]
/// rather than simply returning, because `disconnect()` is not cancel-safe and
/// a dropped client leaks its background tasks.
pub async fn run_supervisor(
    policy: RotationPolicy,
    mut events: mpsc::Receiver<ClientEvent>,
    commands: mpsc::Sender<ClientCommand>,
    inflight: InflightCount,
    shutdown: impl std::future::Future<Output = ()>,
) {
    use std::sync::atomic::Ordering;
    use tokio::time::Instant;

    tokio::pin!(shutdown);
    let mut rotate_at = policy.period.map(|period| Instant::now() + period);
    // Set once a rotation comes due and is waiting for the transport to go
    // idle; the instant is when it stops waiting and rotates regardless.
    let mut defer_deadline: Option<Instant> = None;

    loop {
        let wake = match (defer_deadline, rotate_at) {
            (Some(_), _) => Some(Instant::now() + DEFER_RECHECK),
            (None, Some(at)) => Some(at),
            (None, None) => None,
        };

        tokio::select! {
            _ = &mut shutdown => {
                let _ = commands.send(ClientCommand::Disconnect).await;
                return;
            }
            event = events.recv() => match event {
                Some(ClientEvent::Died) => {
                    tracing::warn!("the mixnet client died; rebuilding");
                    if commands.send(ClientCommand::Rebuild).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(policy.rebuild_backoff).await;
                    // A rebuild is a fresh identity, so the linkage window
                    // starts over: the rotation clock restarts with it.
                    rotate_at = policy.period.map(|period| Instant::now() + period);
                    defer_deadline = None;
                }
                // The driver is gone; there is no client to supervise.
                None => return,
            },
            _ = sleep_until_maybe(wake) => {
                let deadline = *defer_deadline
                    .get_or_insert_with(|| Instant::now() + policy.defer_limit);
                let idle = inflight.load(Ordering::Relaxed) == 0;
                if !idle && Instant::now() < deadline {
                    // Something is still waiting for a reply its current SURBs
                    // would carry. Re-check shortly rather than strand it.
                    continue;
                }
                if !idle {
                    tracing::warn!(
                        "rotating the mixnet client with requests still in flight; \
                         they will fail closed and be retried"
                    );
                }
                tracing::info!("rotating the mixnet client's identity");
                if commands.send(ClientCommand::Rebuild).await.is_err() {
                    return;
                }
                rotate_at = policy.period.map(|period| Instant::now() + period);
                defer_deadline = None;
            }
        }
    }
}

/// Sleep until `at`, or forever when there is nothing scheduled.
async fn sleep_until_maybe(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
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

    /// M1': the wait to be ACCEPTED by the transport must be inside the request
    /// timeout, not outside it. A requests channel whose one slot is held blocks
    /// the accept `send`; the submit must still fail closed with `Timeout` within
    /// the budget, rather than hang unbounded above it (which is what falsified
    /// the plan's `timeout * addresses` latency claim before this fix).
    #[tokio::test]
    async fn a_backpressured_transport_times_out_within_the_budget() {
        let (tx, _rx) = mpsc::channel::<Request>(1);
        // Hold the one slot so the handle's send has nowhere to go, and keep the
        // receiver alive so the channel is full-but-open rather than closed.
        let _permit = tx.reserve().await.expect("channel open");
        let targets = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let handle = NymHandle::new(tx.clone(), Duration::from_millis(150), targets);

        let started = std::time::Instant::now();
        let result = handle.submit(&[0u8; 8]).await;

        assert_eq!(result, Err(NymError::Timeout));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the accept wait must be bounded by the request timeout"
        );
    }

    /// L1': a closed requests channel with a request still in flight must not
    /// hot-loop the select. On the current-thread runtime this test uses, a spin
    /// that never yields (the pre-fix behaviour, the request arm firing on
    /// `recv() == None` every turn) starves this very delivery and the test hangs;
    /// with the `requests_open` guard the loop parks on inbound and the reply is
    /// delivered and the transport exits.
    #[tokio::test]
    async fn a_closed_requests_channel_with_a_reply_in_flight_does_not_spin() {
        let (req_tx, req_rx) = mpsc::channel::<Request>(4);
        let (out_tx, mut out_rx) = mpsc::channel::<OutFrame>(4);
        let (in_tx, in_rx) = mpsc::channel::<Zeroizing<Vec<u8>>>(4);
        let inflight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task = tokio::spawn(run_transport(req_rx, out_tx, in_rx, inflight));

        // One submit goes out: a permit is taken and a waiter is left pending.
        let nonce = [9u8; 16];
        let frame = wire::encode_submit(&nonce, &[0u8; 8]).unwrap();
        let (waiter_tx, waiter_rx) = oneshot::channel();
        req_tx
            .send(Request {
                nonce,
                frame,
                reply_surbs: SUBMIT_REPLY_SURBS,
                waiter: Waiter::Ack(waiter_tx),
                target: 0,
            })
            .await
            .unwrap();
        out_rx.recv().await.expect("the frame is emitted to the driver");

        // Close requests while the reply is still outstanding, then deliver it.
        drop(req_tx);
        let ack = wire::encode_ack(&nonce, AckKind::Accepted);
        in_tx.send(Zeroizing::new(ack.to_vec())).await.unwrap();

        let delivered = tokio::time::timeout(Duration::from_secs(2), waiter_rx)
            .await
            .expect("the reply must be delivered, not starved by a spin")
            .expect("the waiter fired");
        assert_eq!(delivered, AckKind::Accepted);

        // With the last reply drained and requests closed, the transport exits.
        drop(in_tx);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the transport exits once pending drains")
            .unwrap();
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
