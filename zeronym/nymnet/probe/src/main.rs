//! Probe binary for the zeronym Nym localnet harness.
//!
//! Drives real `nym-sdk` clients through the local mixnet started by
//! `../localnet.sh` to answer the SDK-behaviour questions NYM_PLAN.md
//! currently answers from documentation: SURB accounting, sender-tag
//! lifecycle, anonymous-reply mechanics, and round-trip shape.
//!
//! Subcommands:
//!   topology <run_dir> <out>       assemble network.json from the nodes'
//!                                  host-information dumps (written by localnet.sh)
//!   smoke    <network.json>        SubmitV1-shaped round-trips: 64 KiB anonymous
//!                                  submit, 64-byte SURB ack, sender-tag stability,
//!                                  fresh tag after a client rebuild
//!   lookup   <network.json> <n>    LookupV1-shaped round-trip: 64-byte request
//!                                  with n attached SURBs, full 64 KiB anonymous
//!                                  reply; times the reply to expose SURB
//!                                  re-request round trips

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use nym_sdk::mixnet::{
    IncludedSurbs, MixnetClient, MixnetClientBuilder, MixnetMessageSender,
};
use nym_topology::{
    CachedEpochRewardedSet, EntryDetails, HardcodedTopologyProvider, NymTopology,
    NymTopologyMetadata, RoutingNode, SupportedRoles,
};

/// Matches FRAME_BYTES in the shim/hub wire modules.
const FRAME_BYTES: usize = 65536;
/// Matches ACK_BYTES in the shim/hub wire modules.
const ACK_BYTES: usize = 64;
/// D3's fixed submit SURB count (the 12-15 range; middle picked).
const SUBMIT_SURBS: u32 = 13;

#[tokio::main]
async fn main() -> Result<()> {
    nym_bin_common::logging::setup_tracing_logger();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("topology") => {
            let run_dir = PathBuf::from(args.get(2).context("usage: topology <run_dir> <out>")?);
            let out = PathBuf::from(args.get(3).context("usage: topology <run_dir> <out>")?);
            cmd_topology(&run_dir, &out)
        }
        Some("smoke") => {
            let network = PathBuf::from(args.get(2).context("usage: smoke <network.json>")?);
            cmd_smoke(&network).await
        }
        Some("lookup") => {
            let network = PathBuf::from(args.get(2).context("usage: lookup <network.json> <surbs>")?);
            let surbs: u32 = args
                .get(3)
                .context("usage: lookup <network.json> <surbs>")?
                .parse()?;
            cmd_lookup(&network, surbs).await
        }
        _ => bail!("usage: probe topology|smoke|lookup ..."),
    }
}

/// Assemble a `NymTopology` json from the four nodes' /api/v1/host-information
/// dumps. Built with the SDK's own types and serde, so the file is correct by
/// construction for `NymTopology::new_from_file` at the pinned version (the
/// upstream scripts/build_topology.py emits a stale format).
fn cmd_topology(run_dir: &Path, out: &Path) -> Result<()> {
    let names = ["mix1", "mix2", "mix3", "gateway"];
    let mut nodes = Vec::new();
    let mut rotation = 0u32;
    for (i, name) in names.iter().enumerate() {
        let path = run_dir.join(format!("{name}.host.json"));
        let v: serde_json::Value = serde_json::from_reader(
            std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?,
        )?;
        let keys = &v["data"]["keys"];
        let identity = keys["ed25519_identity"]
            .as_str()
            .with_context(|| format!("{name}: missing ed25519_identity"))?;
        let sphinx = keys["primary_x25519_sphinx_key"]["public_key"]
            .as_str()
            .with_context(|| format!("{name}: missing primary sphinx key"))?;
        rotation = keys["primary_x25519_sphinx_key"]["rotation_id"]
            .as_u64()
            .with_context(|| format!("{name}: missing rotation_id"))? as u32;
        let node_id = (i + 1) as u32;
        let is_gateway = *name == "gateway";
        nodes.push(RoutingNode {
            node_id,
            mix_host: format!("127.0.0.1:{}", 10000 + node_id).parse()?,
            entry: is_gateway.then(|| EntryDetails {
                ip_addresses: vec!["127.0.0.1".parse().unwrap()],
                clients_ws_port: 9000,
                hostname: None,
                clients_wss_port: None,
            }),
            identity_key: identity.parse().context("bad identity key")?,
            sphinx_key: sphinx.parse().context("bad sphinx key")?,
            supported_roles: SupportedRoles {
                mixnode: !is_gateway,
                mixnet_entry: is_gateway,
                mixnet_exit: false,
            },
        });
    }

    let mut rewarded = CachedEpochRewardedSet::default();
    rewarded.layer1.insert(1);
    rewarded.layer2.insert(2);
    rewarded.layer3.insert(3);
    rewarded.entry_gateways.insert(4);

    let metadata = NymTopologyMetadata::new(rotation, 0, time::OffsetDateTime::now_utc());
    let topology = NymTopology::new(metadata, rewarded, nodes);
    serde_json::to_writer_pretty(std::fs::File::create(out)?, &topology)?;

    // Sanity: reload through the exact loader the clients will use.
    NymTopology::new_from_file(out).context("self-check reload failed")?;
    println!("wrote {} (sphinx key rotation {rotation})", out.display());
    Ok(())
}

