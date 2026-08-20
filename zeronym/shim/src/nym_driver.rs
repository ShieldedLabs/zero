//! The mixnet driver: the one place in the shim that owns a `nym-sdk` client.
//!
//! Everything else in the outbound path ([`crate::nym`]) is SDK-free and speaks
//! only in channels (D5): [`OutFrame`]s to put on the mixnet, raw bytes back,
//! [`ClientCommand`]s in, [`ClientEvent`]s out. This module is the other end of
//! those channels and nothing more. It owns one client, moves bytes across it,
//! and obeys the supervisor:
//!
//!   * an [`OutFrame`] is sent to the hub address named by its `target` INDEX
//!     (D10 multi-homing: the transport never learns what a Nym address is),
//!     with the fixed reply-SURB count the frame already carries (D3/D4), as an
//!     ANONYMOUS send so the hub sees a single-use sender tag and never the
//!     shim's own address (D3);
//!   * every inbound reconstructed message is handed back as raw bytes, except
//!     the empty ones the SDK emits to replenish SURBs (D12), which are dropped
//!     here exactly as the correlator would drop them;
//!   * [`ClientCommand::Rebuild`] disconnects the current identity and builds a
//!     fresh one (a new gateway registration, hence a fresh sender tag: the one
//!     lever that bounds hub-side linkage, D11); [`ClientCommand::Disconnect`]
//!     shuts the client down cleanly and stops;
//!   * when the SDK gives up on its gateway (D12: 20 send failures and it stops
//!     for good, no reconnect), `wait_for_messages` yields `None`; the driver
//!     reports [`ClientEvent::Died`] and waits to be rebuilt.
//!
//! The client lifecycle is why `disconnect()` is a command rather than a drop:
//! it is not cancel-safe and a dropped LIVE client leaks its background tasks
//! (D12), so a clean rotation must run it to completion. A client that has
//! already DIED has stopped those tasks on its own, so it is dropped rather than
//! disconnected.

#![cfg(feature = "mixnet-driver")]

use tokio::sync::mpsc;
use zeroize::Zeroizing;

use nym_sdk::mixnet::{
    IncludedSurbs, MixnetClient, MixnetClientBuilder, MixnetMessageSender, Recipient,
};

use crate::nym::{ClientCommand, ClientEvent, MixnetStatus, OutFrame, TargetCount};

/// Which Nym network the driver connects to.
///
/// A plain-data choice, not a trait: production is the default network baked
/// into the SDK; the localnet variant (compiled only with `mixnet-localnet`)
/// points the same driver at the mixnet the nymnet harness starts, so the
/// shipped driver is what the end-to-end test exercises rather than a stand-in.
pub enum MixnetNetwork {
    /// The default network the SDK ships with (mainnet). Production.
    Default,
    /// A hardcoded topology loaded from a file: the local mixnet started by
    /// `nymnet/localnet.sh`, for end-to-end tests.
    #[cfg(feature = "mixnet-localnet")]
    TopologyFile(std::path::PathBuf),
}

/// Parse one operator-configured hub Nym address into the SDK recipient the
/// driver sends to. A malformed address is a configuration error the operator
/// must fix, surfaced at startup rather than swallowed into a silent fail-closed.
pub fn parse_address(addr: &str) -> Result<Recipient, String> {
    addr.parse::<Recipient>()
        .map_err(|err| format!("invalid hub Nym address {addr:?}: {err}"))
}

/// A rotating chooser over the operator-configured entry gateways.
///
/// Empty means the SDK picks a gateway at random, the original behaviour. A
/// non-empty list pins the ENTRY gateway to one of them and advances on every
/// build, so a gateway that dies OR backpressures is escaped on the next rebuild.
/// The latter is why this is the throughput lever and not just resilience: the
/// client's send rate is capped by its gateway's backpressure
/// (`SendingDelayController`), so landing on a healthier gateway is what lifts the
/// ceiling. Rotation is free here (D11): a rebuild already mints a fresh identity,
/// so changing gateway costs no extra linkability.
struct GatewaySelector {
    gateways: Vec<String>,
    next: usize,
}

