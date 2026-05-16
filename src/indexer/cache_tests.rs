//! Unit tests for `indexer::cache`.

use std::sync::Arc;
use tower_lsp::lsp_types::{SymbolKind, Url};

use super::*;
use crate::types::{FileData, SymbolEntry, Visibility};

fn uri(path: &str) -> Url {
    Url::parse(&format!("file:///test{path}")).unwrap()
}

use crate::indexer::test_helpers::with_xdg_cache;

/// `cache_entry_to_file_result` must reconstruct supertypes from `FileData.lines`
/// even when the `FileCacheEntry` was loaded from disk (lines are always cached).
#[test]
fn cache_entry_to_file_result_supertypes_extracted() {
    let u = uri("/Cat.kt");
    let mut data = FileData {
        lines: Arc::new(vec![
            "class Cat : IAnimal {".into(),
            "    fun meow() {}".into(),
            "}".into(),
        ]),
        ..FileData::default()
    };
    data.symbols.push(SymbolEntry {
        name: "Cat".into(),
        kind: SymbolKind::CLASS,
        visibility: Visibility::Public,
        range: Default::default(),
        selection_range: Default::default(),
        detail: String::new(),
        type_params: Vec::new(),
        extension_receiver: String::new(),
        extension_receiver_type: String::new(),
        container: None,
        params: String::new(),
        param_counts: (0, 0),
    });
    data.supers.push((0, "IAnimal".into(), vec![]));

    let entry = FileCacheEntry {
        mtime_secs: 100,
        file_size: 0,
        content_hash: 42,
        file_data: std::sync::Arc::new(data),
        qualified_keys: vec![],
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
    let mut data = FileData {
        lines: Arc::new(vec!["class Foo".into()]),
        ..FileData::default()
    };
    data.symbols.push(SymbolEntry {
        name: "Foo".into(),
        kind: SymbolKind::CLASS,
        visibility: Visibility::Public,
        range: Default::default(),
        selection_range: Default::default(),
        detail: String::new(),
        type_params: Vec::new(),
        extension_receiver: String::new(),
        extension_receiver_type: String::new(),
        container: None,
        params: String::new(),
        param_counts: (0, 0),
    });

    let entry = FileCacheEntry {
        mtime_secs: 0,
        file_size: 0,
        content_hash: 0xdeadbeef,
        file_data: std::sync::Arc::new(data),
        qualified_keys: vec![],
    };

    let result = cache_entry_to_file_result(&u, &entry);
    assert_eq!(result.content_hash, 0xdeadbeef);
}

/// `workspace_cache_path` must be stable: same root → same path.
#[test]
fn workspace_cache_path_stable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("my_project");
    with_xdg_cache(tmp.path(), || {
        let p1 = workspace_cache_path(&root);
        let p2 = workspace_cache_path(&root);
        assert_eq!(p1, p2);
    });
}

/// Different roots must produce different cache paths.
#[test]
fn workspace_cache_path_differs_for_different_roots() {
    let tmp = tempfile::tempdir().expect("tempdir");
    with_xdg_cache(tmp.path(), || {
        let p1 = workspace_cache_path(&tmp.path().join("project_a"));
        let p2 = workspace_cache_path(&tmp.path().join("project_b"));
        assert_ne!(p1, p2);
    });
}

/// `try_load_cache` must return `None` for a non-existent root (no panic).
#[test]
fn try_load_cache_missing_returns_none() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).expect("create workspace dir");

    with_xdg_cache(tmp.path(), || {
        let result = try_load_cache(&root);
        assert!(result.is_none());
    });
}

