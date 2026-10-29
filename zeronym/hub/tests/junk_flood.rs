//! A burst of junk against the queue's entry cap.
//!
//! Junk costs nothing to send: bytes that do not parse have no expiry and never
//! reach the chain. These tests pin what one burst does. It fills the
//! unverified lane for one flush window, and the proven lane stays open: a
//! migration must carry proofs that verify, which junk cannot. The flush that
//! offers the burst ends it: the node cannot parse the bytes, and that answer is
//! a verdict. Before, the hub retried it, so one burst held the cap for
//! `MAX_REQUEUE_ATTEMPTS` more flushes and reached every indexer that many more
//! times.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use zero_indexer_hub::batcher::{self, BatchParams, TipGuard, TipTracker};
use zero_indexer_hub::chain::ChainClient;
use zero_indexer_hub::queue::{Admission, Queue, Refusal, UNVERIFIED_LANE_ENTRIES};

mod common;
use common::{spawn_mock_indexer, spawn_status_indexer};

/// Low enough that the fixture clears admission whatever its expiry.
const TIP: u32 = 100;

/// The size of each junk payload. Before the lanes, 1024 of them, 64 KiB, a
/// thousandth of the queue's byte budget, filled its whole entry cap.
const JUNK_BYTES: usize = 64;

/// An honest migration, for the submission the burst shuts out.
const V6_ORCHARD_ONLY: &[u8] = include_bytes!("fixtures/v6_proven_a.bin");

/// zebrad's answer to bytes it cannot deserialize, as lightwalletd relays it.
const ZEBRAD_PARSE_ERROR: &str = "parse error: bad tx header";

/// The same answer as zaino relays it: a trailers-only INTERNAL whose message is
/// its `Malformed` rejection, percent-encoded as tonic sends it.
const ZAINO_MALFORMED_INTERNAL: &str = "InternalServerError:%20error%20receiving%20data%20from%20\
    backing%20node:%20critical%20error%20in%20backing%20block%20source:%20validator%20rejected%20\
    the%20query:%20malformed%20transaction:%20parse%20error:%20bad%20tx%20header";

/// Distinct junk. An all-`0xff` header is an unknown transaction version.
fn junk(i: usize) -> Vec<u8> {
    let mut bytes = vec![0xff; JUNK_BYTES];
    bytes[JUNK_BYTES - 8..].copy_from_slice(&(i as u64).to_le_bytes());
    bytes
}

fn admit(queue: &Queue, tx: &[u8]) -> Admission {
    let params = BatchParams::default();
    queue.admit(tx, TIP, params.flush_interval, params.mining_margin)
}

/// Fill the unverified lane with junk, every entry admitted without a txid.
fn fill_with_junk(queue: &Queue) {
    for i in 0..UNVERIFIED_LANE_ENTRIES {
        assert_eq!(admit(queue, &junk(i)), Admission::Admitted { txid: None });
    }
}

#[test]
fn a_free_burst_of_junk_fills_its_lane_and_leaves_migrations_open() {
    let queue = Queue::new();
    fill_with_junk(&queue);

    assert_eq!(
        admit(&queue, &junk(UNVERIFIED_LANE_ENTRIES)),
        Admission::Refused(Refusal::Full),
        "more junk after the burst is refused"
    );
    assert!(
        matches!(
            admit(&queue, V6_ORCHARD_ONLY),
            Admission::Admitted { txid: Some(_) }
        ),
        "the honest migration after the burst is admitted"
    );
}

/// Fill the lane, flush once against `indexer`, and check that the flush ended
/// the burst: every junk entry offered once, none held, and admission open again.
async fn one_flush_ends_the_burst(indexer: SocketAddr, seen: Arc<Mutex<Vec<Vec<u8>>>>) {
    let queue = Arc::new(Queue::new());
    let chain = Arc::new(ChainClient::new(vec![indexer], None).unwrap());
    let tip = Arc::new(TipTracker::new(TipGuard::Unguarded));
    tip.observe(&[TIP]);
    fill_with_junk(&queue);

    let achieved = batcher::flush(&queue, &chain, &tip, BatchParams::default()).await;

    assert_eq!(achieved, 0);
    assert_eq!(
        seen.lock().unwrap().len(),
        UNVERIFIED_LANE_ENTRIES,
        "each offered once"
    );
    assert!(queue.is_empty(), "no junk is held for the next flush");
    assert!(matches!(
        admit(&queue, &junk(UNVERIFIED_LANE_ENTRIES)),
        Admission::Admitted { txid: None }
    ));
}

#[tokio::test]
async fn lightwalletd_relaying_a_parse_error_ends_the_burst_at_one_flush() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_mock_indexer(-22, ZEBRAD_PARSE_ERROR, seen.clone()).await;
    one_flush_ends_the_burst(indexer, seen).await;
}

#[tokio::test]
async fn zaino_relaying_a_parse_error_ends_the_burst_at_one_flush() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let indexer = spawn_status_indexer("13", ZAINO_MALFORMED_INTERNAL, seen.clone()).await;
    one_flush_ends_the_burst(indexer, seen).await;
}
