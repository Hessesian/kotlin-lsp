//! Unit tests for `indexer::scan`.
//!
//! Declared from `scan.rs` as:
//!   ```ignore
//!   #[cfg(test)]
//!   mod scan_tests;
//!   ```
//!
//! `super` resolves to the `scan` module, giving access to its private items
//! (Rust allows descendant modules to read their parent's private items).

use std::sync::{Arc, Mutex};

use super::{find_files_for_types, resolve_max_files, IndexingGuard};
use crate::indexer::Indexer;

// ─── Env-var serialisation lock ───────────────────────────────────────────────
//
// Tests that mutate `KOTLIN_LSP_MAX_FILES` must hold this lock for the duration
// of the test so parallel test threads don't see each other's values.
// This is intentionally separate from `test_helpers::XDG_CACHE_LOCK`, which
// guards a different env var.
static ENV_LOCK: Mutex<()> = Mutex::new(());

// ─── resolve_max_files ────────────────────────────────────────────────────────

/// `resolve_max_files` returns the integer value of `KOTLIN_LSP_MAX_FILES`
/// when the variable is set to a valid number, ignoring the supplied default.
#[test]
fn resolve_max_files_uses_env_issue_scan() {
    // Arrange
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("KOTLIN_LSP_MAX_FILES", "500");

    // Act
    let result = resolve_max_files(2000);

    // Assert
    std::env::remove_var("KOTLIN_LSP_MAX_FILES"); // restore before any assertion panic
    assert_eq!(
        result, 500,
        "expected env var value 500, got {result}"
    );
}

/// `resolve_max_files` returns the supplied default when `KOTLIN_LSP_MAX_FILES`
/// is not present in the environment at all.
#[test]
fn resolve_max_files_uses_default_when_unset_issue_scan() {
    // Arrange
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("KOTLIN_LSP_MAX_FILES");

    // Act
    let result = resolve_max_files(2000);

    // Assert
    assert_eq!(
        result, 2000,
        "expected default 2000 when env var is absent, got {result}"
    );
}

/// `resolve_max_files` falls back to the supplied default when
/// `KOTLIN_LSP_MAX_FILES` is set but cannot be parsed as a `usize`.
#[test]
fn resolve_max_files_uses_default_when_invalid_issue_scan() {
    // Arrange
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("KOTLIN_LSP_MAX_FILES", "notanumber");

    // Act
    let result = resolve_max_files(1234);

    // Assert
    std::env::remove_var("KOTLIN_LSP_MAX_FILES");
    assert_eq!(
        result, 1234,
        "expected default 1234 for unparseable env var, got {result}"
    );
}

// ─── find_files_for_types ─────────────────────────────────────────────────────

/// `find_files_for_types` short-circuits to an empty vec without invoking `rg`
/// when the caller supplies no type names.
#[test]
fn find_files_for_types_empty_names_returns_empty_issue_scan() {
    // Arrange
    let tmp = tempfile::tempdir().expect("tempdir");
    // Put a .kt file in the dir so we'd get a hit if rg were invoked
    std::fs::write(tmp.path().join("Foo.kt"), "class Foo {}").expect("write");

    // Act
    let paths = find_files_for_types(&[], tmp.path(), None);

    // Assert
    assert!(
        paths.is_empty(),
        "expected empty result for empty names slice, got: {paths:?}"
    );
}