/// `save_cache` → `try_load_cache` roundtrip: symbols survive disk persistence.
#[test]
fn save_and_load_cache_roundtrip() {
    use crate::indexer::Indexer;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("workspace");
    std::fs::create_dir(&root).expect("create workspace dir");

    let src = "package com.example\nclass RoundtripClass";
    let kt_file = tmp.path().join("RoundtripClass.kt");
    std::fs::write(&kt_file, src).expect("write kt file");
    let u = Url::from_file_path(&kt_file).expect("valid file URL");

    let idx = Indexer::new();
    idx.index_content(&u, src);

    with_xdg_cache(tmp.path(), || {
        save_cache(
            &root,
            &idx.files,
            &idx.content_hashes,
            &idx.library_uris,
            true,
        );

        let loaded = try_load_cache(&root).expect("cache should exist after save");
        assert_eq!(loaded.version, CACHE_VERSION);
        assert!(loaded.complete_scan);

        let file_path = kt_file.to_string_lossy().to_string();
        let entry = loaded
            .entries
            .get(&file_path)
            .expect("entry should be present");
        let has_class = entry
            .file_data
            .symbols
            .iter()
            .any(|s| s.name == "RoundtripClass");
        assert!(
            has_class,
            "RoundtripClass symbol missing from cache roundtrip"
        );
    });
}

// ── Chunked library cache tests ──────────────────────────────────────────────

fn make_dummy_entry(_path: &std::path::Path) -> FileCacheEntry {
    FileCacheEntry {
        mtime_secs: 0,
        file_size: 0,
        content_hash: 0,
        file_data: Arc::new(FileData::default()),
        qualified_keys: vec![],
    }
}

fn write_dummy_chunk(dir: &std::path::Path, idx: u32, keys: &[&str]) {
    let entries: HashMap<String, FileCacheEntry> = keys
        .iter()
        .map(|k| (k.to_string(), make_dummy_entry(std::path::Path::new(k))))
        .collect();
    let cache = IndexCache {
        version: CACHE_VERSION,
        complete_scan: true,
        entries,
    };
    let bytes = bincode::serialize(&cache).unwrap();
    std::fs::write(library_chunk_path(dir, idx), bytes).unwrap();
}

fn write_manifest(dir: &std::path::Path, chunk_count: u32) {
    let manifest = LibraryManifest {
        version: CACHE_VERSION,
        chunk_count,
    };
    let bytes = bincode::serialize(&manifest).unwrap();
    std::fs::write(library_manifest_path(dir), bytes).unwrap();
}

/// Missing manifest → `try_load_library_manifest` returns `None`.
#[test]
fn library_manifest_missing_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    with_xdg_cache(tmp.path(), || {
        let source_paths = vec![tmp.path().to_string_lossy().to_string()];
        assert!(
            try_load_library_manifest(&source_paths).is_none(),
            "should return None when no manifest exists"
        );
    });
}

/// Present manifest + all chunks → round-trip returns correct chunk count.
#[test]
fn library_manifest_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    with_xdg_cache(tmp.path(), || {
        let source_paths = vec![tmp.path().to_string_lossy().to_string()];
        let dir = library_chunks_dir(&source_paths);
        std::fs::create_dir_all(&dir).unwrap();

        write_dummy_chunk(&dir, 0, &["/lib/A.kt"]);
        write_dummy_chunk(&dir, 1, &["/lib/B.kt"]);
        write_manifest(&dir, 2);

        let result = try_load_library_manifest(&source_paths);
        assert!(result.is_some(), "should find manifest");
        let (returned_dir, chunk_count) = result.unwrap();
        assert_eq!(returned_dir, dir);
        assert_eq!(chunk_count, 2);
    });
}

/// Loading a missing chunk returns `None` without panicking.
#[test]
fn load_library_chunk_missing_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    assert!(
        load_library_chunk(dir, 0).is_none(),
        "missing chunk should return None"
    );
}

/// A chunk with a wrong CACHE_VERSION is rejected.
#[test]
fn load_library_chunk_version_mismatch_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // Write a chunk with the wrong version.
    let cache = IndexCache {
        version: 0, // intentionally wrong
        complete_scan: true,
        entries: HashMap::new(),
    };
    let bytes = bincode::serialize(&cache).unwrap();
    std::fs::write(library_chunk_path(dir, 0), bytes).unwrap();

    assert!(
        load_library_chunk(dir, 0).is_none(),
        "version-mismatched chunk should return None"
    );
}

/// A corrupt (truncated) chunk file returns `None` without panicking.
#[test]
fn load_library_chunk_corrupt_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(library_chunk_path(dir, 0), b"not valid bincode").unwrap();
    assert!(
        load_library_chunk(dir, 0).is_none(),
        "corrupt chunk should return None"
    );
}

