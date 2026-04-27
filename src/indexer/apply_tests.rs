//! Tests for `indexer::apply` — the "write path" of the index.
//!
//! `super` = `crate::indexer::apply`
//! `crate::indexer` = the parent `indexer` module.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use tower_lsp::lsp_types::*;

use crate::indexer::{Indexer, file_contributions, stale_keys_for, build_bare_names};
use super::super::cache::{FileCacheEntry, cache_entry_to_file_result};
use crate::types::{WorkspaceIndexResult, IndexStats};

// ── Test helpers ──────────────────────────────────────────────────────────────

/// Build a `file://` URL rooted under the system temp directory.
fn uri(name: &str) -> Url {
    let path = std::env::temp_dir().join(name.trim_start_matches('/'));
    Url::from_file_path(path).unwrap()
}

/// Convenience: create an Indexer and call `index_content` once.
fn indexed(name: &str, src: &str) -> (Url, Indexer) {
    let u = uri(name);
    let idx = Indexer::new();
    idx.index_content(&u, src);
    (u, idx)
}

// ─────────────────────────────────────────────────────────────────────────────
//  Migrated tests (originally in indexer.rs inline test module)
// ─────────────────────────────────────────────────────────────────────────────

/// `parse_file` must return the URI, package, and both symbols from the source.
#[test]
fn parse_file_returns_symbols_issue_apply() {
    let src = r#"
package com.example

class Foo {
    fun bar(): String = "test"
}
"#;
    let u = uri("/Foo.kt");
    let result = Indexer::parse_file(&u, src);

    assert_eq!(result.uri, u);
    assert_eq!(result.data.package, Some("com.example".to_string()));
    assert_eq!(result.data.symbols.len(), 2, "expected class + fun, got: {:?}",
        result.data.symbols.iter().map(|s| &s.name).collect::<Vec<_>>());
    assert!(result.data.symbols.iter().any(|s| s.name == "Foo"), "Foo missing");
    assert!(result.data.symbols.iter().any(|s| s.name == "bar"), "bar missing");
    assert!(result.error.is_none());
}

/// `parse_file` must populate `supertypes` when a class extends an interface.
#[test]
fn parse_file_extracts_supertypes_issue_apply() {
    let src = r#"
interface Base
class Child : Base
"#;
    let u = uri("/Child.kt");
    let result = Indexer::parse_file(&u, src);

    assert!(
        result.supertypes.iter().any(|(name, _)| name == "Base"),
        "expected (\"Base\", _) in supertypes; got: {:?}", result.supertypes,
    );
}

/// `apply_file_result` must insert the symbol into `definitions` with the file's URI.
#[test]
fn apply_file_result_updates_index_issue_apply() {
    let u = uri("/Test.kt");
    let result = Indexer::parse_file(&u, "class TestClass");

    let idx = Indexer::new();
    idx.apply_file_result(&result);

    assert!(idx.definitions.contains_key("TestClass"), "TestClass missing from definitions");
    let locs = idx.definitions.get("TestClass").unwrap();
    assert_eq!(locs.len(), 1);
    assert_eq!(locs[0].uri, u);
}

/// Re-indexing the same file with new content must remove the old symbol.
#[test]
fn apply_file_result_clears_stale_entries_issue_apply() {
    let u = uri("/Test.kt");
    let idx = Indexer::new();

    // First pass: OldName
    idx.apply_file_result(&Indexer::parse_file(&u, "class OldName"));
    assert!(idx.definitions.contains_key("OldName"), "OldName must exist after first apply");

    // Second pass: NewName (same URI)
    idx.apply_file_result(&Indexer::parse_file(&u, "class NewName"));

    let old_empty = idx.definitions.get("OldName")
        .map(|v| v.is_empty())
        .unwrap_or(true);
    assert!(old_empty, "OldName locations should be cleared after re-index");
    assert!(idx.definitions.contains_key("NewName"), "NewName should be present");
}