impl GatewaySelector {
    fn new(gateways: Vec<String>) -> Self {
        GatewaySelector { gateways, next: 0 }
    }

    /// The gateway IDENTITY to pin for the next build, advancing the rotation.
    /// `None` when none are configured, meaning "let the SDK choose".
    fn take(&mut self) -> Option<String> {
        if self.gateways.is_empty() {
            return None;
        }
        let gateway = self.gateways[self.next % self.gateways.len()].clone();
        self.next = self.next.wrapping_add(1);
        Some(gateway)
    }
}

/// Build (or rebuild) a mixnet client. Ephemeral by construction (D11): a fresh
/// build is a fresh identity, a fresh gateway registration, and therefore a
/// fresh `AnonymousSenderTag`, which is the only lever that bounds how long a
/// hub can link one shim's submissions.
///
/// `gateway`, when set, pins the entry gateway by identity key
/// (`MixnetClientBuilder::request_gateway`); `None` lets the SDK pick at random.
async fn build_client(
    network: &MixnetNetwork,
    gateway: Option<String>,
) -> Result<MixnetClient, String> {
    let builder = MixnetClientBuilder::new_ephemeral();
    let builder = match gateway {
        Some(gateway) => builder.request_gateway(gateway),
        None => builder,
    };
    // LOCALNET FIDELITY ONLY. A loopback mixnet with one tenant never gives the
    // client any backpressure, so its `SendingDelayController` sits at multiplier
    // 1 (~50 packets/s) while a real shared gateway pegs it at MAX_DELAY_MULTIPLIER
    // = 6 (~8 packets/s). That 6x is the entire difference between a localnet
    // lookup answering in ~1.3 s and a production one exceeding the 25 s budget,
    // so a localnet run at the default rate certifies a latency the public network
    // cannot deliver. `ZIS_LOCALNET_SEND_DELAY_MS` lets the harness emulate the
    // throttled rate and measure against it.
    //
    // Gated on `mixnet-localnet` so a PRODUCTION binary cannot read it at all: a
    // non-default send rate would make this client's traffic distinguishable from
    // every other Nym client, which is a fingerprint, and the shaping is what
    // hides the divert in the first place.
    #[cfg(feature = "mixnet-localnet")]
    let builder = match std::env::var("ZIS_LOCALNET_SEND_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(ms) => {
            let mut debug = nym_sdk::DebugConfig::default();
            debug.traffic.message_sending_average_delay = std::time::Duration::from_millis(ms);
            tracing::warn!(
                send_delay_ms = ms,
                "LOCALNET: emulating a throttled send rate; this is a test knob, never production"
            );
            builder.debug_config(debug)
        }
        None => builder,
    };
    let builder = match network {
        MixnetNetwork::Default => builder,
        #[cfg(feature = "mixnet-localnet")]
        MixnetNetwork::TopologyFile(path) => {
            let provider = nym_topology::HardcodedTopologyProvider::new_from_file(path)
                .map_err(|err| format!("loading topology {}: {err}", path.display()))?;
            builder.custom_topology_provider(Box::new(provider))
        }
    };
    builder
        .build()
        .map_err(|err| format!("building the mixnet client: {err}"))?
        .connect_to_mixnet()
        .await
        .map_err(|err| format!("connecting to the mixnet: {err}"))
}

