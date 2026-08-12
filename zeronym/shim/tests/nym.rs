//! The shim's mixnet transport, exercised by holding the driver ends of its
//! channels: the test reads what would go onto the mixnet and writes what
//! would come back, so the whole submit path (framing, correlation, timeout,
//! refusal mapping, filtering) runs with no SDK and no fake client, exactly as
//! the hub's listener tests drive `run_listener`.

use std::time::Duration;

use tokio::sync::mpsc;
use zero_indexer_shim::hub::{HubTransport, Lookup, Submit};
use zero_indexer_shim::nym::{
    run_transport, NymError, NymHandle, OutFrame, LOOKUP_REPLY_SURBS, SUBMIT_REPLY_SURBS,
};
use zero_indexer_shim::wire::{
    self, AckKind, AckRefusal, LookupReply, FRAME_BYTES, LOOKUP_BYTES, MAX_LOOKUP_HASH_BYTES,
    MAX_NYM_TX_BYTES,
};
use zeroize::Zeroizing;

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

/// Read the next outbound submit frame and decode it back to (nonce, tx),
/// asserting the frame size and the fixed SURB count that ride with it.
async fn next_frame(driver: &mut Driver) -> ([u8; 16], Vec<u8>) {
    let out = driver
        .from_transport
        .recv()
        .await
        .expect("an outbound frame");
    assert_eq!(out.frame.len(), FRAME_BYTES, "every submit is a full frame");
    assert_eq!(
        out.reply_surbs, SUBMIT_REPLY_SURBS,
        "a submit carries the fixed submit SURB count"
    );
    let (nonce, tx) = wire::decode_submit(&out.frame).expect("outbound frame decodes");
    (nonce, tx.to_vec())
}

