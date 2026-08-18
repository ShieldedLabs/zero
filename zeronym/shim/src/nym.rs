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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use rand::RngCore;
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use crate::wire::{self, AckKind, LookupReply, Nonce, WireError};

/// Ceiling on one LOOKUP round trip. (Submits no longer wait for the hub at all;
/// they are bounded by [`SUBMIT_DISPATCH_TIMEOUT`] instead.)
///
/// Raised from 25 s to 90 s on 2026-08-14, against measurement. The old value was
/// sized for a ~10 s round trip, which is what an unthrottled client gives; a real
/// gateway's backpressure pegs the client at ~8 packets/s, and a lookup is ~101
/// packets (60 reply SURBs out, a full 64 KiB reply back) — **~12.6 s of pure
/// emission before any mix delay or queueing**. Measured against the live pair,
/// 25 s produced 14 consecutive UNAVAILABLE answers across two independent
/// deployments; see the `throughput_budget` tests, which pin this arithmetic.
///
/// Raising it is only cheap because submits stopped waiting: before dispatch-only
/// this constant also bounded every send, so a bigger value meant slower wallets.
/// Now it costs nothing until a wallet actually looks up a just-diverted
/// transaction, which is the one case that is failing today.
///
/// **It multiplies.** `each_target` sweeps the hub address list on timeout, so a
/// fully dead mixnet costs `timeout * addresses` before the wallet hears
/// UNAVAILABLE — 90 s is deliberate for the one-hub deployment and wants
/// revisiting if the list grows. Override per deployment with
/// `ZIS_LOOKUP_TIMEOUT_SECS`; the default is what ships.
///
/// Retuning this is safe from the rotation supervisor's side: a due rotation
/// defers for at least this long ([`RotationPolicy::effective_defer_limit`]), so
/// no value of it can leave a rotation destroying the client an in-flight lookup's
/// reply is addressed to.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

/// Ceiling on handing ONE submission to the transport, for the best-effort submit
/// path (see [`NymHandle::submit`]). Not a round trip: this only bounds the wait
/// to be ACCEPTED into the transport channel, which is sub-millisecond until the
/// channel fills and then blocks under mixnet backpressure. Short so a wallet is
/// never held long: on a healthy mixnet the submit answers success almost at
/// once, and a chronically backpressured transport fails closed quickly so the
/// wallet retries rather than hanging.
pub const SUBMIT_DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Reply SURBs attached to a `SubmitV1` (D3). The ack is a single 64-byte
/// frame, so a small fixed count carries it with no re-request round trip;
/// measured in the nymnet harness, where 13 acked with no re-request at all.
/// Fixed, because the on-wire packet count is a function of frame size PLUS
/// attached-SURB count (D4).
///
/// NOTE (dispatch-only submit): the shim no longer awaits the ack, so most of
/// these SURBs are now unused send-path overhead — the hub spends them replying
/// into a dropped receiver. They stay NON-ZERO because a zero count is what would
/// push the driver off the anonymous-send path (M6/D3); the count simply no
/// longer needs to be large enough to carry an ack. Trimming it toward the
/// anonymity minimum is a throughput follow-up, gated on validating
/// SURB-replenishment behaviour on the localnet — and low priority, since submits
/// are rare (a migration is ~0.77 per block) next to the continuous cover traffic.
pub const SUBMIT_REPLY_SURBS: u32 = 13;

/// Reply SURBs attached to a `LookupV1` (D3 as corrected). The reply is a FULL
/// frame, which the nymnet harness measured at exactly 41 reply packets, and
/// the SDK holds back `minimum_reply_surb_storage_threshold` (10) before it
/// will spend any: below 51 the hub must fire a blocking re-request round,
/// costing a full mixnet round trip per lookup (measured). 60 clears the
/// threshold with margin while staying a fixed, bounded count.
pub const LOOKUP_REPLY_SURBS: u32 = 60;

