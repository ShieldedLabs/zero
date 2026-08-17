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
//! life of the process — and, since the storage below outlives the rebuild loop,
//! ACROSS rebuilds too.
//!
//! **The address survives a client death.** One [`Ephemeral`] store is built
//! once and cloned into every (re)build, and the SDK's initialisation loads
//! existing keys before it generates any, so a rebuild comes back with the same
//! identity key, the same encryption key, and the same gateway registration —
//! hence the same Nym address. That matters because the address is baked into
//! every shim's enclave config and a Caution managed app is immutable, so an
//! address change costs every operator a re-assemble and redeploy. Before this,
//! the client minted a fresh identity on every rebuild and the shim fleet was
//! stale the moment a gateway blipped.
//!
//! What still changes the address: a real process restart (the store is in RAM,
//! and a diskless enclave has nowhere to persist it), and the deliberate
//! fallback below when the stored gateway registration is beyond saving. Both
//! are loud, and both leave existing shims holding the OLD address until they
//! are re-pointed — the address-distribution problem is not solved here, only
//! made rare. `/nym-address` on the serving path is how an attested hub's
//! operator reads the current value without a console.
//!
//! It rebuilds rather than exiting so the hub's clearnet serving and batcher stay
//! up. Shutdown disconnects the client cleanly (D12: `disconnect()` is not
//! cancel-safe and a dropped LIVE client leaks its background tasks); a client
//! that already died is dropped, its tasks having already stopped.

#![cfg(feature = "mixnet-driver")]

use std::time::Duration;

use tokio::sync::mpsc;
use zeroize::Zeroizing;

use nym_sdk::mixnet::{
    AnonymousSenderTag, Ephemeral, MixnetClient, MixnetClientBuilder, MixnetMessageSender,
    Recipient,
};

use crate::nym::{Received, Reply, SenderTag};

/// How long to wait after a dead or failed client before rebuilding, so a
/// gateway that rejects every connection is retried steadily rather than in a
/// hot loop.
const REBUILD_BACKOFF: Duration = Duration::from_secs(5);

/// How many CONSECUTIVE failed rebuilds before the driver abandons the stored
/// gateway registration and takes a fresh identity.
///
/// Deliberately patient. Reusing the storage is exactly what keeps the address
/// stable, and it also pins the client to one gateway — so if that gateway is
/// genuinely gone, retrying it forever would keep the hub off the mixnet. This
/// is the escape hatch, and it is costly to take: a new identity is a new
/// address, which every shim has baked into an immutable enclave config.
///
/// Staying down a while is therefore cheaper than rotating early, and a hub
/// restart would change the address anyway, so this only automates what an
/// operator would otherwise do by hand.
///
/// **About six minutes of unbroken failure, not the five you get by dividing.**
/// Measured on the localnet harness with the gateway killed: 60 failures took
/// ~370 s, because each cycle costs [`REBUILD_BACKOFF`] PLUS however long the
/// connect attempt itself takes to fail — ~1 s against a locally refused port,
/// and longer against a remote gateway that times out rather than refusing. The
/// wall-clock threshold is therefore a floor, not a fixed value; anything
/// tuning this number should re-measure rather than divide by the backoff.
/// `nymnet/localnet.sh` plus the probe's `hub-address-across-rebuild changed`
/// arm is how that measurement is taken.
const REBUILDS_BEFORE_NEW_IDENTITY: u32 = 60;

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