/// Own the mixnet client and move bytes across it until told to stop.
///
/// The channel ends mirror [`crate::nym::run_transport`] and
/// [`crate::nym::run_supervisor`] exactly: `out_frames`/`inbound` are the driver
/// side of the transport's `to_mixnet`/`from_mixnet`, and `commands`/`events`
/// the driver side of the supervisor's `commands`/`events`. `hub_addresses` is
/// the list `target` indexes into (D10); `targets` publishes its length to the
/// handle so a caller never indexes an empty list.
pub async fn run_driver(
    network: MixnetNetwork,
    gateways: Vec<String>,
    hub_addresses: Vec<Recipient>,
    targets: TargetCount,
    status: MixnetStatus,
    mut out_frames: mpsc::Receiver<OutFrame>,
    inbound: mpsc::Sender<Zeroizing<Vec<u8>>>,
    mut commands: mpsc::Receiver<ClientCommand>,
    events: mpsc::Sender<ClientEvent>,
) {
    use std::sync::atomic::Ordering;

    targets.store(hub_addresses.len(), Ordering::Relaxed);
    // Shared into each in-flight send below rather than borrowed by it: a send is
    // not awaited inside the select arm that starts it, so it outlives that arm and
    // must own everything it reads.
    let hub_addresses = std::sync::Arc::new(hub_addresses);
    // Published before the first build attempt, so a shim that NEVER connects
    // still reports "configured but not connected" rather than looking
    // forward-only, which is the case an operator most needs to see.
    status.set_configured();

    // Rotates the pinned entry gateway across (re)builds; empty = the SDK picks.
    let mut gateways = GatewaySelector::new(gateways);

    // The first client is the driver's own; the supervisor only ever asks for a
    // REbuild (rotation, or recovery after a reported death). A failed initial
    // connect enters the same wait-to-be-rebuilt path as a mid-run failure.
    let mut client = match build_client(&network, gateways.take()).await {
        Ok(client) => {
            status.set_connected();
            // The `@gateway` half names the entry gateway the SDK actually
            // picked, which an attested enclave has no console to reveal.
            status.set_address(client.nym_address().to_string());
            client
        }
        Err(err) => {
            tracing::error!(error = %err, "initial mixnet connect failed; awaiting rebuild");
            status.set_rebuild_failed();
            let _ = events.send(ClientEvent::Died).await;
            match build_when_told(&mut commands, &network, &events, &mut gateways, &status).await {
                Some(client) => client,
                None => return,
            }
        }
    };
    // An owned, independent sender split off the client, so the send arm below
    // touches `sender` while the receive arm touches `client`: two disjoint
    // borrows in one `select!`, no dance between `&self` send and `&mut self`
    // receive. Re-split on every rebuild, since the old sender points at the
    // client that just went away.
    let mut sender = client.split_sender();

    // INBOUND LIVENESS. A client can register with a gateway, report itself
    // connected, send successfully — and never receive a single inbound message
    // for its whole life. Measured 2026-08-14: of four deployed shims on
    // identical config, two answered lookups and two never did, one of them
    // broken three minutes after boot and still broken hours later. Nothing
    // recovered them, because the SDK only reports a death when it gives up on
    // its gateway, and a gateway that accepts sends is never given up on. So
    // `client_deaths` stayed 0, no rebuild was ever requested, and the shim sat
    // there healthy-looking and useless. On an immutable enclave that is
    // terminal: no restart, no in-place update, a 25-minute redeploy to recover.
    //
    // The probe is a message to our OWN address. That exercises the half that
    // fails — gateway delivering INTO this client — without depending on the hub
    // being up, so a hub outage cannot drive an endless rebuild loop. Its payload
    // is empty on purpose: the receive arm already filters empty messages out as
    // SURB-replenishment artifacts, so the probe is counted for liveness and
    // never reaches the correlator to be puzzled over.
    //
    // Which half of the unlucky draw is at fault — the gateway, or this client's
    // registration with it — is still unknown, and deliberately does not matter:
    // a rebuild rerolls BOTH.
    let mut own = *client.nym_address();
    let mut probe = tokio::time::interval(PROBE_INTERVAL);
    // The first tick is immediate; that is wanted, since a bad draw is bad from
    // boot and the point is to catch it before any wallet does. Delay, rather
    // than Burst, so a stalled loop does not fire a backlog of probes at once.
    probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Inbound count when the outstanding probe was sent, and how many probe
    // rounds in a row have seen no inbound traffic at all.
    let mut inbound_at_probe: Option<u64> = None;
    let mut silent_rounds: u32 = 0;

    // THE ONE SEND IN FLIGHT, driven by its own arm instead of awaited inside the
    // arm that starts it.
    //
    // Handing a message to the SDK must never be an awaited step inside a select
    // arm. The SDK's input channel holds a single `InputMessage` and is drained at
    // the client's throttled Poisson send rate, so the hand-off blocks for as long
    // as that rate dictates: seconds, under the backpressure a real shared gateway
    // applies. For every one of those seconds an inline await would stop this loop
    // polling `wait_for_messages`, leaving reply SURBs already waiting at the
    // gateway unread until their round trip had timed out. That is the same hazard
    // `correlate` in `crate::nym` documents one channel upstream and avoids with
    // `reserve()`; here the send is held as a pinned future and driven by its own
    // arm, so the receive arm keeps running for the whole of it.
    //
    // Backpressure comes from the guards rather than from an await: neither a frame
    // nor a probe is started while one send is still outstanding, so the pressure
    // lands on `out_frames` and therefore on the transport's `reserve()`, which is
    // where the caller is meant to feel it. Nothing is spawned, so there is no
    // second queue growing out of sight.
    let mut in_flight: Option<InFlight> = None;

    /// Report what a teardown is about to abandon, as COUNTS ONLY.
    ///
    /// None of these paths can save the frames: there is no drain-then-disconnect
    /// in the SDK, so `disconnect()` discards its one-slot input, its 8-deep batch
    /// channel and its unbounded transmission buffer, and some of those frames are
    /// SUBMITS ALREADY ANSWERED SUCCESS to a wallet. What was missing was any
    /// record that it happened: every teardown was silent, so a migration
    /// acknowledged and then destroyed left no trace at all (Hornby review,
    /// 2026-08-19).
    ///
    /// Counts only, never a txid, never a body, never a per-entry identifier. In
    /// an enclave this output reaches the parent host, which is exactly who the
    /// system withholds those from. A count says "some were lost"; it cannot say
    /// which.
    macro_rules! report_abandoned {
        ($why:expr, $queued:expr, $sending:expr) => {{
            let queued = $queued;
            let sending = $sending;
            if queued > 0 || sending {
                tracing::warn!(
                    reason = $why,
                    queued_frames = queued,
                    send_in_flight = sending,
                    "mixnet client torn down with work outstanding; these frames are \
                     discarded and MAY INCLUDE SUBMITS ALREADY ANSWERED SUCCESS to a wallet"
                );
            } else {
                tracing::debug!(reason = $why, "mixnet client torn down cleanly");
            }
        }};
    }

    // The select decides WHAT happened; the client lifecycle (which consumes the
    // client to disconnect, or replaces it on rebuild) is handled AFTER the
    // select, where none of its futures still borrow the client.
    loop {
        let step = tokio::select! {
            // Inbound liveness. Cheap to keep in the select: the tick itself does
            // no I/O, and the probe send is a single empty message. Guarded on
            // nothing being in flight because the probe IS a send and the SDK takes
            // them one at a time; a tick that falls during a send is served on the
            // next turn, which is what MissedTickBehavior::Delay is already set for.
            _ = probe.tick(), if in_flight.is_none() => {
                let seen = status.inbound_total();
                match inbound_at_probe {
                    // A probe was outstanding and nothing at all has arrived since.
                    //
                    // But "since" only means anything if the probe actually LEFT.
                    // The mark is stamped when the SDK accepts the probe into its
                    // one-slot input channel, not when it is emitted, and behind
                    // that slot sit an 8-deep batch channel and an unbounded FIFO
                    // drained at the throttled rate. Under a send backlog the probe
                    // is still queued behind every frame ahead of it, nothing has
                    // been asked of the gateway yet, and "silent" is a statement
                    // about OUR queue, not about delivery. Rebuilding on it would
                    // disconnect a healthy client and discard that whole queue --
                    // including submits already answered success to a wallet,
                    // which the supervisor's inflight count cannot protect because
                    // a submit's waiter is swept the moment it is dispatched. So a
                    // backlog defers the verdict: the round is not counted, and
                    // the next probe re-asks once the queue has drained.
                    // `out_frames.len()` is the backlog we can see: this arm only
                    // runs when `in_flight` is None, so anything still queued to us
                    // has not even reached the SDK yet, let alone the gateway. The
                    // SDK's own internal buffer is not inspectable, but once we
                    // stop feeding it it can only drain, and two silent rounds is
                    // 120 s of drain at the throttled rate -- far more than any
                    // residual it could be holding. So "our queue is empty and two
                    // rounds passed" is a sound proxy for "the probe was emitted
                    // and nothing came back".
                    Some(mark) if seen == mark && out_frames.len() == 0 => {
                        silent_rounds += 1;
                        if silent_rounds >= SILENT_ROUNDS_BEFORE_REBUILD {
                            tracing::error!(
                                silent_rounds,
                                gateway = %own.gateway(),
                                "no inbound mixnet traffic across consecutive probes with the \
                                 send queue idle; the client registered but is not being \
                                 delivered to. Reporting it as a death so the supervisor \
                                 rebuilds with its backoff and rotation clock, rather than \
                                 disconnecting inline."
                            );
                            silent_rounds = 0;
                            inbound_at_probe = None;
                            // Through the supervisor, not `Step::Rebuild` inline. The
                            // supervisor owns rebuild backoff (so a gateway that is
                            // silent on every draw is retried steadily, not in a
                            // hot loop) and restarts the rotation clock; the inline
                            // path bypassed both. Its Died handler sends Rebuild
                            // back, which is the same rebuild this used to do.
                            let _ = events.send(ClientEvent::Died).await;
                            Step::Ferried
                        } else {
                            tracing::warn!(
                                silent_rounds,
                                "no inbound mixnet traffic since the last probe; watching"
                            );
                            in_flight = Some(probe_send(sender.clone(), own));
                            Step::Ferried
                        }
                    }
                    // Either the first round, or traffic HAS arrived since the
                    // last probe — which is all the liveness we need, whether it
                    // came from the probe or from real wallet lookups.
                    _ => {
                        silent_rounds = 0;
                        in_flight = Some(probe_send(sender.clone(), own));
                        Step::Ferried
                    }
                }
            },
            command = commands.recv() => match command {
                Some(ClientCommand::Rebuild) => Step::Rebuild,
                // A dropped commands channel is the supervisor gone: nothing left
                // to obey, so shut the client down cleanly like an explicit stop.
                Some(ClientCommand::Disconnect) | None => Step::Stop,
            },
            // Guarded so a frame is only taken once the previous one has been
            // accepted by the SDK: one in flight at a time, the rest left in
            // `out_frames` where the transport can see the queue and slow down.
            frame = out_frames.recv(), if in_flight.is_none() => match frame {
                Some(out) => {
                    in_flight = Some(frame_send(sender.clone(), hub_addresses.clone(), out));
                    Step::Ferried
                }
                // The transport loop is gone; there is nothing to carry.
                None => Step::Stop,
            },
            // The outstanding send, if there is one. Losing a turn here to inbound
            // traffic costs nothing: only `drive`'s own future is dropped, never the
            // boxed send behind the `&mut`, so it resumes from where it stopped.
            sent = drive(&mut in_flight), if in_flight.is_some() => {
                in_flight = None;
                match sent {
                    // Counted at acceptance by the SDK, not at the moment the frame
                    // was taken off the channel: the count means "handed to the
                    // mixnet", and only this future completing says that.
                    Sent::Frame => status.record_send(),
                    // The mark means "inbound seen as of the probe going out", so it
                    // is read here and not when the probe was queued.
                    Sent::Probe => inbound_at_probe = Some(status.inbound_total()),
                }
                Step::Ferried
            },
            messages = client.wait_for_messages() => match messages {
                Some(messages) => {
                    for message in messages {
                        // Counted BEFORE the empty-filter below, so the diagnostic
                        // can tell "no inbound traffic at all" from "inbound
                        // traffic, but never a reply frame" — the distinction the
                        // enclave lookup failure turns on.
                        status.record_inbound(message.message.is_empty());
                        // Empty inbound messages are SURB-replenishment artifacts,
                        // not replies (D12); the correlator would drop them anyway,
                        // but keeping them out of the channel keeps it for frames.
                        // Zeroizing on the way in: a LookupReplyV1 carries a
                        // transaction in cleartext.
                        if !message.message.is_empty() {
                            let _ = inbound.send(Zeroizing::new(message.message)).await;
                        }
                    }
                    Step::Ferried
                }
                // The SDK has given up on its gateway for good (D12).
                None => Step::Died,
            },
        };

        match step {
            Step::Ferried => {}
            Step::Stop => {
                report_abandoned!("stop", out_frames.len(), in_flight.is_some());
                client.disconnect().await;
                return;
            }
            Step::Rebuild => {
                // Any outstanding send belongs to the client about to go away, so
                // it goes with it. Nothing is left half-written by that: the SDK's
                // send is cancel-safe (the message is either fully queued or not
                // sent at all).
                //
                // But be clear about what disconnect() DOES discard: everything the
                // SDK still holds internally -- its one-slot input, an 8-deep batch
                // channel, and an unbounded transmission buffer drained at the
                // throttled rate. There is no drain-then-disconnect in the SDK.
                // Frames in there may include SUBMITS ALREADY ANSWERED SUCCESS to a
                // wallet, and nothing upstream can protect them: a submit's waiter
                // is swept the moment it is dispatched, so the supervisor's
                // inflight count never sees it. That is why the liveness probe
                // above refuses to call for a rebuild while our own queue is
                // non-empty, and why a scheduled rotation is deferred while
                // requests are in flight. The residual exposure is a rebuild that
                // arrives anyway -- the SDK's own death, or a rotation whose
                // deferral ran out -- while its buffer is non-empty; that window
                // is real, bounded by the drain rate, and recorded in
                // PRODUCTION.md rather than papered over here.
                report_abandoned!("rebuild", out_frames.len(), in_flight.is_some());
                in_flight = None;
                // A live rotation: disconnect the current identity to completion
                // (D12: disconnect is not cancel-safe), then mint a fresh one.
                client.disconnect().await;
                status.set_died();
                client = match build_client(&network, gateways.take()).await {
                    Ok(client) => {
                        status.set_connected();
                        client
                    }
                    Err(err) => {
                        tracing::error!(error = %err, "rebuild failed; awaiting retry");
                        status.set_rebuild_failed();
                        let _ = events.send(ClientEvent::Died).await;
                        match build_when_told(
                            &mut commands,
                            &network,
                            &events,
                            &mut gateways,
                            &status,
                        )
                        .await
                        {
                            Some(client) => client,
                            None => return,
                        }
                    }
                };
                sender = client.split_sender();
                // A rebuild mints a new identity at a possibly different
                // gateway, so the probe target moves with it. Reset the probe
                // state too: the old mark belongs to a client that no longer
                // exists, and carrying it over would charge the fresh client
                // with the dead one's silence.
                own = *client.nym_address();
                status.set_address(own.to_string());
                inbound_at_probe = None;
                silent_rounds = 0;
            }
            Step::Died => {
                report_abandoned!("died", out_frames.len(), in_flight.is_some());
                // Dropped with the client it was addressed to, for the same reason
                // as the rebuild path above.
                in_flight = None;
                // The dead client's tasks have already stopped, so it is dropped
                // (by the reassignment below), not disconnected. Report and wait
                // for the supervisor to ask for a rebuild.
                status.set_died();
                let _ = events.send(ClientEvent::Died).await;
                client =
                    match build_when_told(&mut commands, &network, &events, &mut gateways, &status)
                        .await
                    {
                        Some(client) => client,
                        None => return,
                    };
                sender = client.split_sender();
                // A rebuild mints a new identity at a possibly different
                // gateway, so the probe target moves with it. Reset the probe
                // state too: the old mark belongs to a client that no longer
                // exists, and carrying it over would charge the fresh client
                // with the dead one's silence.
                own = *client.nym_address();
                status.set_address(own.to_string());
                inbound_at_probe = None;
                silent_rounds = 0;
            }
        }
    }
}

