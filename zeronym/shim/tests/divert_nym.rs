//! The divert and lookup paths over the MIXNET transport, end to end.
//!
//! The same properties `divert.rs` asserts for the clearnet hop, asserted for
//! the Nym transport across the whole intercept path: a wallet's gRPC call
//! enters the real shim, is classified, framed, correlated, answered, and
//! returned, with the operator's connection-counting indexer held at zero
//! throughout.
//!
//! The hub here is a task holding the DRIVER ends of the transport's channels:
//! it reads the frames that would go onto the mixnet, decodes them with the
//! same wire codec the real hub uses, and writes back the frames a hub would
//! send. No SDK and no fake client, exactly as the transport's own tests and
//! the hub listener's tests are driven; what the mixnet itself does to those
//! bytes is proven separately by the nymnet harness.
//!
//! Two things differ from the HTTP path and are asserted here rather than
//! assumed: the wallet's txid is computed LOCALLY from the diverted bytes
//! (the ack deliberately carries none, D5), and a silent hub fails closed on
//! the transport's own timeout instead of a refused TCP connection.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use zaino_proto::proto::service::TxFilter;
use zeroize::Zeroizing;

use zero_indexer_shim::intercept::Diversion;
use zero_indexer_shim::nym::{run_transport, NymHandle};
use zero_indexer_shim::wire::{self, AckKind, AckRefusal, LookupReply, MAX_NYM_TX_BYTES};

mod common;
use common::{
    connect_h2, decode_raw_transaction, decode_send_response, expected_txid, get_transaction,
    get_transaction_filter, send_tx, send_tx_reply, spawn_counting_backend, wire_hash,
    V6_IRONWOOD_ONLY, V6_MIGRATION,
};

/// The transport's per-request timeout for these tests. Generous on purpose:
/// the two tests that deliberately wait it out pay this latency once each, which
/// is a cheap price for the reply-expecting tests never flaking. At 400 ms a
/// loaded machine could miss an instant local reply and fail with a confusing
/// "expected a reply, got Timeout" panic (L5); seconds of margin makes that
/// unreachable while keeping the wait-it-out tests bounded.
const TIMEOUT: Duration = Duration::from_secs(2);

/// How the mixnet hub answers a submission.
#[derive(Clone)]
enum OnSubmit {
    Accept,
    Refuse(AckRefusal),
    /// Never replies: the mixnet is up but the hub is not answering, which is
    /// the failure the transport's timeout exists for.
    Silent,
}

/// How the mixnet hub answers a lookup.
#[derive(Clone)]
enum OnLookup {
    Found { data: Vec<u8>, height: u64 },
    NotFound,
    /// The hub could not answer (its indexer failed, or it could not frame the
    /// reply). Must never reach a wallet as "your transaction does not exist".
    Error,
    Silent,
}

/// What the hub saw, for the tests that assert the request as well as the reply.
#[derive(Default)]
struct HubSeen {
    submits: Mutex<Vec<Vec<u8>>>,
    lookups: Mutex<Vec<Vec<u8>>>,
}