/// Externally observable health of the shim's mixnet client.
///
/// An attested shim has no SSH, and dispatch-only submit answers the wallet as
/// soon as a migration enters the in-process transport — so without this, a shim
/// whose mixnet client is dead is INDISTINGUISHABLE from a healthy one while it
/// silently drops every migration (measured 2026-08-14). This is the minimum an
/// operator needs to tell those apart.
///
/// **What it deliberately does NOT expose.** Nothing about user traffic: no send
/// counts, no timestamps of sends, no txids, no queue depth. A "last diverted at"
/// field would be an oracle telling any poller exactly when a migration went out,
/// which is the timing correlation the whole system exists to prevent. Everything
/// here is a property of the CLIENT LIFECYCLE — which turns over on gateway churn
/// and network events, not on whether a user sent anything — and the cover-traffic
/// stream runs continuously either way, so none of it is a divert oracle.
#[derive(Clone, Default)]
pub struct MixnetStatus(std::sync::Arc<MixnetStatusInner>);

/// Wall-clock seconds since the epoch, saturating to 0 if the clock is before
/// it. Only used by the temporary diagnostic block.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Default)]
struct MixnetStatusInner {
    /// Diversion over the mixnet is configured at all (`--hub-nym` was set).
    configured: AtomicBool,
    /// A client is currently built and connected to its gateway.
    connected: AtomicBool,
    /// How many times the client has died since start. Climbing means gateway
    /// churn; historically this reached 6,305.
    deaths: AtomicU64,
    /// Consecutive failed rebuilds. Non-zero means we are currently down and
    /// retrying, which is the state that silently swallows migrations.
    consecutive_failures: AtomicU64,

    // ---- INBOUND LIVENESS COUNTERS: PERMANENT, NOT the diagnostic block ---
    //
    // `replies_received` and `empty_inbound` are load-bearing: the driver's
    // liveness probe reads them through `inbound_total` to decide whether the
    // client is receiving anything at all, and rebuilds it when it is not.
    // KEEP THEM when the `ZIS_DIAG` endpoint below is deleted — removing them
    // silently removes the self-heal.
    //
    // They are still deliberately absent from `to_json`: the doc comment above
    // this type is a promise about `/nym-status`, and send counts with
    // timestamps ARE a divert oracle. They are exposed only on the gated path.
    /// Whether the diagnostic endpoint answers at all. Off unless `ZIS_DIAG`.
    diag_enabled: AtomicBool,
    /// Non-empty inbound mixnet messages the driver has taken off the client.
    /// THE number that matters: zero here with sends climbing means replies are
    /// not arriving, rather than arriving and going unread.
    replies_received: AtomicU64,
    /// Empty inbound messages — the SDK's SURB-replenishment artifacts (D12).
    /// Counted separately because they prove the inbound path is alive even
    /// when no reply has been reassembled.
    empty_inbound: AtomicU64,
    /// Frames handed to the SDK sender.
    sends_dispatched: AtomicU64,
    /// Unix seconds of the last non-empty inbound message; 0 = never.
    last_reply_unix: AtomicU64,
    /// Unix seconds at which the driver first reported in.
    started_unix: AtomicU64,
    /// Our own Nym address, whose `@gateway` half names the entry gateway the
    /// client actually registered with — unreadable on an attested enclave, and
    /// the value every gateway hypothesis has needed.
    address: std::sync::Mutex<Option<String>>,
}

impl MixnetStatus {
    /// Mark that diversion is configured. Called once at startup, before the
    /// first build, so a shim that never connects still reports honestly.
    pub fn set_configured(&self) {
        self.0.configured.store(true, Ordering::Relaxed);
        self.0.started_unix.store(unix_now(), Ordering::Relaxed);
    }

    /// A client is up. Clears the consecutive-failure run.
    pub fn set_connected(&self) {
        self.0.connected.store(true, Ordering::Relaxed);
        self.0.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// The client died: no longer connected, and one more death on the counter.
    pub fn set_died(&self) {
        self.0.connected.store(false, Ordering::Relaxed);
        self.0.deaths.fetch_add(1, Ordering::Relaxed);
    }

    /// A rebuild attempt failed. Still down; the run of failures grows.
    pub fn set_rebuild_failed(&self) {
        self.0.connected.store(false, Ordering::Relaxed);
        self.0.consecutive_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// The status as the JSON the endpoint serves. Hand-rolled rather than
    /// pulling in serde_json for four scalars.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"diversion_configured\":{},\"mixnet_connected\":{},\"client_deaths\":{},\"consecutive_rebuild_failures\":{}}}",
            self.0.configured.load(Ordering::Relaxed),
            self.0.connected.load(Ordering::Relaxed),
            self.0.deaths.load(Ordering::Relaxed),
            self.0.consecutive_failures.load(Ordering::Relaxed),
        )
    }