/// What one turn of the driver loop resolved to. Kept out of the `select!` so the
/// client can be consumed (disconnect) or replaced (rebuild) once no arm future
/// still borrows it.
enum Step {
    /// Bytes moved in one direction or the other; carry on.
    Ferried,
    /// Rotate the client's identity.
    Rebuild,
    /// The client died; report it and wait to be rebuilt.
    Died,
    /// Shut down cleanly and stop.
    Stop,
}

/// One message handed to the SDK and not yet accepted by it.
///
/// Boxed and pinned so the driver can hold it across select turns and poll it
/// from a dedicated arm; owned rather than borrowing the sender, so that a
/// rebuild can replace the sender without the outstanding send pinning a borrow
/// on it.
type InFlight = std::pin::Pin<Box<dyn std::future::Future<Output = Sent> + Send>>;

/// Which send just completed, so the loop can run the bookkeeping that belongs
/// after a send even though the send no longer finishes inside the arm that
/// started it.
enum Sent {
    /// An outbound frame for a hub.
    Frame,
    /// An inbound-liveness probe to our own address.
    Probe,
}

/// Poll the outstanding send, if there is one.
///
/// Taking the `Option` by reference is what makes this arm cancel-safe: when
/// another arm wins the turn, only this future is dropped and the send behind the
/// reference is untouched. The `None` case parks forever rather than returning,
/// since a select arm that resolved instantly would spin the loop; in practice it
/// is unreachable behind the arm's `is_some()` guard.
async fn drive(in_flight: &mut Option<InFlight>) -> Sent {
    match in_flight {
        Some(send) => send.await,
        None => std::future::pending::<Sent>().await,
    }
}

