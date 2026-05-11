use std::sync::Arc;

use tokio::sync::RwLock;

use crate::indexer::{Indexer, NoopReporter};
use crate::workspace::phase::State;
use crate::workspace::Config;

use super::ScanHandler;

fn make_handler(indexer: Arc<Indexer>) -> ScanHandler<NoopReporter> {
    ScanHandler::new(
        indexer,
        Arc::new(NoopReporter),
        Arc::new(RwLock::new(State::Uninitialized)),
    )
}

#[tokio::test]
async fn handle_initialize_updates_root_and_source_paths() {
    let indexer = Arc::new(Indexer::new());
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path().to_path_buf();
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
        indexer.workspace_root.read().unwrap().as_deref(),
        Some(root.as_path())
    );
    let source_paths = handler.current_source_paths().await;
    assert!(source_paths.contains(&"/some/lib".to_string()));
    assert!(indexer
        .source_paths_raw
        .read()
        .unwrap()
        .contains(&"/some/lib".to_string()));
}
