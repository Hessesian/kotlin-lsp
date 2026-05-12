use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

use crate::indexer::{Indexer, NoopReporter};
use crate::workspace::phase::State;
use crate::workspace::Config;

use super::ScanHandler;

fn make_handler(indexer: Arc<Indexer>) -> ScanHandler<NoopReporter> {
    let (scan_done_tx, _scan_done_rx) = mpsc::unbounded_channel();
    ScanHandler::new(
        indexer,
        Arc::new(NoopReporter),
        Arc::new(RwLock::new(State::Uninitialized)),
        scan_done_tx,
    )
}

#[tokio::test]
async fn handle_initialize_updates_root_and_source_paths() {
    let indexer = Arc::new(Indexer::new());
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
    // Opt out of real external sources so the test doesn't scan ~/.kotlin-lsp/sources.
    std::fs::write(root.join("workspace.json"), r#"{"sourcePaths":[]}"#).unwrap();
    let handler = make_handler(Arc::clone(&indexer));

    handler
        .handle_initialize(
            Config {
                root: root.clone(),
                explicit_source_paths: vec!["/some/lib".to_string()],
                ignore_patterns: Vec::new(),
                pin_workspace: false,
            },
            None,
        )
        .await;

    assert_eq!(
        indexer.workspace_root.get().as_deref(),
        Some(root.as_path())
    );
    let state = handler.state_stream();
    let source_paths = state
        .read()
        .await
        .ready()
        .map(|ready| ready.source_paths.clone())
        .unwrap_or_default();
    assert!(source_paths.contains(&"/some/lib".to_string()));
    assert!(indexer
        .source_paths_raw
        .read()
        .unwrap()
        .contains(&"/some/lib".to_string()));
}
