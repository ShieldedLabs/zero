//! The mixnet driver: the one place in the hub that owns a `nym-sdk` client.
//!
//! [`crate::nym::run_listener`] is SDK-free and speaks only in channels: a
//! [`Received`] per inbound request, a [`Reply`] per answer. This module is the
//! other end. It owns one client and does nothing but move bytes:
//!
//!   * every non-empty inbound message with a sender tag becomes a [`Received`]
//!     (the SDK's `AnonymousSenderTag` converted to the opaque [`SenderTag`], so
//!     the listener never sees an SDK type); the empty ones are SURB-
//!     replenishment artifacts (D12) and are dropped;
//!   * a message that arrives WITHOUT a sender tag is a shim that exposed its own
//!     address instead of sending anonymously (D3). The hub cannot reply to it
//!     and would not want to hold it, so it is dropped with a warning, never
//!     queued;
//!   * each [`Reply`] goes back to its tag as an anonymous SURB reply.
//!
//! Unlike the shim's driver there is no rotation and no supervisor: the hub's
//! address is what every shim sends to (D10), so it holds ONE identity for the
//! life of the process. It still handles the client dying (D12: after 20 send
//! failures the SDK stops for good) by rebuilding and logging the NEW address it
//! mints. Be honest about the limit (D10): a diskless hub's address changes on
//! every restart or rebuild regardless, and existing shims hold the OLD one, so
//! this recovery restores the shim->hub path ONLY for shims that then learn the
//! new address through whatever address distribution the deployment uses — an
//! open humans decision, not something this rebuild fixes on its own. It rebuilds
//! rather than exiting so the hub's clearnet serving and batcher stay up, but for
//! the mixnet path a rebuild without republication is a moved address, not a
//! recovered one. Shutdown disconnects the client cleanly (D12: `disconnect()` is
//! not cancel-safe and a dropped LIVE client leaks its background tasks); a
//! client that already died is dropped, its tasks having already stopped.

#![cfg(feature = "mixnet-driver")]

use std::time::Duration;

use tokio::sync::mpsc;
use zeroize::Zeroizing;

use nym_sdk::mixnet::{
    AnonymousSenderTag, MixnetClient, MixnetClientBuilder, MixnetMessageSender, Recipient,
};

use crate::nym::{Received, Reply, SenderTag};

/// How long to wait after a dead or failed client before rebuilding, so a
/// gateway that rejects every connection is retried steadily rather than in a
/// hot loop.
const REBUILD_BACKOFF: Duration = Duration::from_secs(5);

/// Which Nym network the driver connects to. Plain data, not a trait: production
/// is the default network baked into the SDK; the localnet variant (compiled
/// only with `mixnet-localnet`) points the same driver at the mixnet the nymnet
/// harness starts, so the shipped driver is what the end-to-end test exercises.
pub enum MixnetNetwork {
    /// The default network the SDK ships with (mainnet). Production.
    Default,
    /// A hardcoded topology loaded from a file: the local mixnet started by
    /// `nymnet/localnet.sh`, for end-to-end tests.
    #[cfg(feature = "mixnet-localnet")]
    TopologyFile(std::path::PathBuf),
}

