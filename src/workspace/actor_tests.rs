//! Integration tests for [`WorkspaceActor`].

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::indexer::{Indexer, NoopReporter};
use crate::workspace::{WorkspaceActor, WorkspaceConfig, WorkspaceEvent};

// ─── helpers ─────────────────────────────────────────────────────────────────

fn make_actor(
    indexer: Arc<Indexer>,
) -> (WorkspaceActor<NoopReporter>, mpsc::Sender<WorkspaceEvent>) {
    let (tx, rx) = mpsc::channel(16);
    let actor = WorkspaceActor::new(indexer, Arc::new(NoopReporter), rx);
    (actor, tx)
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

// ─── actor tests ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn initialize_sets_workspace_root() {
    let indexer = Arc::new(Indexer::new());
    let tmp = temp_dir();
    let root = tmp.path().to_path_buf();

    let (actor, tx) = make_actor(Arc::clone(&indexer));
    tokio::spawn(actor.run());

    tx.send(WorkspaceEvent::Initialize {
        config: WorkspaceConfig {
            root: root.clone(),
            explicit_source_paths: Vec::new(),
            ignore_patterns: Vec::new(),
        },
    })
    .await
    .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

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

    tx.send(WorkspaceEvent::Initialize {
        config: WorkspaceConfig {
            root: root.clone(),
            explicit_source_paths: vec!["/some/lib".to_string()],
            ignore_patterns: Vec::new(),
        },
    })
    .await
    .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

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
    tx.send(WorkspaceEvent::ChangeRoot { root: root.clone() })
        .await
        .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

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
    tokio::time::timeout(tokio::time::Duration::from_secs(2), handle)
        .await
        .expect("actor did not stop within 2s after sender drop")
        .unwrap();
}
