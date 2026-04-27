//! Unit tests for `indexer::scan`.
//!
//! Declared from `scan.rs` as:
//!   ```ignore
//!   #[cfg(test)]
//!   #[path = "scan_tests.rs"]
//!   mod tests;
//!   ```
//!
//! `super` resolves to the `scan` module, giving access to its private items
//! (Rust allows descendant modules to read their parent's private items).

use std::sync::Arc;

use super::{find_files_for_types, resolve_max_files, IndexingGuard};
use crate::indexer::Indexer;
use crate::indexer::test_helpers::{ENV_VAR_LOCK, with_env_var, with_env_var_unset};

// ─── resolve_max_files ────────────────────────────────────────────────────────

#[test]
fn resolve_max_files_uses_env_issue_scan() {
    with_env_var("KOTLIN_LSP_MAX_FILES", "500", &ENV_VAR_LOCK, || {
        assert_eq!(resolve_max_files(2000), 500, "expected env var value 500");
    });
}

#[test]
fn resolve_max_files_uses_default_when_unset_issue_scan() {
    with_env_var_unset("KOTLIN_LSP_MAX_FILES", &ENV_VAR_LOCK, || {
        assert_eq!(resolve_max_files(2000), 2000, "expected default 2000 when env var is absent");
    });
}

#[test]
fn resolve_max_files_uses_default_when_invalid_issue_scan() {
    with_env_var("KOTLIN_LSP_MAX_FILES", "notanumber", &ENV_VAR_LOCK, || {
        assert_eq!(resolve_max_files(1234), 1234, "expected default 1234 for unparseable env var");
    });
}

// ─── find_files_for_types ─────────────────────────────────────────────────────

#[test]
fn find_files_for_types_empty_names_returns_empty_issue_scan() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("Foo.kt"), "class Foo {}").expect("write");
    let paths = find_files_for_types(&[], tmp.path(), None);
    assert!(paths.is_empty(), "expected empty result for empty names slice, got: {paths:?}");
}

#[test]
fn find_files_for_types_finds_class_issue_scan() {
    // Check rg is available first; skip test if not.
    let rg_available = std::process::Command::new("rg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !rg_available {
        eprintln!("skipping find_files_for_types_finds_class_issue_scan: rg not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let kt = tmp.path().join("Foo.kt");
    std::fs::write(&kt, "class Foo {}").expect("write Foo.kt");

    let paths = find_files_for_types(&["Foo".to_owned()], tmp.path(), None);
    assert!(
        paths.iter().any(|p| p.file_name().and_then(|n| n.to_str()) == Some("Foo.kt")),
        "Foo.kt should appear in results for names=[\"Foo\"], got: {paths:?}"
    );
}

// ─── IndexingGuard ────────────────────────────────────────────────────────────

#[test]
fn indexing_guard_clears_flag_on_drop_issue_scan() {
    use std::sync::atomic::Ordering;

    let idx = Arc::new(Indexer::new());
    idx.indexing_in_progress.store(true, Ordering::Release);

    {
        let _guard = IndexingGuard { indexer: Arc::clone(&idx) };
        assert!(idx.indexing_in_progress.load(Ordering::Acquire), "flag should remain true while guard is live");
    }

    assert!(!idx.indexing_in_progress.load(Ordering::Acquire), "IndexingGuard::drop must set indexing_in_progress to false");
}

// ─── index_workspace_full ────────────────────────────────────────────────────

#[tokio::test]
async fn index_workspace_full_indexes_kt_files_issue_scan() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("mkdir workspace");
    std::fs::write(workspace.join("Foo.kt"), "class Foo {}").expect("write Foo.kt");

    let idx = Arc::new(Indexer::new());

    // Set XDG_CACHE_HOME so save_cache_to_disk writes into the temp dir.
    // EnvVarGuard is drop-safe even across .await points, unlike the sync
    // with_xdg_cache helper.
    let _lock = crate::indexer::test_helpers::XDG_CACHE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _xdg = crate::indexer::test_helpers::EnvVarGuard::set("XDG_CACHE_HOME", tmp.path());

    Arc::clone(&idx).index_workspace_full(&workspace, None).await;

    assert!(
        idx.definitions.contains_key("Foo"),
        "Foo must appear in definitions after index_workspace_full; got: {:?}",
        idx.definitions.iter().map(|e| e.key().clone()).collect::<Vec<_>>()
    );
}

// ─── queued reindex ───────────────────────────────────────────────────────────

#[tokio::test]
async fn queued_reindex_executes_after_first_scan_completes_issue_scan() {
    use std::sync::atomic::Ordering;

    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("mkdir");
    std::fs::write(workspace.join("Foo.kt"), "class Foo {}").expect("write Foo.kt");

    let idx = Arc::new(Indexer::new());
    let _lock = crate::indexer::test_helpers::XDG_CACHE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _xdg = crate::indexer::test_helpers::EnvVarGuard::set("XDG_CACHE_HOME", tmp.path());

    // Simulate a running scan by manually setting the flag.
    idx.indexing_in_progress.store(true, Ordering::Release);

    // A concurrent scan request queues itself and returns immediately (aborted).
    Arc::clone(&idx).index_workspace(&workspace, None).await;

    assert!(
        idx.pending_reindex.load(Ordering::Acquire),
        "pending_reindex must be true after queueing during active scan"
    );
    assert!(
        idx.definitions.is_empty(),
        "definitions must stay empty — queued scan hasn't run yet"
    );

    // Simulate the first scan completing: clear the flag so run_pending_reindex can proceed.
    idx.indexing_in_progress.store(false, Ordering::Release);

    // Drain the queue — this should run the queued reindex.
    Arc::clone(&idx).run_pending_reindex(None).await;

    assert!(
        !idx.pending_reindex.load(Ordering::Acquire),
        "pending_reindex must be cleared after run_pending_reindex drains the queue"
    );
    assert!(
        idx.definitions.contains_key("Foo"),
        "Foo must be indexed after queued reindex executes; got: {:?}",
        idx.definitions.iter().map(|e| e.key().clone()).collect::<Vec<_>>()
    );
}


#[tokio::test]
async fn index_workspace_skips_second_concurrent_run_issue_scan() {
    use std::sync::atomic::Ordering;

    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("mkdir workspace");
    std::fs::write(workspace.join("Bar.kt"), "class Bar {}").expect("write Bar.kt");

    let idx = Arc::new(Indexer::new());
    idx.indexing_in_progress.store(true, Ordering::Release);

    Arc::clone(&idx).index_workspace(&workspace, None).await;

    // indexing_in_progress was already true → impl returned early WITHOUT creating
    // the guard, so the flag was NOT cleared. The flag stays true.
    assert!(
        idx.indexing_in_progress.load(Ordering::Acquire),
        "flag must remain true when second run aborts (guard not created)"
    );
    assert!(
        idx.definitions.is_empty(),
        "definitions must stay empty when scan aborts due to concurrent run; got: {:?}",
        idx.definitions.iter().map(|e| e.key().clone()).collect::<Vec<_>>()
    );
}