/// When file `Foo.kt` declares `class Bar`, `apply_file_result` must insert
/// *both* `com.example.Bar` (pkg.Sym) and `com.example.Foo.Bar` (pkg.FileStem.Sym).
/// After re-indexing with Bar removed, both qualified keys must be gone.
#[test]
fn apply_file_result_removes_both_stale_qualified_keys_issue_apply() {
    let idx = Indexer::new();
    let u = Url::parse("file:///pkg/Foo.kt").unwrap();

    // First index: Bar exists
    idx.index_content(&u, "package com.example\nclass Bar {}");
    assert!(idx.qualified.contains_key("com.example.Bar"),     "initial pkg.Sym missing");
    assert!(idx.qualified.contains_key("com.example.Foo.Bar"), "initial pkg.Stem.Sym missing");

    // Re-index: Bar removed
    idx.index_content(&u, "package com.example\n// empty");
    assert!(!idx.qualified.contains_key("com.example.Bar"),     "stale pkg.Sym not removed");
    assert!(!idx.qualified.contains_key("com.example.Foo.Bar"), "stale pkg.Stem.Sym not removed");
}

/// `apply_workspace_result` must index files that were cache-hits (not just freshly parsed).
#[test]
fn apply_workspace_result_includes_cached_files_issue_apply() {
    let u = uri("/Cached.kt");

    // Simulate a cache hit: parse first, then wrap in a FileCacheEntry.
    let parsed = Indexer::parse_file(&u, "class CachedClass");
    let entry = FileCacheEntry {
        mtime_secs:   0,
        file_size:    0,
        content_hash: 0,
        file_data:    parsed.data.clone(),
    };
    let cached_result = cache_entry_to_file_result(&u, &entry);

    let workspace_result = WorkspaceIndexResult {
        files:          vec![cached_result],
        stats:          IndexStats { cache_hits: 1, ..Default::default() },
        workspace_root: std::path::PathBuf::from("/"),
        aborted:        false,
        complete_scan:  true,
    };

    let idx = Indexer::new();
    idx.apply_workspace_result(&workspace_result);

    assert!(
        idx.definitions.contains_key("CachedClass"),
        "CachedClass from cache hit should be in definitions index",
    );
    assert!(
        idx.files.contains_key(u.as_str()),
        "CachedClass file should be in files map",
    );
}

/// Switching workspaces: `apply_workspace_result` must clear symbols from the
/// previous workspace before applying the new one.
#[test]
fn apply_workspace_result_clears_stale_workspace_issue_apply() {
    let idx = Indexer::new();

    // First workspace: ClassA
    let u1 = uri("/A.kt");
    idx.apply_workspace_result(&WorkspaceIndexResult {
        files:          vec![Indexer::parse_file(&u1, "class ClassA")],
        stats:          IndexStats::default(),
        workspace_root: std::path::PathBuf::from("/workspace_a"),
        aborted:        false,
        complete_scan:  true,
    });
    assert!(idx.definitions.contains_key("ClassA"), "ClassA must be present after first apply");

    // Second workspace: ClassB only
    let u2 = uri("/B.kt");
    idx.apply_workspace_result(&WorkspaceIndexResult {
        files:          vec![Indexer::parse_file(&u2, "class ClassB")],
        stats:          IndexStats::default(),
        workspace_root: std::path::PathBuf::from("/workspace_b"),
        aborted:        false,
        complete_scan:  true,
    });

    assert!(
        !idx.definitions.contains_key("ClassA"),
        "ClassA must be gone after workspace switch",
    );
    assert!(
        idx.definitions.contains_key("ClassB"),
        "ClassB must be present after workspace switch",
    );
}

/// `apply_workspace_result` must combine both cache-hit and freshly-parsed files.
#[test]
fn apply_workspace_result_mixed_cache_and_parsed_issue_apply() {
    let u_cached = uri("/Cached.kt");
    let u_parsed  = uri("/Parsed.kt");

    let cached_parse = Indexer::parse_file(&u_cached, "class CachedClass");
    let entry = FileCacheEntry {
        mtime_secs: 0, file_size: 0, content_hash: 0,
        file_data: cached_parse.data.clone(),
    };
    let cached_result = cache_entry_to_file_result(&u_cached, &entry);
    let parsed_result  = Indexer::parse_file(&u_parsed, "class ParsedClass");

    let idx = Indexer::new();
    idx.apply_workspace_result(&WorkspaceIndexResult {
        files:          vec![cached_result, parsed_result],
        stats:          IndexStats { cache_hits: 1, files_parsed: 1, ..Default::default() },
        workspace_root: std::path::PathBuf::from("/"),
        aborted:        false,
        complete_scan:  true,
    });

    assert!(idx.definitions.contains_key("CachedClass"),  "CachedClass (from cache) should be in index");
    assert!(idx.definitions.contains_key("ParsedClass"),  "ParsedClass (freshly parsed) should be in index");
    assert_eq!(idx.files.len(), 2, "exactly 2 files in index");
}