/// The in-flight future for one outbound frame.
///
/// It owns its sender and address list because it outlives the select turn that
/// started it; the sender is a cheap handle over the client's input channel, so
/// cloning one per send costs nothing.
fn frame_send(
    sender: nym_sdk::mixnet::MixnetClientSender,
    hub_addresses: std::sync::Arc<Vec<Recipient>>,
    out: OutFrame,
) -> InFlight {
    Box::pin(async move {
        send_frame(&sender, &hub_addresses, out).await;
        Sent::Frame
    })
}

/// The in-flight future for one liveness probe, owned for the same reason as
/// [`frame_send`]'s.
fn probe_send(sender: nym_sdk::mixnet::MixnetClientSender, own: Recipient) -> InFlight {
    Box::pin(async move {
        send_probe(&sender, own).await;
        Sent::Probe
    })
}

/// How often to check that inbound traffic is still arriving.
///
/// Not tuned aggressively: a rebuild costs a fresh gateway registration and a new
/// sender tag, so reacting to one quiet minute would trade a rare permanent
/// failure for frequent self-inflicted churn. Two rounds of silence is ~2 minutes
/// to self-heal, against a failure whose current alternative is a 25-minute
/// redeploy by a human who has not noticed yet.
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Consecutive probe rounds with NO inbound traffic before the client is torn
/// down and rebuilt. Two, not one, so a single dropped probe cannot trigger a
/// rebuild on an otherwise healthy client.
const SILENT_ROUNDS_BEFORE_REBUILD: u32 = 2;

