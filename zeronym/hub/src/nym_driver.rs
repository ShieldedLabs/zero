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
//!   * each [`Reply`] goes back to its tag as an anonymous SURB reply, unless
//!     it has outlived [`crate::nym::REPLY_DEADLINE`] in the queue, in which
//!     case it is dropped rather than emitted to a shim that has given up.
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
//!
//! The driver also probes its OWN inbound path (see the liveness section in
//! [`run_driver`]): a client that connects, sends, and is never delivered to is
//! reported by nothing else, and on the one node every shim points at that is a
//! silent, permanent outage that reads as healthy.

#![cfg(feature = "mixnet-driver")]

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use zeroize::Zeroizing;

use nym_sdk::mixnet::{
    AnonymousSenderTag, Ephemeral, IncludedSurbs, MixnetClient, MixnetClientBuilder,
    MixnetMessageSender, Recipient,
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
///
/// "Consecutive" no longer means "with no successful connect in between": a
/// connect whose client dies or goes silent inside [`STABLE_LIFE`] does not
/// reset the count (see [`SHORT_LIVES_BEFORE_NEW_IDENTITY`]). It used to, so a
/// gateway that accepted every registration and then dropped every client
/// looped here forever without ever reaching this fallback.
const REBUILDS_BEFORE_NEW_IDENTITY: u32 = 60;

/// How long a client has to live before its connect counts as proof that the
/// stored registration works, resetting the failure counts below.
///
/// A client that died sooner than this is a connect-then-die and counts against
/// the registration; one that lasted this long served through at least one
/// full probe round while being delivered to (a silent one is torn down inside
/// ~2 rounds and counts regardless of how long that took), which is what "the
/// registration is fine" means. Long enough that the SDK's own give-up (20
/// consecutive send failures) on a gateway that registers and then drops us
/// lands inside it; short enough that an ordinary gateway blip every few hours
/// never accumulates.
const STABLE_LIFE: Duration = Duration::from_secs(3 * 60);

/// How many CONSECUTIVE short-lived clients (connected, then dead or silent
/// inside [`STABLE_LIFE`]) before the fresh-identity fallback, on top of the
/// outright connect failures counted by [`REBUILDS_BEFORE_NEW_IDENTITY`].
///
/// A separate count with a separate threshold because the units differ: a
/// failed connect costs a few seconds, a connect-then-silence costs two probe
/// rounds (~2 min) before it is even detected, so measuring both against 60
/// would make a silent gateway hold the hub hostage for two hours. Five short
/// lives is ~10 min of a gateway that registers the hub and never delivers to
/// it, which is the length of outage a redeploy would otherwise be called for.
const SHORT_LIVES_BEFORE_NEW_IDENTITY: u32 = 5;

/// How often to check that inbound traffic is still arriving.
///
/// Not tuned aggressively: a rebuild re-registers with the gateway and drops
/// whatever reply was in flight, so reacting to one quiet minute would trade a
/// rare permanent failure for frequent self-inflicted churn. Two rounds of
/// silence is ~2 minutes to self-heal, against a failure whose current
/// alternative is a redeploy by a human who has not noticed yet.
const PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// Consecutive probe rounds with NO inbound traffic before the client is torn
/// down and rebuilt. Two, not one, so a single dropped probe cannot trigger a
/// rebuild on an otherwise healthy client.
const SILENT_ROUNDS_BEFORE_REBUILD: u32 = 2;

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
    // How long the SDK waits for a packet's ack before it RETRANSMITS. The SDK
    // computes an expected ack round trip from the CONFIGURED mix delays -- not
    // measured -- and resends after `expected * ack_wait_multiplier +
    // ack_wait_addition` (defaults 1.5x + 1.5 s). On a path slower than that
    // formula predicts, acks arrive late but fine, the packet has already been
    // resent, and the receiver sees it twice. Measured 2026-08-17: a local hub's
    // replies reached a shim with ~1 duplicate fragment per lookup; every
    // DEPLOYED hub's reached the same shim with 15-25 -- and each duplicate is a
    // full send slot at the throttled rate, which is how a 32-packet reply that
    // should take ~5 s took 45-90 s from every enclave and timed out. Not a bug
    // in either side: a fixed retransmission timer tuned for a network the
    // enclave is not on. `ZIH_ACK_WAIT_ADDITION_MS` raises the addition so a
    // slow-but-honest ack is waited for instead of duplicated. It costs nothing
    // on a fast path (acks arrive well inside the wait either way) and only
    // delays retransmission of GENUINELY lost packets by the extra amount.
    let builder = match std::env::var("ZIH_ACK_WAIT_ADDITION_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(ms) => {
            let mut debug = nym_sdk::DebugConfig::default();
            debug.acknowledgements.ack_wait_addition = Duration::from_millis(ms);
            tracing::info!(
                ack_wait_addition_ms = ms,
                "raising the SDK's ack wait before retransmission"
            );
            builder.debug_config(debug)
        }
        None => builder,
    };
    builder
        .build()
        .map_err(|err| format!("building the mixnet client: {err}"))?
        .connect_to_mixnet()
        .await
        .map_err(|err| format!("connecting to the mixnet: {err}"))
}