async fn connect_client(network: &Path) -> Result<MixnetClient> {
    let provider = HardcodedTopologyProvider::new_from_file(network)
        .with_context(|| format!("loading topology {}", network.display()))?;
    let client = MixnetClientBuilder::new_ephemeral()
        .custom_topology_provider(Box::new(provider))
        .build()?
        .connect_to_mixnet()
        .await
        .context("connecting to localnet")?;
    Ok(client)
}

/// Wait for the next non-empty reconstructed message (empty inbound messages
/// are SDK SURB-replenishment artifacts, per NYM_PLAN D12).
async fn wait_nonempty(client: &mut MixnetClient, timeout: Duration) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for a message");
        }
        match tokio::time::timeout(remaining, client.wait_for_messages()).await {
            Err(_) => bail!("timed out waiting for a message"),
            Ok(None) => bail!("client stream ended unexpectedly"),
            Ok(Some(msgs)) => {
                for m in msgs {
                    if !m.message.is_empty() {
                        return Ok(m.message);
                    }
                }
            }
        }
    }
}

/// Submit/ack round-trips shaped like SubmitV1/AckV1: three 64 KiB anonymous
/// submits from one client session (asserting one stable sender tag), a
/// 64-byte SURB ack for each, then a rebuilt client (asserting a fresh tag).
async fn cmd_smoke(network: &Path) -> Result<()> {
    const SAME_SESSION_SUBMITS: usize = 3;
    let total = SAME_SESSION_SUBMITS + 1;

    println!("[hub] connecting...");
    let mut hub = connect_client(network).await?;
    let hub_addr = *hub.nym_address();
    println!("[hub] address: {hub_addr}");
    let hub_sender = hub.split_sender();

    let (tag_tx, mut tag_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, Option<String>)>();
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let hub_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                msgs = hub.wait_for_messages() => {
                    let Some(msgs) = msgs else { break };
                    for m in msgs {
                        if m.message.is_empty() {
                            continue; // SURB replenishment artifact
                        }
                        let mut ack = vec![0u8; ACK_BYTES];
                        ack[..7].copy_from_slice(b"ZNYMACK");
                        ack[7] = m.message[0];
                        match m.sender_tag {
                            Some(tag) => {
                                if let Err(e) = hub_sender.send_reply(tag, ack).await {
                                    eprintln!("[hub] send_reply failed: {e}");
                                }
                                let _ = tag_tx.send((m.message.len(), Some(tag.to_string())));
                            }
                            None => {
                                let _ = tag_tx.send((m.message.len(), None));
                            }
                        }
                    }
                }
            }
        }
        hub.disconnect().await;
    });

    println!("[shim] connecting...");
    let mut shim = connect_client(network).await?;
    println!("[shim] address: {}", shim.nym_address());
    for i in 0..SAME_SESSION_SUBMITS {
        let mut frame = vec![0u8; FRAME_BYTES];
        frame[0] = i as u8;
        let t0 = Instant::now();
        shim.send_message(hub_addr, frame, IncludedSurbs::new(SUBMIT_SURBS))
            .await?;
        let ack = wait_nonempty(&mut shim, Duration::from_secs(120)).await?;
        anyhow::ensure!(ack.len() == ACK_BYTES, "ack was {} bytes", ack.len());
        anyhow::ensure!(ack[7] == i as u8, "ack correlates to wrong submit");
        println!(
            "[shim] submit {i}: 64 KiB + {SUBMIT_SURBS} SURBs sent, {ACK_BYTES}-byte ack in {:.2?}",
            t0.elapsed()
        );
    }

    println!("[shim] disconnecting and rebuilding the client (fresh identity)...");
    shim.disconnect().await;
    let mut shim2 = connect_client(network).await?;
    let mut frame = vec![0u8; FRAME_BYTES];
    frame[0] = 99;
    let t0 = Instant::now();
    shim2
        .send_message(hub_addr, frame, IncludedSurbs::new(SUBMIT_SURBS))
        .await?;
    let ack = wait_nonempty(&mut shim2, Duration::from_secs(120)).await?;
    anyhow::ensure!(ack.len() == ACK_BYTES && ack[7] == 99);
    println!("[shim] rebuilt-client submit acked in {:.2?}", t0.elapsed());
    shim2.disconnect().await;

    let _ = stop_tx.send(());
    hub_task.await?;

    let mut sizes = Vec::new();
    let mut tags = Vec::new();
    while let Ok((len, tag)) = tag_rx.try_recv() {
        sizes.push(len);
        tags.push(tag);
    }
    anyhow::ensure!(sizes.len() == total, "hub saw {} submits, expected {total}", sizes.len());
    anyhow::ensure!(
        sizes.iter().all(|&s| s == FRAME_BYTES),
        "hub saw non-frame-sized submits: {sizes:?}"
    );
    anyhow::ensure!(
        tags.iter().all(Option::is_some),
        "a submit arrived without a sender tag (self-address exposed or SURBs missing?)"
    );
    let session_tags: Vec<&String> = tags[..SAME_SESSION_SUBMITS].iter().flatten().collect();
    anyhow::ensure!(
        session_tags.windows(2).all(|w| w[0] == w[1]),
        "sender tag was NOT stable within one session: {session_tags:?}"
    );
    let rebuilt_tag = tags[SAME_SESSION_SUBMITS].as_ref().unwrap();
    anyhow::ensure!(
        rebuilt_tag != session_tags[0],
        "rebuilt client did NOT get a fresh sender tag"
    );

    println!();
    println!("smoke: PASS");
    println!("  - {SAME_SESSION_SUBMITS} submits from one session all carried sender tag {}", session_tags[0]);
    println!("  - rebuilt client presented fresh tag {rebuilt_tag}");
    println!("  - every submit was anonymous (tag present, no exposed self-address path)");
    println!("  - every ack was {ACK_BYTES} bytes and correlated");
    Ok(())
}