// ─────────────────────────────────────────────────────────────────────────────
//  New tests: `file_contributions` (pure)
// ─────────────────────────────────────────────────────────────────────────────

/// `file_contributions` must map each symbol name to a location in `definitions`.
#[test]
fn file_contributions_produces_definitions_issue_apply() {
    let u = uri("/Bar.kt");
    let result = Indexer::parse_file(&u, "class Bar");
    let contrib = file_contributions(&result);

    assert!(
        contrib.definitions.contains_key("Bar"),
        "definitions must contain the symbol name",
    );
    let locs = &contrib.definitions["Bar"];
    assert_eq!(locs.len(), 1);
    assert_eq!(locs[0].uri, u);
}

/// `file_contributions` must insert `pkg.Sym` into the qualified map.
#[test]
fn file_contributions_produces_qualified_pkg_sym_issue_apply() {
    let u = uri("/Bar.kt");
    let result = Indexer::parse_file(&u, "package com.example\nclass Bar");
    let contrib = file_contributions(&result);

    assert!(
        contrib.qualified.contains_key("com.example.Bar"),
        "qualified must contain pkg.Sym; keys: {:?}",
        contrib.qualified.keys().collect::<Vec<_>>(),
    );
}

/// When the file stem differs from the symbol name, `file_contributions` must
/// insert BOTH `pkg.Sym` and `pkg.FileStem.Sym` into the qualified map.
#[test]
fn file_contributions_dual_qualified_keys_when_stem_differs_issue_apply() {
    // File is Foo.kt but defines class Bar → stem("Foo") ≠ name("Bar")
    let u = uri("/Foo.kt");
    let result = Indexer::parse_file(&u, "package com.example\nclass Bar");
    let contrib = file_contributions(&result);

    assert!(
        contrib.qualified.contains_key("com.example.Bar"),
        "pkg.Sym key missing; keys: {:?}",
        contrib.qualified.keys().collect::<Vec<_>>(),
    );
    assert!(
        contrib.qualified.contains_key("com.example.Foo.Bar"),
        "pkg.FileStem.Sym key missing; keys: {:?}",
        contrib.qualified.keys().collect::<Vec<_>>(),
    );
}

/// When the symbol name equals the file stem, NO extra `pkg.Stem.Sym` key
/// should be produced (it would duplicate the primary `pkg.Sym` key).
#[test]
fn file_contributions_no_stem_key_when_sym_equals_stem_issue_apply() {
    // File is Foo.kt and defines class Foo → stem == name
    let u = uri("/Foo.kt");
    let result = Indexer::parse_file(&u, "package com.example\nclass Foo");
    let contrib = file_contributions(&result);

    // Primary key must exist
    assert!(
        contrib.qualified.contains_key("com.example.Foo"),
        "pkg.Sym key must be present",
    );
    // Duplicate stem key must NOT exist
    assert!(
        !contrib.qualified.contains_key("com.example.Foo.Foo"),
        "pkg.Stem.Sym must NOT be inserted when stem == sym",
    );
}

/// `file_contributions` must populate `subtypes` from `result.supertypes`.
#[test]
fn file_contributions_populates_subtypes_from_supertypes_issue_apply() {
    let u = uri("/Child.kt");
    // Child : Base  →  supertypes = [("Base", location-of-Child)]
    let result = Indexer::parse_file(&u, "class Child : Base");
    let contrib = file_contributions(&result);

    assert!(
        contrib.subtypes.contains_key("Base"),
        "subtypes must contain the supertype name; keys: {:?}",
        contrib.subtypes.keys().collect::<Vec<_>>(),
    );
    let locs = &contrib.subtypes["Base"];
    assert!(!locs.is_empty(), "subtypes[\"Base\"] must have at least one location");
}

