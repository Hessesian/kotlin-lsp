//! Integration tests for [`Actor`].

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::indexer::{Indexer, NoopReporter};
use crate::workspace::{Actor, Config, Event};

// ─── helpers ─────────────────────────────────────────────────────────────────

fn make_actor(
    indexer: Arc<Indexer>,
) -> (Actor<NoopReporter>, mpsc::Sender<Event>) {
    let (tx, rx) = mpsc::channel(16);
    let actor = Actor::new(indexer, Arc::new(NoopReporter), rx, None);
    (actor, tx)
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Poll `condition` every yield until it returns `true` or `timeout` elapses.
async fn poll_until<F: Fn() -> bool>(condition: F, timeout: Duration) {
    tokio::time::timeout(timeout, async {
        loop {
            if condition() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition not met within timeout");
}

// ─── actor tests ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn initialize_sets_workspace_root() {
    let indexer = Arc::new(Indexer::new());
    let tmp = temp_dir();
    let root = tmp.path().to_path_buf();

    let (actor, tx) = make_actor(Arc::clone(&indexer));
    tokio::spawn(actor.run());

    tx.send(Event::Initialize {
        config: Config {
            root: root.clone(),
            explicit_source_paths: Vec::new(),
            ignore_patterns: Vec::new(),
        },
        completion_tx: None,
    })
    .await
    .unwrap();

    // workspace_root is set synchronously in handle_initialize before the scan
    // spawns, so polling for it is deterministic and avoids the full scan latency.
    let expected = root.clone();
    let idx = Arc::clone(&indexer);
    poll_until(
        move || {
            idx.workspace_root
                .read()
                .ok()
                .and_then(|g| g.clone())
                .map(|r| r == expected)
                .unwrap_or(false)
        },
        Duration::from_secs(2),
    )
    .await;

    let actual_root = indexer.workspace_root.read().unwrap().clone();
    assert_eq!(
        actual_root.as_deref(),
        Some(root.as_path()),
        "workspace_root should be set synchronously by handle_initialize before scan starts"
    );
}

#[tokio::test]
async fn initialize_writes_explicit_source_paths() {
    let indexer = Arc::new(Indexer::new());
    let tmp = temp_dir();
    let root = tmp.path().to_path_buf();

    let (actor, tx) = make_actor(Arc::clone(&indexer));
    tokio::spawn(actor.run());

    let (completion_tx, completion_rx) = oneshot::channel();
    tx.send(Event::Initialize {
        config: Config {
            root: root.clone(),
            explicit_source_paths: vec!["/some/lib".to_string()],
            ignore_patterns: Vec::new(),
        },
        completion_tx: Some(completion_tx),
    })
    .await
    .unwrap();

    // source_paths_raw is written synchronously in handle_initialize; the
    // completion_tx fires when the background scan also finishes.  Poll the
    // paths directly so the test doesn't wait for the full scan.
    let idx = Arc::clone(&indexer);
    poll_until(
        move || {
            idx.source_paths_raw
                .read()
                .ok()
                .map(|g| g.contains(&"/some/lib".to_string()))
                .unwrap_or(false)
        },
        Duration::from_secs(2),
    )
    .await;
    drop(completion_rx); // not needed; poll was deterministic

    let paths = indexer.source_paths_raw.read().unwrap().clone();
    assert!(
        paths.contains(&"/some/lib".to_string()),
        "source_paths_raw should contain the explicit path, got: {paths:?}"
    );
}

#[tokio::test]
async fn change_root_updates_workspace_root() {
    let indexer = Arc::new(Indexer::new());
    let tmp = temp_dir();
    let root = tmp.path().to_path_buf();

    let (actor, tx) = make_actor(Arc::clone(&indexer));
    tokio::spawn(actor.run());

    // Send ChangeRoot without a prior Initialize — the actor must set root directly.
    tx.send(Event::ChangeRoot { root: root.clone() })
        .await
        .unwrap();

    let expected = root.clone();
    let idx = Arc::clone(&indexer);
    poll_until(
        move || {
            idx.workspace_root
                .read()
                .ok()
                .and_then(|g| g.clone())
                .map(|r| r == expected)
                .unwrap_or(false)
        },
        Duration::from_secs(2),
    )
    .await;

    let actual = indexer.workspace_root.read().unwrap().clone();
    assert_eq!(
        actual.as_deref(),
        Some(root.as_path()),
        "workspace_root should be updated after ChangeRoot"
    );
}

#[tokio::test]
async fn actor_stops_when_sender_dropped() {
    let indexer = Arc::new(Indexer::new());
    let (actor, tx) = make_actor(indexer);

    let handle = tokio::spawn(actor.run());
    drop(tx);

    // Actor should exit cleanly once the sender is dropped
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("actor did not stop within 2s after sender drop")
        .unwrap();
}