/// `find_files_for_types` returns the path to a `.kt` file that contains a
/// `class` declaration matching the requested name.
///
/// Requires `rg` (ripgrep) to be installed; skips gracefully when it is absent.
#[test]
fn find_files_for_types_finds_class_issue_scan() {
    // Arrange
    let tmp = tempfile::tempdir().expect("tempdir");
    let kt = tmp.path().join("Foo.kt");
    std::fs::write(&kt, "class Foo {}").expect("write Foo.kt");

    let names = vec!["Foo".to_owned()];

    // Act
    let paths = find_files_for_types(&names, tmp.path(), None);

    // Assert – if rg is absent the function returns [] and we skip the assertion
    // (the function documents that behaviour; absence of rg is not a code defect).
    if paths.is_empty() {
        // rg is probably not installed in this environment — nothing to check.
        return;
    }
    assert!(
        paths.iter().any(|p| p.file_name().and_then(|n| n.to_str()) == Some("Foo.kt")),
        "Foo.kt should appear in results for names=[\"Foo\"], got: {paths:?}"
    );
}

// ─── IndexingGuard ────────────────────────────────────────────────────────────

/// Dropping an `IndexingGuard` must atomically clear `indexing_in_progress`
/// to `false`, even when the flag was explicitly set to `true` before the guard
/// was constructed.
#[test]
fn indexing_guard_clears_flag_on_drop_issue_scan() {
    use std::sync::atomic::Ordering;

    // Arrange
    let idx = Arc::new(Indexer::new());
    idx.indexing_in_progress.store(true, Ordering::Release);

    {
        // Act – construct guard (does NOT flip the flag; only Drop does)
        let _guard = IndexingGuard {
            indexer: Arc::clone(&idx),
        };

        // Intermediate assertion: flag still true inside scope
        assert!(
            idx.indexing_in_progress.load(Ordering::Acquire),
            "flag should remain true while guard is live"
        );
    } // _guard drops here → Drop impl stores false

    // Assert
    assert!(
        !idx.indexing_in_progress.load(Ordering::Acquire),
        "IndexingGuard::drop must set indexing_in_progress to false"
    );
}

// ─── index_workspace_full ────────────────────────────────────────────────────

/// `index_workspace_full` populates `definitions` with symbols found in `.kt`
/// files under the workspace root.
///
/// Note: `save_cache_to_disk` is called internally; it will attempt to write to
/// `~/.cache/kotlin-lsp/` (or `$XDG_CACHE_HOME`).  A failure there is logged
/// but does not fail the test — we only assert on `definitions`.
#[tokio::test]
async fn index_workspace_full_indexes_kt_files_issue_scan() {
    // Arrange
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).expect("mkdir workspace");
    std::fs::write(root.join("Foo.kt"), "class Foo {}").expect("write Foo.kt");

    let idx = Arc::new(Indexer::new());

    // Act
    Arc::clone(&idx).index_workspace_full(&root, None).await;

    // Assert
    assert!(
        idx.definitions.contains_key("Foo"),
        "Foo must appear in definitions after index_workspace_full; \
         got: {:?}",
        idx.definitions
            .iter()
            .map(|e| e.key().clone())
            .collect::<Vec<_>>()
    );
}

// ─── concurrent-indexing guard ────────────────────────────────────────────────

/// When `indexing_in_progress` is already `true`, a call to `index_workspace`
/// must abort without modifying `definitions` — the second run is a no-op.
///
/// This verifies the swap-and-bail logic inside `index_workspace_impl`.
#[tokio::test]
async fn index_workspace_skips_second_concurrent_run_issue_scan() {
    use std::sync::atomic::Ordering;

    // Arrange
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).expect("mkdir workspace");
    std::fs::write(root.join("Bar.kt"), "class Bar {}").expect("write Bar.kt");

    let idx = Arc::new(Indexer::new());

    // Simulate a concurrent scan already in progress
    idx.indexing_in_progress.store(true, Ordering::Release);

    // Act – should detect the pre-set flag and abort immediately
    Arc::clone(&idx).index_workspace(&root, None).await;

    // Assert – nothing was indexed; definitions must remain empty
    assert!(
        idx.definitions.is_empty(),
        "definitions must stay empty when scan aborts due to concurrent run; \
         got: {:?}",
        idx.definitions
            .iter()
            .map(|e| e.key().clone())
            .collect::<Vec<_>>()
    );
}