/// Spawn a shim whose hub transport is the mixnet one, with a mock hub on the
/// driver end of its channels. Returns the shim's address and what the hub saw.
async fn spawn_nym_shim(
    backend: SocketAddr,
    on_submit: OnSubmit,
    on_lookup: OnLookup,
) -> (SocketAddr, Arc<HubSeen>) {
    let (req_tx, req_rx) = mpsc::channel(8);
    let (out_tx, mut out_rx) = mpsc::channel(8);
    let (in_tx, in_rx) = mpsc::channel(8);
    tokio::spawn(run_transport(
        req_rx,
        out_tx,
        in_rx,
        Arc::new(AtomicUsize::new(0)),
    ));

    let seen = Arc::new(HubSeen::default());
    let hub_seen = seen.clone();
    tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            // The hub tells a lookup from a submission exactly as the real
            // listener does: by the frame's magic.
            let reply = if wire::peek_lookup_nonce(&out.frame).is_some() {
                let (nonce, hash) = wire::decode_lookup(&out.frame).expect("a lookup frame");
                hub_seen.lookups.lock().unwrap().push(hash);
                match &on_lookup {
                    OnLookup::Silent => continue,
                    OnLookup::Found { data, height } => wire::encode_lookup_reply(
                        &nonce,
                        &LookupReply::Found {
                            height: *height,
                            tx: Zeroizing::new(data.clone()),
                        },
                    )
                    .expect("the fixture fits a reply frame")
                    .to_vec(),
                    OnLookup::NotFound => {
                        wire::encode_lookup_reply(&nonce, &LookupReply::NotFound)
                            .unwrap()
                            .to_vec()
                    }
                    OnLookup::Error => wire::encode_lookup_reply(&nonce, &LookupReply::Error)
                        .unwrap()
                        .to_vec(),
                }
            } else {
                let (nonce, tx) = wire::decode_submit(&out.frame).expect("a submit frame");
                hub_seen.submits.lock().unwrap().push(tx.to_vec());
                match on_submit {
                    OnSubmit::Silent => continue,
                    OnSubmit::Accept => wire::encode_ack(&nonce, AckKind::Accepted).to_vec(),
                    OnSubmit::Refuse(refusal) => {
                        wire::encode_ack(&nonce, AckKind::Refused(refusal)).to_vec()
                    }
                }
            };
            if in_tx.send(Zeroizing::new(reply)).await.is_err() {
                break;
            }
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let diversion = Some(Arc::new(Diversion {
        hub: NymHandle::new(req_tx, TIMEOUT, TIMEOUT, Arc::new(AtomicUsize::new(1))).into(),
    }));
    tokio::spawn(async move {
        let _ = zero_indexer_shim::serve_with_shutdown(
            listener,
            backend,
            None,
            diversion,
            zero_indexer_shim::CautionRelay::default(),
            std::future::pending::<()>(),
        )
        .await;
    });
    (addr, seen)
}

/// Poll `cond` until true or a short deadline. The submit path is best-effort: the
/// wallet is answered the moment the frame is dispatched to the mixnet, so the hub
/// records it a beat later. A test that asserts the hub's view must wait for it,
/// where the old ack-waiting path had already observed it by the time submit
/// returned.
async fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if cond() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    cond()
}

#[tokio::test]
async fn a_migration_is_diverted_over_the_mixnet_and_the_operator_is_never_connected() {
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let (shim, seen) =
        spawn_nym_shim(backend, OnSubmit::Accept, OnLookup::NotFound).await;

    let mut sender = connect_h2(shim).await;
    let body = send_tx(&mut sender, shim, V6_MIGRATION).await;

    // The wallet gets a synthetic success carrying the LOCALLY computed txid:
    // the ack carries none by design, so this is the shim's own hash of the
    // bytes it diverted, and it must match what any other party would compute.
    let resp = decode_send_response(&body);
    assert_eq!(resp.error_code, 0);
    assert_eq!(resp.error_message, expected_txid(V6_MIGRATION));

    // The hub received the exact migration bytes, unpadded out of the frame.
    // Best-effort: the wallet was answered on dispatch, so wait for the frame to
    // land at the hub rather than expecting it already recorded.
    assert!(
        eventually(|| !seen.submits.lock().unwrap().is_empty()).await,
        "the hub received the migration"
    );
    assert_eq!(
        seen.submits.lock().unwrap().as_slice(),
        &[V6_MIGRATION.to_vec()]
    );

    // And the operator's indexer was never even connected.
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "classify-before-connect: a diverted migration must not dial the operator"
    );
}

#[tokio::test]
async fn a_pass_through_still_reaches_the_operator_and_never_the_mixnet() {
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let (shim, seen) = spawn_nym_shim(backend, OnSubmit::Accept, OnLookup::NotFound).await;

    let mut sender = connect_h2(shim).await;
    let body = send_tx(&mut sender, shim, V6_IRONWOOD_ONLY).await;

    assert_eq!(
        decode_send_response(&body).error_message,
        "operator-answered"
    );
    assert!(
        backend_conns.load(Ordering::SeqCst) >= 1,
        "a pass-through must reach the operator's indexer"
    );
    assert!(
        seen.submits.lock().unwrap().is_empty(),
        "a pass-through must never enter the mixnet"
    );
}

