//! The listener's log hygiene, in its own process because it installs the global
//! `tracing` subscriber (Cargo gives each integration test file its own process,
//! so nothing else writes into the buffer these assertions read).
//!
//! In a Nitro enclave the tracing output reaches the parent host, so a txid, a
//! sender tag, or a request nonce in a log line hands the host exactly what the
//! system exists to withhold. This asserts the listener logs counts and reasons
//! and nothing else.

use std::io::Write;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tracing_subscriber::fmt::MakeWriter;

use zero_indexer_hub::batcher::{BatchParams, TipTracker};
use zero_indexer_hub::chain::ChainClient;
use zero_indexer_hub::nym::{run_listener, Received, SenderTag};
use zero_indexer_hub::queue::Queue;
use zero_indexer_hub::server::Hub;
use zero_indexer_hub::wire::{encode_submit, MAX_NYM_TX_BYTES};

/// A `tracing` writer that keeps everything in memory.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

#[tokio::test]
async fn the_listener_logs_counts_and_reasons_but_never_a_txid_tag_or_nonce() {
    let capture = Capture::default();
    tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .expect("this test process owns the global subscriber");

    let queue = Arc::new(Queue::new());
    let tip = Arc::new(TipTracker::new());
    tip.observe(100);
    let chain = Arc::new(ChainClient::new(vec!["127.0.0.1:1".parse().unwrap()], None).unwrap());
    let hub = Hub {
        queue,
        tip,
        params: BatchParams::default(),
        chain,
    };

    // Distinctive byte patterns so their hex would be obvious in the log if it
    // ever leaked: the sender tag, the good frame's nonce, the bad frame's nonce.
    let tag = SenderTag([0xab; 16]);
    let good = encode_submit(&[0xcd; 16], &[0x11; 80]).unwrap().to_vec();
    let mut bad = encode_submit(&[0xef; 16], &[0x22; 80]).unwrap().to_vec();
    bad[20..24].copy_from_slice(&((MAX_NYM_TX_BYTES + 1) as u32).to_be_bytes());

    let (in_tx, in_rx) = mpsc::channel(8);
    let (out_tx, mut out_rx) = mpsc::channel(8);
    tokio::spawn(run_listener(in_rx, out_tx, hub));
    in_tx
        .send(Received {
            frame: good,
            sender_tag: tag,
        })
        .await
        .unwrap();
    in_tx
        .send(Received {
            frame: bad,
            sender_tag: tag,
        })
        .await
        .unwrap();
    drop(in_tx);
    // Drain so both are fully processed and their log lines are on the page.
    while out_rx.recv().await.is_some() {}

    let log = capture.text();

    // Counts and reasons ARE logged: an admitted submission and a decode failure.
    assert!(
        log.contains("migration admitted to the batch"),
        "log was:\n{log}"
    );
    assert!(log.contains("could not be decoded"), "log was:\n{log}");

    // The sender tag, the nonces, and any txid are NOT.
    assert!(
        !log.contains("abababab"),
        "the sender tag must never be logged, log was:\n{log}"
    );
    assert!(
        !log.contains("cdcdcdcd"),
        "the request nonce must never be logged, log was:\n{log}"
    );
    assert!(
        !log.contains("efefefef"),
        "a bad frame's nonce must never be logged, log was:\n{log}"
    );
    assert!(
        !log.to_lowercase().contains("txid"),
        "no txid may reach the log, log was:\n{log}"
    );
}
