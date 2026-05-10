//! Tests for [`WorkspacePhase`] lifecycle transitions.

use std::path::PathBuf;

use super::{WorkspaceData, WorkspacePhase};

fn stub_data(root: &str) -> WorkspaceData {
    WorkspaceData {
        root: PathBuf::from(root),
        source_paths: vec![format!("{root}/src")],
        ignore_patterns: vec![],
    }
}

#[test]
fn default_phase_is_uninitialized() {
    let phase = WorkspacePhase::default();
    assert!(phase.ready().is_none());
}

#[test]
fn set_ready_transitions_to_ready() {
    let mut phase = WorkspacePhase::default();
    phase.set_ready(stub_data("/workspace"));
    assert!(phase.ready().is_some());
    assert_eq!(phase.ready().unwrap().root, PathBuf::from("/workspace"));
}

#[test]
fn with_ready_runs_block_only_when_ready() {
    let uninitialized = WorkspacePhase::default();
    assert!(uninitialized.with_ready(|d| d.root.clone()).is_none());

    let mut ready = WorkspacePhase::default();
    ready.set_ready(stub_data("/project"));
    let root = ready.with_ready(|d| d.root.clone());
    assert_eq!(root, Some(PathBuf::from("/project")));
}

#[test]
fn set_ready_replaces_existing_ready() {
    let mut phase = WorkspacePhase::default();
    phase.set_ready(stub_data("/old-root"));
    phase.set_ready(stub_data("/new-root"));
    assert_eq!(phase.ready().unwrap().root, PathBuf::from("/new-root"));
}

#[test]
fn source_paths_preserved_in_ready_data() {
    let mut phase = WorkspacePhase::default();
    let mut data = stub_data("/ws");
    data.source_paths.push("/ws/lib".to_owned());
    phase.set_ready(data);
    assert_eq!(
        phase.ready().unwrap().source_paths,
        vec!["/ws/src".to_owned(), "/ws/lib".to_owned()]
    );
}