/// Saving with zero library files produces a manifest with chunk_count=0.
/// `try_load_library_manifest` returns `Some` with 0 chunks.
#[test]
fn save_library_cache_zero_files_writes_empty_manifest() {
    use dashmap::{DashMap, DashSet};
    let tmp = tempfile::tempdir().unwrap();
    with_xdg_cache(tmp.path(), || {
        let source_paths = vec![tmp.path().to_string_lossy().to_string()];
        let files: DashMap<String, Arc<crate::types::FileData>> = DashMap::new();
        let hashes: DashMap<String, u64> = DashMap::new();
        let library_uris: DashSet<String> = DashSet::new();

        save_library_cache(&source_paths, &files, &hashes, &library_uris);

        // Empty library still writes a valid manifest (chunk_count=0).
        let result = try_load_library_manifest(&source_paths);
        assert!(
            result.is_some(),
            "manifest expected even for zero-entry library"
        );
        let (_, chunk_count) = result.unwrap();
        assert_eq!(chunk_count, 0, "chunk_count should be 0 for empty library");
    });
}

/// Manifest with wrong CACHE_VERSION is rejected.
#[test]
fn library_manifest_version_mismatch_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    with_xdg_cache(tmp.path(), || {
        let source_paths = vec![tmp.path().to_string_lossy().to_string()];
        let dir = library_chunks_dir(&source_paths);
        std::fs::create_dir_all(&dir).unwrap();

        let manifest = LibraryManifest {
            version: 0, // wrong version
            chunk_count: 1,
        };
        let bytes = bincode::serialize(&manifest).unwrap();
        std::fs::write(library_manifest_path(&dir), bytes).unwrap();

        assert!(
            try_load_library_manifest(&source_paths).is_none(),
            "version-mismatched manifest should be rejected"
        );
    });
}

/// `library_cache_is_fresh` with an empty first chunk and no second chunk
/// returns `true` (nothing to invalidate).
#[test]
fn library_cache_is_fresh_empty_cache() {
    let tmp = tempfile::tempdir().unwrap();
    with_xdg_cache(tmp.path(), || {
        let source_paths_str = vec![tmp.path().to_string_lossy().to_string()];
        let dir = library_chunks_dir(&source_paths_str);
        std::fs::create_dir_all(&dir).unwrap();
        write_manifest(&dir, 1);

        // Touch the manifest to make it newer than any source directory.
        let manifest_path = library_manifest_path(&dir);
        let source_paths: Vec<std::path::PathBuf> = source_paths_str
            .iter()
            .map(std::path::PathBuf::from)
            .collect();

        let fresh = library_cache_is_fresh(&source_paths, &manifest_path, &HashMap::new(), &dir, 1);
        assert!(
            fresh,
            "empty first chunk with nothing to check should be considered fresh"
        );
    });
}

/// A manifest with an implausibly large `chunk_count` is rejected to prevent OOM.
#[test]
fn library_manifest_absurd_chunk_count_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    with_xdg_cache(tmp.path(), || {
        let source_paths = vec![tmp.path().to_string_lossy().to_string()];
        let dir = library_chunks_dir(&source_paths);
        std::fs::create_dir_all(&dir).unwrap();

        let manifest = LibraryManifest {
            version: CACHE_VERSION,
            chunk_count: u32::MAX,
        };
        let bytes = bincode::serialize(&manifest).unwrap();
        std::fs::write(library_manifest_path(&dir), bytes).unwrap();

        assert!(
            try_load_library_manifest(&source_paths).is_none(),
            "manifest with u32::MAX chunk_count should be rejected"
        );
    });
}

/// A manifest pointing to a non-existent first chunk is rejected.
#[test]
fn library_manifest_missing_chunk0_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    with_xdg_cache(tmp.path(), || {
        let source_paths = vec![tmp.path().to_string_lossy().to_string()];
        let dir = library_chunks_dir(&source_paths);
        std::fs::create_dir_all(&dir).unwrap();

        // Write manifest claiming 2 chunks but write no chunk files.
        write_manifest(&dir, 2);

        assert!(
            try_load_library_manifest(&source_paths).is_none(),
            "manifest with missing chunk-0000.bin should be rejected"
        );
    });
}