/// LookupV1-shaped exchange: 64-byte request with a configurable attached-SURB
/// count, answered by a full 64 KiB anonymous reply. The elapsed time exposes
/// whether the reply fit the attached SURBs or needed re-request round trips.
async fn cmd_lookup(network: &Path, surbs: u32) -> Result<()> {
    println!("[hub] connecting...");
    let mut hub = connect_client(network).await?;
    let hub_addr = *hub.nym_address();
    let hub_sender = hub.split_sender();

    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let hub_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                msgs = hub.wait_for_messages() => {
                    let Some(msgs) = msgs else { break };
                    for m in msgs {
                        if m.message.is_empty() {
                            continue;
                        }
                        let Some(tag) = m.sender_tag else {
                            eprintln!("[hub] request had no sender tag");
                            continue;
                        };
                        let reply = vec![0xABu8; FRAME_BYTES];
                        let t0 = Instant::now();
                        // NOTE: send_reply returning only means the reply was
                        // handed to the reply controller. With too few SURBs it
                        // buffers fragments and re-requests more behind the
                        // scenes; the shim-side elapsed time below is the
                        // number that matters.
                        match hub_sender.send_reply(tag, reply).await {
                            Ok(()) => println!(
                                "[hub] send_reply(64 KiB) accepted in {:.2?}",
                                t0.elapsed()
                            ),
                            Err(e) => eprintln!("[hub] send_reply failed: {e}"),
                        }
                    }
                }
            }
        }
        hub.disconnect().await;
    });

    println!("[shim] connecting...");
    let mut shim = connect_client(network).await?;
    let request = vec![0x01u8; 64];
    let t0 = Instant::now();
    shim.send_message(hub_addr, request, IncludedSurbs::new(surbs))
        .await?;
    println!("[shim] 64-byte lookup sent with {surbs} attached SURBs");
    let reply = wait_nonempty(&mut shim, Duration::from_secs(300)).await?;
    let elapsed = t0.elapsed();
    anyhow::ensure!(
        reply.len() == FRAME_BYTES,
        "reply was {} bytes, expected {FRAME_BYTES}",
        reply.len()
    );
    shim.disconnect().await;
    let _ = stop_tx.send(());
    hub_task.await?;

    println!();
    println!("lookup: PASS");
    println!("  - surbs={surbs}: full 64 KiB anonymous reply reassembled in {elapsed:.2?}");
    println!("  - compare across surb counts: a jump in elapsed time marks the");
    println!("    re-request threshold (the number M4's lookup constant must clear)");
    Ok(())
}
