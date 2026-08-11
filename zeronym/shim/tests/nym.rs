//! The shim's mixnet transport, exercised by holding the driver ends of its
//! channels: the test reads what would go onto the mixnet and writes what
//! would come back, so the whole submit path (framing, correlation, timeout,
//! refusal mapping, filtering) runs with no SDK and no fake client, exactly as
//! the hub's listener tests drive `run_listener`.

use std::time::Duration;

use tokio::sync::mpsc;
use zero_indexer_shim::hub::{HubTransport, Submit};
use zero_indexer_shim::nym::{run_transport, NymError, NymHandle, OutFrame};
use zero_indexer_shim::wire::{
    self, AckKind, AckRefusal, FRAME_BYTES, MAX_NYM_TX_BYTES,
};

/// A V6 migration fixture: real, parseable transaction bytes (shared with the
/// classifier's vector tests), so the locally computed txid is a real hash.
const V6_MIGRATION: &[u8] = include_bytes!("fixtures/v6_migration.bin");

/// The driver ends of a running transport: what the mixnet would see, and the
/// way back in.
struct Driver {
    handle: NymHandle,
    from_transport: mpsc::Receiver<OutFrame>,
    to_transport: mpsc::Sender<Vec<u8>>,
}

/// Spawn `run_transport` and hand back its driver ends. The timeout is short:
/// these tests either answer promptly or assert the timeout itself.
fn start(timeout: Duration) -> Driver {
    let (req_tx, req_rx) = mpsc::channel(8);
    let (out_tx, out_rx) = mpsc::channel(8);
    let (in_tx, in_rx) = mpsc::channel(8);
    tokio::spawn(run_transport(req_rx, out_tx, in_rx));
    Driver {
        handle: NymHandle::new(req_tx, timeout),
        from_transport: out_rx,
        to_transport: in_tx,
    }
}

/// Read the next outbound frame and decode it back to (nonce, tx).
async fn next_frame(driver: &mut Driver) -> ([u8; 16], Vec<u8>) {
    let out = driver
        .from_transport
        .recv()
        .await
        .expect("an outbound frame");
    assert_eq!(out.frame.len(), FRAME_BYTES, "every submit is a full frame");
    let (nonce, tx) = wire::decode_submit(&out.frame).expect("outbound frame decodes");
    (nonce, tx.to_vec())
}

#[tokio::test]
async fn a_submit_is_framed_sent_and_acked() {
    let mut driver = start(Duration::from_secs(5));
    let handle = driver.handle.clone();
    let submit = tokio::spawn(async move { handle.submit(b"tx bytes").await });

    let (nonce, tx) = next_frame(&mut driver).await;
    assert_eq!(tx, b"tx bytes");
    driver
        .to_transport
        .send(wire::encode_ack(&nonce, AckKind::Accepted).to_vec())
        .await
        .unwrap();

    assert_eq!(submit.await.unwrap(), Ok(AckKind::Accepted));
}

#[tokio::test]
async fn every_refusal_comes_back_typed() {
    let mut driver = start(Duration::from_secs(5));
    for refusal in [
        AckRefusal::ExpiryTooTight,
        AckRefusal::TooLarge,
        AckRefusal::QueueFull,
        AckRefusal::TipStale,
        AckRefusal::BadFrame,
    ] {
        let handle = driver.handle.clone();
        let submit = tokio::spawn(async move { handle.submit(b"tx").await });
        let (nonce, _) = next_frame(&mut driver).await;
        driver
            .to_transport
            .send(wire::encode_ack(&nonce, AckKind::Refused(refusal)).to_vec())
            .await
            .unwrap();
        assert_eq!(submit.await.unwrap(), Ok(AckKind::Refused(refusal)));
    }
}

#[tokio::test]
async fn no_ack_times_out() {
    let mut driver = start(Duration::from_millis(50));
    let handle = driver.handle.clone();
    let submit = tokio::spawn(async move { handle.submit(b"tx").await });
    // The frame goes out, but nothing comes back.
    let _ = next_frame(&mut driver).await;
    assert_eq!(submit.await.unwrap(), Err(NymError::Timeout));
}

#[tokio::test]
async fn an_unknown_nonce_is_dropped_and_the_real_ack_still_lands() {
    let mut driver = start(Duration::from_secs(5));
    let handle = driver.handle.clone();
    let submit = tokio::spawn(async move { handle.submit(b"tx").await });
    let (nonce, _) = next_frame(&mut driver).await;

    let mut wrong = nonce;
    wrong[0] ^= 0xff;
    driver
        .to_transport
        .send(wire::encode_ack(&wrong, AckKind::Refused(AckRefusal::QueueFull)).to_vec())
        .await
        .unwrap();
    driver
        .to_transport
        .send(wire::encode_ack(&nonce, AckKind::Accepted).to_vec())
        .await
        .unwrap();

    assert_eq!(submit.await.unwrap(), Ok(AckKind::Accepted));
}

#[tokio::test]
async fn empty_and_undecodable_inbound_messages_are_filtered() {
    let mut driver = start(Duration::from_secs(5));
    let handle = driver.handle.clone();
    let submit = tokio::spawn(async move { handle.submit(b"tx").await });
    let (nonce, _) = next_frame(&mut driver).await;

    // An empty message (SURB replenishment artifact) and garbage bytes, then
    // the real ack: the first two must not disturb the correlation.
    driver.to_transport.send(Vec::new()).await.unwrap();
    driver.to_transport.send(vec![0x77; 30]).await.unwrap();
    driver
        .to_transport
        .send(wire::encode_ack(&nonce, AckKind::Accepted).to_vec())
        .await
        .unwrap();

    assert_eq!(submit.await.unwrap(), Ok(AckKind::Accepted));
}