    // ---- TEMPORARY DIAGNOSTIC API (delete with the fields above) ----------

    /// Open the diagnostic endpoint. Called once at startup iff `ZIS_DIAG` is
    /// set; left off, the path proxies through like any unknown path.
    pub fn enable_diag(&self) {
        self.0.diag_enabled.store(true, Ordering::Relaxed);
    }

    /// Whether the diagnostic endpoint should answer.
    pub fn diag_enabled(&self) -> bool {
        self.0.diag_enabled.load(Ordering::Relaxed)
    }

    /// One frame handed to the SDK sender.
    pub fn record_send(&self) {
        self.0.sends_dispatched.fetch_add(1, Ordering::Relaxed);
    }

    /// One inbound mixnet message. `empty` distinguishes a SURB-replenishment
    /// artifact from a real reply frame.
    pub fn record_inbound(&self, empty: bool) {
        if empty {
            self.0.empty_inbound.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.0.replies_received.fetch_add(1, Ordering::Relaxed);
        self.0.last_reply_unix.store(unix_now(), Ordering::Relaxed);
    }

    /// Every inbound mixnet message seen, reply frames and SURB-replenishment
    /// artifacts alike. The liveness probe watches this rather than
    /// `replies_received`, because ANY inbound traffic proves the gateway is
    /// delivering to us — which is the property that fails — while a reply frame
    /// additionally requires the hub to be up and answering.
    pub fn inbound_total(&self) -> u64 {
        self.0.replies_received.load(Ordering::Relaxed)
            + self.0.empty_inbound.load(Ordering::Relaxed)
    }

    /// Publish the client's own Nym address after a (re)build.
    pub fn set_address(&self, address: String) {
        if let Ok(mut slot) = self.0.address.lock() {
            *slot = Some(address);
        }
    }

    /// The diagnostic payload. Served only when [`Self::diag_enabled`].
    ///
    /// Deliberately NOT merged into [`Self::to_json`]: `sends_dispatched` and
    /// `last_reply_unix` are exactly the divert oracle that endpoint promises
    /// not to be. This one is closed by default and temporary.
    pub fn diag_json(&self) -> String {
        let started = self.0.started_unix.load(Ordering::Relaxed);
        let address = self
            .0
            .address
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .unwrap_or_default();
        // The gateway half of `identity.encryption@gateway`, and ONLY that half.
        //
        // The full address is the sender identity every diverted migration goes
        // out under, so publishing it on an unauthenticated wallet-facing
        // listener would hand an observer the link between this shim and the
        // submissions the hub receives -- the exact unlinkability the design is
        // built to hold. The gateway is what the diagnostic ever needed (it says
        // which entry point delivery is being tested through) and it is already
        // public in the topology, so it costs nothing to expose.
        let gateway = address.rsplit('@').next().unwrap_or("").to_owned();
        format!(
            "{{\"replies_received\":{},\"empty_inbound\":{},\"sends_dispatched\":{},\
             \"last_reply_unix\":{},\"started_unix\":{},\"uptime_secs\":{},\
             \"gateway\":\"{}\",\
             \"mixnet_connected\":{},\"client_deaths\":{},\"consecutive_rebuild_failures\":{}}}",
            self.0.replies_received.load(Ordering::Relaxed),
            self.0.empty_inbound.load(Ordering::Relaxed),
            self.0.sends_dispatched.load(Ordering::Relaxed),
            self.0.last_reply_unix.load(Ordering::Relaxed),
            started,
            unix_now().saturating_sub(started),
            gateway,
            self.0.connected.load(Ordering::Relaxed),
            self.0.deaths.load(Ordering::Relaxed),
            self.0.consecutive_failures.load(Ordering::Relaxed),
        )
    }

    /// Whether the shim can currently carry a migration: either diversion is not
    /// configured at all (forward-only, nothing to be down), or the client is up.
    pub fn is_healthy(&self) -> bool {
        !self.0.configured.load(Ordering::Relaxed) || self.0.connected.load(Ordering::Relaxed)
    }
}

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
    ///
    /// Whatever is configured here, the supervisor applies at least
    /// [`REQUEST_TIMEOUT`]; see [`RotationPolicy::effective_defer_limit`] for why
    /// the floor is derived rather than trusted.
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
            // The lookup budget itself rather than a number of its own, for the
            // reason `effective_defer_limit` gives, and stated here so the
            // configured value and the applied one cannot read differently: a
            // standalone 60 s default is precisely what outlived the budget's rise
            // to 90 s without anyone noticing they had crossed.
            defer_limit: REQUEST_TIMEOUT,
            rebuild_backoff: Duration::from_secs(5),
        }
    }

    /// The deferral a due rotation actually gets: never shorter than
    /// [`REQUEST_TIMEOUT`].
    ///
    /// A rotation destroys the client that minted the SURBs an in-flight reply is
    /// addressed to, so a deferral shorter than the lookup budget lets the
    /// rotation fire under a request that still has time left on its clock. That
    /// request can then never be answered: the reply has nowhere to land, and it
    /// burns the rest of its budget before failing closed. The floor is derived
    /// from the timeout rather than trusting the configured number because the two
    /// were only ever safe by coincidence, and that coincidence broke silently
    /// when the budget was raised from 25 s to 90 s against a 60 s limit; nobody
    /// retuning the timeout should have to know this coupling exists.
    ///
    /// What it guarantees is bounded to the requests in flight when the rotation
    /// came DUE. One that starts during the deferral can still be cut short, which
    /// is the unavoidable price of bounding the deferral at all, and it is also
    /// why the floor is exactly the budget and not a multiple of it. One residual:
    /// a deployment that raises the lookup budget past this constant with
    /// `ZIS_LOOKUP_TIMEOUT_SECS` reopens the gap by its excess, since the
    /// supervisor is handed a policy and never sees that override.
    pub fn effective_defer_limit(&self) -> Duration {
        self.defer_limit.max(REQUEST_TIMEOUT)
    }
}

