//! The hub's mixnet listener, driven end to end through its channels.
//!
//! No SDK and no fake client: the test IS the driver. It feeds `Received`
//! frames into the listener's inbound channel and reads the `Reply` acks off the
//! outbound one, exactly as the real driver will, and asserts the properties that
//! matter across the whole path (what gets admitted, what gets refused, what gets
//! no reply at all, and that a reply goes back to the sender it came from).

use std::sync::Arc;

use tokio::sync::mpsc;

use zero_indexer_hub::batcher::{BatchParams, TipTracker};
use zero_indexer_hub::chain::ChainClient;
use zero_indexer_hub::nym::{run_listener, Received, Reply, SenderTag};
use zero_indexer_hub::queue::Queue;
use zero_indexer_hub::server::Hub;
use zero_indexer_hub::wire::{decode_ack, encode_submit, AckKind, AckRefusal, Nonce, MAX_NYM_TX_BYTES};

/// A real V6 carrying Orchard actions, the same corpus the shim uses.
const V6_MIGRATION: &[u8] = include_bytes!("../../shim/tests/fixtures/v6_migration.bin");

/// A height any fixture expiry clears, so these tests exercise the listener, not
/// expiry arithmetic (that has its own unit tests).
const TIP: u32 = 100;

/// One tag, reused where the sender identity does not matter to the assertion.
const TAG: SenderTag = SenderTag([0x07; 16]);

fn test_hub(observed_tip: Option<u32>) -> Hub {
    let queue = Arc::new(Queue::new());
    let tip = Arc::new(TipTracker::new());
    if let Some(height) = observed_tip {
        tip.observe(height);
    }
    // A never-dialled indexer: the listener only admits, which never touches the
    // chain, so any address satisfies the (non-empty) ChainClient.
    let chain = Arc::new(ChainClient::new(vec!["127.0.0.1:1".parse().unwrap()], None).unwrap());
    Hub {
        queue,
        tip,
        params: BatchParams::default(),
        chain,
    }
}

fn nonce(seed: u8) -> Nonce {
    [seed; 16]
}

fn msg(tag: SenderTag, frame: Vec<u8>) -> Received {
    Received {
        frame,
        sender_tag: tag,
    }
}

/// Run one round: feed every submission in, close the inbound channel, and
/// collect every reply the listener produced.
async fn run_round(hub: Hub, submissions: Vec<Received>) -> Vec<Reply> {
    let (in_tx, in_rx) = mpsc::channel(64);
    let (out_tx, mut out_rx) = mpsc::channel(64);
    tokio::spawn(run_listener(in_rx, out_tx, hub));

    for submission in submissions {
        in_tx.send(submission).await.expect("listener is up");
    }
    drop(in_tx);

    let mut replies = Vec::new();
    while let Some(reply) = out_rx.recv().await {
        replies.push(reply);
    }
    replies
}

fn ack(reply: &Reply) -> AckKind {
    decode_ack(&reply.ack).expect("a well-formed ack").1
}

fn ack_nonce(reply: &Reply) -> Nonce {
    decode_ack(&reply.ack).expect("a well-formed ack").0
}

#[tokio::test]
async fn a_framed_migration_is_admitted_and_acked_accepted() {
    let hub = test_hub(Some(TIP));
    let frame = encode_submit(&nonce(1), V6_MIGRATION).unwrap().to_vec();

    let replies = run_round(hub.clone(), vec![msg(TAG, frame)]).await;

    assert_eq!(replies.len(), 1);
    assert_eq!(ack(&replies[0]), AckKind::Accepted);
    assert_eq!(ack_nonce(&replies[0]), nonce(1), "the ack echoes the request nonce");
    assert_eq!(replies[0].sender_tag, TAG, "the reply goes back to the sender");
    assert_eq!(hub.queue.len(), 1, "the migration is held for the batch");
}

#[tokio::test]
async fn a_duplicate_frame_is_acked_accepted_and_does_not_inflate_the_queue() {
    // Cross-hub submission and honest retries are designed behaviour, so identical
    // bytes collapse to one entry while both submissions still ack accepted.
    let hub = test_hub(Some(TIP));
    let frame = encode_submit(&nonce(2), V6_MIGRATION).unwrap().to_vec();

    let replies = run_round(hub.clone(), vec![msg(TAG, frame.clone()), msg(TAG, frame)]).await;

    assert_eq!(replies.len(), 2);
    assert!(replies.iter().all(|reply| ack(reply) == AckKind::Accepted));
    assert_eq!(hub.queue.len(), 1, "identical bytes collapse to one entry");
}

