//! Tests for long-poll shutdown wait helpers.
use super::wait::{wait_notify_or_shutdown, wait_shutdown_signal};
use crate::hub::queue::InMemoryQueue;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

#[tokio::test]
async fn wait_notify_or_shutdown_returns_when_shutdown_signaled() {
    let queue = Arc::new(InMemoryQueue::new());
    let (tx, rx) = watch::channel(false);
    let mut shutdown_rx = rx.clone();

    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = tx.send(true);
    });

    let start = Instant::now();
    let notified = wait_notify_or_shutdown(queue.as_ref(), &mut shutdown_rx, "v1", 30).await;
    handle.await.unwrap();

    assert!(!notified);
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "expected fast return on shutdown, took {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn wait_shutdown_signal_returns_immediately_when_already_shutting_down() {
    let (_tx, rx) = watch::channel(true);
    let mut shutdown_rx = rx;

    let start = Instant::now();
    wait_shutdown_signal(&mut shutdown_rx).await;

    assert!(start.elapsed() < Duration::from_millis(50));
}
