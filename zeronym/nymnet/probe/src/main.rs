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
/// Matches LOOKUP_BYTES in the shim/hub wire modules.
const LOOKUP_BYTES: usize = 64;
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
        Some("wire") => {
            let network = PathBuf::from(args.get(2).context("usage: wire <network.json> <vectors.bin>")?);
            let vectors = PathBuf::from(args.get(3).context("usage: wire <network.json> <vectors.bin>")?);
            cmd_wire(&network, &vectors).await
        }
        Some("e2e") => {
            let network = PathBuf::from(args.get(2).context("usage: e2e <network.json>")?);
            cmd_e2e(&network).await
        }
        _ => bail!("usage: probe topology|smoke|lookup|wire|e2e ..."),
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

/// Carry the crates' committed golden-vector frames through the real mixnet.
///
/// Reads `wire_v1_vectors.bin` (the fixture both the shim and hub commit
/// byte-identically), sends its actual `SubmitV1` and `LookupV1` bytes from a
/// shim-side client, has a hub-side client verify them byte-for-byte and
/// answer with the fixture's own `AckV1` and found-`LookupReplyV1`, then
/// verifies the replies byte-for-byte AND with an independent offset-level
/// decode of the layout (a third implementation, so it doubles as a
/// differential check on the codec). Proves the committed frames ride SDK
/// chunking, sphinx transport, and SURB replies unmodified.
async fn cmd_wire(network: &Path, vectors: &Path) -> Result<()> {
    const ACKS: usize = 6;
    const REPLIES: usize = 4;
    let data = std::fs::read(vectors)
        .with_context(|| format!("reading {}", vectors.display()))?;
    let expect_len = FRAME_BYTES + ACKS * ACK_BYTES + LOOKUP_BYTES + REPLIES * FRAME_BYTES;
    anyhow::ensure!(
        data.len() == expect_len,
        "fixture is {} bytes, expected {expect_len} (vector stream layout changed?)",
        data.len()
    );
    let submit = data[..FRAME_BYTES].to_vec();
    let ack_accepted = data[FRAME_BYTES..FRAME_BYTES + ACK_BYTES].to_vec();
    let lookup_off = FRAME_BYTES + ACKS * ACK_BYTES;
    let lookup = data[lookup_off..lookup_off + LOOKUP_BYTES].to_vec();
    let reply_off = lookup_off + LOOKUP_BYTES;
    let reply_found = data[reply_off..reply_off + FRAME_BYTES].to_vec();

    println!("[hub] connecting...");
    let mut hub = connect_client(network).await?;
    let hub_addr = *hub.nym_address();
    let hub_sender = hub.split_sender();
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let (submit_c, lookup_c) = (submit.clone(), lookup.clone());
    let (ack_c, reply_c) = (ack_accepted.clone(), reply_found.clone());
    let hub_task = tokio::spawn(async move {
        let mut failures: Vec<String> = Vec::new();
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
                            failures.push("a request arrived without a sender tag".into());
                            continue;
                        };
                        let (name, expected, reply) = match m.message.get(..4) {
                            Some(magic) if magic == b"ZNS1" => ("SubmitV1", &submit_c, ack_c.clone()),
                            Some(magic) if magic == b"ZNL1" => ("LookupV1", &lookup_c, reply_c.clone()),
                            _ => {
                                failures.push(format!(
                                    "unexpected frame: {} bytes, magic {:?}",
                                    m.message.len(),
                                    m.message.get(..4)
                                ));
                                continue;
                            }
                        };
                        if &m.message == expected {
                            println!("[hub] {name} arrived byte-identical ({} bytes)", m.message.len());
                        } else {
                            failures.push(format!("{name} bytes were altered in transit"));
                        }
                        if let Err(e) = hub_sender.send_reply(tag, reply).await {
                            failures.push(format!("send_reply failed: {e}"));
                        }
                    }
                }
            }
        }
        hub.disconnect().await;
        failures
    });

    println!("[shim] connecting...");
    let mut shim = connect_client(network).await?;

    let t0 = Instant::now();
    shim.send_message(hub_addr, submit.clone(), IncludedSurbs::new(13))
        .await?;
    let ack = wait_nonempty(&mut shim, Duration::from_secs(120)).await?;
    anyhow::ensure!(ack == ack_accepted, "AckV1 bytes were altered in transit");
    // Independent offset-level decode: magic, echoed nonce, accepted, no refusal.
    anyhow::ensure!(&ack[0..4] == b"ZNA1" && ack[4..20] == submit[4..20]);
    anyhow::ensure!(ack[20] == 0 && ack[21] == 0);
    anyhow::ensure!(ack[22..].iter().all(|&b| b == 0), "ack padding not zero");
    println!("[shim] SubmitV1 -> byte-identical accepted AckV1 in {:.2?}", t0.elapsed());

    let t1 = Instant::now();
    shim.send_message(hub_addr, lookup.clone(), IncludedSurbs::new(60))
        .await?;
    let reply = wait_nonempty(&mut shim, Duration::from_secs(300)).await?;
    anyhow::ensure!(reply == reply_found, "LookupReplyV1 bytes were altered in transit");
    // Independent offset-level decode: magic, echoed nonce, found at the
    // canonical height, the canonical 64-byte transaction, zero padding.
    anyhow::ensure!(&reply[0..4] == b"ZNR1" && reply[4..20] == lookup[4..20]);
    anyhow::ensure!(reply[20] == 0, "disposition should be found");
    let height = u64::from_be_bytes(reply[21..29].try_into().expect("eight bytes"));
    anyhow::ensure!(height == 778_899, "height was {height}");
    let tx_len = u32::from_be_bytes(reply[29..33].try_into().expect("four bytes")) as usize;
    anyhow::ensure!(tx_len == 64, "tx_len was {tx_len}");
    let tx_ok = reply[33..33 + 64]
        .iter()
        .enumerate()
        .all(|(i, &b)| b == i as u8);
    anyhow::ensure!(tx_ok, "transaction bytes do not match the canonical vector");
    anyhow::ensure!(reply[33 + 64..].iter().all(|&b| b == 0), "reply padding not zero");
    println!("[shim] LookupV1 -> byte-identical found LookupReplyV1 in {:.2?}", t1.elapsed());

    shim.disconnect().await;
    let _ = stop_tx.send(());
    let failures = hub_task.await?;
    anyhow::ensure!(failures.is_empty(), "hub-side failures: {failures:?}");

    println!();
    println!("wire: PASS");
    println!("  - the committed golden vectors rode the real mixnet unmodified,");
    println!("    both directions, and an independent offset-level decode agreed");
    println!("    with the crates' codecs on every field");
    Ok(())
}

