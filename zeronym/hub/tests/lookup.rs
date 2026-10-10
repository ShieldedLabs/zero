//! What a lookup that misses the queue tells the caller, by what the indexer
//! answered.
//!
//! "Found, height 0, no bytes" is the hub's own reply for a transaction it is
//! holding unbroadcast, and a wallet renders "pending" from it. An indexer
//! answer without a body must therefore never be passed on as found: it is that
//! endpoint failing, and the other endpoints decide.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use zero_indexer_hub::batcher::{BatchParams, TipTracker};
use zero_indexer_hub::chain::ChainClient;
use zero_indexer_hub::queue::Queue;
use zero_indexer_hub::server::{Hub, LookupOutcome};

mod common;
use common::{spawn_mock_indexer_full, GetTx};

/// The tip the hub is parked at, as in the other integration tests.
const TIP: u32 = 100;

/// A hub at `TIP` whose indexers are `indexers`, in that order.
fn hub_against(indexers: Vec<SocketAddr>) -> Hub {
    let tip = Arc::new(TipTracker::new());
    tip.observe(TIP);
    Hub {
        queue: Arc::new(Queue::new()),
        tip,
        params: BatchParams::default(),
        chain: Arc::new(ChainClient::new(indexers, None).unwrap()),
    }
}

/// An indexer that accepts every broadcast and answers every `GetTransaction`
/// with `get_tx`.
async fn indexer_answering(get_tx: GetTx) -> SocketAddr {
    spawn_mock_indexer_full(
        0,
        "txid",
        get_tx,
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
    )
    .await
}

/// What a lookup told the caller, in the terms a wallet sees.
fn describe(outcome: &LookupOutcome) -> String {
    match outcome {
        LookupOutcome::Found { data, height } if data.is_empty() && *height == 0 => {
            "found, height 0, no bytes (the shim shows this as PENDING)".to_string()
        }
        LookupOutcome::Found { data, height } => {
            format!("found, height {height}, {} bytes", data.len())
        }
        LookupOutcome::NotFound => "not found".to_string(),
        LookupOutcome::Unavailable => "unavailable".to_string(),
    }
}

/// An indexer answer of "found, height 0, no bytes" must not reach the wallet
/// as "pending" for a transaction nobody holds. On the wire it is exactly the
/// hub's own "queued here".
#[tokio::test]
async fn an_empty_answer_from_the_indexer_is_not_reported_as_queued() {
    let hub = hub_against(vec![
        indexer_answering(GetTx::Found {
            data: Vec::new(),
            height: 0,
        })
        .await,
    ]);
    assert!(hub.queue.is_empty(), "precondition: the hub holds nothing");

    let outcome = hub.lookup(&[0x11; 32]).await;

    assert!(
        !matches!(&outcome, LookupOutcome::Found { data, height } if data.is_empty() && *height == 0),
        "the hub holds nothing, and told the caller: {}",
        describe(&outcome)
    );
    assert!(
        matches!(outcome, LookupOutcome::Unavailable),
        "the only indexer did not answer usefully, and the caller was told: {}",
        describe(&outcome)
    );
}

#[tokio::test]
async fn an_empty_answer_at_a_real_height_is_unavailable_not_found() {
    // No body is no answer at any height, not only at the mempool sentinel.
    let hub = hub_against(vec![
        indexer_answering(GetTx::Found {
            data: Vec::new(),
            height: 5,
        })
        .await,
    ]);

    let outcome = hub.lookup(&[0x11; 32]).await;

    assert!(
        matches!(outcome, LookupOutcome::Unavailable),
        "the caller was told: {}",
        describe(&outcome)
    );
}

#[tokio::test]
async fn an_empty_answer_does_not_mask_an_endpoint_that_has_the_transaction() {
    // The empty answer is listed first, where a `Found` would win the fold.
    let body = vec![0xAB; 64];
    let hub = hub_against(vec![
        indexer_answering(GetTx::Found {
            data: Vec::new(),
            height: 0,
        })
        .await,
        indexer_answering(GetTx::Found {
            data: body.clone(),
            height: 5,
        })
        .await,
    ]);

    let outcome = hub.lookup(&[0x11; 32]).await;

    assert!(
        matches!(&outcome, LookupOutcome::Found { data, height } if **data == body && *height == 5),
        "the caller was told: {}",
        describe(&outcome)
    );
}

#[tokio::test]
async fn an_empty_answer_beside_not_found_is_not_found() {
    let hub = hub_against(vec![
        indexer_answering(GetTx::Found {
            data: Vec::new(),
            height: 0,
        })
        .await,
        indexer_answering(GetTx::NotFound).await,
    ]);

    let outcome = hub.lookup(&[0x11; 32]).await;

    assert!(
        matches!(outcome, LookupOutcome::NotFound),
        "the caller was told: {}",
        describe(&outcome)
    );
}