// ─────────────────────────────────────────────────────────────────────────────
//  New tests: `stale_keys_for` (pure)
// ─────────────────────────────────────────────────────────────────────────────

/// `stale_keys_for` must return every symbol name from the old FileData.
#[test]
fn stale_keys_returns_definition_names_issue_apply() {
    let u = uri("/Foo.kt");
    let result = Indexer::parse_file(&u, "class Foo\nfun bar() {}");
    let stale = stale_keys_for(&u, &result.data);

    assert!(
        stale.definition_names.contains(&"Foo".to_string()),
        "Foo missing from stale definition_names: {:?}", stale.definition_names,
    );
    assert!(
        stale.definition_names.contains(&"bar".to_string()),
        "bar missing from stale definition_names: {:?}", stale.definition_names,
    );
}

/// When stem ≠ symbol, `stale_keys_for` must return both `pkg.Sym` and `pkg.Stem.Sym`.
#[test]
fn stale_keys_returns_both_qualified_keys_when_stem_differs_issue_apply() {
    // File Foo.kt declares class Bar in package com.example
    let u = uri("/Foo.kt");
    let result = Indexer::parse_file(&u, "package com.example\nclass Bar");
    let stale = stale_keys_for(&u, &result.data);

    assert!(
        stale.qualified_keys.contains(&"com.example.Bar".to_string()),
        "pkg.Sym key missing from stale keys: {:?}", stale.qualified_keys,
    );
    assert!(
        stale.qualified_keys.contains(&"com.example.Foo.Bar".to_string()),
        "pkg.Stem.Sym key missing from stale keys: {:?}", stale.qualified_keys,
    );
}

/// When stem == symbol, `stale_keys_for` must NOT produce a duplicate stem key.
#[test]
fn stale_keys_no_stem_key_when_sym_equals_stem_issue_apply() {
    // File Foo.kt declares class Foo — stem == name
    let u = uri("/Foo.kt");
    let result = Indexer::parse_file(&u, "package com.example\nclass Foo");
    let stale = stale_keys_for(&u, &result.data);

    assert!(
        stale.qualified_keys.contains(&"com.example.Foo".to_string()),
        "pkg.Sym must be present: {:?}", stale.qualified_keys,
    );
    assert!(
        !stale.qualified_keys.contains(&"com.example.Foo.Foo".to_string()),
        "duplicate pkg.Stem.Sym must NOT appear when stem == sym: {:?}",
        stale.qualified_keys,
    );
}

/// `stale_keys_for` must propagate the package from the old FileData.
#[test]
fn stale_keys_propagates_package_issue_apply() {
    let u = uri("/Foo.kt");
    let result = Indexer::parse_file(&u, "package com.example\nclass Foo");
    let stale = stale_keys_for(&u, &result.data);

    assert_eq!(
        stale.package.as_deref(),
        Some("com.example"),
        "package field should match the parsed package",
    );
}

// ─────────────────────────────────────────────────────────────────────────────
//  New tests: `build_bare_names` (pure)
// ─────────────────────────────────────────────────────────────────────────────

/// `build_bare_names` must return a sorted, deduplicated list of keys.
#[test]
fn build_bare_names_sorted_and_deduped_issue_apply() {
    let map: DashMap<String, Vec<Location>> = DashMap::new();
    map.insert("Zebra".to_string(), vec![]);
    map.insert("Alpha".to_string(), vec![]);
    map.insert("Zebra".to_string(), vec![]); // duplicate key — DashMap overwrites

    let names = build_bare_names(&map);

    // Sorted
    assert!(
        names.windows(2).all(|w| w[0] <= w[1]),
        "names must be sorted; got: {names:?}",
    );
    // No duplicates
    let unique_count = {
        let mut v = names.clone();
        v.dedup();
        v.len()
    };
    assert_eq!(names.len(), unique_count, "names must be deduplicated; got: {names:?}");

    // Contains both symbols
    assert!(names.contains(&"Alpha".to_string()), "Alpha missing: {names:?}");
    assert!(names.contains(&"Zebra".to_string()), "Zebra missing: {names:?}");
}