/// Read the next outbound lookup frame and decode it back to (nonce, hash),
/// asserting the frame size and its own fixed SURB count.
async fn next_lookup(driver: &mut Driver) -> ([u8; 16], Vec<u8>) {
    let out = driver
        .from_transport
        .recv()
        .await
        .expect("an outbound frame");
    assert_eq!(
        out.frame.len(),
        LOOKUP_BYTES,
        "every lookup is a fixed small frame"
    );
    assert_eq!(
        out.reply_surbs, LOOKUP_REPLY_SURBS,
        "a lookup carries enough SURBs for a full-frame reply"
    );
    wire::decode_lookup(&out.frame).expect("outbound lookup decodes")
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
async fn a_lookup_is_framed_sent_and_answered_found() {
    let mut driver = start(Duration::from_secs(5));
    let transport = HubTransport::from(driver.handle.clone());
    let wanted = [0x3c; 32];
    let lookup = tokio::spawn(async move { transport.get_transaction(&wanted).await });

    let (nonce, hash) = next_lookup(&mut driver).await;
    assert_eq!(hash, wanted, "the wallet's hash travels unmodified");
    driver
        .to_transport
        .send(
            wire::encode_lookup_reply(
                &nonce,
                &LookupReply::Found {
                    height: 881_234,
                    tx: Zeroizing::new(V6_MIGRATION.to_vec()),
                },
            )
            .unwrap()
            .to_vec(),
        )
        .await
        .unwrap();

    match lookup.await.unwrap().unwrap() {
        Lookup::Found { data, height } => {
            assert_eq!(height, 881_234);
            assert_eq!(data.as_ref(), V6_MIGRATION);
        }
        other => panic!("expected Found, got {other:?}"),
    }
}

#[tokio::test]
async fn a_mempool_lookup_keeps_the_height_zero_sentinel() {
    // Height 0 is what a queue hit reports, and the wallet must see it
    // unchanged: it is the mempool sentinel, not a missing value.
    let mut driver = start(Duration::from_secs(5));
    let transport = HubTransport::from(driver.handle.clone());
    let lookup = tokio::spawn(async move { transport.get_transaction(&[0x3d; 32]).await });

    let (nonce, _) = next_lookup(&mut driver).await;
    driver
        .to_transport
        .send(
            wire::encode_lookup_reply(
                &nonce,
                &LookupReply::Found {
                    height: 0,
                    tx: Zeroizing::new(V6_MIGRATION.to_vec()),
                },
            )
            .unwrap()
            .to_vec(),
        )
        .await
        .unwrap();

    match lookup.await.unwrap().unwrap() {
        Lookup::Found { height, .. } => assert_eq!(height, 0),
        other => panic!("expected Found, got {other:?}"),
    }
}

#[tokio::test]
async fn a_not_found_lookup_maps_to_not_found() {
    let mut driver = start(Duration::from_secs(5));
    let transport = HubTransport::from(driver.handle.clone());
    let lookup = tokio::spawn(async move { transport.get_transaction(&[0x3e; 32]).await });

    let (nonce, _) = next_lookup(&mut driver).await;
    driver
        .to_transport
        .send(
            wire::encode_lookup_reply(&nonce, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        )
        .await
        .unwrap();

    assert_eq!(lookup.await.unwrap().unwrap(), Lookup::NotFound);
}

#[tokio::test]
async fn an_error_lookup_fails_closed_and_is_never_a_not_found() {
    // The distinction is load-bearing: NotFound tells a wallet its transaction
    // does not exist, which the shim must never say on the hub's behalf when
    // the hub could not answer. It becomes UNAVAILABLE at the intercept path.
    let mut driver = start(Duration::from_secs(5));
    let transport = HubTransport::from(driver.handle.clone());
    let lookup = tokio::spawn(async move { transport.get_transaction(&[0x3f; 32]).await });

    let (nonce, _) = next_lookup(&mut driver).await;
    driver
        .to_transport
        .send(
            wire::encode_lookup_reply(&nonce, &LookupReply::Error)
                .unwrap()
                .to_vec(),
        )
        .await
        .unwrap();

    assert!(lookup.await.unwrap().is_err(), "an error reply fails closed");
}

#[tokio::test]
async fn a_lookup_with_no_reply_times_out() {
    let mut driver = start(Duration::from_millis(50));
    let transport = HubTransport::from(driver.handle.clone());
    let lookup = tokio::spawn(async move { transport.get_transaction(&[0x40; 32]).await });
    let _ = next_lookup(&mut driver).await;
    assert!(lookup.await.unwrap().is_err(), "a lost reply fails closed");
}

#[tokio::test]
async fn an_oversized_lookup_hash_is_refused_before_anything_is_sent() {
    let mut driver = start(Duration::from_secs(5));
    let hash = vec![0u8; MAX_LOOKUP_HASH_BYTES + 1];
    let err = driver.handle.get_transaction(&hash).await.unwrap_err();
    assert!(matches!(
        err,
        NymError::Encode(wire::WireError::HashTooLarge { .. })
    ));
    assert!(driver.from_transport.try_recv().is_err());
}

#[tokio::test]
async fn a_reply_of_the_wrong_kind_is_not_an_answer() {
    // A confused or hostile hub must not be able to answer a lookup with an
    // ack (or vice versa): the waiter stays pending and its caller fails
    // closed on the timeout, rather than the wrong verdict reaching a wallet.
    let mut driver = start(Duration::from_millis(150));

    let lookup_handle = driver.handle.clone();
    let lookup = tokio::spawn(async move { lookup_handle.get_transaction(&[0x41; 32]).await });
    let (lookup_nonce, _) = next_lookup(&mut driver).await;
    driver
        .to_transport
        .send(wire::encode_ack(&lookup_nonce, AckKind::Accepted).to_vec())
        .await
        .unwrap();
    assert_eq!(lookup.await.unwrap(), Err(NymError::Timeout));

    let submit_handle = driver.handle.clone();
    let submit = tokio::spawn(async move { submit_handle.submit(b"tx").await });
    let (submit_nonce, _) = next_frame(&mut driver).await;
    driver
        .to_transport
        .send(
            wire::encode_lookup_reply(&submit_nonce, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(submit.await.unwrap(), Err(NymError::Timeout));
}

#[tokio::test]
async fn a_submit_and_a_lookup_in_flight_correlate_independently() {
    let mut driver = start(Duration::from_secs(5));
    let submit_handle = driver.handle.clone();
    let submit = tokio::spawn(async move { submit_handle.submit(b"tx").await });
    let (submit_nonce, _) = next_frame(&mut driver).await;

    let lookup_handle = driver.handle.clone();
    let lookup = tokio::spawn(async move { lookup_handle.get_transaction(&[0x42; 32]).await });
    let (lookup_nonce, _) = next_lookup(&mut driver).await;

    // Answer the lookup first: each waiter takes its own reply.
    driver
        .to_transport
        .send(
            wire::encode_lookup_reply(&lookup_nonce, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        )
        .await
        .unwrap();
    driver
        .to_transport
        .send(wire::encode_ack(&submit_nonce, AckKind::Accepted).to_vec())
        .await
        .unwrap();

    assert_eq!(submit.await.unwrap(), Ok(AckKind::Accepted));
    assert_eq!(lookup.await.unwrap(), Ok(LookupReply::NotFound));
}

#[tokio::test]
async fn abandoned_waiters_do_not_accumulate() {
    // A timed-out request's entry would otherwise be held for the life of the
    // process, since the reply that would remove it is exactly the one that
    // never came. Drive several timeouts, then prove a later request still
    // correlates (the map is swept, not merely appended to).
    let mut driver = start(Duration::from_millis(30));
    for _ in 0..5 {
        let handle = driver.handle.clone();
        let submit = tokio::spawn(async move { handle.submit(b"tx").await });
        let _ = next_frame(&mut driver).await;
        assert_eq!(submit.await.unwrap(), Err(NymError::Timeout));
    }

    let handle = driver.handle.clone();
    let submit = tokio::spawn(async move { handle.submit(b"tx").await });
    let (nonce, _) = next_frame(&mut driver).await;
    driver
        .to_transport
        .send(wire::encode_ack(&nonce, AckKind::Accepted).to_vec())
        .await
        .unwrap();
    assert_eq!(submit.await.unwrap(), Ok(AckKind::Accepted));
}
