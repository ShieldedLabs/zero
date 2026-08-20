//! The shim's total inbound concurrency is bounded.
//!
//! The per-request buffer was already capped at `MAX_SEND_TX_BYTES` (4 MiB), but
//! nothing capped the AGGREGATE: the accept loop spawned a task per socket with
//! no semaphore, no counter and no admission control, so the real ceiling was
//! however many sockets an attacker chose to open (Hornby review, 2026-08-19).
//! Against a fixed 2048 MB enclave that is roughly 512 concurrent 4 MiB bodies
//! before the process is killed — taking the mixnet client, its identity, and
//! every acknowledged-but-unemitted submit with it.
//!
//! These tests hold connections open without speaking HTTP/2, which is exactly
//! what the attack does: the permit is taken before the connection task is
//! spawned and held for its whole life, so an idle socket costs a slot.
//!
//! **How refused is told apart from served.** Hyper's HTTP/2 server sends its
//! SETTINGS frame as soon as it accepts a connection, without waiting for the
//! client to say anything. So a served connection yields bytes immediately and a
//! refused one yields EOF. Measured, not assumed: a single idle connection to an
//! otherwise-empty shim reads one byte rather than blocking.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

mod common;
use common::{spawn_counting_backend, spawn_forward_only_shim};

/// Mirrors `proxy::MAX_INFLIGHT_CONNECTIONS`, which is deliberately not public:
/// an operator who could raise it could re-open the hole.
const LIMIT: usize = 256;

/// Open `n` connections and keep them alive, returning the handles.
async fn hold(shim: std::net::SocketAddr, n: usize) -> Vec<TcpStream> {
    let mut held = Vec::with_capacity(n);
    for _ in 0..n {
        match TcpStream::connect(shim).await {
            Ok(stream) => held.push(stream),
            // A machine with a tight descriptor budget may refuse before we
            // reach the limit; the callers assert on `held.len()` rather than
            // assuming it succeeded.
            Err(_) => break,
        }
    }
    held
}

#[tokio::test]
async fn a_connection_arriving_over_the_limit_is_closed_not_queued() {
    let connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(connections.clone()).await;
    let shim = spawn_forward_only_shim(backend).await;

    // Fill every slot with sockets that connect and then say nothing. Each one
    // sits in `serve_connection` waiting for an HTTP/2 preface that never comes,
    // holding its permit the whole time.
    let held = hold(shim, LIMIT).await;
    assert_eq!(held.len(), LIMIT, "precondition: all slots were taken");

    // Give the accept loop time to have taken a permit for each.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut over = TcpStream::connect(shim).await.expect("connect");
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), over.read(&mut buf)).await;

    match read {
        Ok(Ok(0)) => {}  // EOF: refused, as intended
        Ok(Err(_)) => {} // reset: also refused
        Ok(Ok(n)) => panic!(
            "a connection over the limit was SERVED ({n} bytes of SETTINGS): the bound is \
             not being applied"
        ),
        Err(_) => panic!(
            "a connection over the limit was HELD OPEN for 5s. Parking it rebuilds the \
             unbounded pile the bound exists to prevent"
        ),
    }

    drop(held);
}

#[tokio::test]
async fn slots_are_returned_when_connections_close() {
    // A bound that never released would be its own denial of service: one burst
    // of idle sockets would take the shim out permanently.
    let connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_counting_backend(connections.clone()).await;
    let shim = spawn_forward_only_shim(backend).await;

    let held = hold(shim, LIMIT).await;
    assert_eq!(held.len(), LIMIT, "precondition: all slots were taken");
    tokio::time::sleep(Duration::from_millis(500)).await;

    drop(held);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut fresh = TcpStream::connect(shim).await.expect("connect");
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), fresh.read(&mut buf)).await;

    match read {
        Ok(Ok(0)) => panic!(
            "a connection was refused after every held slot had closed: permits are not \
             being released, which makes the bound its own denial of service"
        ),
        Ok(Ok(_)) => {} // served: the server's SETTINGS frame arrived
        Ok(Err(err)) => panic!("connection reset after slots were freed: {err}"),
        Err(_) => panic!("a served connection sent no SETTINGS frame within 5s"),
    }
}