/// Build (or rebuild) a mixnet client. Ephemeral by construction: the hub keeps
/// no on-disk identity, so a rebuild after a death is a fresh registration.
async fn build_client(network: &MixnetNetwork) -> Result<MixnetClient, String> {
    let builder = MixnetClientBuilder::new_ephemeral();
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

/// Own the mixnet client and move bytes across it until told to shut down.
///
/// `incoming`/`outgoing` are the driver side of [`crate::nym::run_listener`]'s
/// two channels. `address_out` publishes the hub's Nym address on every
/// (re)build, because it is what an operator must hand to every shim (D10) and
/// it changes whenever the client is rebuilt. `shutdown` resolving is the cue to
/// disconnect cleanly and return.
pub async fn run_driver(
    network: MixnetNetwork,
    incoming: mpsc::Sender<Received>,
    mut outgoing: mpsc::Receiver<Reply>,
    address_out: mpsc::Sender<Recipient>,
    shutdown: impl std::future::Future<Output = ()>,
) {
    tokio::pin!(shutdown);

    // Outer loop: (re)build the client. Each pass holds one identity for as long
    // as it lives; a death falls out of the inner loop and comes back here.
    loop {
        let mut client = match build_client(&network).await {
            Ok(client) => client,
            Err(err) => {
                tracing::error!(error = %err, "hub mixnet connect failed; retrying");
                tokio::select! {
                    _ = &mut shutdown => return,
                    _ = tokio::time::sleep(REBUILD_BACKOFF) => continue,
                }
            }
        };
        let address = *client.nym_address();
        tracing::info!(%address, "hub mixnet client connected; publish this to shims");
        // Best-effort: the operator also has it in the log above.
        let _ = address_out.send(address).await;
        // An owned sender, so the reply arm below touches `sender` while the
        // receive arm touches `client`: two disjoint borrows in one `select!`.
        let sender = client.split_sender();

        // Inner loop: serve until shutdown, the listener going away, or a death.
        // The select decides WHAT happened; consuming the client (disconnect)
        // happens after the loop, where no arm future still borrows it.
        let step = loop {
            let step = tokio::select! {
                _ = &mut shutdown => Step::Stop,
                reply = outgoing.recv() => match reply {
                    Some(reply) => {
                        let tag = AnonymousSenderTag::from_bytes(reply.sender_tag.0);
                        if let Err(err) = sender.send_reply(tag, reply.frame.to_vec()).await {
                            tracing::warn!(error = %err, "mixnet reply send failed");
                        }
                        Step::Ferried
                    }
                    // The listener is gone; there is nothing left to answer.
                    None => Step::Stop,
                },
                messages = client.wait_for_messages() => match messages {
                    Some(messages) => {
                        for message in messages {
                            deliver(&incoming, message).await;
                        }
                        Step::Ferried
                    }
                    // The SDK has given up on its gateway for good (D12).
                    None => Step::Died,
                },
            };
            match step {
                Step::Ferried => continue,
                stop_or_died => break stop_or_died,
            }
        };

        match step {
            Step::Stop => {
                client.disconnect().await;
                return;
            }
            Step::Died => {
                // The dead client's tasks have already stopped, so it is dropped
                // (at the end of this scope), not disconnected. Back off, then the
                // outer loop rebuilds with a fresh address.
                tracing::warn!("hub mixnet client died; rebuilding with a fresh address");
                tokio::select! {
                    _ = &mut shutdown => return,
                    _ = tokio::time::sleep(REBUILD_BACKOFF) => {}
                }
            }
            Step::Ferried => unreachable!("the inner loop only breaks on Stop or Died"),
        }
    }
}

/// What one turn of the inner loop resolved to. Kept out of the `select!` so the
/// client can be consumed (disconnect) once no arm future still borrows it.
enum Step {
    /// Bytes moved in one direction or the other; keep serving.
    Ferried,
    /// The client died; rebuild.
    Died,
    /// Shut down cleanly and stop.
    Stop,
}

/// Hand one inbound reconstructed message to the listener as a [`Received`],
/// unless it is an artifact or an anonymity failure.
async fn deliver(incoming: &mpsc::Sender<Received>, message: nym_sdk::mixnet::ReconstructedMessage) {
    // Wrap the cleartext in Zeroizing FIRST, so EVERY return path below wipes it
    // on drop, not only the one that reaches the listener. A SubmitV1 here holds a
    // diverted migration in cleartext, and freeing it un-wiped is the one thing an
    // attestation cannot excuse in a diskless enclave (nym.rs) — the empty-artifact
    // and tagless-drop returns free the same Vec and must wipe it too.
    let frame = Zeroizing::new(message.message);
    // Empty inbound messages are SURB-replenishment artifacts, not requests (D12);
    // the listener would drop them anyway, but keeping them out of the channel
    // keeps it for real frames.
    if frame.is_empty() {
        return;
    }
    // A request WITHOUT a sender tag is a shim that exposed its own address
    // instead of sending anonymously (D3). There is no tag to reply to, and
    // holding the frame is exactly what this hop exists to avoid, so drop it —
    // wiped, via the Zeroizing wrapper above.
    let Some(tag) = message.sender_tag else {
        tracing::warn!("a request arrived with no sender tag; dropping it");
        return;
    };
    let _ = incoming
        .send(Received {
            frame,
            sender_tag: SenderTag(tag.to_bytes()),
        })
        .await;
}
