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

    // The select decides WHAT happened; the client lifecycle (which consumes the
    // client to disconnect, or replaces it on rebuild) is handled AFTER the
    // select, where none of its futures still borrow the client.
    loop {
        let step = tokio::select! {
            command = commands.recv() => match command {
                Some(ClientCommand::Rebuild) => Step::Rebuild,
                // A dropped commands channel is the supervisor gone: nothing left
                // to obey, so shut the client down cleanly like an explicit stop.
                Some(ClientCommand::Disconnect) | None => Step::Stop,
            },
            frame = out_frames.recv() => match frame {
                Some(out) => {
                    send_frame(&sender, &hub_addresses, out).await;
                    status.record_send();
                    Step::Ferried
                }
                // The transport loop is gone; there is nothing to carry.
                None => Step::Stop,
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
                client.disconnect().await;
                return;
            }
            Step::Rebuild => {
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
                        match build_when_told(&mut commands, &network, &events, &mut gateways, &status).await {
                            Some(client) => client,
                            None => return,
                        }
                    }
                };
                sender = client.split_sender();
            }
            Step::Died => {
                // The dead client's tasks have already stopped, so it is dropped
                // (by the reassignment below), not disconnected. Report and wait
                // for the supervisor to ask for a rebuild.
                status.set_died();
                let _ = events.send(ClientEvent::Died).await;
                client = match build_when_told(&mut commands, &network, &events, &mut gateways, &status).await {
                    Some(client) => client,
                    None => return,
                };
                sender = client.split_sender();
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
        tracing::error!(index = out.target, "no hub address at that index; dropping frame");
        return;
    };
    if let Err(err) = sender
        .send_message(recipient, out.frame.to_vec(), IncludedSurbs::new(out.reply_surbs))
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
        assert_eq!(selector.take().as_deref(), Some("gw-a"), "no other to rotate to");
    }

    #[test]
    fn several_gateways_rotate_and_wrap() {
        let mut selector =
            GatewaySelector::new(vec!["gw-a".to_owned(), "gw-b".to_owned(), "gw-c".to_owned()]);
        // Each build advances, so a rebuild after a bad gateway lands elsewhere;
        // the sequence wraps rather than running off the end.
        let seen: Vec<_> = (0..4).filter_map(|_| selector.take()).collect();
        assert_eq!(seen, ["gw-a", "gw-b", "gw-c", "gw-a"]);
    }
}