#[tokio::test]
async fn an_oversized_transaction_is_refused_before_anything_is_sent() {
    let mut driver = start(Duration::from_secs(5));
    let tx = vec![0u8; MAX_NYM_TX_BYTES + 1];
    let err = driver.handle.submit(&tx).await.unwrap_err();
    assert!(matches!(err, NymError::Encode(wire::WireError::TxTooLarge { .. })));
    // Nothing reached the mixnet: the gate is at the frame boundary, and an
    // over-budget transaction is never sent in any form.
    assert!(driver.from_transport.try_recv().is_err());
}

#[tokio::test]
async fn concurrent_submits_correlate_independently() {
    let mut driver = start(Duration::from_secs(5));
    let first_handle = driver.handle.clone();
    let first = tokio::spawn(async move { first_handle.submit(b"first").await });
    let (first_nonce, first_tx) = next_frame(&mut driver).await;
    assert_eq!(first_tx, b"first");

    let second_handle = driver.handle.clone();
    let second = tokio::spawn(async move { second_handle.submit(b"second").await });
    let (second_nonce, second_tx) = next_frame(&mut driver).await;
    assert_eq!(second_tx, b"second");

    // Answer in reverse order; each waiter gets its own verdict.
    driver
        .to_transport
        .send(wire::encode_ack(&second_nonce, AckKind::Refused(AckRefusal::TipStale)).to_vec())
        .await
        .unwrap();
    driver
        .to_transport
        .send(wire::encode_ack(&first_nonce, AckKind::Accepted).to_vec())
        .await
        .unwrap();

    assert_eq!(first.await.unwrap(), Ok(AckKind::Accepted));
    assert_eq!(
        second.await.unwrap(),
        Ok(AckKind::Refused(AckRefusal::TipStale))
    );
}

#[tokio::test]
async fn a_gone_driver_fails_the_waiter_closed() {
    let mut driver = start(Duration::from_secs(5));
    let handle = driver.handle.clone();
    let submit = tokio::spawn(async move { handle.submit(b"tx").await });
    let _ = next_frame(&mut driver).await;

    // The driver dies: both of its channel ends drop. The pending waiter is
    // released immediately with TransportGone, not left to the timeout.
    drop(driver.from_transport);
    drop(driver.to_transport);
    assert_eq!(submit.await.unwrap(), Err(NymError::TransportGone));
}

#[tokio::test]
async fn the_transport_arm_maps_verdicts_for_the_wallet() {
    let mut driver = start(Duration::from_secs(5));
    let transport = HubTransport::from(driver.handle.clone());

    // Accepted: the wallet's txid is computed locally from the bytes (the ack
    // carries none), with the same computation the hub applies, so a real
    // parseable transaction yields a real display-order txid.
    let submit = tokio::spawn(async move { transport.submit(V6_MIGRATION).await });
    let (nonce, tx) = next_frame(&mut driver).await;
    assert_eq!(tx, V6_MIGRATION);
    driver
        .to_transport
        .send(wire::encode_ack(&nonce, AckKind::Accepted).to_vec())
        .await
        .unwrap();
    match submit.await.unwrap().unwrap() {
        Submit::Accepted { txid } => {
            assert_eq!(txid.len(), 64, "display-order txid hex");
            assert!(txid.chars().all(|c| c.is_ascii_hexdigit()));
        }
        other => panic!("expected Accepted, got {other:?}"),
    }

    // Refused: the wallet hears the typed reason string, exactly as the HTTP
    // path surfaces a hub rejection.
    let transport = HubTransport::from(driver.handle.clone());
    let submit = tokio::spawn(async move { transport.submit(b"unparseable").await });
    let (nonce, _) = next_frame(&mut driver).await;
    driver
        .to_transport
        .send(wire::encode_ack(&nonce, AckKind::Refused(AckRefusal::QueueFull)).to_vec())
        .await
        .unwrap();
    assert_eq!(
        submit.await.unwrap().unwrap(),
        Submit::Rejected {
            reason: "queue_full".to_string()
        }
    );
}

#[tokio::test]
async fn an_unparseable_accepted_divert_has_an_empty_txid() {
    // The fail-safe divert: bytes the shim could not parse are still diverted
    // and admitted; there is no txid to show, matching today's behaviour.
    let mut driver = start(Duration::from_secs(5));
    let transport = HubTransport::from(driver.handle.clone());
    let submit = tokio::spawn(async move { transport.submit(b"not a transaction").await });
    let (nonce, _) = next_frame(&mut driver).await;
    driver
        .to_transport
        .send(wire::encode_ack(&nonce, AckKind::Accepted).to_vec())
        .await
        .unwrap();
    assert_eq!(
        submit.await.unwrap().unwrap(),
        Submit::Accepted {
            txid: String::new()
        }
    );
}

#[tokio::test]
async fn a_lookup_over_nym_fails_closed_for_now() {
    let driver = start(Duration::from_secs(5));
    let transport = HubTransport::from(driver.handle.clone());
    assert!(transport.get_transaction(&[0u8; 32]).await.is_err());
}