/// `build_bare_names` on an empty map must return an empty vec.
#[test]
fn build_bare_names_empty_map_returns_empty_issue_apply() {
    let map: DashMap<String, Vec<Location>> = DashMap::new();
    let names = build_bare_names(&map);
    assert!(names.is_empty(), "expected empty vec for empty map; got: {names:?}");
}

// ─────────────────────────────────────────────────────────────────────────────
//  New tests: `Indexer::index_content`
// ─────────────────────────────────────────────────────────────────────────────

/// `index_content` must return `None` (and NOT increment `parse_count`) when the
/// content hash is identical to the previously indexed content.
#[test]
fn index_content_returns_none_on_unchanged_content_issue_apply() {
    let u = uri("/Foo.kt");
    let idx = Indexer::new();
    let src = "class Foo";

    // First call: parsed
    let result1 = idx.index_content(&u, src);
    assert!(result1.is_some(), "first index_content must return Some");
    let count_after_first = idx.parse_count.load(Ordering::Relaxed);

    // Second call: identical content → should skip
    let result2 = idx.index_content(&u, src);
    assert!(result2.is_none(), "second index_content with same content must return None");
    assert_eq!(
        idx.parse_count.load(Ordering::Relaxed),
        count_after_first,
        "parse_count must NOT increase when content is unchanged",
    );
}

/// `index_content` must return `Some` and increment `parse_count` when the content
/// has changed since the last index.
#[test]
fn index_content_returns_some_and_increments_parse_count_on_change_issue_apply() {
    let u = uri("/Foo.kt");
    let idx = Indexer::new();

    idx.index_content(&u, "class Foo");
    let count_after_first = idx.parse_count.load(Ordering::Relaxed);

    let result = idx.index_content(&u, "class FooUpdated");
    assert!(result.is_some(), "changed content must yield Some(data)");
    assert_eq!(
        idx.parse_count.load(Ordering::Relaxed),
        count_after_first + 1,
        "parse_count must increment by 1 on content change",
    );
}

/// `index_content` must remove the URI's entry from `completion_cache` so that
/// stale completions are not served after a file changes.
#[test]
fn index_content_clears_completion_cache_for_uri_issue_apply() {
    let u = uri("/Foo.kt");
    let idx = Indexer::new();

    // Seed the cache with a fake entry for this URI.
    idx.completion_cache.insert(u.to_string(), Arc::new(vec![]));
    assert!(
        idx.completion_cache.contains_key(u.as_str()),
        "pre-condition: completion_cache should have the entry",
    );

    // Indexing new content must evict the cache entry.
    idx.index_content(&u, "class Foo");

    assert!(
        !idx.completion_cache.contains_key(u.as_str()),
        "completion_cache entry must be removed after index_content",
    );
}

// ─────────────────────────────────────────────────────────────────────────────
//  New tests: `Indexer::rebuild_bare_name_cache`
// ─────────────────────────────────────────────────────────────────────────────

/// After indexing a file, `bare_name_cache` must contain the symbol names from
/// that file.
#[test]
fn rebuild_bare_name_cache_contains_symbol_names_issue_apply() {
    let u = uri("/MyClass.kt");
    let idx = Indexer::new();
    idx.index_content(&u, "class MyClass\nfun helperFn() {}");

    let cache = idx.bare_name_cache.read().unwrap();
    assert!(
        cache.contains(&"MyClass".to_string()),
        "bare_name_cache must contain MyClass; cache: {cache:?}",
    );
    assert!(
        cache.contains(&"helperFn".to_string()),
        "bare_name_cache must contain helperFn; cache: {cache:?}",
    );
}

/// `rebuild_bare_name_cache` is idempotent: calling it twice produces the same result.
#[test]
fn rebuild_bare_name_cache_is_idempotent_issue_apply() {
    let (_, idx) = indexed("/X.kt", "class X\nfun x() {}");

    idx.rebuild_bare_name_cache();
    let first: Vec<String> = idx.bare_name_cache.read().unwrap().clone();

    idx.rebuild_bare_name_cache();
    let second: Vec<String> = idx.bare_name_cache.read().unwrap().clone();

    assert_eq!(first, second, "rebuild_bare_name_cache must be idempotent");
}