#[tokio::test]
async fn a_hub_refusal_is_not_surfaced_under_best_effort() {
    // Dispatch-only: the shim answers success once the frame is on the mixnet and
    // does NOT await the hub's verdict, which is a full round trip away. A hub that
    // would refuse (queue full) therefore does not surface that refusal to the
    // wallet -- a deliberate trade for never blocking on the round trip. The
    // migration still went ONLY to the hub, and the wallet learns the true outcome
    // by confirmation; a resend is safe (D6 dedup at the hub).
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let (shim, seen) = spawn_nym_shim(
        backend,
        OnSubmit::Refuse(AckRefusal::QueueFull),
        OnLookup::NotFound,
    )
    .await;

    let mut sender = connect_h2(shim).await;
    let resp = decode_send_response(&send_tx(&mut sender, shim, V6_MIGRATION).await);

    assert_eq!(
        resp.error_code, 0,
        "best-effort: the wallet is answered success on dispatch, not the refusal"
    );
    assert_eq!(resp.error_message, expected_txid(V6_MIGRATION));
    assert!(
        eventually(|| !seen.submits.lock().unwrap().is_empty()).await,
        "the migration still reached the hub"
    );
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "still never handed to the operator"
    );
}

#[tokio::test]
async fn a_silent_hub_answers_best_effort_success_and_never_the_operator() {
    // A hub that never acks is the COMMON case under mixnet latency, so it must not
    // fail the wallet: dispatch-only answers success as soon as the frame is on its
    // way, and confirmation comes via the wallet's own sync. The invariant that
    // survives unchanged: best-effort is NEVER a fallback to the operator's indexer.
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let (shim, seen) = spawn_nym_shim(backend, OnSubmit::Silent, OnLookup::NotFound).await;

    let mut sender = connect_h2(shim).await;
    let resp = decode_send_response(&send_tx(&mut sender, shim, V6_MIGRATION).await);

    assert_eq!(
        resp.error_code, 0,
        "best-effort success on dispatch, despite the hub's silence"
    );
    assert_eq!(resp.error_message, expected_txid(V6_MIGRATION));
    assert!(
        eventually(|| !seen.submits.lock().unwrap().is_empty()).await,
        "the hub was still sent the migration"
    );
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "best-effort is never a fallback to the operator"
    );
}

#[tokio::test]
async fn an_over_cap_transaction_is_resource_exhausted_not_a_retry_forever() {
    // UNAVAILABLE is the status that tells a wallet to retry, and a
    // transaction that does not fit the frame can never succeed on this
    // transport: it would retry until it gave up. The intercept path buffers
    // up to 4 MiB while the frame budget is ~64 KiB, so the window between
    // them is wide and reachable.
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let (shim, seen) = spawn_nym_shim(backend, OnSubmit::Accept, OnLookup::NotFound).await;

    // An Orchard-touching migration padded past the frame budget. Trailing
    // bytes after a valid transaction still classify as a migration, which is
    // what routes it down the divert path in the first place.
    let mut oversized = V6_MIGRATION.to_vec();
    oversized.resize(MAX_NYM_TX_BYTES + 1, 0);

    let mut sender = connect_h2(shim).await;
    let reply = send_tx_reply(&mut sender, shim, &oversized).await;

    assert_eq!(reply.status, 8, "over-cap maps to gRPC RESOURCE_EXHAUSTED");
    assert!(
        seen.submits.lock().unwrap().is_empty(),
        "it is never framed, so it never enters the mixnet"
    );
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "and it is never handed to the operator to broadcast instead"
    );
}

#[tokio::test]
async fn a_get_transaction_is_answered_over_the_mixnet_and_the_operator_is_never_dialled() {
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let (shim, seen) = spawn_nym_shim(
        backend,
        OnSubmit::Accept,
        OnLookup::Found {
            data: V6_MIGRATION.to_vec(),
            height: 0,
        },
    )
    .await;

    let mut sender = connect_h2(shim).await;
    let hash = wire_hash(V6_MIGRATION);
    let reply = get_transaction(&mut sender, shim, &hash).await;

    assert_eq!(reply.status, 0);
    let raw = decode_raw_transaction(&reply.body);
    assert_eq!(raw.data, V6_MIGRATION);
    assert_eq!(raw.height, 0, "height 0 is the mempool sentinel");

    // The hub was asked with the wallet's bytes unmodified.
    assert_eq!(seen.lookups.lock().unwrap().as_slice(), &[hash.clone()]);
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "a hub-served GetTransaction must not dial the operator"
    );
}

#[tokio::test]
async fn a_mined_height_from_the_hub_is_relayed() {
    let backend = spawn_counting_backend(Arc::new(AtomicUsize::new(0))).await;
    let (shim, _) = spawn_nym_shim(
        backend,
        OnSubmit::Accept,
        OnLookup::Found {
            data: V6_MIGRATION.to_vec(),
            height: 424_242,
        },
    )
    .await;

    let mut sender = connect_h2(shim).await;
    let reply = get_transaction(&mut sender, shim, &wire_hash(V6_MIGRATION)).await;
    assert_eq!(decode_raw_transaction(&reply.body).height, 424_242);
}

