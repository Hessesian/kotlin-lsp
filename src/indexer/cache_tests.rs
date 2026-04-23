//! Unit tests for `indexer::cache`.

use std::sync::Arc;
use tower_lsp::lsp_types::{SymbolKind, Url};

use super::*;
use crate::types::{FileData, SymbolEntry, Visibility};

fn uri(path: &str) -> Url {
    Url::parse(&format!("file:///test{path}")).unwrap()
}

/// `cache_entry_to_file_result` must reconstruct supertypes from `FileData.lines`
/// even when the `FileCacheEntry` was loaded from disk (lines are always cached).
#[test]
fn cache_entry_to_file_result_supertypes_extracted() {
    let u = uri("/Cat.kt");
    let mut data = FileData::default();
    data.lines = Arc::new(vec![
        "class Cat : IAnimal {".into(),
        "    fun meow() {}".into(),
        "}".into(),
    ]);
    data.symbols.push(SymbolEntry {
        name: "Cat".into(),
        kind: SymbolKind::CLASS,
        visibility: Visibility::Public,
        range: Default::default(),
        selection_range: Default::default(),
        detail: String::new(),
    });

    let entry = FileCacheEntry {
        mtime_secs: 100,
        file_size: 0,
        content_hash: 42,
        file_data: data,
    };

    let result = cache_entry_to_file_result(&u, &entry);
    let super_names: Vec<&str> = result.supertypes.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        super_names.contains(&"IAnimal"),
        "IAnimal missing from supertypes: {super_names:?}",
    );
}

/// `cache_entry_to_file_result` must copy `content_hash` through unchanged.
#[test]
fn cache_entry_to_file_result_preserves_hash() {
    let u = uri("/Foo.kt");
    let mut data = FileData::default();
    data.lines = Arc::new(vec!["class Foo".into()]);
    data.symbols.push(SymbolEntry {
        name: "Foo".into(),
        kind: SymbolKind::CLASS,
        visibility: Visibility::Public,
        range: Default::default(),
        selection_range: Default::default(),
        detail: String::new(),
    });

    let entry = FileCacheEntry {
        mtime_secs: 0,
        file_size: 0,
        content_hash: 0xdeadbeef,
        file_data: data,
    };

    let result = cache_entry_to_file_result(&u, &entry);
    assert_eq!(result.content_hash, 0xdeadbeef);
}

/// `workspace_cache_path` must be stable: same root → same path.
#[test]
fn workspace_cache_path_stable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("my_project");
    let p1 = workspace_cache_path(&root);
    let p2 = workspace_cache_path(&root);
    assert_eq!(p1, p2);
}

/// Different roots must produce different cache paths.
#[test]
fn workspace_cache_path_differs_for_different_roots() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let p1 = workspace_cache_path(&tmp.path().join("project_a"));
    let p2 = workspace_cache_path(&tmp.path().join("project_b"));
    assert_ne!(p1, p2);
}

/// `try_load_cache` must return `None` for a non-existent root (no panic).
#[test]
fn try_load_cache_missing_returns_none() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).expect("create workspace dir");

    // Ensure no stale cache exists for this fresh root.
    let cache_path = workspace_cache_path(&root);
    if cache_path.exists() {
        std::fs::remove_file(&cache_path).expect("remove pre-existing cache file");
    }

    let result = try_load_cache(&root);
    assert!(result.is_none());
}

/// `save_cache` → `try_load_cache` roundtrip: symbols survive disk persistence.
#[test]
fn save_and_load_cache_roundtrip() {
    use crate::indexer::Indexer;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).expect("create workspace dir");

    // Build a minimal indexer with one file backed by a real temp path.
    let src = "package com.example\nclass RoundtripClass";
    let kt_file = tmp.path().join("RoundtripClass.kt");
    std::fs::write(&kt_file, src).expect("write kt file");
    let u = Url::from_file_path(&kt_file).expect("valid file URL");

    let idx = Indexer::new();
    idx.index_content(&u, src);

    // Write cache to the workspace root (workspace_cache_path uses XDG_CACHE_HOME
    // which may point to ~/.cache; we accept that for this test and clean up below).
    save_cache(&root, &idx.files, &idx.content_hashes, &idx.library_uris, true);
    let cache_path = workspace_cache_path(&root);

    // Read cache back.
    let loaded = try_load_cache(&root).expect("cache should exist after save");
    assert_eq!(loaded.version, CACHE_VERSION);
    assert!(loaded.complete_scan);

    let file_path = kt_file.to_string_lossy().to_string();
    let entry = loaded.entries.get(&file_path).expect("entry should be present");
    let has_class = entry.file_data.symbols.iter().any(|s| s.name == "RoundtripClass");
    assert!(has_class, "RoundtripClass symbol missing from cache roundtrip");

    // Clean up the cache file written to the user's cache dir.
    let _ = std::fs::remove_file(&cache_path);
}