/// The next scheduled rotation instant, floored so a rotation can never fire
/// faster than the rebuild backoff. A period at or above the backoff (the norm)
/// is unaffected; the floor only stops a misconfigured near-zero period from
/// hot-looping the supervisor (L2).
fn next_rotation(policy: &RotationPolicy) -> Option<tokio::time::Instant> {
    policy
        .period
        .map(|period| tokio::time::Instant::now() + period.max(policy.rebuild_backoff))
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
    /// The round-trip budget for a LOOKUP: dispatch plus the hub's reply (D5).
    timeout: Duration,
    /// The much shorter budget for a best-effort SUBMIT, which only waits to be
    /// accepted by the transport, never for the hub's reply (see [`Self::submit`]).
    dispatch_timeout: Duration,
    targets: TargetCount,
    /// Where the next request starts its sweep of the address list, so load is
    /// spread across a multi-homed hub's gateways instead of always leaning on
    /// the first.
    cursor: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl NymHandle {
    pub fn new(
        requests: mpsc::Sender<Request>,
        timeout: Duration,
        dispatch_timeout: Duration,
        targets: TargetCount,
    ) -> Self {
        NymHandle {
            requests,
            timeout,
            dispatch_timeout,
            targets,
            cursor: Default::default(),
        }
    }

    /// Frame `tx_bytes` and submit it, trying each configured hub address in
    /// Divert a transaction to the hub, BEST-EFFORT: answer success as soon as the
    /// frame is dispatched to the mixnet, without waiting for the hub's end-to-end
    /// ack.
    ///
    /// The ack is a full mixnet round trip (~10 s even healthy, minutes under the
    /// gateway backpressure that caps our send rate) and, since neither the shim
    /// nor the hub runs a validator (`hub/src/chain.rs`), it only ever confirmed
    /// the hub QUEUED the frame — never that the transaction is valid or in a
    /// mempool. A `SendTransaction` success has likewise never promised block
    /// inclusion. So the diverted path already relies on the wallet's own
    /// confirmation-via-sync for both validity and delivery, and blocking it on
    /// the round trip only adds the very latency this system is fighting. We
    /// therefore treat a successful hand-off to the transport as the answer.
    ///
    /// A waiter is registered so the frame carries a nonce the hub CAN ack against
    /// (the frame still carries reply SURBs, M6), but its receiver is dropped and
    /// the reply is never awaited; the correlator sweeps the unclaimed waiter, and
    /// an unmatched ack is discarded. Emission does not depend on a live waiter:
    /// `run_transport` sends the `OutFrame` before it records the waiter.
    ///
    /// Fail closed ONLY when the frame cannot be handed off at all: no hub address
    /// configured, the transport is gone, or it stays backpressured past
    /// [`SUBMIT_DISPATCH_TIMEOUT`] (the wallet then retries, safe by D6 dedup). A
    /// too-large frame stays a typed `Encode` error the wallet hears as its own
    /// size failure. The txid the caller returns is computed locally (D5); the ack
    /// carried none anyway.
    ///
    /// Returns `Ok(())` for a dispatch, not an `AckKind`: since the ack is never
    /// awaited there is no hub verdict to return, and claiming an `Accepted` we
    /// never received would be a lie. The caller maps `Ok(())` to the wallet's
    /// success.
    pub async fn submit(&self, tx_bytes: &[u8]) -> Result<(), NymError> {
        let targets = self.targets.load(Ordering::Relaxed);
        if targets == 0 {
            // No hub address to send to: nothing was dispatched. Fail closed.
            return Err(NymError::TransportGone);
        }

        // Send to EVERY configured hub address, not one (REVIEW #6).
        //
        // The deployment is many shims to ONE hub -- which is what makes the batch,
        // and so the anonymity set, the union of every operator's migrations -- but
        // that hub FAILS OVER, and `--hub-nym` is the list of addresses it may be
        // at (D10: the current one and the just-rotated one, since a diskless hub
        // mints a new address on restart).
        //
        // Picking a single address per submit would be wrong here in a way that is
        // invisible: no ack is awaited, so a frame sent to the address that is
        // currently down is dropped by the driver while the wallet has ALREADY been
        // told success. With a two-address failover list that silently loses about
        // half of all migrations. The old ack-waiting path swept to the next
        // address on timeout and so recovered; dispatch-only cannot, and must
        // instead not choose.
        //
        // WHAT MAKES THE DUPLICATES SAFE, precisely. Each hub deduplicates its OWN
        // queue on the payload hash (D6), which collapses a resend to the same hub.
        // It does NOT deduplicate across hubs: the hub has no notion of being
        // active or standby, so any hub that RECEIVES a migration will queue and
        // broadcast it. Sending to every address is therefore safe only while the
        // other addresses are DEAD -- which is the failover model this list was
        // built for (D10): a diskless hub mints a new address when it restarts, so
        // the list is "the current address, and the one it just rotated away from",
        // and nothing is listening at the stale one.
        //
        // If two hubs were ever live at once, both would broadcast the same
        // transaction. On-chain that is harmless (the second is a known txid), but
        // it would publish the migration in two different batches at two different
        // moments, which is strictly worse for the batching this design exists to
        // provide, and it doubles the number of enclaves holding the plaintext.
        // Running a hot standby therefore needs an explicit passive mode in the
        // hub, which does not exist today.
        //
        // The cost is N frames of packets on a path that carries a migration
        // roughly 0.77 times per block, against a cover-traffic stream running
        // continuously anyway -- and, because nothing is awaited, no wallet latency.
        let deadline = tokio::time::Instant::now() + self.dispatch_timeout;
        let mut dispatched = 0usize;

        for target in 0..targets {
            // A FRESH nonce per address: two hubs answering the same nonce would be
            // indistinguishable to the correlator, and the ack is unread anyway.
            let nonce = fresh_nonce();
            // The one early return left, and not the trap the failure arms below
            // avoid: framing depends only on `tx_bytes` and a fixed-size nonce, so
            // it fails identically for every address or for none, and it can only
            // fire on the first pass with nothing dispatched yet. The wallet must
            // hear that as its own size failure rather than as unavailability.
            let frame = wire::encode_submit(&nonce, tx_bytes).map_err(NymError::Encode)?;
            let (ack_tx, _drop_receiver) = oneshot::channel();
            let request = Request {
                nonce,
                frame,
                reply_surbs: SUBMIT_REPLY_SURBS,
                waiter: Waiter::Ack(ack_tx),
                target,
            };
            match tokio::time::timeout_at(deadline, self.requests.send(request)).await {
                Ok(Ok(())) => dispatched += 1,
                // Two ways to be unable to reach the REMAINING addresses: the
                // transport loop is gone, or it stayed backpressured past the
                // shared budget. Both stop the sweep rather than hold the wallet
                // any longer, and neither says anything about the frames already
                // handed over, so both break and let the count below decide.
                //
                // The gone-transport case used to return here, which reported
                // failure for a migration that was ALREADY on the mixnet: with two
                // addresses, target 0 accepted and the transport closing before
                // target 1, the live hub still queues and broadcasts that frame
                // while the wallet is told the send failed and resends. The hub's
                // payload-hash dedup (D6) keeps that from being fatal, but the user
                // is shown a false failure for a transaction that is spent.
                Ok(Err(_)) | Err(_) => break,
            }
        }

        // One successful hand-off is enough: the migration is on the mixnet, bound
        // for at least one address the hub may be at. This is the ONLY verdict the
        // function reaches, which is what the failure arms above break for: a
        // hand-off that has already happened cannot be undone by whatever went
        // wrong on the address after it. Nothing dispatched is the one case that
        // fails closed, so the wallet retries a frame that never left.
        if dispatched > 0 {
            Ok(())
        } else {
            Err(NymError::TransportGone)
        }
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
    let mut rotate_at = next_rotation(&policy);
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
                    // Back off before reacting again, so a client that dies
                    // instantly on every rebuild is retried steadily rather than
                    // in a hot loop. Interruptible: a shutdown during the backoff
                    // must not have to wait it out.
                    tokio::select! {
                        _ = &mut shutdown => {
                            let _ = commands.send(ClientCommand::Disconnect).await;
                            return;
                        }
                        _ = tokio::time::sleep(policy.rebuild_backoff) => {}
                    }
                    // A rebuild is a fresh identity, so the linkage window
                    // starts over: the rotation clock restarts with it.
                    rotate_at = next_rotation(&policy);
                    defer_deadline = None;
                }
                // The driver is gone; there is no client to supervise.
                None => return,
            },
            _ = sleep_until_maybe(wake) => {
                let deadline = *defer_deadline
                    .get_or_insert_with(|| Instant::now() + policy.effective_defer_limit());
                let idle = inflight.load(Ordering::Relaxed) == 0;
                if !idle && Instant::now() < deadline {
                    // Something is still waiting for a reply its current SURBs
                    // would carry. Re-check shortly rather than strand it.
                    continue;
                }
                if !idle {
                    // Whatever is still waiting here started AFTER the rotation
                    // came due: the deferral covers a whole lookup budget, so
                    // anything in flight when it began has already run out its own
                    // clock. That is the residual the floor cannot remove without
                    // making a busy shim never rotate at all.
                    tracing::warn!(
                        "rotating the mixnet client with requests still in flight; \
                         they will fail closed and be retried"
                    );
                }
                tracing::info!("rotating the mixnet client's identity");
                if commands.send(ClientCommand::Rebuild).await.is_err() {
                    return;
                }
                rotate_at = next_rotation(&policy);
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
mod throughput_budget {
    //! Does a request still FIT its timeout at the send rate the public mixnet
    //! actually gives us?
    //!
    //! This exists because the localnet said yes while production said no. A
    //! loopback mixnet with a single tenant never backpressures its client, so
    //! `SendingDelayController` stays at multiplier 1 and `e2e-driver` measured a
    //! 1.3 s lookup — while the same code against a shared public gateway pegged
    //! at multiplier 6 and could not answer inside 25 s (measured 2026-08-14, 14
    //! consecutive failures across two independently deployed pairs). The
    //! end-to-end test was green about a latency the network cannot deliver.
    //!
    //! So the budget is asserted from the constants instead, where it is
    //! deterministic and needs no mixnet: packets on the wire are a function of
    //! frame size and attached-SURB count (D4), and time is packets over the
    //! throttled rate. A change to `FRAME_BYTES`, either SURB count, or
    //! `REQUEST_TIMEOUT` that breaks the budget fails HERE, at review time,
    //! instead of silently in production.
    use super::*;

    /// Sphinx payload per packet, approximately. Nym's regular packet carries
    /// ~2 KB; used only to turn frame bytes into a packet count.
    const PACKET_BYTES: usize = 2 * 1024;

    /// The client's own floor on sending, `MAX_DELAY_MULTIPLIER` (6) times the
    /// 20 ms default `message_sending_average_delay`. This is the rate a real
    /// gateway's backpressure drives us to, and the one the localnet never sees.
    const THROTTLED_PACKETS_PER_SEC: f64 = 1000.0 / 120.0;

    fn packets(bytes: usize) -> usize {
        bytes.div_ceil(PACKET_BYTES)
    }

    fn seconds_to_emit(packets: usize) -> f64 {
        packets as f64 / THROTTLED_PACKETS_PER_SEC
    }

    #[test]
    fn a_submit_fits_its_dispatch_budget_at_the_throttled_rate() {
        // Dispatch-only submit only has to hand the frame to the transport, so
        // this is really a sanity check that a full frame plus its SURBs is not
        // absurd at the throttled rate.
        let on_wire = packets(wire::FRAME_BYTES) + SUBMIT_REPLY_SURBS as usize;
        let secs = seconds_to_emit(on_wire);
        assert!(
            secs < 30.0,
            "a submit is {on_wire} packets = {secs:.1}s to emit at the throttled rate"
        );
    }

    /// The one that matters: the lookup that was failing in production.
    #[test]
    fn a_lookup_round_trip_fits_the_request_timeout_with_margin() {
        // Out: a tiny request carrying the reply SURBs. Back: a FULL frame, since
        // the reply carries a transaction padded to hide its length.
        let out = packets(wire::LOOKUP_BYTES) + LOOKUP_REPLY_SURBS as usize;
        let back = packets(wire::FRAME_BYTES);
        let secs = seconds_to_emit(out + back);
        let budget = REQUEST_TIMEOUT.as_secs_f64();

        // At 25 s this assertion was inverted: the emission alone (~12.6 s) left
        // nothing for mix delay or gateway queueing, and production answered
        // UNAVAILABLE 14 times running. 90 s leaves roughly 6x the emission cost
        // as headroom for the queueing we cannot compute from constants.
        assert!(
            secs * 3.0 < budget,
            "a lookup is {} packets = {secs:.1}s of pure emission at the throttled rate; \
             REQUEST_TIMEOUT is {budget:.0}s, which leaves under 3x headroom for mix delay \
             and gateway queueing. Raise the timeout, or cut packets (frame size / SURBs).",
            out + back
        );
    }

    #[test]
    fn the_localnet_rate_is_what_makes_the_localnet_pass() {
        // Same packets, unthrottled: comfortably inside the budget, which is
        // exactly why e2e-driver is green and production is not. Documents the
        // discrepancy rather than leaving it to be rediscovered.
        let on_wire =
            packets(wire::LOOKUP_BYTES) + LOOKUP_REPLY_SURBS as usize + packets(wire::FRAME_BYTES);
        let unthrottled = on_wire as f64 / (1000.0 / 20.0);
        assert!(
            unthrottled < REQUEST_TIMEOUT.as_secs_f64(),
            "unthrottled the same {on_wire} packets take {unthrottled:.1}s"
        );
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

    /// A best-effort submit must stay BOUNDED under backpressure, not hang. A full
    /// requests channel (the transport draining slower than the wallet submits)
    /// blocks the hand-off; submit must fail closed within the dispatch budget so
    /// the wallet retries, rather than block on a hand-off that is not happening.
    #[tokio::test]
    async fn a_backpressured_submit_fails_closed_within_the_dispatch_budget() {
        let (tx, _rx) = mpsc::channel::<Request>(1);
        // Hold the one slot so the handle's send has nowhere to go, and keep the
        // receiver alive so the channel is full-but-open rather than closed.
        let _permit = tx.reserve().await.expect("channel open");
        let targets = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1));
        // A short dispatch budget keeps the test fast; the round-trip timeout is
        // irrelevant to submit, which never awaits a reply.
        let handle = NymHandle::new(
            tx.clone(),
            Duration::from_secs(25),
            Duration::from_millis(150),
            targets,
        );

        let started = std::time::Instant::now();
        let result = handle.submit(&[0u8; 8]).await;

        assert_eq!(
            result,
            Err(NymError::TransportGone),
            "a frame that cannot be handed off is unsendable, so the wallet retries"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the dispatch wait must be bounded by the dispatch budget"
        );
    }

    /// Sending to every address means the sweep can fail PART WAY, and a frame
    /// already accepted is already on its way to a hub that will queue and
    /// broadcast it. Reporting failure for it tells the wallet to resend a
    /// transaction that is spent, which is a false failure the user sees even
    /// though the hub's dedup keeps it from being fatal.
    #[tokio::test(start_paused = true)]
    async fn a_submit_that_reached_one_address_before_the_transport_died_is_a_success() {
        // Capacity one is what makes the interleaving deterministic rather than a
        // race: target 0's frame takes the only slot and is dispatched, target 1
        // then waits for capacity that nobody frees, and the transport goes away
        // underneath it. Time is paused, so the wait costs nothing real.
        let (tx, rx) = mpsc::channel::<Request>(1);
        let targets = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(2));
        let handle = NymHandle::new(tx, Duration::from_secs(25), Duration::from_secs(5), targets);

        let transport = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(rx);
        });

        let result = handle.submit(&[0u8; 8]).await;
        transport.await.unwrap();

        assert_eq!(
            result,
            Ok(()),
            "one hand-off is enough; the migration is already on the mixnet"
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
        out_rx
            .recv()
            .await
            .expect("the frame is emitted to the driver");

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

    /// L2: a zero (or sub-backoff) rotation period must not hot-loop the
    /// supervisor. Before the floor, `rotate_at` reset to `now` and re-fired
    /// immediately, filling the command channel with Rebuilds; the floor holds
    /// rotations to at most one per rebuild_backoff.
    #[tokio::test]
    async fn a_zero_rotation_period_does_not_hot_loop() {
        let policy = RotationPolicy {
            period: Some(Duration::ZERO),
            defer_limit: Duration::from_secs(60),
            rebuild_backoff: Duration::from_millis(100),
        };
        let (_events_tx, events_rx) = mpsc::channel::<ClientEvent>(4);
        let (commands_tx, mut commands_rx) = mpsc::channel::<ClientCommand>(64);
        let inflight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(run_supervisor(
            policy,
            events_rx,
            commands_tx,
            inflight,
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        // A window well under the 100 ms backoff. A hot loop fills the 64-slot
        // command channel before it elapses; the floor emits none in this window.
        tokio::time::sleep(Duration::from_millis(30)).await;

        let mut rebuilds = 0;
        while commands_rx.try_recv().is_ok() {
            rebuilds += 1;
        }
        assert!(
            rebuilds <= 1,
            "a zero rotation period must not hot-loop; got {rebuilds} rebuilds"
        );
    }

    /// The floor that keeps a rotation from destroying the client an in-flight
    /// lookup's reply is addressed to. Asserted against the constant rather than
    /// against a number, so raising [`REQUEST_TIMEOUT`] carries the floor with it
    /// instead of re-opening the window the way 25 s to 90 s did.
    #[test]
    fn a_due_rotation_never_defers_for_less_than_the_lookup_budget() {
        let impatient = RotationPolicy {
            period: Some(Duration::from_secs(3600)),
            defer_limit: Duration::from_secs(1),
            rebuild_backoff: Duration::from_secs(5),
        };
        assert_eq!(impatient.effective_defer_limit(), REQUEST_TIMEOUT);
        assert!(RotationPolicy::never().effective_defer_limit() >= REQUEST_TIMEOUT);
        assert!(
            RotationPolicy::every(Duration::from_secs(3600)).effective_defer_limit()
                >= REQUEST_TIMEOUT
        );

        // A floor, not a value: a policy asking for more patience keeps it.
        let patient = RotationPolicy {
            defer_limit: REQUEST_TIMEOUT + Duration::from_secs(30),
            ..impatient
        };
        assert_eq!(
            patient.effective_defer_limit(),
            REQUEST_TIMEOUT + Duration::from_secs(30)
        );
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