#[tokio::test]
async fn an_unparseable_payload_is_admitted_not_refused() {
    // REVIEW #5: the shim diverts what it could not read, so refusing an
    // unparseable payload here would invert its fail-safe into a leak. It is
    // queued and published like any other; the node is the only authority.
    let hub = test_hub(Some(TIP));
    let frame = encode_submit(&nonce(3), &[0xab; 64]).unwrap().to_vec();

    let replies = run_round(hub.clone(), vec![msg(TAG, frame)]).await;

    assert_eq!(ack(&replies[0]), AckKind::Accepted);
    assert_eq!(hub.queue.len(), 1);
}

#[tokio::test]
async fn a_frame_with_a_bad_tx_len_is_acked_bad_frame_with_the_recovered_nonce() {
    let hub = test_hub(Some(TIP));
    let mut frame = encode_submit(&nonce(4), V6_MIGRATION).unwrap().to_vec();
    // Corrupt only tx_len so it overruns the frame; the magic and nonce survive,
    // so the listener can still send a correlatable bad_frame ack.
    frame[20..24].copy_from_slice(&((MAX_NYM_TX_BYTES + 1) as u32).to_be_bytes());

    let replies = run_round(hub.clone(), vec![msg(TAG, frame)]).await;

    assert_eq!(replies.len(), 1);
    assert_eq!(ack(&replies[0]), AckKind::Refused(AckRefusal::BadFrame));
    assert_eq!(ack_nonce(&replies[0]), nonce(4), "the recoverable nonce is echoed");
    assert_eq!(hub.queue.len(), 0);
}

#[tokio::test]
async fn an_unrecoverable_frame_gets_logged_and_dropped_with_no_reply() {
    let hub = test_hub(Some(TIP));
    // Right size, wrong magic: there is no trustworthy nonce, so there is nothing
    // to correlate and the shim falls back to its submit timeout.
    let mut frame = encode_submit(&nonce(5), V6_MIGRATION).unwrap().to_vec();
    frame[0] ^= 0xff;

    let replies = run_round(hub.clone(), vec![msg(TAG, frame)]).await;

    assert!(replies.is_empty(), "a frame with no recoverable nonce gets no reply");
    assert_eq!(hub.queue.len(), 0);
}

#[tokio::test]
async fn a_stale_tip_is_acked_tip_stale() {
    // A tracker that never observed a height is stale by definition, so admission
    // stops and fails closed rather than trusting an unknown schedule.
    let hub = test_hub(None);
    let frame = encode_submit(&nonce(6), V6_MIGRATION).unwrap().to_vec();

    let replies = run_round(hub.clone(), vec![msg(TAG, frame)]).await;

    assert_eq!(ack(&replies[0]), AckKind::Refused(AckRefusal::TipStale));
    assert_eq!(ack_nonce(&replies[0]), nonce(6));
    assert_eq!(hub.queue.len(), 0);
}

#[tokio::test]
async fn an_empty_message_is_filtered_with_no_reply() {
    // The SDK delivers SURB-replenishment traffic as empty messages; they are not
    // submissions and must not reach the codec.
    let hub = test_hub(Some(TIP));

    let replies = run_round(hub.clone(), vec![msg(TAG, Vec::new())]).await;

    assert!(replies.is_empty());
    assert_eq!(hub.queue.len(), 0);
}

#[tokio::test]
async fn each_reply_goes_back_to_its_own_senders_tag() {
    let hub = test_hub(Some(TIP));
    let a = SenderTag([0xaa; 16]);
    let b = SenderTag([0xbb; 16]);
    let frame_a = encode_submit(&nonce(10), V6_MIGRATION).unwrap().to_vec();
    let frame_b = encode_submit(&nonce(11), V6_MIGRATION).unwrap().to_vec();

    let replies = run_round(hub, vec![msg(a, frame_a), msg(b, frame_b)]).await;

    assert_eq!(replies.len(), 2);
    // Match each reply to its sender by the nonce it carries.
    for reply in &replies {
        match ack_nonce(reply) {
            n if n == nonce(10) => assert_eq!(reply.sender_tag, a),
            n if n == nonce(11) => assert_eq!(reply.sender_tag, b),
            other => panic!("unexpected nonce {other:?}"),
        }
    }
}
