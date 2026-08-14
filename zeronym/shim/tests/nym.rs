//! The shim's mixnet transport, exercised by holding the driver ends of its
//! channels: the test reads what would go onto the mixnet and writes what
//! would come back, so the whole submit path (framing, correlation, timeout,
//! refusal mapping, filtering) runs with no SDK and no fake client, exactly as
//! the hub's listener tests drive `run_listener`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
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
    to_transport: mpsc::Sender<Zeroizing<Vec<u8>>>,
}

/// Spawn `run_transport` and hand back its driver ends. The timeout is short:
/// these tests either answer promptly or assert the timeout itself.
fn start(timeout: Duration) -> Driver {
    start_with_targets(timeout, 1).0
}

/// The same, against a hub multi-homed at `targets` addresses (D10), also
/// handing back the in-flight count the transport publishes for the supervisor.
fn start_with_targets(timeout: Duration, targets: usize) -> (Driver, Arc<AtomicUsize>) {
    start_full(timeout, targets, 8)
}

/// The same, with an explicit driver-channel capacity: the number of frames the
/// driver can be handed before it must take one, which is what a real driver
/// mid-emission exhausts.
fn start_full(timeout: Duration, targets: usize, driver_capacity: usize) -> (Driver, Arc<AtomicUsize>) {
    let (req_tx, req_rx) = mpsc::channel(8);
    let (out_tx, out_rx) = mpsc::channel(driver_capacity);
    let (in_tx, in_rx) = mpsc::channel(8);
    let inflight = Arc::new(AtomicUsize::new(0));
    tokio::spawn(run_transport(req_rx, out_tx, in_rx, inflight.clone()));
    (
        Driver {
            handle: NymHandle::new(req_tx, timeout, timeout, Arc::new(AtomicUsize::new(targets))),
            from_transport: out_rx,
            to_transport: in_tx,
        },
        inflight,
    )
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
async fn a_submit_is_framed_and_dispatched() {
    let mut driver = start(Duration::from_secs(5));
    let handle = driver.handle.clone();
    let submit = tokio::spawn(async move { handle.submit(b"tx bytes").await });

    // Best-effort dispatch: the submit answers as soon as the frame is on the
    // mixnet, so the outbound frame is what we assert, not a hub ack (which is
    // never awaited). The frame still carries the exact bytes.
    let (_nonce, tx) = next_frame(&mut driver).await;
    assert_eq!(tx, b"tx bytes");
    assert_eq!(submit.await.unwrap(), Ok(()));
}

#[test]
fn every_refusal_round_trips_through_the_ack_codec() {
    // Dispatch-only submit no longer surfaces the hub's typed refusals (they are
    // a round trip away and not awaited), but the ack wire codec still carries
    // them -- an unmatched ack is decoded before it is dropped -- and the
    // vocabulary must stay stable. Tested at the codec rather than through submit.
    let nonce = [0x5a; 16];
    for refusal in [
        AckRefusal::ExpiryTooTight,
        AckRefusal::TooLarge,
        AckRefusal::QueueFull,
        AckRefusal::TipStale,
        AckRefusal::BadFrame,
    ] {
        let frame = wire::encode_ack(&nonce, AckKind::Refused(refusal));
        let (got_nonce, got_kind) = wire::decode_ack(&frame).expect("an ack decodes");
        assert_eq!(got_nonce, nonce);
        assert_eq!(got_kind, AckKind::Refused(refusal));
    }
}

#[tokio::test]
async fn a_submit_needs_no_ack_to_succeed() {
    let mut driver = start(Duration::from_millis(50));
    let handle = driver.handle.clone();
    let submit = tokio::spawn(async move { handle.submit(b"tx").await });
    // The frame goes out; nothing ever comes back -- the common case under mixnet
    // latency. Best-effort still answers success: the migration is on its way.
    let _ = next_frame(&mut driver).await;
    assert_eq!(submit.await.unwrap(), Ok(()));
}

#[tokio::test]
async fn an_unknown_nonce_is_dropped_and_the_real_lookup_reply_still_lands() {
    // The correlation machinery (shared by submit and lookup) is now exercised
    // through the lookup path, which still awaits its reply.
    let mut driver = start(Duration::from_secs(5));
    let transport = HubTransport::from(driver.handle.clone());
    let lookup = tokio::spawn(async move { transport.get_transaction(&[0x42; 32]).await });
    let (nonce, _) = next_lookup(&mut driver).await;

    // A reply under the wrong nonce must not satisfy the waiter; the real one does.
    let mut wrong = nonce;
    wrong[0] ^= 0xff;
    driver
        .to_transport
        .send(Zeroizing::new(
            wire::encode_lookup_reply(&wrong, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        ))
        .await
        .unwrap();
    driver
        .to_transport
        .send(Zeroizing::new(
            wire::encode_lookup_reply(
                &nonce,
                &LookupReply::Found {
                    height: 5,
                    tx: Zeroizing::new(V6_MIGRATION.to_vec()),
                },
            )
            .unwrap()
            .to_vec(),
        ))
        .await
        .unwrap();

    match lookup.await.unwrap().unwrap() {
        Lookup::Found { height, .. } => assert_eq!(height, 5),
        other => panic!("expected Found, got {other:?}"),
    }
}

#[tokio::test]
async fn empty_and_undecodable_inbound_messages_do_not_disturb_a_lookup() {
    let mut driver = start(Duration::from_secs(5));
    let transport = HubTransport::from(driver.handle.clone());
    let lookup = tokio::spawn(async move { transport.get_transaction(&[0x43; 32]).await });
    let (nonce, _) = next_lookup(&mut driver).await;

    // An empty message (SURB replenishment artifact) and garbage bytes, then
    // the real reply: the first two must not disturb the correlation.
    driver.to_transport.send(Zeroizing::new(Vec::new())).await.unwrap();
    driver.to_transport.send(Zeroizing::new(vec![0x77; 30])).await.unwrap();
    driver
        .to_transport
        .send(Zeroizing::new(
            wire::encode_lookup_reply(&nonce, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        ))
        .await
        .unwrap();

    assert_eq!(lookup.await.unwrap().unwrap(), Lookup::NotFound);
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
async fn concurrent_lookups_correlate_independently() {
    // The concurrent-correlation machinery, exercised through the lookup path
    // (submit no longer awaits a reply, so it cannot drive this).
    let mut driver = start(Duration::from_secs(5));
    let first_transport = HubTransport::from(driver.handle.clone());
    let first = tokio::spawn(async move { first_transport.get_transaction(&[0x44; 32]).await });
    let (first_nonce, _) = next_lookup(&mut driver).await;

    let second_transport = HubTransport::from(driver.handle.clone());
    let second = tokio::spawn(async move { second_transport.get_transaction(&[0x45; 32]).await });
    let (second_nonce, _) = next_lookup(&mut driver).await;

    // Answer in reverse order; each waiter gets its own reply, matched by nonce.
    driver
        .to_transport
        .send(Zeroizing::new(
            wire::encode_lookup_reply(&second_nonce, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        ))
        .await
        .unwrap();
    driver
        .to_transport
        .send(Zeroizing::new(
            wire::encode_lookup_reply(
                &first_nonce,
                &LookupReply::Found {
                    height: 7,
                    tx: Zeroizing::new(V6_MIGRATION.to_vec()),
                },
            )
            .unwrap()
            .to_vec(),
        ))
        .await
        .unwrap();

    match first.await.unwrap().unwrap() {
        Lookup::Found { height, .. } => assert_eq!(height, 7),
        other => panic!("expected Found, got {other:?}"),
    }
    assert_eq!(second.await.unwrap().unwrap(), Lookup::NotFound);
}

#[tokio::test]
async fn a_gone_driver_fails_a_pending_lookup_closed() {
    let mut driver = start(Duration::from_secs(5));
    let handle = driver.handle.clone();
    let lookup = tokio::spawn(async move { handle.get_transaction(&[0x46; 32]).await });
    let _ = next_lookup(&mut driver).await;

    // The driver dies: both of its channel ends drop. The pending lookup waiter is
    // released immediately with TransportGone, not left to the timeout.
    drop(driver.from_transport);
    drop(driver.to_transport);
    assert_eq!(lookup.await.unwrap(), Err(NymError::TransportGone));
}

#[tokio::test]
async fn the_transport_arm_maps_a_dispatch_to_the_wallet() {
    let mut driver = start(Duration::from_secs(5));
    let transport = HubTransport::from(driver.handle.clone());

    // A dispatch answers Submit::Accepted with the txid computed locally from the
    // bytes (the ack carries none and is not awaited), with the same computation
    // the hub applies, so a real parseable transaction yields a real display-order
    // txid. There is no Refused mapping: dispatch-only never returns a hub verdict.
    let submit = tokio::spawn(async move { transport.submit(V6_MIGRATION).await });
    let (_nonce, tx) = next_frame(&mut driver).await;
    assert_eq!(tx, V6_MIGRATION);
    match submit.await.unwrap().unwrap() {
        Submit::Accepted { txid } => {
            assert_eq!(txid.len(), 64, "display-order txid hex");
            assert!(txid.chars().all(|c| c.is_ascii_hexdigit()));
        }
        other => panic!("expected Accepted, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unparseable_dispatch_has_an_empty_txid() {
    // The fail-safe divert: bytes the shim could not parse are still dispatched;
    // there is no txid to show, matching the HTTP path.
    let mut driver = start(Duration::from_secs(5));
    let transport = HubTransport::from(driver.handle.clone());
    let submit = tokio::spawn(async move { transport.submit(b"not a transaction").await });
    let _ = next_frame(&mut driver).await;
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
        .send(Zeroizing::new(
            wire::encode_lookup_reply(
                &nonce,
                &LookupReply::Found {
                    height: 881_234,
                    tx: Zeroizing::new(V6_MIGRATION.to_vec()),
                },
            )
            .unwrap()
            .to_vec(),
        ))
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
        .send(Zeroizing::new(
            wire::encode_lookup_reply(
                &nonce,
                &LookupReply::Found {
                    height: 0,
                    tx: Zeroizing::new(V6_MIGRATION.to_vec()),
                },
            )
            .unwrap()
            .to_vec(),
        ))
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
        .send(Zeroizing::new(
            wire::encode_lookup_reply(&nonce, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        ))
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
        .send(Zeroizing::new(
            wire::encode_lookup_reply(&nonce, &LookupReply::Error)
                .unwrap()
                .to_vec(),
        ))
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
    // ack: the waiter stays pending and its caller fails closed on the timeout,
    // rather than the wrong verdict reaching a wallet.
    let mut driver = start(Duration::from_millis(150));

    let lookup_handle = driver.handle.clone();
    let lookup = tokio::spawn(async move { lookup_handle.get_transaction(&[0x41; 32]).await });
    let (lookup_nonce, _) = next_lookup(&mut driver).await;
    driver
        .to_transport
        .send(Zeroizing::new(wire::encode_ack(&lookup_nonce, AckKind::Accepted).to_vec()))
        .await
        .unwrap();
    assert_eq!(lookup.await.unwrap(), Err(NymError::Timeout));
}

#[tokio::test]
async fn a_submit_and_a_lookup_in_flight_do_not_interfere() {
    let mut driver = start(Duration::from_secs(5));
    let submit_handle = driver.handle.clone();
    let submit = tokio::spawn(async move { submit_handle.submit(b"tx").await });
    let (_submit_nonce, _) = next_frame(&mut driver).await;

    let lookup_handle = driver.handle.clone();
    let lookup = tokio::spawn(async move { lookup_handle.get_transaction(&[0x42; 32]).await });
    let (lookup_nonce, _) = next_lookup(&mut driver).await;

    // The submit dispatched (best-effort success); the lookup gets its own reply,
    // matched by nonce, undisturbed by the submit's frame or its dropped waiter.
    driver
        .to_transport
        .send(Zeroizing::new(
            wire::encode_lookup_reply(&lookup_nonce, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        ))
        .await
        .unwrap();

    assert_eq!(submit.await.unwrap(), Ok(()));
    assert_eq!(lookup.await.unwrap(), Ok(LookupReply::NotFound));
}

#[tokio::test]
async fn a_timed_out_address_fails_over_to_the_next() {
    // A Nym address dies with its gateway (D10), so a hub is hosted at several and
    // a silent one must not take the shim's hub path down. Exercised through the
    // lookup path, which still sweeps addresses on timeout; dispatch-only submit
    // sends to one address and does not sweep.
    let (mut driver, _inflight) = start_with_targets(Duration::from_millis(60), 3);
    let handle = driver.handle.clone();
    let lookup = tokio::spawn(async move { handle.get_transaction(&[0x50; 32]).await });

    // The first two addresses stay silent; the third answers.
    let first = driver.from_transport.recv().await.expect("first attempt");
    let second = driver.from_transport.recv().await.expect("second attempt");
    let third = driver.from_transport.recv().await.expect("third attempt");
    assert_ne!(
        first.target, second.target,
        "a retry must go to a different address"
    );
    assert_ne!(second.target, third.target);
    let (nonce, _) = wire::decode_lookup(&third.frame).unwrap();
    driver
        .to_transport
        .send(Zeroizing::new(
            wire::encode_lookup_reply(&nonce, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        ))
        .await
        .unwrap();

    assert_eq!(lookup.await.unwrap(), Ok(LookupReply::NotFound));
}

#[tokio::test]
async fn every_attempt_carries_its_own_nonce() {
    // A late reply from an address that was given up on must not be mistaken
    // for the answer of the one that followed it.
    let (mut driver, _inflight) = start_with_targets(Duration::from_millis(40), 2);
    let handle = driver.handle.clone();
    let lookup = tokio::spawn(async move { handle.get_transaction(&[0x51; 32]).await });

    let first = driver.from_transport.recv().await.expect("first attempt");
    let second = driver.from_transport.recv().await.expect("second attempt");
    let (first_nonce, _) = wire::decode_lookup(&first.frame).unwrap();
    let (second_nonce, _) = wire::decode_lookup(&second.frame).unwrap();
    assert_ne!(first_nonce, second_nonce);

    // The abandoned attempt's reply arrives late, under the OLD nonce: it
    // correlates to nothing, so the still-pending second attempt fails closed.
    driver
        .to_transport
        .send(Zeroizing::new(
            wire::encode_lookup_reply(&first_nonce, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(lookup.await.unwrap(), Err(NymError::Timeout));
}

#[tokio::test]
async fn a_final_verdict_is_not_retried_elsewhere() {
    // A NotFound comes from a live hub (only a TIMEOUT means a dead address and
    // moves on). Asking another address would not change the answer, and every
    // extra attempt is another mixnet round trip inside a wallet's call.
    let (mut driver, _inflight) = start_with_targets(Duration::from_secs(5), 3);
    let handle = driver.handle.clone();
    let lookup = tokio::spawn(async move { handle.get_transaction(&[0x52; 32]).await });

    let first = driver.from_transport.recv().await.expect("first attempt");
    let (nonce, _) = wire::decode_lookup(&first.frame).unwrap();
    driver
        .to_transport
        .send(Zeroizing::new(
            wire::encode_lookup_reply(&nonce, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        ))
        .await
        .unwrap();

    assert_eq!(lookup.await.unwrap(), Ok(LookupReply::NotFound));
    assert!(
        driver.from_transport.try_recv().is_err(),
        "no second address was tried"
    );
}

#[tokio::test]
async fn a_silent_hub_fails_closed_only_after_every_address() {
    let (mut driver, _inflight) = start_with_targets(Duration::from_millis(40), 3);
    let handle = driver.handle.clone();
    let lookup = tokio::spawn(async move { handle.get_transaction(&[0x53; 32]).await });

    let mut targets = Vec::new();
    for _ in 0..3 {
        targets.push(
            driver
                .from_transport
                .recv()
                .await
                .expect("an attempt per address")
                .target,
        );
    }
    assert_eq!(lookup.await.unwrap(), Err(NymError::Timeout));

    targets.sort_unstable();
    assert_eq!(targets, vec![0, 1, 2], "every address was tried exactly once");
    assert!(
        driver.from_transport.try_recv().is_err(),
        "and no more than once"
    );
}

#[tokio::test]
async fn requests_start_at_rotating_addresses() {
    // Always starting at the first address would lean the whole shim's load on
    // one of a multi-homed hub's gateways.
    let (mut driver, _inflight) = start_with_targets(Duration::from_millis(30), 3);
    let mut starts = Vec::new();
    for _ in 0..3 {
        let handle = driver.handle.clone();
        let submit = tokio::spawn(async move { handle.submit(b"tx").await });
        starts.push(
            driver
                .from_transport
                .recv()
                .await
                .expect("a first attempt")
                .target,
        );
        // Drain this request's remaining attempts before the next request.
        let _ = submit.await.unwrap();
        while driver.from_transport.try_recv().is_ok() {}
    }
    assert_eq!(
        starts,
        vec![0, 1, 2],
        "consecutive requests begin at successive addresses"
    );
}

#[tokio::test]
async fn no_configured_address_fails_closed_without_sending() {
    let (mut driver, _inflight) = start_with_targets(Duration::from_secs(5), 0);
    assert_eq!(
        driver.handle.submit(b"tx").await,
        Err(NymError::TransportGone)
    );
    assert!(driver.from_transport.try_recv().is_err());
}

#[tokio::test]
async fn every_outbound_frame_asks_for_an_anonymous_reply() {
    // The in-crate half of the hop's central property (D3). The transport
    // cannot ask for `IncludedSurbs::ExposeSelfAddress` -- `OutFrame` has no
    // field that could -- and what it DOES carry is a non-zero reply-SURB
    // count on every frame of both types. That count is what forces the
    // driver's anonymous send: a zero would leave the driver with no reply
    // path and a reason to reach for the self-address variant.
    //
    // The other half, that the driver actually sends anonymously, is only
    // observable from the receiving side and is asserted by the nymnet e2e
    // probe, which fails if any request reaches the hub without a sender tag.
    let (mut driver, _inflight) = start_with_targets(Duration::from_millis(60), 1);

    let submit_handle = driver.handle.clone();
    let submit = tokio::spawn(async move { submit_handle.submit(b"tx").await });
    let out = driver.from_transport.recv().await.expect("a submit frame");
    assert!(
        out.reply_surbs > 0,
        "a submit with no reply SURBs could not be acknowledged anonymously"
    );
    let _ = submit.await;

    let lookup_handle = driver.handle.clone();
    let lookup = tokio::spawn(async move { lookup_handle.get_transaction(&[0x43; 32]).await });
    let out = driver.from_transport.recv().await.expect("a lookup frame");
    assert!(
        out.reply_surbs > 0,
        "a lookup with no reply SURBs could not be answered anonymously"
    );
    let _ = lookup.await;
}

#[tokio::test]
async fn a_backed_up_driver_does_not_stall_replies_already_in_flight() {
    // The correlator must keep delivering replies while the driver is busy.
    // Handing a frame over used to be an awaited step inside the select loop,
    // so a driver mid-emission (the design budgets ~1 s to emit a 64 KiB frame,
    // more under backpressure) stopped inbound processing entirely: a request
    // whose reply had ALREADY arrived timed out anyway and failed closed.
    // Exercised through the lookup path, which awaits its reply (dispatch-only
    // submit does not).
    let (mut driver, _inflight) = start_full(Duration::from_millis(400), 1, 1);

    // A is in flight, and its frame is off the channel, so the driver is idle.
    let a = driver.handle.clone();
    let a = tokio::spawn(async move { a.get_transaction(&[0x60; 32]).await });
    let (a_nonce, _) = next_lookup(&mut driver).await;

    // B fills the driver's capacity, C has nowhere to go: the transport is now
    // holding a request it cannot hand over.
    let b = driver.handle.clone();
    let _b = tokio::spawn(async move { b.get_transaction(&[0x61; 32]).await });
    let c = driver.handle.clone();
    let _c = tokio::spawn(async move { c.get_transaction(&[0x62; 32]).await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A's reply arrives while the driver is still backed up. It must be
    // delivered, not left in the channel until A's budget expires.
    driver
        .to_transport
        .send(Zeroizing::new(
            wire::encode_lookup_reply(&a_nonce, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(
        a.await.unwrap(),
        Ok(LookupReply::NotFound),
        "a reply must land even while the driver cannot take more frames"
    );
}

#[tokio::test]
async fn the_inflight_count_tracks_requests_the_caller_still_wants() {
    // This is what the supervisor reads before rotating the client's identity:
    // rotating under a live request strands it, so the count must rise while a
    // caller is waiting, fall when it is answered, and fall again when a
    // caller gives up (whose entry no reply will ever remove).
    let (mut driver, inflight) = start_with_targets(Duration::from_millis(60), 1);
    assert_eq!(inflight.load(Ordering::Relaxed), 0);

    let handle = driver.handle.clone();
    let lookup = tokio::spawn(async move { handle.get_transaction(&[0x63; 32]).await });
    let (nonce, _) = next_lookup(&mut driver).await;
    assert_eq!(inflight.load(Ordering::Relaxed), 1, "a request is in flight");

    driver
        .to_transport
        .send(Zeroizing::new(
            wire::encode_lookup_reply(&nonce, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(lookup.await.unwrap(), Ok(LookupReply::NotFound));
    assert_eq!(
        inflight.load(Ordering::Relaxed),
        0,
        "an answered request is no longer in flight"
    );

    // A caller that timed out is not in flight either, even though nothing
    // ever answered it and no further traffic arrives to prompt a sweep: the
    // transport's own sweep tick is what clears it.
    let handle = driver.handle.clone();
    let lookup = tokio::spawn(async move { handle.get_transaction(&[0x64; 32]).await });
    let _ = next_lookup(&mut driver).await;
    assert_eq!(lookup.await.unwrap(), Err(NymError::Timeout));
    let cleared = tokio::time::timeout(Duration::from_secs(3), async {
        while inflight.load(Ordering::Relaxed) != 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        cleared.is_ok(),
        "an abandoned request must not pin the supervisor's rotation forever"
    );
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
        let lookup = tokio::spawn(async move { handle.get_transaction(&[0x65; 32]).await });
        let _ = next_lookup(&mut driver).await;
        assert_eq!(lookup.await.unwrap(), Err(NymError::Timeout));
    }

    let handle = driver.handle.clone();
    let lookup = tokio::spawn(async move { handle.get_transaction(&[0x66; 32]).await });
    let (nonce, _) = next_lookup(&mut driver).await;
    driver
        .to_transport
        .send(Zeroizing::new(
            wire::encode_lookup_reply(&nonce, &LookupReply::NotFound)
                .unwrap()
                .to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(lookup.await.unwrap(), Ok(LookupReply::NotFound));
}
