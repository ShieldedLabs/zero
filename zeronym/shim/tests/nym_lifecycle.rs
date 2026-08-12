//! The mixnet client's lifecycle, driven through the supervisor's channels.
//!
//! The supervisor is what makes the D11 rotation policy a parameter rather than
//! a redeploy, and what keeps the SDK's own hard stop (D12) from ending the
//! shim's ability to divert. It touches no SDK: these tests hold the driver
//! ends and assert the commands it emits, exactly as the transport's tests hold
//! the mixnet ends.
//!
//! Time is PAUSED throughout (`tokio::time::pause`), so a rotation period of
//! an hour is exercised in microseconds and the assertions are about ordering
//! and cause, never about how long a test slept.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use zero_indexer_shim::nym::{
    run_supervisor, ClientCommand, ClientEvent, InflightCount, RotationPolicy,
};

const HOUR: Duration = Duration::from_secs(3600);

/// The supervisor's driver ends: report events in, read commands out, and a
/// shutdown trigger.
struct Supervised {
    events: mpsc::Sender<ClientEvent>,
    commands: mpsc::Receiver<ClientCommand>,
    inflight: InflightCount,
    shutdown: Option<oneshot::Sender<()>>,
}

fn supervise(policy: RotationPolicy) -> Supervised {
    let (event_tx, event_rx) = mpsc::channel(8);
    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let inflight: InflightCount = Arc::new(AtomicUsize::new(0));
    let watched = inflight.clone();
    tokio::spawn(async move {
        run_supervisor(policy, event_rx, cmd_tx, watched, async {
            let _ = shutdown_rx.await;
        })
        .await;
    });
    Supervised {
        events: event_tx,
        commands: cmd_rx,
        inflight,
        shutdown: Some(shutdown_tx),
    }
}

/// Let the spawned supervisor run to its next await point, so a command it was
/// always going to send has actually been sent before an assertion looks.
async fn settle() {
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
}

#[tokio::test(start_paused = true)]
async fn a_scheduled_rotation_asks_for_a_rebuild() {
    // The whole point of D11: the linkage window is the rotation period, and
    // the period is a parameter.
    let mut sup = supervise(RotationPolicy::every(HOUR));

    tokio::time::advance(HOUR - Duration::from_secs(1)).await;
    settle().await;
    assert!(
        sup.commands.try_recv().is_err(),
        "nothing rotates before the period is up"
    );

    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    assert_eq!(sup.commands.recv().await, Some(ClientCommand::Rebuild));
}

