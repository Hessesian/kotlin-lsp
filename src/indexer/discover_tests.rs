//! Unit tests for `indexer::discover`.

use std::path::Path;

use super::{find_source_files, find_source_files_unconstrained, warm_discover_files};
use crate::rg::IgnoreMatcher;

/// `find_source_files` on a directory with no source files returns an empty vec.
#[test]
fn find_source_files_empty_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = find_source_files(tmp.path(), None);
    assert!(paths.is_empty(), "expected no files in empty dir, got: {paths:?}");
}

/// `find_source_files` discovers .kt files.
#[test]
fn find_source_files_finds_kt() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("Foo.kt"), "class Foo").expect("write");
    std::fs::write(tmp.path().join("Bar.txt"), "text").expect("write");

    let paths = find_source_files(tmp.path(), None);
    let names: Vec<_> = paths.iter()
        .filter_map(|p| p.file_name()?.to_str())
        .collect();
    assert!(names.contains(&"Foo.kt"), "Foo.kt missing: {names:?}");
    assert!(!names.contains(&"Bar.txt"), "Bar.txt should not be included");
}

/// `find_source_files` discovers .java files.
#[test]
fn find_source_files_finds_java() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("Hello.java"), "class Hello {}").expect("write");

    let paths = find_source_files(tmp.path(), None);
    let names: Vec<_> = paths.iter()
        .filter_map(|p| p.file_name()?.to_str())
        .collect();
    assert!(names.contains(&"Hello.java"), "Hello.java missing: {names:?}");
}

/// `find_source_files` with an IgnoreMatcher that matches the file should exclude it.
#[test]
fn find_source_files_respects_ignore_matcher() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let sub = tmp.path().join("generated");
    std::fs::create_dir(&sub).expect("mkdir");
    std::fs::write(sub.join("Gen.kt"), "class Gen").expect("write");
    std::fs::write(tmp.path().join("Keep.kt"), "class Keep").expect("write");

    let matcher = IgnoreMatcher::new(vec!["generated/**".to_owned()], tmp.path());
    let paths = find_source_files(tmp.path(), Some(&matcher));
    let names: Vec<_> = paths.iter()
        .filter_map(|p| p.file_name()?.to_str())
        .collect();
    assert!(names.contains(&"Keep.kt"), "Keep.kt should be found");
    assert!(!names.contains(&"Gen.kt"), "Gen.kt inside 'generated/' should be excluded");
}

/// `find_source_files_unconstrained` finds .kt files without skipping `build` dirs.
#[test]
fn find_source_files_unconstrained_includes_build_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let build = tmp.path().join("build");
    std::fs::create_dir(&build).expect("mkdir build");
    std::fs::write(build.join("Generated.kt"), "class Generated").expect("write");

    let paths = find_source_files_unconstrained(tmp.path());
    let names: Vec<_> = paths.iter()
        .filter_map(|p| p.file_name()?.to_str())
        .collect();
    assert!(names.contains(&"Generated.kt"), "Generated.kt in build/ should be found by unconstrained scan");
}

/// `warm_discover_files` on a fresh cache with a real file returns that file.
#[test]
fn warm_discover_files_returns_cached_existing_files() {
    use std::collections::HashMap;
    use crate::indexer::cache::{FileCacheEntry, IndexCache, CACHE_VERSION};
    use crate::types::FileData;

    let tmp = tempfile::tempdir().expect("tempdir");
    let kt = tmp.path().join("Main.kt");
    std::fs::write(&kt, "class Main").expect("write");

    let mut entries = HashMap::new();
    entries.insert(
        kt.to_string_lossy().to_string(),
        FileCacheEntry {
            mtime_secs: 0,
            file_size: 0,
            content_hash: 0,
            file_data: FileData::default(),
        },
    );
    let cache = IndexCache {
        version: CACHE_VERSION,
        complete_scan: true,
        entries,
    };

    let paths = warm_discover_files(tmp.path(), &cache, None);
    let names: Vec<_> = paths.iter()
        .filter_map(|p| p.file_name()?.to_str())
        .collect();
    assert!(names.contains(&"Main.kt"), "Main.kt should be returned by warm_discover_files");
}

/// `warm_discover_files` excludes cached files that no longer exist on disk.
#[test]
fn warm_discover_files_skips_deleted_files() {
    use std::collections::HashMap;
    use crate::indexer::cache::{FileCacheEntry, IndexCache, CACHE_VERSION};
    use crate::types::FileData;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ghost = tmp.path().join("Deleted.kt");
    // Do NOT create the file — it's "in the cache" but deleted on disk.

    let mut entries = HashMap::new();
    entries.insert(
        ghost.to_string_lossy().to_string(),
        FileCacheEntry {
            mtime_secs: 0,
            file_size: 0,
            content_hash: 0,
            file_data: FileData::default(),
        },
    );
    let cache = IndexCache {
        version: CACHE_VERSION,
        complete_scan: true,
        entries,
    };

    let paths = warm_discover_files(tmp.path(), &cache, None);
    assert!(
        !paths.iter().any(|p| p.file_name().map(|n| n == "Deleted.kt").unwrap_or(false)),
        "deleted file should not appear in warm_discover_files result"
    );
}
