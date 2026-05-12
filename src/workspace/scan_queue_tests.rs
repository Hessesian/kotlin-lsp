//! Unit tests for [`ScanQueue`].

use tokio::sync::oneshot;

use super::{ScanArgs, ScanKind, ScanQueue};
use std::path::PathBuf;

fn dummy_args(root: &str) -> ScanArgs {
    ScanArgs {
        root: PathBuf::from(root),
        kind: ScanKind::Full,
        completion_tx: None,
        expected_generation: 0,
        reset_before_scan: false,
    }
}

#[test]
fn starts_when_idle() {
    let mut q = ScanQueue::new();
    q.request(dummy_args("/a"));
    let args = q.try_start().expect("should start when idle");
    assert_eq!(args.root, PathBuf::from("/a"));
    assert!(q.is_in_progress());
}

#[test]
fn returns_none_when_busy() {
    let mut q = ScanQueue::new();
    q.request(dummy_args("/a"));
    q.try_start();
    q.request(dummy_args("/b"));
    assert!(q.try_start().is_none(), "should not start a second scan");
}

#[test]
fn last_write_wins() {
    let mut q = ScanQueue::new();
    q.request(dummy_args("/a"));
    q.try_start(); // scan A starts
    q.request(dummy_args("/b"));
    q.request(dummy_args("/c")); // /b is replaced
    q.completed();
    let args = q.try_start().expect("pending should have /c");
    assert_eq!(args.root, PathBuf::from("/c"));
}

#[test]
fn completion_tx_dropped_when_superseded() {
    let mut q = ScanQueue::new();
    q.request(dummy_args("/a"));
    q.try_start(); // scan A starts — in_progress

    let (tx1, mut rx1) = oneshot::channel::<()>();
    q.request(ScanArgs {
        root: PathBuf::from("/b"),
        kind: ScanKind::Full,
        completion_tx: Some(tx1),
        expected_generation: 0,
        reset_before_scan: false,
    });
    // /b pending is now replaced by /c, dropping tx1
    q.request(dummy_args("/c"));

    // tx1 was dropped when /b was superseded — receiver must see Closed, not just Empty
    assert!(matches!(
        rx1.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
}

#[test]
fn idle_when_no_pending_after_complete() {
    let mut q = ScanQueue::new();
    q.request(dummy_args("/a"));
    q.try_start();
    q.completed();
    assert!(!q.is_in_progress());
    assert!(q.try_start().is_none(), "nothing pending");
}