/// The run of failures against the stored registration, and whether it has
/// reached the point of abandoning that registration for a fresh identity.
///
/// Two counts, not one, because two different things fail (see the two
/// thresholds); either reaching its threshold takes the fallback. Both are
/// reset together, and only by a client that lived out [`STABLE_LIFE`].
#[derive(Default)]
struct Failures {
    /// Connect attempts that failed outright, in a row.
    rebuilds: u32,
    /// Clients that connected and then died or went silent inside
    /// [`STABLE_LIFE`], in a row.
    short_lives: u32,
}

impl Failures {
    fn exhausted(&self) -> bool {
        self.rebuilds >= REBUILDS_BEFORE_NEW_IDENTITY
            || self.short_lives >= SHORT_LIVES_BEFORE_NEW_IDENTITY
    }
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
    let mut failures = Failures::default();
    // What was last handed to `address_out`, so an unchanged address is not
    // re-announced on every rebuild. With a stable identity that is the normal
    // case, and re-announcing would turn a healthy reconnect into a log line
    // that reads like a migration-breaking change.
    let mut announced: Option<Recipient> = None;

    // Outer loop: (re)build the client. Each pass holds one identity for as long
    // as it lives; a death falls out of the inner loop and comes back here.
    loop {
        // The stored registration is not coming back, whether the gateway
        // refuses us outright or registers us and then drops or starves every
        // client. Give up on it and mint a new identity, which is the only way
        // back onto the mixnet — at the cost of an address every shim must be
        // re-pointed to, hence the volume of this warning. Checked at the top
        // of the loop so both failure paths below feed one decision.
        if failures.exhausted() {
            storage = Ephemeral::default();
            // Also DROP any pinned gateway. The pin exists only to keep the
            // address stable (D10); once we have accepted a fresh identity (and
            // so a new address) that rationale is gone, and re-requesting the
            // same gateway would just fail forever if it is what died --
            // resetting storage but re-pinning a dead gateway is an infinite
            // fresh-identity loop that never reconnects. Letting the SDK choose
            // lands us on a live gateway (as far as the enclave's egress rule
            // allows).
            gateway = None;
            tracing::warn!(
                after_failed_connects = failures.rebuilds,
                after_short_lived_clients = failures.short_lives,
                "the hub's gateway registration is unrecoverable; taking a FRESH \
                 identity and dropping the gateway pin. The hub's Nym address WILL \
                 change: read the new one from /nym-address and re-point every shim, \
                 or migrations keep failing closed"
            );
            failures = Failures::default();
        }
        let mut client = match build_client(&network, &storage, gateway.as_deref()).await {
            // NOT a reset of the failure counts: a connect proves nothing until
            // the client has lived a while, and resetting here is what let a
            // connect-then-die gateway evade the fallback forever. The reset is
            // below, on a client that outlived STABLE_LIFE.
            Ok(client) => client,
            Err(err) => {
                failures.rebuilds += 1;
                status.set_rebuild_failed();
                tracing::error!(
                    error = %err,
                    consecutive_failures = failures.rebuilds,
                    "hub mixnet connect failed; retrying"
                );
                tokio::select! {
                    _ = &mut shutdown => return,
                    _ = tokio::time::sleep(REBUILD_BACKOFF) => continue,
                }
            }
        };
        let connected_at = Instant::now();
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

        // INBOUND LIVENESS. A client can register with a gateway, report itself
        // connected, send successfully — and never receive a single inbound
        // message for its whole life. The shim's driver measured exactly this on
        // 2026-08-14 (two of four identical shims never answered a lookup, one
        // broken three minutes after boot and still broken hours later), and
        // nothing recovers it, because the SDK only reports a death when it
        // gives up on its gateway, and a gateway that accepts sends is never
        // given up on. The hub had NO equivalent of the shim's probe, and the
        // hub is worse placed than any shim: it is the one node every shim
        // points at, so a hub in this state is a fleet-wide outage that reports
        // `mixnet_connected: true, client_deaths: 0`.
        //
        // The probe is a message to our OWN address. That exercises the half that
        // fails — the gateway delivering INTO this client — without depending on
        // any shim, so a quiet fleet cannot drive an endless rebuild loop. Its
        // payload is empty on purpose: `deliver` already filters empty messages
        // out as SURB-replenishment artifacts, so the probe is counted for
        // liveness and never reaches the listener to be puzzled over. And it is
        // a normal send, not an anonymous one, so it arrives tagless; `deliver`
        // drops it on emptiness before it can warn about the missing tag.
        //
        // The hub's rebuild reuses its storage, so unlike the shim's it does NOT
        // reroll the gateway or the registration; it re-establishes the
        // connection on the same registration, which is enough for a stuck
        // gateway session and nothing else. If the silence survives that, the
        // short-life accounting below walks it to the fresh-identity fallback,
        // which is the reroll.
        let own = *client.nym_address();
        let mut probe = tokio::time::interval(PROBE_INTERVAL);
        // The first tick is immediate; that is wanted, since a bad draw is bad
        // from boot and the point is to catch it before any shim does. Delay,
        // rather than Burst, so a stalled loop does not fire a backlog of probes
        // at once.
        probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Every reconstructed message this client has received, probes and
        // artifacts included; the count when the outstanding probe was sent;
        // and how many probe rounds in a row have seen no inbound at all.
        let mut inbound_total: u64 = 0;
        let mut inbound_at_probe: Option<u64> = None;
        let mut silent_rounds: u32 = 0;

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

        // Inner loop: serve until shutdown, the listener going away, a death, or
        // inbound silence. The select decides WHAT happened; consuming the client
        // (disconnect) happens after the loop, where no arm future still borrows it.
        let step = loop {
            let step = tokio::select! {
                _ = &mut shutdown => Step::Stop,
                // Inbound liveness. Cheap to keep in the select: the tick itself
                // does no I/O, and the probe send is a single empty message.
                // Guarded on nothing being in flight because the probe IS a send
                // and the SDK takes them one at a time; a tick that falls during a
                // send is served on the next turn, which is what
                // MissedTickBehavior::Delay is already set for.
                _ = probe.tick(), if in_flight.is_none() => {
                    match inbound_at_probe {
                        // A probe was outstanding and nothing at all has arrived
                        // since.
                        Some(mark) if inbound_total == mark => {
                            silent_rounds += 1;
                            if silent_rounds >= SILENT_ROUNDS_BEFORE_REBUILD {
                                tracing::error!(
                                    silent_rounds,
                                    gateway = %own.gateway(),
                                    "no inbound mixnet traffic across consecutive probes; the hub \
                                     is registered but not being delivered to. Rebuilding on the \
                                     same registration; if that stays silent the fresh-identity \
                                     fallback follows."
                                );
                                Step::Silent
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
                        // last probe — which is all the liveness we need, whether
                        // it came from the probe or from real shim requests.
                        _ => {
                            silent_rounds = 0;
                            in_flight = Some(probe_send(sender.clone(), own));
                            Step::Ferried
                        }
                    }
                },
                // Guarded so a reply is only taken once the previous one has been
                // accepted by the SDK: one in flight at a time, the rest left in
                // `outgoing` where the listener can see the queue.
                reply = outgoing.recv(), if in_flight.is_none() => match reply {
                    // A reply older than the shim's budget is dropped HERE, at
                    // the last moment before it would cost anything. Emitting it
                    // would spend a full 41-packet emission (~5 s under a real
                    // gateway's backpressure) on an answer the shim has already
                    // given up waiting for, and every one of those seconds is
                    // taken from the next reply in the queue, which is how a
                    // burst turns into a FIFO of dead answers starving every live
                    // one behind them. Dropping and taking the next reply on the
                    // next turn drains a dead backlog at channel speed instead.
                    Some(reply) if reply.is_dead() => {
                        tracing::warn!(
                            age_secs = reply.age().as_secs(),
                            "reply outlived the shim's request budget in the queue; dropped unsent"
                        );
                        Step::Ferried
                    }
                    Some(reply) => {
                        let tag = AnonymousSenderTag::from_bytes(reply.sender_tag.0);
                        in_flight = Some(reply_send(sender.clone(), tag, reply.frame.to_vec()));
                        Step::Ferried
                    }
                    // The listener is gone; there is nothing left to answer.
                    None => Step::Stop,
                },
                // The outstanding send, if there is one. Losing a turn here to an
                // inbound request costs nothing: only `drive`'s own future is
                // dropped, never the boxed send behind the `&mut`, so it resumes
                // from where it stopped.
                sent = drive(&mut in_flight), if in_flight.is_some() => {
                    in_flight = None;
                    match sent {
                        Sent::Reply => {}
                        // The mark means "inbound seen as of the probe going out",
                        // so it is read here and not when the probe was queued.
                        Sent::Probe => inbound_at_probe = Some(inbound_total),
                    }
                    Step::Ferried
                },
                messages = client.wait_for_messages() => match messages {
                    Some(messages) => {
                        for message in messages {
                            // Counted BEFORE `deliver` filters, so the probe (an
                            // empty message) and SURB artifacts count as
                            // liveness: the question is whether the gateway
                            // delivers to us at all, not what it delivers.
                            inbound_total += 1;
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
            Step::Died | Step::Silent => {
                let silent = matches!(step, Step::Silent);
                if silent {
                    // A silent client is LIVE as far as the SDK knows, so it is
                    // disconnected to completion (D12: a dropped live client leaks
                    // its background tasks) before the same storage is rebuilt
                    // from.
                    client.disconnect().await;
                } else {
                    // The dead client's tasks have already stopped, so it is
                    // dropped (at the end of this scope), not disconnected.
                    tracing::warn!("hub mixnet client died; rebuilding with the same address");
                }
                // Make the death VISIBLE. The address stays published (shims are
                // baked against it and it returns on rebuild), so without this the
                // hub goes on answering /nym-address and /healthz with 200 while
                // carrying no mixnet traffic at all — which is exactly how an
                // afternoon went into suspecting the mixnet on 2026-08-14. A
                // silence teardown counts as a death for the same reason: it is
                // a client that carried nothing inbound.
                status.set_died();
                // The connect-then-die accounting. A client that lasted out
                // STABLE_LIFE while being delivered to proved the stored
                // registration works and clears the slate; one that died sooner,
                // or was torn down for silence however long that took to detect,
                // is another strike against it, and the top of the outer loop
                // decides when the strikes are enough.
                let lived = connected_at.elapsed();
                if silent || lived < STABLE_LIFE {
                    failures.short_lives += 1;
                    tracing::warn!(
                        lived_secs = lived.as_secs(),
                        silent,
                        consecutive_short_lives = failures.short_lives,
                        "hub mixnet client did not prove the stored registration; \
                         counting it against it"
                    );
                } else {
                    failures = Failures::default();
                }
                // Back off, then the outer loop rebuilds from the SAME storage, so
                // the address the shims hold keeps working.
                tokio::select! {
                    _ = &mut shutdown => return,
                    _ = tokio::time::sleep(REBUILD_BACKOFF) => {}
                }
            }
            Step::Ferried => unreachable!("the inner loop only breaks on Stop, Died or Silent"),
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
    /// The client is alive but nothing inbound has reached it across
    /// [`SILENT_ROUNDS_BEFORE_REBUILD`] probes; disconnect it and rebuild.
    Silent,
    /// Shut down cleanly and stop.
    Stop,
}

/// One message handed to the SDK and not yet accepted by it.
///
/// Boxed and pinned so the driver can hold it across select turns and poll it
/// from a dedicated arm; owned rather than borrowing the sender, so a client that
/// dies mid-send leaves nothing borrowing the sender it took with it.
type InFlight = std::pin::Pin<Box<dyn std::future::Future<Output = Sent> + Send>>;

/// Which send just completed, so the loop can run the bookkeeping that belongs
/// after a send even though the send no longer finishes inside the arm that
/// started it.
enum Sent {
    /// A reply to a shim.
    Reply,
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
        Sent::Reply
    })
}

/// The in-flight future for one liveness probe, owned for the same reason as
/// [`reply_send`]'s.
fn probe_send(sender: nym_sdk::mixnet::MixnetClientSender, own: Recipient) -> InFlight {
    Box::pin(async move {
        send_probe(&sender, own).await;
        Sent::Probe
    })
}

/// A liveness probe: an empty message to our own address.
///
/// Empty because `deliver` already discards empty inbound messages as
/// SURB-replenishment artifacts, so this is counted for liveness and never
/// reaches the listener. Zero attached SURBs: nothing has to reply to it, the
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

/// Hand one inbound reconstructed message to the listener as a [`Received`],
/// unless it is an artifact or an anonymity failure.
async fn deliver(
    incoming: &mpsc::Sender<Received>,
    message: nym_sdk::mixnet::ReconstructedMessage,
) {
    // Wrap the cleartext in Zeroizing FIRST, so EVERY return path below wipes it
    // on drop, not only the one that reaches the listener. A SubmitV1 here holds a
    // diverted migration in cleartext, and freeing it un-wiped is the one thing an
    // attestation cannot excuse in a diskless enclave (nym.rs) — the empty-artifact
    // and tagless-drop returns free the same Vec and must wipe it too.
    let frame = Zeroizing::new(message.message);
    // Empty inbound messages are SURB-replenishment artifacts, not requests (D12),
    // and so is our own liveness probe; the listener would drop them anyway, but
    // keeping them out of the channel keeps it for real frames.
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