/// A liveness probe: an empty message to our own address.
///
/// Empty because the receive arm already discards empty inbound messages as
/// SURB-replenishment artifacts, so this is counted for liveness and never
/// reaches the correlator. Zero attached SURBs: nothing has to reply to it, the
/// arrival is the whole signal.
async fn send_probe(sender: &nym_sdk::mixnet::MixnetClientSender, own: Recipient) {
    if let Err(err) = sender
        .send_message(own, Vec::new(), IncludedSurbs::new(0))
        .await
    {
        // Not fatal on its own: the next round either sees inbound traffic or
        // counts another silent round and rebuilds.
        tracing::warn!(error = %err, "inbound liveness probe could not be sent");
    }
}

/// Send one outbound frame to the hub address its `target` names, anonymously,
/// with the reply-SURB count the frame carries.
///
/// A send failure is logged and dropped, not retried here: the SDK's own
/// auto-reconnect covers a transient gateway blip. For a LOOKUP the caller then
/// fails closed on its own timeout; for a best-effort SUBMIT the wallet has
/// ALREADY been answered success on dispatch, so a dropped submit frame is
/// unrecoverable at this layer and the wallet only learns of it via
/// no-confirmation (a resend is safe, D6 dedup). An out-of-range index is a
/// transport/driver disagreement about the address list and is logged loudly.
async fn send_frame(
    sender: &nym_sdk::mixnet::MixnetClientSender,
    hub_addresses: &[Recipient],
    out: OutFrame,
) {
    let Some(recipient) = hub_addresses.get(out.target).copied() else {
        tracing::error!(
            index = out.target,
            "no hub address at that index; dropping frame"
        );
        return;
    };
    if let Err(err) = sender
        .send_message(
            recipient,
            out.frame.to_vec(),
            IncludedSurbs::new(out.reply_surbs),
        )
        .await
    {
        tracing::warn!(error = %err, "mixnet send failed; the caller will fail closed on timeout");
    }
}