#[tokio::test(start_paused = true)]
async fn rotation_repeats_on_the_period() {
    let mut sup = supervise(RotationPolicy::every(HOUR));
    for _ in 0..3 {
        tokio::time::advance(HOUR + Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(sup.commands.recv().await, Some(ClientCommand::Rebuild));
    }
}

#[tokio::test(start_paused = true)]
async fn no_period_never_rotates() {
    // Never rotating is a legitimate (documented) choice: the linkage window
    // becomes the process uptime. What it must not do is rotate anyway.
    let mut sup = supervise(RotationPolicy::never());
    tokio::time::advance(HOUR * 24).await;
    settle().await;
    assert!(sup.commands.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn a_dead_client_is_rebuilt_immediately_even_with_requests_in_flight() {
    // After the SDK's 20-failure hard stop nothing is deliverable, so the
    // in-flight requests are already lost to their timeouts: waiting for them
    // would only extend the outage.
    let mut sup = supervise(RotationPolicy::never());
    sup.inflight.store(3, Ordering::Relaxed);

    sup.events.send(ClientEvent::Died).await.unwrap();
    settle().await;

    assert_eq!(sup.commands.recv().await, Some(ClientCommand::Rebuild));
}

#[tokio::test(start_paused = true)]
async fn a_due_rotation_waits_for_the_transport_to_go_idle() {
    // Rotating under an in-flight request strands it: its reply comes back
    // through SURBs the old client minted.
    let mut sup = supervise(RotationPolicy::every(HOUR));
    sup.inflight.store(1, Ordering::Relaxed);

    tokio::time::advance(HOUR + Duration::from_secs(1)).await;
    settle().await;
    assert!(
        sup.commands.try_recv().is_err(),
        "a due rotation defers while a request is still expecting an answer"
    );

    // The request finishes; the deferred rotation goes ahead on its next check.
    sup.inflight.store(0, Ordering::Relaxed);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(sup.commands.recv().await, Some(ClientCommand::Rebuild));
}

#[tokio::test(start_paused = true)]
async fn a_busy_shim_still_rotates_at_the_defer_limit() {
    // The opposite failure to the one above: a shim that is never idle would
    // otherwise never rotate, and the linkage window would be unbounded in
    // practice however short the configured period is.
    let policy = RotationPolicy {
        defer_limit: Duration::from_secs(30),
        ..RotationPolicy::every(HOUR)
    };
    let mut sup = supervise(policy);
    sup.inflight.store(1, Ordering::Relaxed);

    tokio::time::advance(HOUR + Duration::from_secs(1)).await;
    settle().await;
    assert!(sup.commands.try_recv().is_err(), "it defers first");

    tokio::time::advance(Duration::from_secs(31)).await;
    settle().await;
    assert_eq!(
        sup.commands.recv().await,
        Some(ClientCommand::Rebuild),
        "but not forever"
    );
}

#[tokio::test(start_paused = true)]
async fn a_rebuild_restarts_the_rotation_clock() {
    // A rebuild already minted a fresh identity, so the window starts over;
    // rotating again on the original schedule would spend a gateway
    // registration for nothing.
    let mut sup = supervise(RotationPolicy::every(HOUR));

    tokio::time::advance(HOUR - Duration::from_secs(60)).await;
    settle().await;
    sup.events.send(ClientEvent::Died).await.unwrap();
    settle().await;
    assert_eq!(sup.commands.recv().await, Some(ClientCommand::Rebuild));

    // The original deadline passes with no second rebuild.
    tokio::time::advance(Duration::from_secs(120)).await;
    settle().await;
    assert!(
        sup.commands.try_recv().is_err(),
        "the clock restarted at the rebuild"
    );

    // A full period after the rebuild, it rotates again.
    tokio::time::advance(HOUR).await;
    settle().await;
    assert_eq!(sup.commands.recv().await, Some(ClientCommand::Rebuild));
}

#[tokio::test(start_paused = true)]
async fn shutdown_disconnects_rather_than_dropping_the_client() {
    // `disconnect()` is not cancel-safe and a dropped client leaks its
    // background tasks (D12), so the driver must be TOLD to shut down.
    let mut sup = supervise(RotationPolicy::every(HOUR));
    sup.shutdown.take().unwrap().send(()).unwrap();
    settle().await;

    assert_eq!(sup.commands.recv().await, Some(ClientCommand::Disconnect));
    // And the supervisor is finished: the command channel closes behind it.
    assert_eq!(sup.commands.recv().await, None);
}

#[tokio::test(start_paused = true)]
async fn shutdown_beats_a_due_rotation() {
    // A rotation that comes due during shutdown must not leave a freshly built
    // client behind for nobody to disconnect.
    let mut sup = supervise(RotationPolicy::every(HOUR));
    sup.inflight.store(1, Ordering::Relaxed);
    tokio::time::advance(HOUR + Duration::from_secs(1)).await;
    settle().await;

    sup.shutdown.take().unwrap().send(()).unwrap();
    settle().await;

    assert_eq!(sup.commands.recv().await, Some(ClientCommand::Disconnect));
    assert_eq!(sup.commands.recv().await, None);
}

#[tokio::test(start_paused = true)]
async fn a_gone_driver_ends_the_supervisor() {
    let sup = supervise(RotationPolicy::every(HOUR));
    drop(sup.events);
    settle().await;
    // The command channel closing is how the supervisor's exit is observed.
    let mut commands = sup.commands;
    assert_eq!(commands.recv().await, None);
}