#[tokio::test]
async fn a_hub_reply_for_a_different_txid_is_refused_not_served() {
    // L4: a hub that answers a lookup with a transaction OTHER than the one
    // queried must not have it served to the wallet as that txid. The shim
    // verifies the reply against the query and fails closed as NOT_FOUND, rather
    // than hand the wallet the wrong transaction under its own txid.
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let (shim, _) = spawn_nym_shim(
        backend,
        OnSubmit::Accept,
        OnLookup::Found {
            data: V6_MIGRATION.to_vec(),
            height: 0,
        },
    )
    .await;

    let mut sender = connect_h2(shim).await;
    // Query a hash that is NOT V6_MIGRATION's txid; the hub returns V6_MIGRATION.
    let reply = get_transaction(&mut sender, shim, &[0x11u8; 32]).await;

    assert_eq!(
        reply.status, 5,
        "a mismatched lookup reply is refused as NOT_FOUND, not served"
    );
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "refusing a mismatched reply must not fall back to the operator"
    );
}

#[tokio::test]
async fn an_unknown_txid_is_not_found_and_never_touches_the_operator() {
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let (shim, seen) = spawn_nym_shim(backend, OnSubmit::Accept, OnLookup::NotFound).await;

    let mut sender = connect_h2(shim).await;
    let reply = get_transaction(&mut sender, shim, &[0x55u8; 32]).await;

    assert_eq!(reply.status, 5, "unknown txid maps to gRPC NOT_FOUND");
    assert_eq!(seen.lookups.lock().unwrap().len(), 1, "the hub was asked");
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "a not-found lookup must never fall back to the operator"
    );
}

#[tokio::test]
async fn an_error_reply_fails_closed_rather_than_claiming_not_found() {
    // The distinction the wallet acts on: NOT_FOUND says the transaction does
    // not exist, UNAVAILABLE says ask again. A hub that could not answer must
    // never be rendered as the former.
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let (shim, _) = spawn_nym_shim(backend, OnSubmit::Accept, OnLookup::Error).await;

    let mut sender = connect_h2(shim).await;
    let reply = get_transaction(&mut sender, shim, &[0x33u8; 32]).await;

    assert_eq!(reply.status, 14, "an unanswerable lookup is UNAVAILABLE");
    assert_eq!(backend_conns.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_silent_hub_fails_the_lookup_closed() {
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let (shim, seen) = spawn_nym_shim(backend, OnSubmit::Accept, OnLookup::Silent).await;

    let mut sender = connect_h2(shim).await;
    let reply = get_transaction(&mut sender, shim, &[0x44u8; 32]).await;

    assert_eq!(reply.status, 14, "a lost reply is UNAVAILABLE");
    assert_eq!(seen.lookups.lock().unwrap().len(), 1, "the hub was tried");
    assert_eq!(
        backend_conns.load(Ordering::SeqCst),
        0,
        "failing closed means the operator is never dialled"
    );
}

#[tokio::test]
async fn a_malformed_filter_is_rejected_locally_and_never_framed() {
    // Rejected before the transport, so nothing enters the mixnet and no
    // frame is spent on a request the shim already knows is invalid.
    let backend_conns = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(backend_conns.clone()).await;
    let (shim, seen) = spawn_nym_shim(backend, OnSubmit::Accept, OnLookup::NotFound).await;

    let mut sender = connect_h2(shim).await;
    let short = get_transaction(&mut sender, shim, &[0x77u8; 17]).await;
    assert_eq!(short.status, 3, "a wrong-length hash is INVALID_ARGUMENT");

    let by_block = get_transaction_filter(
        &mut sender,
        shim,
        TxFilter {
            block: Some(zaino_proto::proto::service::BlockId {
                height: 100,
                hash: Vec::new(),
            }),
            index: 3,
            hash: Vec::new(),
        },
    )
    .await;
    assert_eq!(by_block.status, 3, "a block+index filter is INVALID_ARGUMENT");

    assert!(
        seen.lookups.lock().unwrap().is_empty(),
        "a bad filter never reaches the mixnet"
    );
    assert_eq!(backend_conns.load(Ordering::SeqCst), 0);
}