/// The real shim transport and the real hub listener, end to end over the real
/// mixnet.
///
/// This is the M5 prototype: the shim side runs `zero_indexer_shim`'s actual
/// `run_transport` correlator behind `HubTransport::Nym`, the hub side runs
/// `zero_indexer_hub`'s actual `run_listener` over the actual `Hub` admission
/// and lookup cores, and each is glued to its own real Nym client by a driver
/// task that does nothing but move bytes -- exactly the boundary both crates
/// drew for the SDK. Asserts, over the public codepaths only:
///
/// 1. a submitted migration round-trips to an accepted ack, the wallet-facing
///    txid is computed locally, and the hub's queue holds the bytes;
/// 2. a lookup for that txid is answered found at the mempool sentinel with
///    the exact bytes (the queue path; the indexer is unreachable on purpose);
/// 3. a lookup for an unknown hash fails CLOSED (`error`, because the indexer
///    cannot be reached), never a guess.
async fn cmd_e2e(network: &Path) -> Result<()> {
    use std::sync::Arc;

    use nym_sdk::mixnet::AnonymousSenderTag;
    use zero_indexer_hub::batcher::{BatchParams, TipTracker};
    use zero_indexer_hub::chain::ChainClient;
    use zero_indexer_hub::nym as hub_nym;
    use zero_indexer_hub::queue::Queue;
    use zero_indexer_hub::server::Hub;
    use zero_indexer_shim::hub::{HubTransport, Submit};
    use zero_indexer_shim::nym as shim_nym;
    use zero_indexer_shim::wire as shim_wire;

    const V6_MIGRATION: &[u8] = include_bytes!("../../../shim/tests/fixtures/v6_migration.bin");
    const LOOKUP_SURBS: u32 = 60;

    // ---- The hub half: a real listener over the real cores, driven by a glue
    // task that is the M5 hub driver in miniature.
    println!("[hub] connecting...");
    let mut hub_client = connect_client(network).await?;
    let hub_addr = *hub_client.nym_address();
    let hub_sender = hub_client.split_sender();
    let queue = Arc::new(Queue::new());
    let tip = Arc::new(TipTracker::new());
    tip.observe(100);
    // Unreachable on purpose: assertion 2 must be answered by the queue alone,
    // and assertion 3 must fail closed.
    let chain = Arc::new(ChainClient::new(vec!["127.0.0.1:1".parse().unwrap()], None).unwrap());
    let hub = Hub {
        queue: queue.clone(),
        tip,
        params: BatchParams::default(),
        chain,
    };
    let (hub_in_tx, hub_in_rx) = tokio::sync::mpsc::channel(64);
    let (hub_out_tx, mut hub_out_rx) = tokio::sync::mpsc::channel::<hub_nym::Reply>(64);
    tokio::spawn(hub_nym::run_listener(hub_in_rx, hub_out_tx, hub));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                msgs = hub_client.wait_for_messages() => {
                    let Some(msgs) = msgs else { break };
                    for m in msgs {
                        // No tag means no way to reply (SURB artifacts arrive
                        // tagless); the listener's own empty-filter handles the
                        // rest.
                        let Some(tag) = m.sender_tag else { continue };
                        let _ = hub_in_tx
                            .send(hub_nym::Received {
                                frame: m.message,
                                sender_tag: hub_nym::SenderTag(tag.to_bytes()),
                            })
                            .await;
                    }
                }
                reply = hub_out_rx.recv() => {
                    let Some(reply) = reply else { break };
                    let tag = AnonymousSenderTag::from_bytes(reply.sender_tag.0);
                    if let Err(e) = hub_sender.send_reply(tag, reply.frame.to_vec()).await {
                        eprintln!("[hub] send_reply failed: {e}");
                    }
                }
            }
        }
    });
    println!("[hub] listening at {hub_addr}");

    // ---- The shim half: the real correlator behind HubTransport::Nym, driven
    // by a glue task that is the M5 shim driver in miniature.
    println!("[shim] connecting...");
    let mut shim_client = connect_client(network).await?;
    let (req_tx, req_rx) = tokio::sync::mpsc::channel(8);
    let (shim_out_tx, mut shim_out_rx) = tokio::sync::mpsc::channel::<shim_nym::OutFrame>(8);
    let (shim_in_tx, shim_in_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(shim_nym::run_transport(req_rx, shim_out_tx, shim_in_rx));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                out = shim_out_rx.recv() => {
                    let Some(out) = out else { break };
                    if let Err(e) = shim_client
                        .send_message(hub_addr, out.frame.to_vec(), IncludedSurbs::new(13))
                        .await
                    {
                        eprintln!("[shim] send failed: {e}");
                    }
                }
                msgs = shim_client.wait_for_messages() => {
                    let Some(msgs) = msgs else { break };
                    for m in msgs {
                        let _ = shim_in_tx.send(m.message).await;
                    }
                }
            }
        }
    });
    let transport = HubTransport::from(shim_nym::NymHandle::new(
        req_tx,
        Duration::from_secs(60),
    ));

    // 1) Submit through the whole real path.
    let t0 = Instant::now();
    let verdict = transport
        .submit(V6_MIGRATION)
        .await
        .map_err(|e| anyhow::anyhow!("submit failed: {e}"))?;
    let txid = match verdict {
        Submit::Accepted { txid } => txid,
        other => bail!("expected an accepted submit, got {other:?}"),
    };
    anyhow::ensure!(txid.len() == 64, "locally computed txid is display hex");
    anyhow::ensure!(queue.len() == 1, "the hub queue holds the migration");
    println!("[e2e] submit accepted in {:.2?}, txid computed locally, queue holds 1", t0.elapsed());

    // 2) Look the migration up by its txid: answered from the queue, found at
    // the mempool sentinel, byte-identical.
    // Fixed nonces: correlation only has to hold within this one probe run,
    // and skipping a rand dependency keeps the merged shim+hub+nym dependency
    // graph resolvable.
    let mut poll_client = connect_client(network).await?;
    let nonce = [0xA7u8; 16];
    let request = shim_wire::encode_lookup(&nonce, &hex::decode(&txid)?)?;
    let t1 = Instant::now();
    poll_client
        .send_message(hub_addr, request.to_vec(), IncludedSurbs::new(LOOKUP_SURBS))
        .await?;
    let reply = wait_nonempty(&mut poll_client, Duration::from_secs(120)).await?;
    let (echoed, verdict) = shim_wire::decode_lookup_reply(&reply)
        .map_err(|e| anyhow::anyhow!("lookup reply did not decode: {e}"))?;
    anyhow::ensure!(echoed == nonce, "the reply echoes the lookup nonce");
    match verdict {
        shim_wire::LookupReply::Found { height, tx } => {
            anyhow::ensure!(height == 0, "a queued entry is at the mempool sentinel");
            anyhow::ensure!(tx.as_slice() == V6_MIGRATION, "the exact bytes come back");
        }
        other => bail!("expected found, got {other:?}"),
    }
    println!("[e2e] lookup answered from the queue in {:.2?}", t1.elapsed());

    // 3) An unknown hash fails closed: the indexer is unreachable, so the only
    // honest answer is error, and that is what must come back.
    let nonce = [0xA8u8; 16];
    let request = shim_wire::encode_lookup(&nonce, &[0xEE; 32])?;
    let t2 = Instant::now();
    poll_client
        .send_message(hub_addr, request.to_vec(), IncludedSurbs::new(LOOKUP_SURBS))
        .await?;
    let reply = wait_nonempty(&mut poll_client, Duration::from_secs(120)).await?;
    let (echoed, verdict) = shim_wire::decode_lookup_reply(&reply)
        .map_err(|e| anyhow::anyhow!("lookup reply did not decode: {e}"))?;
    anyhow::ensure!(echoed == nonce && verdict == shim_wire::LookupReply::Error);
    println!("[e2e] unknown hash failed closed (error) in {:.2?}", t2.elapsed());

    poll_client.disconnect().await;
    println!();
    println!("e2e: PASS");
    println!("  - the real shim correlator and the real hub listener, glued to");
    println!("    real Nym clients, round-tripped a submit and both lookup");
    println!("    verdicts over the local mixnet");
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