/// Build (or rebuild) a mixnet client against the caller's storage.
///
/// Taking the store by reference and cloning it is the whole trick behind a
/// stable address: `MixnetClientBuilder::new_ephemeral()` would mint a fresh
/// [`Ephemeral`] (and therefore fresh keys) on every call, whereas the SDK's
/// initialisation loads existing keys before generating any, so handing it the
/// SAME store returns the SAME identity, encryption key, and gateway
/// registration. Still ephemeral in the sense that matters for a diskless
/// enclave: the store is in RAM and dies with the process.
async fn build_client(
    network: &MixnetNetwork,
    storage: &Ephemeral,
    gateway: Option<&str>,
) -> Result<MixnetClient, String> {
    let builder = MixnetClientBuilder::new_with_storage(storage.clone());
    // A SINGLE pinned entry gateway, unlike the shim's rotating list: the hub's
    // address embeds its gateway and must stay stable (D10), so it holds one. Once
    // the storage carries a registration a rebuild loads it, so this only bites on
    // the FIRST registration and on the fresh-identity fallback; applying it every
    // build is harmless and keeps the choice in one place. The egress rule must
    // allow this gateway's IP or connect fails closed with no console.
    let builder = match gateway {
        Some(gateway) => builder.request_gateway(gateway.to_owned()),
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

/// Own the mixnet client and move bytes across it until told to shut down.
///
/// `incoming`/`outgoing` are the driver side of [`crate::nym::run_listener`]'s
/// two channels. `address_out` publishes the hub's Nym address on every
/// (re)build, because it is what an operator must hand to every shim (D10) and
/// it changes whenever the client is rebuilt. `shutdown` resolving is the cue to
/// disconnect cleanly and return.
pub async fn run_driver(
    network: MixnetNetwork,
    mut gateway: Option<String>,
    incoming: mpsc::Sender<Received>,
    mut outgoing: mpsc::Receiver<Reply>,
    address_out: mpsc::Sender<Recipient>,
    status: crate::server::NymAddress,
    shutdown: impl std::future::Future<Output = ()>,
) {
    tokio::pin!(shutdown);

    // Built ONCE, outside the loop, and cloned into every (re)build: this is what
    // carries the identity (and so the address) across a client death. Replaced
    // only by the fallback below, when the gateway it is registered with stops
    // accepting us for good.
    let mut storage = Ephemeral::default();
    let mut failed_rebuilds: u32 = 0;
    // What was last handed to `address_out`, so an unchanged address is not
    // re-announced on every rebuild. With a stable identity that is the normal
    // case, and re-announcing would turn a healthy reconnect into a log line
    // that reads like a migration-breaking change.
    let mut announced: Option<Recipient> = None;

    // Outer loop: (re)build the client. Each pass holds one identity for as long
    // as it lives; a death falls out of the inner loop and comes back here.
    loop {
        let mut client = match build_client(&network, &storage, gateway.as_deref()).await {
            Ok(client) => {
                failed_rebuilds = 0;
                client
            }
            Err(err) => {
                failed_rebuilds += 1;
                status.set_rebuild_failed();
                tracing::error!(
                    error = %err,
                    consecutive_failures = failed_rebuilds,
                    "hub mixnet connect failed; retrying"
                );
                // The stored registration is not coming back. Give up on it and
                // mint a new identity, which is the only way back onto the
                // mixnet — at the cost of an address every shim must be
                // re-pointed to, hence the volume of this warning.
                if failed_rebuilds >= REBUILDS_BEFORE_NEW_IDENTITY {
                    storage = Ephemeral::default();
                    failed_rebuilds = 0;
                    // Also DROP any pinned gateway. The pin exists only to keep
                    // the address stable (D10); once we have accepted a fresh
                    // identity (and so a new address) that rationale is gone, and
                    // re-requesting the same gateway would just fail forever if it
                    // is what died -- resetting storage but re-pinning a dead
                    // gateway is an infinite fresh-identity loop that never
                    // reconnects. Letting the SDK choose lands us on a live
                    // gateway (as far as the enclave's egress rule allows).
                    gateway = None;
                    tracing::warn!(
                        after_failures = REBUILDS_BEFORE_NEW_IDENTITY,
                        "the hub's gateway registration is unrecoverable; taking a FRESH \
                         identity and dropping the gateway pin. The hub's Nym address WILL \
                         change: read the new one from /nym-address and re-point every shim, \
                         or migrations keep failing closed"
                    );
                }
                tokio::select! {
                    _ = &mut shutdown => return,
                    _ = tokio::time::sleep(REBUILD_BACKOFF) => continue,
                }
            }
        };
        let address = *client.nym_address();
        if announced != Some(address) {
            tracing::info!(%address, "hub mixnet client connected; publish this to shims");
            announced = Some(address);
        } else {
            tracing::info!("hub mixnet client reconnected; address unchanged");
        }
        // Best-effort, and sent on EVERY build even when unchanged: `/nym-address`
        // reads the value this publishes, and a receiver that restarted or missed
        // the first send must still be able to answer.
        let _ = address_out.send(address).await;
        // An owned sender, so the reply arm below touches `sender` while the
        // receive arm touches `client`: two disjoint borrows in one `select!`.
        let sender = client.split_sender();

        // THE ONE REPLY IN FLIGHT, driven by its own arm instead of awaited inside
        // the arm that starts it.
        //
        // Handing a reply to the SDK must never be an awaited step inside a select
        // arm. The SDK's input channel holds a single `InputMessage` and is drained
        // at the client's throttled Poisson send rate, so the hand-off blocks for as
        // long as that rate dictates, which under a real gateway's backpressure runs
        // to whole seconds. For every one of those seconds an inline await
        // would stop this loop polling `wait_for_messages`, so requests already
        // delivered to our gateway would sit unread while we queued the answer to an
        // earlier one, and every shim waiting on those would time out. The shim's
        // `correlate` (in its `nym` module) documents the same hazard one channel
        // upstream and avoids it with `reserve()`; here the send is held as a pinned
        // future and driven by its own arm, so the receive arm keeps running for the
        // whole of it.
        //
        // Backpressure comes from the guard rather than from an await: no reply is
        // taken from `outgoing` while one is still outstanding, so the queue stays
        // visible to the listener instead of being absorbed into spawned tasks.
        let mut in_flight: Option<InFlight> = None;

        // Inner loop: serve until shutdown, the listener going away, or a death.
        // The select decides WHAT happened; consuming the client (disconnect)
        // happens after the loop, where no arm future still borrows it.
        let step = loop {
            let step = tokio::select! {
                _ = &mut shutdown => Step::Stop,
                // Guarded so a reply is only taken once the previous one has been
                // accepted by the SDK: one in flight at a time, the rest left in
                // `outgoing` where the listener can see the queue.
                reply = outgoing.recv(), if in_flight.is_none() => match reply {
                    Some(reply) => {
                        let tag = AnonymousSenderTag::from_bytes(reply.sender_tag.0);
                        in_flight = Some(reply_send(sender.clone(), tag, reply.frame.to_vec()));
                        Step::Ferried
                    }
                    // The listener is gone; there is nothing left to answer.
                    None => Step::Stop,
                },
                // The outstanding reply, if there is one. Losing a turn here to an
                // inbound request costs nothing: only `drive`'s own future is
                // dropped, never the boxed send behind the `&mut`, so it resumes
                // from where it stopped.
                _ = drive(&mut in_flight), if in_flight.is_some() => {
                    in_flight = None;
                    Step::Ferried
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
        // Whatever was still in flight belongs to the client that has just stopped
        // serving, so it goes with it rather than lingering across the backoff
        // below. Nothing is left half-written by that: the SDK's send is cancel-safe
        // (the message is either fully queued or not sent at all), and a reply
        // queued into a client about to be disconnected would not have reached the
        // mixnet either. It also bounds the plaintext copy of the reply frame to the
        // life of the client that was going to send it, rather than letting it sit
        // through the backoff.
        drop(in_flight);

        match step {
            Step::Stop => {
                client.disconnect().await;
                return;
            }
            Step::Died => {
                // The dead client's tasks have already stopped, so it is dropped
                // (at the end of this scope), not disconnected. Back off, then the
                // outer loop rebuilds from the SAME storage, so the address the
                // shims hold keeps working.
                tracing::warn!("hub mixnet client died; rebuilding with the same address");
                // Make the death VISIBLE. The address stays published (shims are
                // baked against it and it returns on rebuild), so without this the
                // hub goes on answering /nym-address and /healthz with 200 while
                // carrying no mixnet traffic at all — which is exactly how an
                // afternoon went into suspecting the mixnet on 2026-08-14.
                status.set_died();
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

/// One reply handed to the SDK and not yet accepted by it.
///
/// Boxed and pinned so the driver can hold it across select turns and poll it
/// from a dedicated arm; owned rather than borrowing the sender, so a client that
/// dies mid-send leaves nothing borrowing the sender it took with it.
type InFlight = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Poll the outstanding reply, if there is one.
///
/// Taking the `Option` by reference is what makes this arm cancel-safe: when
/// another arm wins the turn, only this future is dropped and the send behind the
/// reference is untouched. The `None` case parks forever rather than returning,
/// since a select arm that resolved instantly would spin the loop; in practice it
/// is unreachable behind the arm's `is_some()` guard.
async fn drive(in_flight: &mut Option<InFlight>) {
    match in_flight {
        Some(send) => send.await,
        None => std::future::pending::<()>().await,
    }
}

/// The in-flight future for one outbound reply.
///
/// It owns its sender and frame because it outlives the select turn that started
/// it; the sender is a cheap handle over the client's input channel, so cloning
/// one per reply costs nothing.
fn reply_send(
    sender: nym_sdk::mixnet::MixnetClientSender,
    tag: AnonymousSenderTag,
    frame: Vec<u8>,
) -> InFlight {
    Box::pin(async move {
        if let Err(err) = sender.send_reply(tag, frame).await {
            tracing::warn!(error = %err, "mixnet reply send failed");
        }
    })
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