/// Wait for the supervisor to ask for a rebuild, then return the fresh client.
///
/// Entered whenever there is no live client: after a reported death, or after a
/// build itself failed. Each failed build reports [`ClientEvent::Died`] again so
/// the supervisor keeps pacing the retries with its backoff rather than this
/// spinning. Returns `None` when told to disconnect (or the supervisor is gone),
/// which is the driver's cue to stop.
async fn build_when_told(
    commands: &mut mpsc::Receiver<ClientCommand>,
    network: &MixnetNetwork,
    events: &mpsc::Sender<ClientEvent>,
    gateways: &mut GatewaySelector,
    status: &MixnetStatus,
) -> Option<MixnetClient> {
    loop {
        match commands.recv().await {
            Some(ClientCommand::Rebuild) => match build_client(network, gateways.take()).await {
                Ok(client) => {
                    status.set_connected();
                    return Some(client);
                }
                Err(err) => {
                    tracing::error!(error = %err, "rebuild failed; awaiting the next");
                    status.set_rebuild_failed();
                    let _ = events.send(ClientEvent::Died).await;
                }
            },
            Some(ClientCommand::Disconnect) | None => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_gateways_lets_the_sdk_choose() {
        let mut selector = GatewaySelector::new(Vec::new());
        assert_eq!(selector.take(), None);
        assert_eq!(selector.take(), None, "still None on every build");
    }

    #[test]
    fn one_gateway_is_pinned_on_every_build() {
        let mut selector = GatewaySelector::new(vec!["gw-a".to_owned()]);
        assert_eq!(selector.take().as_deref(), Some("gw-a"));
        assert_eq!(
            selector.take().as_deref(),
            Some("gw-a"),
            "no other to rotate to"
        );
    }

    #[test]
    fn several_gateways_rotate_and_wrap() {
        let mut selector = GatewaySelector::new(vec![
            "gw-a".to_owned(),
            "gw-b".to_owned(),
            "gw-c".to_owned(),
        ]);
        // Each build advances, so a rebuild after a bad gateway lands elsewhere;
        // the sequence wraps rather than running off the end.
        let seen: Vec<_> = (0..4).filter_map(|_| selector.take()).collect();
        assert_eq!(seen, ["gw-a", "gw-b", "gw-c", "gw-a"]);
    }
}
