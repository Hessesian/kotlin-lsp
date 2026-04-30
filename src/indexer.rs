use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};

use dashmap::{DashMap, DashSet};
use tower_lsp::lsp_types::*;

use crate::types::{FileData, CursorPos};

// Re-export rg-module items that existing callers reach via `crate::indexer::`.
pub(crate) use crate::rg::IgnoreMatcher;
pub use crate::rg::SOURCE_EXTENSIONS;

mod doc;

mod infer;
// Re-export pure helpers from submodules so existing callers within this file
// and the inline test module (`use super::*`) continue to resolve them by name.
#[allow(unused_imports)]
pub(crate) use self::infer::{
    // sig.rs
    collect_signature,
    find_fun_signature_full,
    find_fun_signature_with_receiver,
    collect_all_fun_params_texts,
    nth_fun_param_type_str,
    last_fun_param_type_str,
    strip_trailing_call_args,
    // args.rs
    find_as_call_arg_type,
    extract_first_arg,
    extract_named_arg_name,
    find_named_param_type_in_sig,
    lambda_param_position_on_line,
    has_named_params_not_it,
    // it_this.rs
    find_it_element_type,
    find_it_element_type_in_lines,
    find_this_element_type_in_lines,
    find_named_lambda_param_type_in_lines,
    find_named_lambda_param_type,
    is_lambda_param,
    lambda_receiver_type_from_context,
    line_has_lambda_param,
    lambda_brace_pos_for_param,
    find_last_dot_at_depth_zero,
};

mod cache;
pub(crate) use self::cache::workspace_cache_path;

mod discover;

mod scan;
pub const MAX_FILES_UNLIMITED: usize = usize::MAX;

mod apply;
#[allow(unused_imports)]
pub(crate) use self::apply::{file_contributions, stale_keys_for, build_bare_names};

mod lookup;

mod node_ext;
pub(crate) use node_ext::NodeExt;

mod scope;
pub(crate) use scope::is_id_char;
pub(crate) use scope::last_ident_in;
pub(crate) use scope::find_enclosing_call_name;

pub(crate) mod live_tree;
pub(crate) use live_tree::LiveDoc;
mod live_tree_impl;

// Re-export cache/scan items needed by the inline test module below.
#[cfg(test)]
use self::cache::{FileCacheEntry, cache_entry_to_file_result};
#[cfg(test)]
#[allow(unused_imports)]
use crate::types::{IndexStats, FileIndexResult};
#[cfg(test)]
use crate::rg::regex_escape;
#[cfg(test)]
use std::path::Path;

#[cfg(test)]
pub(crate) mod test_helpers;

// ─── Pure helper types ────────────────────────────────────────────────────────

/// Everything a single file *adds* to the index. Pure value — no DashMaps.
pub(crate) struct FileContributions {
    pub definitions:    HashMap<String, Vec<Location>>,
    /// Both `pkg.Sym` and `pkg.FileStem.Sym` keys.
    pub qualified:      HashMap<String, Location>,
    pub packages:       HashMap<String, Vec<String>>,
    pub subtypes:       HashMap<String, Vec<Location>>,
    pub file_data:      (String, Arc<crate::types::FileData>),
    pub content_hash:   (String, u64),
}

/// Keys to remove from the index when a file is replaced.
pub(crate) struct StaleKeys {
    pub definition_names: Vec<String>,
    /// Both aliases: `pkg.Sym` AND `pkg.FileStem.Sym`.
    pub qualified_keys:   Vec<String>,
    pub package:          Option<String>,
}

pub struct Indexer {
    /// URI string → parsed file data.
    pub(crate) files:       DashMap<String, Arc<FileData>>,
    /// Short name → definition locations  (fast first-pass lookup).
    pub(crate) definitions: DashMap<String, Vec<Location>>,
    /// Fully-qualified name → location   (e.g. "com.example.Foo" → …).
    pub(crate) qualified:   DashMap<String, Location>,
    /// Package name → vec of URI strings (for same-package resolution).
    pub(crate) packages:    DashMap<String, Vec<String>>,
    /// Absolute path to the workspace root, set once on first `index_workspace`.
    pub(crate) workspace_root: RwLock<Option<PathBuf>>,
    /// URI string → xxHash of last indexed content (skip identical re-parses).
    content_hashes: DashMap<String, u64>,
    /// Semaphore capping concurrent parse workers.
    parse_sem: Arc<tokio::sync::Semaphore>,
    /// Times tree-sitter actually ran (used in tests).
    pub parse_count: AtomicU64,
    /// URI string → pre-built completion items for that file.
    /// Populated lazily on first dot-completion hit; cleared on re-index.
    pub(crate) completion_cache: DashMap<String, Arc<Vec<CompletionItem>>>,
    /// URI string → lines of the CURRENT document content.
    /// Updated synchronously on every did_change, bypassing the 120ms debounce.
    /// Used by `completions()` so dot-detection always sees the latest text.
    /// Arc-wrapped so `.clone()` is a cheap refcount bump, not a full Vec copy.
    pub(crate) live_lines: DashMap<String, Arc<Vec<String>>>,
    /// Reverse supertype index: supertype name → locations of implementing/extending classes.
    /// Populated during `index_content()` for fast `goToImplementation` lookups.
    pub(crate) subtypes: DashMap<String, Vec<Location>>,
    /// Cached sorted list of all project class/symbol names for bare-word completion.
    /// Rebuilt after each file index; avoids iterating `definitions` on every keystroke.
    pub(crate) bare_name_cache: std::sync::RwLock<Vec<String>>,
    /// Last completion result: (uri, context_key, items).
    /// `context_key` = line text up to (but not including) the current word.
    /// When the key matches, the cached items are returned without recomputation —
    /// covers the common "typing more characters in the same word/after same dot" case.
    pub(crate) last_completion: std::sync::Mutex<Option<(String, String, Vec<CompletionItem>)>>,
    /// Monotonically increasing generation counter.  Incremented on every root
    /// switch so that background tasks spawned for an older root can detect
    /// staleness and bail out early.
    pub(crate) root_generation: AtomicU64,
    /// Guard to prevent concurrent background indexing runs on same Indexer.
    pub(crate) indexing_in_progress: std::sync::atomic::AtomicBool,
    /// Set when a reindex request arrives while a scan is already running.
    /// The pending scan is started by the active scan's caller once its full
    /// workflow (impl + apply + source_paths + save_cache) completes.
    pub(crate) pending_reindex: std::sync::atomic::AtomicBool,
    /// Root to use for the pending reindex. `None` means use the current workspace root.
    /// Written under a mutex so the *last* concurrent caller wins (RA OpQueue semantics).
    pub(crate) pending_reindex_root: RwLock<Option<PathBuf>>,
    /// Max files cap for the pending reindex. Preserves the intent of the last caller:
    /// a full (unbounded) reindex queued during a bounded scan keeps its unlimited cap.
    pub(crate) pending_reindex_max: std::sync::atomic::AtomicUsize,
    /// Number of parse tasks completed in current indexing run (for progress tracking).
    pub(crate) parse_tasks_completed: std::sync::atomic::AtomicUsize,
    /// Total number of parse tasks spawned in current indexing run.
    pub(crate) parse_tasks_total: std::sync::atomic::AtomicUsize,
    /// Paths currently scheduled or in-flight: canonical path -> generation when scheduled.
    /// Prevents duplicate scheduling of identical parse work for same generation.
    scheduled_paths: DashMap<String, u64>,
    /// Set when workspace was explicitly configured (env var, config file, or changeRoot command).
    /// When true, `did_open` auto-detection will NOT override the workspace.
    pub(crate) workspace_pinned: std::sync::atomic::AtomicBool,
    /// Set to true after a non-truncated workspace scan; false after a truncated one.
    /// Drives `complete_scan` on the on-disk cache so warm-manifest mode is only
    /// used when the cache is known to be a full workspace snapshot.
    pub(crate) last_scan_complete: std::sync::atomic::AtomicBool,
    /// User-configured ignore patterns from LSP `initializationOptions`.
    /// Applied during file discovery to exclude matching paths.
    pub(crate) ignore_matcher: RwLock<Option<Arc<IgnoreMatcher>>>,
    /// Raw source paths from `initializationOptions.indexingOptions.sourcePaths`.
    /// Stored unresolved; resolved against workspace root at indexing time.
    pub(crate) source_paths_raw: RwLock<Vec<String>>,
    /// URIs of files indexed from `sourcePaths` that lie outside the workspace root.
    /// These are treated as library sources: available for hover/definition/autocomplete
    /// but excluded from findReferences and rename.
    pub(crate) library_uris: DashSet<String>,
    /// Simple name → sorted vec of importable FQNs.
    /// e.g. "Composable" → ["androidx.compose.runtime.Composable"]
    /// Built from top-level symbols only (no synthetic file-stem keys).
    /// Rebuilt in rebuild_bare_name_cache(); used by complete_bare for auto-import edits.
    pub(crate) importable_fqns: std::sync::RwLock<std::collections::HashMap<String, Vec<String>>>,
    /// URI string → live parse tree for currently-open editor files.
    /// Updated synchronously on every `did_open` / `did_change`; removed on `did_close`.
    /// Not cleared on `reset_index_state` — open-file trees survive workspace reindex.
    pub(crate) live_trees: DashMap<String, Arc<LiveDoc>>,
}

impl crate::indexer::infer::InferDeps for Indexer {
    fn find_fun_params_text(&self, fn_name: &str, uri: &Url) -> Option<String> {
        find_fun_signature_full(fn_name, self, uri)
    }
    fn find_var_type(&self, var_name: &str, uri: &Url) -> Option<String> {
        crate::resolver::infer_variable_type_raw(self, var_name, uri)
    }
}

impl Indexer {
    pub fn parse_sem(&self) -> Arc<tokio::sync::Semaphore> {
        Arc::clone(&self.parse_sem)
    }

    pub fn new() -> Self {
        Self {
            files:          DashMap::new(),
            definitions:    DashMap::new(),
            qualified:      DashMap::new(),
            packages:       DashMap::new(),
            workspace_root: RwLock::new(None),
            content_hashes: DashMap::new(),
            // Allow configurable concurrent parse workers. Default to number of CPU cores.
            // Use env KOTLIN_LSP_PARSE_WORKERS to override.
            parse_sem: {
                // Default to half of available CPUs to avoid saturating system.
                let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
                let default = (cpus / 2).max(1);
                let configured = std::env::var("KOTLIN_LSP_PARSE_WORKERS").ok()
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(default);
                Arc::new(tokio::sync::Semaphore::new(configured))
            },
            parse_count:    AtomicU64::new(0),
            completion_cache: DashMap::new(),
            live_lines:     DashMap::new(),
            subtypes:       DashMap::new(),
            bare_name_cache: std::sync::RwLock::new(Vec::new()),
            last_completion: std::sync::Mutex::new(None),
            root_generation: AtomicU64::new(0),
            indexing_in_progress: std::sync::atomic::AtomicBool::new(false),
            pending_reindex: std::sync::atomic::AtomicBool::new(false),
            pending_reindex_root: RwLock::new(None),
            pending_reindex_max: std::sync::atomic::AtomicUsize::new(0),
            parse_tasks_completed: std::sync::atomic::AtomicUsize::new(0),
            parse_tasks_total: std::sync::atomic::AtomicUsize::new(0),
            scheduled_paths: DashMap::new(),
            workspace_pinned: std::sync::atomic::AtomicBool::new(false),
            last_scan_complete: std::sync::atomic::AtomicBool::new(false),
            ignore_matcher: RwLock::new(None),
            source_paths_raw: RwLock::new(Vec::new()),
            library_uris: DashSet::new(),
            importable_fqns: std::sync::RwLock::new(std::collections::HashMap::new()),
            live_trees: DashMap::new(),
        }
    }

    /// Clear all index maps. Called before a full workspace re-index and on root switch.
    /// Clears everything: files, definitions, qualified, packages, subtypes, content_hashes,
    /// completion_cache, bare_name_cache. Does NOT touch orchestration fields
    /// (workspace_root, parse_sem, generation counters, live_lines).
    pub fn reset_index_state(&self) {
        self.files.clear();
        self.definitions.clear();
        self.qualified.clear();
        self.packages.clear();
        self.subtypes.clear();
        self.content_hashes.clear();
        self.completion_cache.clear();
        self.library_uris.clear();
        if let Ok(mut cache) = self.bare_name_cache.write() {
            cache.clear();
        }
        if let Ok(mut map) = self.importable_fqns.write() {
            map.clear();
        }
        if let Ok(mut last) = self.last_completion.lock() {
            *last = None;
        }
    }

    /// Update the live-lines cache for `uri` without any debounce.
    /// Called from `did_change` before the debounced re-index so that
    /// `completions()` always sees the current document text.
    pub fn set_live_lines(&self, uri: &Url, content: &str) {
        let lines: Arc<Vec<String>> = Arc::new(content.lines().map(String::from).collect());
        self.live_lines.insert(uri.to_string(), lines);
    }

    /// Returns lines for `uri` from the in-memory caches only (no disk I/O).
    /// Prefers live (unsaved) lines; falls back to the last indexed snapshot.
    /// Use this on hot paths (completion, hover, signature help).
    /// For cold-start / rg-based paths that may need disk, use `scope::lines_for`.
    pub(crate) fn mem_lines_for(&self, uri: &str) -> Option<Arc<Vec<String>>> {
        if let Some(live) = self.live_lines.get(uri) {
            return Some(live.clone());
        }
        self.files.get(uri).map(|f| f.lines.clone())
    }

    // ─── completion helpers (methods on Indexer) ─────────────────────────────

    /// Ensures the file at `uri` is indexed, loading from disk if needed.
    /// Called on the completion hot-path before the debounced re-index finishes.
    fn ensure_indexed(&self, uri: &Url) {
        if !self.files.contains_key(uri.as_str()) {
            if let Ok(path) = uri.to_file_path() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    self.index_content(uri, &content);
                }
            }
        }
    }

    /// Returns the text of line `line_idx` for `uri`, preferring live lines.
    fn line_for_position(&self, uri: &Url, line_idx: u32) -> Option<String> {
        let idx = line_idx as usize;
        if let Some(ll) = self.live_lines.get(uri.as_str()) {
            return ll.get(idx).cloned();
        }
        self.files.get(uri.as_str())?.lines.get(idx).cloned()
    }

    /// Resolves the element type for an `it`/`this`/named-param dot-receiver.
    fn resolve_lambda_recv_type(
        &self,
        recv: &str,
        before: &str,
        cursor_line: usize,
        cursor_col: usize,
        uri: &Url,
    ) -> Option<String> {
        if recv == "it" || recv == "this" {
            // Try single-line first (fast path: `obj.run { this. }` on same line).
            let t = find_it_element_type(before, self, uri);
            if t.is_some() && recv == "it" {
                return t;
            }
            // Multi-line fallback: lambda opened on a previous line.
            let lines = self.mem_lines_for(uri.as_str());
            let pos = CursorPos { line: cursor_line, utf16_col: cursor_col };
            let ml = lines.and_then(|ls| {
                if recv == "this" {
                    find_this_element_type_in_lines(&ls, pos, self, uri)
                } else {
                    find_it_element_type_in_lines(&ls, pos, self, uri)
                }
            });
            if ml.is_some() {
                return ml;
            }
            if recv == "this" {
                return self.enclosing_class_at(uri, cursor_line as u32);
            }
            None
        } else {
            find_named_lambda_param_type(before, recv, self, uri, cursor_line)
        }
    }

    /// Appends lambda-parameter completions for bare-word (non-dot) completion.
    fn add_lambda_param_completions(
        &self,
        items: &mut Vec<CompletionItem>,
        uri: &Url,
        line_idx: usize,
        prefix: &str,
    ) {
        let prefix_lower = prefix.to_lowercase();
        for param in self.lambda_params_at(uri, line_idx) {
            if param.to_lowercase().starts_with(&prefix_lower) {
                if !items.iter().any(|i| i.label == param) {
                    items.push(CompletionItem {
                        label:     param.clone(),
                        kind:      Some(CompletionItemKind::VARIABLE),
                        sort_text: Some(format!("005:{param}")),
                        ..Default::default()
                    });
                }
            }
        }
    }

    ///
    /// Uses `live_lines` (updated synchronously on every keystroke) for the
    /// current file's line text, falling back to indexed lines or disk.
    pub fn completions(&self, uri: &Url, position: Position, snippets: bool) -> (Vec<CompletionItem>, bool) {
        self.ensure_indexed(uri);

        let Some(line) = self.line_for_position(uri, position.line) else {
            return (vec![], false);
        };
        let before = before_cursor(&line, position.character);
        let (prefix, before_prefix) = split_prefix(&before);

        // ── completion result cache ──────────────────────────────────────────
        // Dot-completion: all members of a type are returned regardless of what
        // the user has typed after the dot — the client fuzzy-filters them.
        // Cache key omits prefix so repeated keystrokes after "." are fast.
        //
        // Bare-word completion: results are scored/capped by prefix. Including
        // prefix in the key forces a fresh, precise query for each keystroke and
        // avoids serving a stale "C"-query cap when the user types "ChildDash".
        let cache_key = if before_prefix.ends_with('.') {
            format!("{}|{}|{}", uri.as_str(), before_prefix, position.line)
        } else {
            format!("{}|{}|{}|{}", uri.as_str(), before_prefix, position.line, prefix)
        };
        if let Ok(guard) = self.last_completion.lock() {
            if let Some((ref k, _, ref cached)) = *guard {
                if k == &cache_key {
                    return (cached.clone(), false);
                }
            }
        }

        let dot_recv = dot_receiver(&before_prefix);

        // `this.` / `it.` / named-param dot-completion.
        // `this` can mean: (a) scope-function receiver, (b) enclosing class.
        // `it` means: implicit lambda param.
        // Named lambda params are detected via `is_lambda_param`.
        if let Some(ref recv) = dot_recv {
            if recv == "it" || recv == "this"
                || is_lambda_param(recv, &before, self, uri, position.line as usize)
            {
                let cursor_line = position.line as usize;
                let cursor_col  = before.chars().count();
                let elem_type = self.resolve_lambda_recv_type(recv, &before, cursor_line, cursor_col, uri);
                if let Some(elem_type) = elem_type {
                    let (items, _) = crate::resolver::complete_symbol(self, &prefix, Some(&elem_type), uri, snippets);
                    if items.is_empty() {
                        // Type name known (e.g. generic param `T`, `StateType`) but not
                        // indexed — show a single hint item so the user sees the inferred type.
                        return (vec![CompletionItem {
                            label: format!("{recv}: {elem_type}"),
                            kind: Some(CompletionItemKind::TYPE_PARAMETER),
                            detail: Some(format!("Inferred type: {elem_type}")),
                            sort_text: Some("~hint".into()),
                            ..Default::default()
                        }], false);
                    }
                    return (items, false);
                }
                // Recognised lambda param but type unresolvable — return empty.
                return (vec![], false);
            }
        }

        let annotation_only = dot_recv.is_none()
            && crate::resolver::is_annotation_context(&before, &prefix);
        let (mut items, hit_cap) = crate::resolver::complete_symbol_with_context(
            self, &prefix, dot_recv.as_deref(), uri, snippets, annotation_only,
        );

        // Add scope-aware lambda parameter names (bare-word completion only).
        if dot_recv.is_none() {
            self.add_lambda_param_completions(&mut items, uri, position.line as usize, &prefix);
        }

        // Store in last_completion cache.
        if let Ok(mut guard) = self.last_completion.lock() {
            *guard = Some((cache_key, prefix.clone(), items.clone()));
        }

        (items, hit_cap)
    }
}

// ─── completion helpers (free functions) ─────────────────────────────────────

/// Returns a copy of `line` up to the UTF-16 column `utf16_col`.
fn before_cursor(line: &str, utf16_col: u32) -> String {
    let target = utf16_col as usize;
    let mut utf16 = 0usize;
    let mut byte_end = line.len();
    for (bi, ch) in line.char_indices() {
        if utf16 >= target { byte_end = bi; break; }
        utf16 += ch.len_utf16();
    }
    line[..byte_end].to_owned()
}

/// Splits `before` into the trailing identifier fragment (`prefix`) and
/// everything that precedes it (`before_prefix`).
fn split_prefix(before: &str) -> (String, String) {
    let prefix = last_ident_in(before).to_owned();
    let before_prefix = before[..before.len() - prefix.len()].to_owned();
    (prefix, before_prefix)
}

/// Returns the expression immediately before a trailing dot in `before_prefix`,
/// or `None` if `before_prefix` does not end with a dot.
///
/// Handles one level of qualification: `Outer.Inner.` → `"Outer.Inner"`.
fn dot_receiver(before_prefix: &str) -> Option<String> {
    let before_dot = before_prefix.strip_suffix('.')?;
    let inner = last_ident_in(before_dot);
    if inner.is_empty() { return None; }
    let remaining = &before_dot[..before_dot.len() - inner.len()];
    if remaining.ends_with('.')
        && inner.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
    {
        let outer = last_ident_in(&remaining[..remaining.len() - 1]);
        if !outer.is_empty()
            && outer.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
        {
            return Some(format!("{outer}.{inner}"));
        }
    }
    Some(inner.to_owned())
}

// ─── rg cross-file fallback ──────────────────────────────────────────────────

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(path: &str) -> Url {
        Url::parse(&format!("file:///test{path}")).unwrap()
    }

    fn indexed(path: &str, src: &str) -> (Url, Indexer) {
        let u = uri(path);
        let idx = Indexer::new();
        idx.index_content(&u, src);
        (u, idx)
    }

    #[test]
    fn symbol_found_after_indexing() {
        let (u, idx) = indexed("/t.kt", "class MyViewModel");
        assert!(!idx.find_definition("MyViewModel", &u).is_empty());
    }

    #[test]
    fn data_class_single_definition() {
        let (u, idx) = indexed("/t.kt", "data class Foo(val x: Int)");
        assert_eq!(idx.find_definition("Foo", &u).len(), 1);
    }

    #[test]
    fn stale_removed_on_reindex() {
        let u = uri("/t.kt");
        let idx = Indexer::new();
        idx.index_content(&u, "class OldName");
        idx.index_content(&u, "class NewName");
        assert!(idx.find_definition("OldName", &u).is_empty(), "stale entry not removed");
        assert!(!idx.find_definition("NewName", &u).is_empty());
    }

    #[test]
    fn qualified_index_populated() {
        let (_, idx) = indexed("/t.kt", "package com.example\nclass Foo");
        assert!(idx.qualified.contains_key("com.example.Foo"));
    }

    #[test]
    fn qualified_removed_on_reindex() {
        let u = uri("/t.kt");
        let idx = Indexer::new();
        idx.index_content(&u, "package com.example\nclass OldName");
        idx.index_content(&u, "package com.example\nclass NewName");
        assert!(!idx.qualified.contains_key("com.example.OldName"), "stale qualified entry");
        assert!(idx.qualified.contains_key("com.example.NewName"));
    }

    #[test]
    fn packages_map_populated() {
        let (u, idx) = indexed("/t.kt", "package com.example\nclass Foo");
        let uris = idx.packages.get("com.example").unwrap();
        assert!(uris.contains(&u.to_string()));
    }

    // ── parse_count: verify deduplication ───────────────────────────────────

    #[test]
    fn index_same_content_parses_only_once() {
        let u   = uri("/Dedup.kt");
        let idx = Indexer::new();
        let src = "package com.test\nclass Dedup";

        // Call index_content 50 times with identical content.
        for _ in 0..50 {
            idx.index_content(&u, src);
        }
        assert_eq!(
            idx.parse_count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "identical content should only trigger one tree-sitter parse"
        );
    }

    #[test]
    fn index_changed_content_reparses() {
        let u   = uri("/Changed.kt");
        let idx = Indexer::new();

        idx.index_content(&u, "class A");
        idx.index_content(&u, "class A"); // same — skipped
        idx.index_content(&u, "class B"); // different — must reparse
        idx.index_content(&u, "class B"); // same again — skipped

        assert_eq!(
            idx.parse_count.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "should parse exactly twice: once for 'class A', once for 'class B'"
        );
    }

    // ── completions ──────────────────────────────────────────────────────────

    #[test]
    fn dot_completion_triggers_on_dot() {
        let vm_uri   = uri("/ViewModel.kt");
        let repo_uri = uri("/Repository.kt");
        let idx = Indexer::new();
        idx.index_content(&repo_uri,
            "package com.pkg\nclass Repository {\n  fun findById(id: Int) {}\n  fun save(obj: Any) {}\n}");
        idx.index_content(&vm_uri,
            "package com.pkg\nclass ViewModel(\n  private val repo: Repository\n) {\n  fun load() { return repo. }\n}");

        // Position after the dot on line 4
        let line = "  fun load() { return repo. }";
        let dot_col = (line.find("repo.").unwrap() + "repo.".len()) as u32;
        let (items, _) = idx.completions(&vm_uri, Position::new(4, dot_col), true);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"findById"), "findById missing; got: {labels:?}");
        assert!(labels.contains(&"save"),     "save missing; got: {labels:?}");
    }

    #[test]
    fn dot_completion_with_prefix() {
        let vm_uri   = uri("/ViewModel2.kt");
        let repo_uri = uri("/Repo2.kt");
        let idx = Indexer::new();
        idx.index_content(&repo_uri,
            "package com.pkg2\nclass Repo2 {\n  fun findAll() {}\n  fun save() {}\n}");
        idx.index_content(&vm_uri,
            "package com.pkg2\nclass ViewModel2(\n  private val repo: Repo2\n) {\n  fun run() { repo.fin }\n}");

        let line = "  fun run() { repo.fin }";
        let col = (line.find("repo.fin").unwrap() + "repo.fin".len()) as u32;
        let (items, _) = idx.completions(&vm_uri, Position::new(4, col), true);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"findAll"), "findAll missing; got: {labels:?}");
    }

    #[test]
    fn dot_completion_qualified_nested_type() {
        // Typing `DPSCoordinator.Kind.` — receiver is "DPSCoordinator.Kind".
        // Should show enum cases (victory, defeat), NOT members of DPSCoordinator.
        let coordinator_uri = uri("/DPSCoordinator.swift");
        let vm_uri = uri("/DPSChangeVictoryViewModel.swift");
        let idx = Indexer::new();
        idx.index_content(&coordinator_uri,
            "class DPSCoordinator {\n    enum Kind {\n        case victory\n        case defeat\n    }\n    func deposit() {}\n    var strategy: String = \"\"\n}");
        idx.index_content(&vm_uri,
            "class DPSChangeVictoryViewModel {\n    let coordinator: DPSCoordinator\n    func update() { let k = DPSCoordinator.Kind. }\n}");

        let line = "    func update() { let k = DPSCoordinator.Kind. }";
        let col = (line.find("DPSCoordinator.Kind.").unwrap() + "DPSCoordinator.Kind.".len()) as u32;
        let (items, _) = idx.completions(&vm_uri, Position::new(2, col), false);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"victory"), "victory case missing; got: {labels:?}");
        assert!(labels.contains(&"defeat"),  "defeat case missing; got: {labels:?}");
        assert!(!labels.contains(&"deposit"),  "deposit (DPSCoordinator method) must NOT appear; got: {labels:?}");
        assert!(!labels.contains(&"strategy"), "strategy (DPSCoordinator prop) must NOT appear; got: {labels:?}");
    }

    #[test]
    fn generic_it_type_shows_hint_completion() {
        // `map: ((T) -> ProductDetailSheetModel)` — `it` resolves to generic `T`.
        // Completion of `it.` should return a hint item showing the inferred type name.
        let src = "package com.example
fun lazyLoad(map: ((T) -> Model)) {}
class Model
";
        let (u, idx) = indexed("/t.kt", src);
        idx.set_live_lines(&u, src);
        // Simulate: cursor on `it.` inside `lazyLoad { it. }`
        let src_with_call = "package com.example
fun lazyLoad(map: ((T) -> Model)) {}
class Model
fun use() { lazyLoad { it. } }
";
        let (u2, idx2) = indexed("/u.kt", src_with_call);
        idx2.set_live_lines(&u2, src_with_call);
        let line = "fun use() { lazyLoad { it. } }";
        let col = (line.find("it.").unwrap() + "it.".len()) as u32;
        let (items, _) = idx2.completions(&u2, Position::new(3, col), false);
        // Must include a hint item labelled `it: T`
        let hint = items.iter().find(|i| i.label.contains("it:") && i.label.contains('T'));
        assert!(hint.is_some(), "expected `it: T` hint item; got: {:?}", items.iter().map(|i| &i.label).collect::<Vec<_>>());
        let _ = (u, idx); // suppress unused warning
    }

    #[test]
    fn nested_class_qualified_key() {
        // AccountContract.kt defines a sealed class State nested inside it.
        // The qualified index should store BOTH:
        //   "com.example.State"                    (primary)
        //   "com.example.AccountContract.State"    (nested — matches import path)
        let uri = uri("/AccountContract.kt");
        let idx = Indexer::new();
        idx.index_content(&uri,
            "package com.example\nclass AccountContract {\n  sealed class State\n  sealed class Event\n}");

        assert!(idx.qualified.contains_key("com.example.State"),
            "primary qualified key missing");
        assert!(idx.qualified.contains_key("com.example.AccountContract.State"),
            "nested qualified key missing");
        assert!(idx.qualified.contains_key("com.example.AccountContract.Event"),
            "nested Event qualified key missing");
    }

    // ── enclosing_class_at ───────────────────────────────────────────────────

    #[test]
    fn super_resolution_chain() {
        // Full super-resolution chain: two files with same package,
        // enclosing class found from multi-line constructor, supertypes extracted.
        let bar_src = "package com.example\nopen class Bar {\n  open fun doIt() {}\n}\n";
        let foo_src = "\
package com.example
class Foo @Inject constructor(
  private val a: A,
  private val b: B,
) : Bar() {
  override fun doIt() {
    super.doIt()
  }
}";
        let (_, idx) = indexed("/Bar.kt", bar_src);
        let foo_uri = uri("/Foo.kt");
        idx.index_content(&foo_uri, foo_src);

        // 1. enclosing_class_at at line 6 ("    super.doIt()") → "Foo"
        let class_name = idx.enclosing_class_at(&foo_uri, 6);
        assert_eq!(class_name.as_deref(), Some("Foo"), "enclosing class");

        // 2. Find Foo's definition and extract supertypes
        let locs = idx.definitions.get("Foo").map(|v| v.clone()).unwrap_or_default();
        assert!(!locs.is_empty(), "Foo must be in definitions");
        let file = idx.files.get(locs[0].uri.as_str()).unwrap();
        let start_line = locs[0].range.start.line;
        let supers: Vec<String> = file.supers.iter()
            .filter(|(l, _)| *l == start_line)
            .map(|(_, n)| n.clone())
            .collect();
        assert!(supers.contains(&"Bar".to_string()), "supers={supers:?}");

        // 3. find_definition_qualified finds Bar (same package)
        let bar_locs = idx.find_definition_qualified("Bar", None, &foo_uri);
        assert!(!bar_locs.is_empty(), "Bar must resolve via same-package");
    }

    #[test]
    fn regex_escape_dots_and_special() {
        assert_eq!(regex_escape("Foo.Bar"), "Foo\\.Bar".to_string());
        assert_eq!(regex_escape("Loading"), "Loading".to_string());
        assert_eq!(regex_escape("get()"), "get\\(\\)".to_string());
    }

    // ── collect_signature ────────────────────────────────────────────────────

    #[test]
    fn signature_single_line_with_brace() {
        let lines = vec!["sealed interface NewsFeedUiState {".to_owned()];
        // The `{` should be stripped; result is just the declaration.
        assert_eq!(
            collect_signature(&lines, 0),
            "sealed interface NewsFeedUiState"
        );
    }

    #[test]
    fn signature_multiline_constructor() {
        let lines = vec![
            "class DetailViewModel @Inject constructor(".to_owned(),
            "  private val mapper: DetailMapper,".to_owned(),
            "  private val loadUseCase: LoadDataUseCase,".to_owned(),
            ") : MviViewModel<Event, State, Effect>() {".to_owned(),
        ];
        let sig = collect_signature(&lines, 0);
        assert!(sig.contains("DetailViewModel"), "should contain class name");
        assert!(sig.contains("MviViewModel"),    "should contain superclass");
        assert!(!sig.contains('{'),              "should not include body brace");
    }

    #[test]
    fn signature_fun_single_line() {
        let lines = vec!["fun doSomething(x: Int): Boolean".to_owned()];
        assert_eq!(collect_signature(&lines, 0), "fun doSomething(x: Int): Boolean");
    }

    #[test]
    fn signature_stops_at_open_brace_on_own_line() {
        // `{` on its own line — body opener, must not appear in output.
        let lines = vec![
            "class Foo(val x: Int)".to_owned(),
            "    : Bar() {".to_owned(),
        ];
        let sig = collect_signature(&lines, 0);
        assert!(!sig.contains('{'), "brace should be stripped");
        assert!(sig.contains("Foo"), "class name must be present");
    }

    #[test]
    fn hover_it_type_detection() {
        let src = "val items: List<Product> = emptyList()\nitems.forEach { it.name }";
        let (u, idx) = indexed("/t.kt", src);
        // Cursor on `it` at line 1: "items.forEach { it.name }"
        // `it` starts at column 16
        let col = "items.forEach { ".len() as u32;
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(1, col));
        assert_eq!(result.as_deref(), Some("Product"), "hover should detect it: Product");
    }

    #[test]
    fn hover_it_type_multiline() {
        // `{` is on a different line than `it`
        let src = "val items: List<User> = emptyList()\nitems.forEach {\n    val x = it.id\n}";
        let (u, idx) = indexed("/t.kt", src);
        // Cursor on `it` at line 2, col 13
        let col = "    val x = ".len() as u32;
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(2, col));
        assert_eq!(result.as_deref(), Some("User"), "hover multiline it: User");
    }

    #[test]
    fn hover_named_param_type_detection() {
        let src = "val items: List<Order> = emptyList()\nitems.forEach { order ->\n    order.id\n}";
        let (u, idx) = indexed("/t.kt", src);
        // Cursor on `order` at line 2 (in the body)
        let col = "    ".len() as u32;
        let result = idx.infer_lambda_param_type_at("order", &u, Position::new(2, col));
        assert_eq!(result.as_deref(), Some("Order"));
    }

    // ── trailing-lambda it type (user-defined function) ───────────────────────

    #[test]
    fn trailing_lambda_it_from_fun_def() {
        let src = concat!(
            "private fun <T : Any> loadProduct(",
            "key: ProductKey, flow: Flow<ResultState<T>>, ",
            "map: (ResultState<T>) -> StatefulModel) {\n}\n",
            "fun use() { loadProduct(k, f) { it.value } }",
        );
        let (u, idx) = indexed("/t.kt", src);
        // `before_brace` as seen by lambda_receiver_type_from_context
        let before = "loadProduct(k, f) ";
        let result = lambda_receiver_type_from_context(before, &idx, &u);
        assert_eq!(result.as_deref(), Some("ResultState"),
            "trailing lambda it should resolve to ResultState, got: {result:?}");
    }

    #[test]
    fn nested_lambda_it_type_resolved_through_outer_brace() {
        // `setState` takes a lambda whose `it` is State.
        // When `setState { it }` is nested inside an outer lambda body like
        // `collectState({ setState { it } }, ...)`, the `before_brace` seen by
        // lambda_receiver_type_from_context is `"    { setState "` — callee has
        // a leading `{` from the outer lambda.  Must still resolve to State.
        let src = "package com.example
fun setState(reducer: (State) -> State) {}
class State
";
        let (u, idx) = indexed("/t.kt", src);
        // before_brace as it arrives from the nested-lambda context
        let before = "    { setState ";
        let result = lambda_receiver_type_from_context(before, &idx, &u);
        assert_eq!(result.as_deref(), Some("State"),
            "it inside nested setState lambda should resolve to State, got: {result:?}");
    }

    // ── inline-lambda param type (Case C) ────────────────────────────────────

    #[test]
    fn inline_lambda_param_type_detection() {
        // `reloadableProduct(ProductKey.FAMILY, { isRefresh -> ... })`
        // The lambda is the 2nd arg (index 1); fun expects `(Boolean) -> Flow<T>`
        let src = concat!(
            "fun reloadableProduct(key: ProductKey, refresher: (Boolean) -> Flow<ResultState<T>>, ",
            "map: (ResultState<T>) -> StatefulModel) {}\n",
            "fun use() { reloadableProduct(ProductKey.FAMILY, { isRefresh -> null }) { it } }",
        );
        let (u, idx) = indexed("/t.kt", src);
        // before_brace = "reloadableProduct(ProductKey.FAMILY, "
        let before = "reloadableProduct(ProductKey.FAMILY, ";
        let result = lambda_receiver_type_from_context(before, &idx, &u);
        assert_eq!(result.as_deref(), Some("Boolean"),
            "inline lambda param should be Boolean, got: {result:?}");
    }

    #[test]
    fn find_last_dot_at_depth_zero_test() {
        // Dot inside args should NOT match.
        assert_eq!(find_last_dot_at_depth_zero("fn(Enum.VALUE, "), None);
        // Simple method chain.
        assert_eq!(find_last_dot_at_depth_zero("items.forEach"), Some(5));
        // Chained calls — only last dot at depth 0.
        assert_eq!(find_last_dot_at_depth_zero("a.b(x).c"), Some(6));
    }

    #[test]
    fn trailing_lambda_method_it_not_confused_by_arg_dot() {
        // `reloadableProduct(ProductKey.FAMILY) { it }` — trailing lambda,
        // but the arg `ProductKey.FAMILY` has a dot. Should still resolve via Case B.
        let src = concat!(
            "fun reloadableProduct(key: ProductKey, map: (ResultState<T>) -> StatefulModel) {}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        // After strip_trailing_call_args: "reloadableProduct"
        let before = "reloadableProduct(ProductKey.FAMILY) ";
        let result = lambda_receiver_type_from_context(before, &idx, &u);
        assert_eq!(result.as_deref(), Some("ResultState"),
            "trailing lambda with dot-in-arg should still resolve, got: {result:?}");
    }

    #[test]
    fn trailing_lambda_it_with_method_call_arg() {
        // `loadProduct(ProductKey.DEPOSIT, productsUseCases.getDepositAccountData()) { it }`
        // The second arg is a method call `x.y()` — after stripping outer `(...)` the
        // callee must be exactly "loadProduct" so Case B fires correctly.
        let src = concat!(
            "private fun <T : Any> loadProduct(\n",
            "    key: ProductKey,\n",
            "    productFlow: Flow<ResultState<T>>,\n",
            "    map: (ResultState<T>) -> StatefulModel\n",
            ") {}\n",
            "fun use() {\n",
            "    loadProduct(ProductKey.DEPOSIT, productsUseCases.getDepositAccountData()) { overviewMapper.depositAccToView(it) }\n",
            "}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        // Test via lambda_receiver_type_from_context directly.
        let before = "    loadProduct(ProductKey.DEPOSIT, productsUseCases.getDepositAccountData()) ";
        let result = lambda_receiver_type_from_context(before, &idx, &u);
        assert_eq!(result.as_deref(), Some("ResultState"),
            "loadProduct trailing lambda it type, got: {result:?}");
    }

    /// `reloadableProduct` has a `(Boolean) -> Flow<T>` param followed by a
    /// `(ResultState<T>) -> Model` trailing lambda.  The `>` in `->` must not
    /// upset the `<>` depth counter so `last_fun_param_type_str` picks `map`
    /// (the last param) instead of `refresher`.
    #[test]
    fn reloadable_product_resultstate_not_boolean() {
        let src = concat!(
            "private fun <T : Any> reloadableProduct(\n",
            "    key: ProductKey,\n",
            "    productFlow: (isRefresh: Boolean) -> Flow<ResultState<T>>,\n",
            "    map: (ResultState<T>) -> StatefulModel<SortableProducts>,\n",
            ") {}\n",
            "fun use() {\n",
            "    reloadableProduct(ProductKey.FAMILY, { isRefresh -> null }) { resultState ->\n",
            "        resultState.value\n",
            "    }\n",
            "}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        // Trailing lambda: `before_brace` after stripping the inline call args
        // should resolve `resultState` to `ResultState`, not `Boolean`.
        let before = "    reloadableProduct(ProductKey.FAMILY, { isRefresh -> null }) ";
        let result = lambda_receiver_type_from_context(before, &idx, &u);
        assert_eq!(result.as_deref(), Some("ResultState"),
            "resultState should resolve to ResultState not Boolean, got: {result:?}");
    }

    #[test]
    fn trailing_lambda_it_infer_at_cursor() {
        let src = concat!(
            "private fun <T : Any> loadProduct(\n",
            "    key: ProductKey,\n",
            "    productFlow: Flow<ResultState<T>>,\n",
            "    map: (ResultState<T>) -> StatefulModel\n",
            ") {}\n",
            "fun use() {\n",
            "    loadProduct(ProductKey.DEPOSIT, productsUseCases.getDepositAccountData()) { overviewMapper.depositAccToView(it) }\n",
            "}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        // Line 6 (0-based): the call line. Column: position of `it`.
        let call_line = "    loadProduct(ProductKey.DEPOSIT, productsUseCases.getDepositAccountData()) { overviewMapper.depositAccToView(";
        let col = call_line.len() as u32;
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(6, col));
        assert_eq!(result.as_deref(), Some("ResultState"),
            "infer_lambda_param_type_at for it in loadProduct, got: {result:?}");
    }

    // ── `this` in scope functions ─────────────────────────────────────────────

    #[test]
    fn this_in_run_resolves_to_receiver_type() {
        // `user.run { this.name }` — `this` should infer as `User`
        let src = "val user: User = User()\nuser.run { this.name }";
        let (u, idx) = indexed("/t.kt", src);
        let col = "user.run { ".len() as u32;
        // `before_brace` via lambda_receiver_type_from_context
        let before = "user.run ";
        let result = lambda_receiver_type_from_context(before, &idx, &u);
        assert_eq!(result.as_deref(), Some("User"),
            "this in obj.run should be User, got: {result:?}");
    }

    #[test]
    fn this_infer_lambda_param_type_at() {
        let src = "val user: User = User()\nuser.run { this.name }";
        let (u, idx) = indexed("/t.kt", src);
        let col = "user.run { ".len() as u32;
        let result = idx.infer_lambda_param_type_at("this", &u, Position::new(1, col));
        assert_eq!(result.as_deref(), Some("User"),
            "infer_lambda_param_type_at for this, got: {result:?}");
    }

    #[test]
    fn with_scope_function_this_type() {
        // `with(user) { this.name }` — `with` is stdlib, first arg is receiver
        let src = "val user: User = User()\nwith(user) { this.name }";
        let (u, idx) = indexed("/t.kt", src);
        let before = "with(user) ";
        let result = lambda_receiver_type_from_context(before, &idx, &u);
        assert_eq!(result.as_deref(), Some("User"),
            "with(user) this should be User, got: {result:?}");
    }

    // ── `this` in class method body ───────────────────────────────────────────

    #[test]
    fn this_in_class_method_resolves_to_class() {
        let src = concat!(
            "class OverviewViewModel {\n",
            "    override fun handleEvent(event: Event) {\n",
            "        this.doSomething()\n",
            "    }\n",
            "}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        // Cursor on `this` at line 2, col 8
        let col = "        ".len() as u32;
        let result = idx.infer_lambda_param_type_at("this", &u, Position::new(2, col));
        assert_eq!(result.as_deref(), Some("OverviewViewModel"),
            "this in class method should resolve to enclosing class, got: {result:?}");
    }

    #[test]
    fn this_in_class_method_lambda_scope_wins() {
        // When `this` is inside a scope-function lambda inside a class method,
        // the lambda scope should win over the class scope.
        let src = concat!(
            "class Vm {\n",
            "    fun go() {\n",
            "        val user: User = getUser()\n",
            "        user.run {\n",
            "            this.name\n",
            "        }\n",
            "    }\n",
            "}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        // Cursor on line 4 (inside user.run lambda)
        let col = "            ".len() as u32;
        let result = idx.infer_lambda_param_type_at("this", &u, Position::new(4, col));
        assert_eq!(result.as_deref(), Some("User"),
            "this inside run lambda should be User not Vm, got: {result:?}");
    }

    #[test]
    fn this_as_named_arg_resolves_param_type() {
        // `.send(channel = this)` — `this` used as a named-arg value.
        // Should resolve to the expected parameter type: `SendChannel`.
        let src = concat!(
            "fun send(channel: SendChannel): Unit = TODO()\n",
            "fun go() {\n",
            "    something.send(channel = this)\n",  // line 2, `this` at col 28
            "}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        let col = "    something.send(channel = ".len() as u32;
        let result = idx.infer_lambda_param_type_at("this", &u, Position::new(2, col));
        assert_eq!(result.as_deref(), Some("SendChannel"),
            "this as named arg should hint param type, got: {result:?}");
    }

    #[test]
    fn it_as_positional_arg_resolves_param_type() {
        // `process(it)` — `it` as positional arg 0.
        let src = concat!(
            "fun process(value: Item): Unit = TODO()\n",
            "fun go() {\n",
            "    list.forEach { process(it) }\n",  // line 2, `it` at col 26
            "}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        let col = "    list.forEach { process(".len() as u32;
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(2, col));
        // Lambda inference for `list.forEach` fails (list not typed).
        // Positional arg fallback: `process(it)` → param 0 = `Item`.
        assert_eq!(result.as_deref(), Some("Item"),
            "it as positional arg should hint param type, got: {result:?}");
    }

    #[test]
    fn it_as_named_arg_resolves_param_type() {
        // `fn(value = it)` — `it` as named arg.
        let src = concat!(
            "fun process(value: Widget): Unit = TODO()\n",
            "fun go() {\n",
            "    process(value = it)\n",  // line 2, `it` at col 20
            "}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        let col = "    process(value = ".len() as u32;
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(2, col));
        assert_eq!(result.as_deref(), Some("Widget"),
            "it as named arg should hint param type, got: {result:?}");
    }

    #[test]
    fn it_positional_second_arg() {
        // `fn(first, it)` — `it` as positional arg 1.
        let src = concat!(
            "fun pair(a: String, b: Number): Unit = TODO()\n",
            "fun go() {\n",
            "    pair(\"x\", it)\n",  // line 2, `it` at col 14
            "}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        let col = "    pair(\"x\", ".len() as u32;
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(2, col));
        assert_eq!(result.as_deref(), Some("Number"),
            "it as second positional arg should be Number, got: {result:?}");
    }


    #[test]
    fn this_in_regular_lambda_no_lambda_hint() {
        // `this` inside a regular lambda `(T) -> R` should NOT get a lambda hint.
        // It refers to the enclosing class, not the lambda param.
        let src = concat!(
            "class Reducer {\n",
            "    fun reduce(event: String, block: (String) -> String): Unit = TODO()\n",
            "    fun go(event: String) {\n",
            "        reduce(event) { this }\n",  // line 3, `this` at col 24
            "    }\n",
            "}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        let col = "        reduce(event) { ".len() as u32;
        let result = idx.infer_lambda_param_type_at("this", &u, Position::new(3, col));
        // `this` inside regular (T)->R lambda must NOT get a lambda-param hint.
        // Falls through to enclosing_class_at → returns enclosing class.
        assert_eq!(result.as_deref(), Some("Reducer"),
            "this in regular lambda should be enclosing class, got: {result:?}");
    }

    #[test]
    fn this_in_receiver_lambda_indexed_function() {
        // `this` inside a receiver lambda `T.() -> R` from an indexed function (concrete type).
        let src = concat!(
            "class Ctx\n",
            "fun withCtx(block: Ctx.() -> Unit): Unit = TODO()\n",
            "val ctx: Ctx = Ctx()\n",
            "val _ = ctx.withCtx { this }\n",  // line 3, `this` after `{ `
        );
        let (u, idx) = indexed("/t.kt", src);
        let col = "val _ = ctx.withCtx { ".len() as u32;
        let result = idx.infer_lambda_param_type_at("this", &u, Position::new(3, col));
        assert_eq!(result.as_deref(), Some("Ctx"),
            "this inside receiver lambda withCtx should be Ctx, got: {result:?}");
    }

    #[test]
    fn named_arg_lambda_it_type_multiline() {
        // `SheetReloadActions(buildingSavings = { setEvent(it) })` — lambda on new line
        // after the constructor call. `it` should resolve to the first input type
        // of `buildingSavings`'s functional type.
        let src = concat!(
            "data class SaveInfo(val id: String)\n",
            "class SheetReloadActions(\n",
            "  val buildingSavings: (SaveInfo) -> Unit,\n",
            "  val cards: () -> Unit,\n",
            ")\n",
            "fun use() {\n",
            "  SheetReloadActions(\n",
            "    buildingSavings = { it },\n",  // line 7, cursor on `it`
            "    cards = {},\n",
            "  )\n",
            "}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        // cursor is on line 7, col inside `it`
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(7, 25));
        assert_eq!(result.as_deref(), Some("SaveInfo"),
            "it in named-arg lambda should be SaveInfo, got: {result:?}");
    }

    #[test]
    fn named_arg_lambda_multi_param_type() {
        // `SheetReloadActions(loan = { loanId, isWustenrot -> ... })` — multi-param.
        // `loanId` should be String (1st input), `isWustenrot` should be Boolean (2nd).
        let src = concat!(
            "class LoanInfo\n",
            "class SheetReloadActions(\n",
            "  val loan: (String, Boolean) -> Unit,\n",
            ")\n",
            "fun use() {\n",
            "  SheetReloadActions(\n",
            "    loan = { loanId, isWustenrot -> loanId },\n",  // line 6
            "  )\n",
            "}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        let result_loanid = idx.infer_lambda_param_type_at("loanId", &u, Position::new(6, 22));
        assert_eq!(result_loanid.as_deref(), Some("String"),
            "loanId should be String, got: {result_loanid:?}");
        let result_is = idx.infer_lambda_param_type_at("isWustenrot", &u, Position::new(6, 30));
        assert_eq!(result_is.as_deref(), Some("Boolean"),
            "isWustenrot should be Boolean, got: {result_is:?}");
    }

    #[test]
    fn extract_named_arg_name_test() {
        assert_eq!(super::extract_named_arg_name("  buildingSavings = "), Some("buildingSavings"));
        assert_eq!(super::extract_named_arg_name("  loan = "),            Some("loan"));
        assert_eq!(super::extract_named_arg_name("  loan="),              Some("loan"));
        // Same-line comma-separated: `, cards = ` — should match
        assert_eq!(super::extract_named_arg_name(", cards = "),           Some("cards"));
        // Uppercase — should NOT match (constructors, not named args)
        assert_eq!(super::extract_named_arg_name("  Foo = "),             None);
        // operator — should NOT match
        assert_eq!(super::extract_named_arg_name("a != "),                None);
        assert_eq!(super::extract_named_arg_name("a <= "),                None);
        // Nested: `(isRefresh = ` — opening `(` before the ident disqualifies
        assert_eq!(super::extract_named_arg_name("(isRefresh = "),        None);
        // Nested inside call args: `fn(x, isRefresh = ` — still has non-ws prefix
        assert_eq!(super::extract_named_arg_name("fn(x, isRefresh = "),   None);
    }

    // ── LoanReducer-style patterns ────────────────────────────────────────────

    #[test]
    fn named_arg_lambda_extension_function_callee() {
        // Mirrors LoanReducer: `flow.lazyLoadProductBottomSheet(map = { mapSheet(it) })`
        // Extension function callee + double-paren `((T) -> R)` param type.
        // `it` inside `map = {` should resolve to the first input of `((LoanDetail) -> Sheet)`.
        let src = concat!(
            "class LoanDetail\n",                                                 // line 0
            "class ProductDetailSheetModel\n",                                    // line 1
            "class Flow\n",                                                       // line 2
            "fun Flow.lazyLoadProductBottomSheet(\n",                             // line 3
            "  reloadAction: () -> Unit,\n",                                      // line 4
            "  map: ((LoanDetail) -> ProductDetailSheetModel),\n",                // line 5
            ") {}\n",                                                             // line 6
            "fun use(flow: Flow) {\n",                                            // line 7
            "  flow.lazyLoadProductBottomSheet(\n",                               // line 8
            "    reloadAction = { },\n",                                          // line 9
            "    map = { it },\n",                                                // line 10
            "  )\n",                                                              // line 11
            "}\n",                                                                // line 12
        );
        let (u, idx) = indexed("/LoanReducer.kt", src);
        // `it` on line 10, col inside the lambda body
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(10, 13));
        assert_eq!(result.as_deref(), Some("LoanDetail"),
            "it inside map lambda should be LoanDetail, got: {result:?}");
    }

    #[test]
    fn named_arg_reload_action_no_it() {
        // `reloadAction: () -> Unit` — lambda has no params, `it` should not resolve.
        let src = concat!(
            "class Flow\n",                                              // line 0
            "fun Flow.lazyLoadProductBottomSheet(\n",                   // line 1
            "  reloadAction: () -> Unit,\n",                            // line 2
            ") {}\n",                                                    // line 3
            "fun use(flow: Flow) {\n",                                  // line 4
            "  flow.lazyLoadProductBottomSheet(\n",                     // line 5
            "    reloadAction = { it },\n",                             // line 6
            "  )\n",                                                     // line 7
            "}\n",                                                       // line 8
        );
        let (u, idx) = indexed("/t.kt", src);
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(6, 21));
        assert_eq!(result, None,
            "it inside reloadAction lambda should not resolve (no params), got: {result:?}");
    }

    #[test]
    fn named_arg_lambda_double_paren_function_type() {
        // Double-paren `((T) -> R)` type — should still extract T as first input.
        let src = concat!(
            "class Item\n",                                              // line 0
            "fun process(\n",                                            // line 1
            "  mapper: ((Item) -> String),\n",                          // line 2
            ") {}\n",                                                    // line 3
            "fun use() {\n",                                             // line 4
            "  process(\n",                                              // line 5
            "    mapper = { it },\n",                                    // line 6
            "  )\n",                                                     // line 7
            "}\n",                                                       // line 8
        );
        let (u, idx) = indexed("/t.kt", src);
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(6, 16));
        assert_eq!(result.as_deref(), Some("Item"),
            "it inside double-paren type lambda should be Item, got: {result:?}");
    }

    // ── Full LoanReducer integration ─────────────────────────────────────────

    // Mirrors the real structure:
    //   flow.lazyLoadProductBottomSheet(
    //     state = state(),
    //     reloadAction = { reloadAction(...) },
    //     map = { mapSheet(it) },            ← it: T (generic param)
    //   ).collect { bottomSheetState ->       ← bottomSheetState: T (Flow element)
    fn loan_reducer_src() -> &'static str {
        concat!(
            "class LoanDetail\n",                                                // 0
            "class ProductDetailSheetModel\n",                                   // 1
            "class Flow\n",                                                      // 2
            "class BottomSheetState\n",                                          // 3
            "fun <T> Flow.lazyLoadProductBottomSheet(\n",                        // 4
            "  state: BottomSheetState,\n",                                      // 5
            "  reloadAction: () -> Unit,\n",                                     // 6
            "  map: ((T) -> ProductDetailSheetModel),\n",                        // 7
            "): Flow {}\n",                                                      // 8
            "fun <T> Flow.collect(action: (T) -> Unit) {}\n",                    // 9
            "fun use(flow: Flow) {\n",                                           // 10
            "  flow.lazyLoadProductBottomSheet(\n",                              // 11
            "    state = flow,\n",                                               // 12
            "    reloadAction = { },\n",                                         // 13
            "    map = { mapSheet(it) },\n",                                     // 14
            "  ).collect { bottomSheetState ->\n",                               // 15
            "    use(bottomSheetState)\n",                                       // 16
            "  }\n",                                                             // 17
            "}\n",                                                               // 18
        )
    }

    #[test]
    fn loan_reducer_map_it_resolves_to_T() {
        let (u, idx) = indexed("/LoanReducer.kt", loan_reducer_src());
        // `it` in `map = { mapSheet(it) }` — line 14, col inside lambda body
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(14, 20));
        assert_eq!(result.as_deref(), Some("T"),
            "it in map lambda should be T (generic param), got: {result:?}");
    }

    #[test]
    fn loan_reducer_reload_action_no_it() {
        let (u, idx) = indexed("/LoanReducer.kt", loan_reducer_src());
        // `reloadAction: () -> Unit` — empty param type, no `it`
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(13, 21));
        assert_eq!(result, None,
            "it in reloadAction lambda should be None (no params), got: {result:?}");
    }

    #[test]
    fn loan_reducer_collect_bottomsheetstate_resolves_to_T() {
        let (u, idx) = indexed("/LoanReducer.kt", loan_reducer_src());
        // `bottomSheetState` in `.collect { bottomSheetState -> ... }` — line 15
        let result = idx.infer_lambda_param_type_at("bottomSheetState", &u, Position::new(16, 8));
        assert_eq!(result.as_deref(), Some("T"),
            "bottomSheetState in collect lambda should be T, got: {result:?}");
    }

    #[test]
    fn suspend_param_type_resolves_it() {
        // `collectIn` has `block: suspend (T) -> Unit` — `suspend` prefix must not block inference.
        let src = concat!(
            "class Flow\n",                                          // 0
            "fun <T> Flow.collectIn(block: suspend (T) -> Unit) {}\n", // 1
            "fun use(flow: Flow) {\n",                               // 2
            "  flow.collectIn { it.doSomething() }\n",               // 3  col 19 = 'it'
            "}\n",                                                   // 4
        );
        let (u, idx) = indexed("/t.kt", src);
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(3, 19));
        assert_eq!(result.as_deref(), Some("T"),
            "it in suspend-param collectIn lambda should be T, got: {result:?}");
    }

    #[test]
    fn find_named_param_type_in_sig_test() {
        let sig = "val buildingSavings: (SaveInfo) -> Unit, val loan: (String, Boolean) -> Unit";
        assert_eq!(
            super::find_named_param_type_in_sig(sig, "loan"),
            Some("(String, Boolean) -> Unit".into())
        );
        assert_eq!(
            super::find_named_param_type_in_sig(sig, "buildingSavings"),
            Some("(SaveInfo) -> Unit".into())
        );
        assert_eq!(super::find_named_param_type_in_sig(sig, "unknown"), None);
    }

    #[test]
    fn has_named_params_not_it_test() {
        // Single-param named → true
        assert!(super::has_named_params_not_it("item -> item.name"));
        // Multi-param named → true
        assert!(super::has_named_params_not_it("loanId, isWustenrot -> setEvent(loanId)"));
        // Implicit `it` → false
        assert!(!super::has_named_params_not_it("it.name"));
        // Block / empty → false
        assert!(!super::has_named_params_not_it("setEvent(something)"));
        assert!(!super::has_named_params_not_it(""));
        // `_` wildcard params — skip
        assert!(!super::has_named_params_not_it("_ -> something"));
    }

    #[test]
    fn it_not_resolved_inside_multi_param_named_lambda() {
        // `it` inside `{ loanId, isWustenrot -> ... }` should return None,
        // NOT `Some("String")` from the first param type.
        let src = concat!(
            "class SheetReloadActions(\n",
            "  val loan: (String, Boolean) -> Unit,\n",
            ")\n",
            "fun use() {\n",
            "  SheetReloadActions(\n",
            "    loan = { loanId, isWustenrot ->\n",  // line 5
            "      it\n",                               // line 6, cursor here
            "    }\n",
            "  )\n",
            "}\n",
        );
        let (u, idx) = indexed("/t.kt", src);
        // `it` inside the multi-param lambda body — should NOT resolve
        // (no implicit `it` when explicit params exist)
        let result = idx.infer_lambda_param_type_at("it", &u, Position::new(6, 6));
        assert!(result.is_none(),
            "it inside multi-param lambda should be None, got: {result:?}");
    }

    // ── subtypes index (goToImplementation) ──────────────────────────────────

    #[test]
    fn subtypes_index_basic() {
        let idx = Indexer::new();
        let iface_uri = uri("/IAnimal.kt");
        idx.index_content(&iface_uri, "interface IAnimal {\n    fun speak(): String\n}");
        let dog_uri = uri("/Dog.kt");
        idx.index_content(&dog_uri, "class Dog : IAnimal {\n    override fun speak() = \"woof\"\n}");
        let cat_uri = uri("/Cat.kt");
        idx.index_content(&cat_uri, "class Cat : IAnimal {\n    override fun speak() = \"meow\"\n}");

        let subs = idx.subtypes.get("IAnimal").expect("should have subtypes for IAnimal");
        let sub_uris: Vec<_> = subs.iter().map(|l| l.uri.to_string()).collect();
        assert!(sub_uris.contains(&dog_uri.to_string()), "Dog should be a subtype");
        assert!(sub_uris.contains(&cat_uri.to_string()), "Cat should be a subtype");
        assert_eq!(subs.len(), 2);
    }

    #[test]
    fn subtypes_index_multiple_supertypes() {
        let idx = Indexer::new();
        idx.index_content(&uri("/A.kt"), "interface Flyable");
        idx.index_content(&uri("/B.kt"), "interface Swimmable");
        idx.index_content(&uri("/Duck.kt"), "class Duck : Flyable, Swimmable {\n}");

        let fly_subs = idx.subtypes.get("Flyable").expect("Flyable subtypes");
        assert_eq!(fly_subs.len(), 1);
        let swim_subs = idx.subtypes.get("Swimmable").expect("Swimmable subtypes");
        assert_eq!(swim_subs.len(), 1);
    }

    #[test]
    fn subtypes_index_reindex_cleans_stale() {
        let idx = Indexer::new();
        let u = uri("/Dog.kt");
        idx.index_content(&u, "class Dog : IAnimal {}");
        assert!(idx.subtypes.get("IAnimal").is_some());

        // Re-index same file without supertype — stale entry should be cleaned.
        idx.index_content(&u, "class Dog {}");
        let subs = idx.subtypes.get("IAnimal");
        let empty = subs.map(|s| s.is_empty()).unwrap_or(true);
        assert!(empty, "stale subtype entry should be removed on re-index");
    }

    #[test]
    fn subtypes_no_false_positive_across_classes() {
        // File with two classes — each should only register its own supertypes.
        let idx = Indexer::new();
        idx.index_content(&uri("/multi.kt"), "\
class Foo : Alpha {\n}\n\
class Bar : Beta {\n}");

        let alpha_subs = idx.subtypes.get("Alpha").map(|s| s.len()).unwrap_or(0);
        let beta_subs = idx.subtypes.get("Beta").map(|s| s.len()).unwrap_or(0);
        assert_eq!(alpha_subs, 1, "Alpha should have exactly 1 subtype (Foo)");
        assert_eq!(beta_subs, 1, "Beta should have exactly 1 subtype (Bar)");
        // Foo should NOT appear as subtype of Beta, and vice versa.
        if let Some(alpha) = idx.subtypes.get("Alpha") {
            let names: Vec<_> = alpha.iter().filter_map(|l| {
                idx.files.get(l.uri.as_str()).and_then(|f|
                    f.symbols.iter().find(|s| s.selection_range == l.range).map(|s| s.name.clone()))
            }).collect();
            assert!(names.contains(&"Foo".to_string()), "Alpha subtype should be Foo, got {names:?}");
            assert!(!names.contains(&"Bar".to_string()), "Bar should NOT be Alpha subtype");
        };
    }

    #[test]
    fn subtypes_sealed_class_inner_objects() {
        // sealed class with inner subtypes — a common Kotlin MVI pattern.
        let idx = Indexer::new();
        idx.index_content(&uri("/StoreState.kt"), "\
sealed class StoreState {
    object Uninitialized : StoreState()
    data class Ready(val data: String) : StoreState()
    data class Error(val msg: String) : StoreState()
}");
        let subs = idx.subtypes.get("StoreState").expect("should have subtypes for StoreState");
        let names: Vec<String> = subs.iter().filter_map(|l| {
            idx.files.get(l.uri.as_str()).and_then(|f|
                f.symbols.iter().find(|s| s.selection_range == l.range).map(|s| s.name.clone()))
        }).collect();
        assert!(names.contains(&"Uninitialized".to_string()), "Uninitialized should be a subtype, got {names:?}");
        assert!(names.contains(&"Ready".to_string()), "Ready should be a subtype, got {names:?}");
        assert!(names.contains(&"Error".to_string()), "Error should be a subtype, got {names:?}");
        assert_eq!(subs.len(), 3, "should find exactly 3 sealed subtypes");
    }

    #[test]
    fn subtypes_generic_supertype() {
        // class extends a generic base: `class Concrete : Base<String>()`
        let idx = Indexer::new();
        idx.index_content(&uri("/ILoader.kt"), "interface ILoader<out T>");
        idx.index_content(&uri("/BaseLoader.kt"), "abstract class BaseLoader<T> : ILoader<T>");
        idx.index_content(&uri("/StringLoader.kt"), "class StringLoader : BaseLoader<String>()");

        // Direct: BaseLoader is subtype of ILoader
        let iloader_subs = idx.subtypes.get("ILoader").expect("ILoader subtypes");
        assert_eq!(iloader_subs.len(), 1, "ILoader should have 1 direct subtype (BaseLoader)");

        // Direct: StringLoader is subtype of BaseLoader
        let base_subs = idx.subtypes.get("BaseLoader").expect("BaseLoader subtypes");
        assert_eq!(base_subs.len(), 1, "BaseLoader should have 1 direct subtype (StringLoader)");
    }

    #[test]
    fn subtypes_constructor_with_params() {
        // Class with constructor params before supertype: `class Foo(val x: Int) : Bar(x)`
        let idx = Indexer::new();
        idx.index_content(&uri("/Bar.kt"), "open class Bar(val x: Int)");
        idx.index_content(&uri("/Foo.kt"), "class Foo(val x: Int) : Bar(x) {\n    fun doStuff() {}\n}");

        let subs = idx.subtypes.get("Bar").expect("Bar subtypes");
        assert_eq!(subs.len(), 1, "Bar should have 1 subtype (Foo)");
    }

    #[test]
    fn subtypes_sealed_generic() {
        // Generic sealed class — subtypes use concrete type args: `StoreState<Nothing>()`
        let idx = Indexer::new();
        idx.index_content(&uri("/State.kt"), "\
sealed class StoreState<out T> {
    object Uninitialized : StoreState<Nothing>()
    data class Ready<out T>(val data: T) : StoreState<T>()
    data class Error(val error: Throwable) : StoreState<Nothing>()
}");
        let subs = idx.subtypes.get("StoreState").expect("should have subtypes for generic StoreState");
        let names: Vec<String> = subs.iter().filter_map(|l| {
            idx.files.get(l.uri.as_str()).and_then(|f|
                f.symbols.iter().find(|s| s.selection_range == l.range).map(|s| s.name.clone()))
        }).collect();
        assert!(names.contains(&"Uninitialized".to_string()), "Uninitialized missing, got {names:?}");
        assert!(names.contains(&"Ready".to_string()), "Ready missing, got {names:?}");
        assert!(names.contains(&"Error".to_string()), "Error missing, got {names:?}");
        assert_eq!(subs.len(), 3, "should find exactly 3 sealed subtypes");
    }

    #[test]
    fn subtypes_transitive_chain_realistic() {
        // Mimics Android interactor pattern:
        // ISimpleLoadDataInteractor <- SimpleLoadDataInteractor (abstract generic base)
        // SimpleLoadDataInteractor <- ConcreteInteractor1, ConcreteInteractor2, ...
        let idx = Indexer::new();
        idx.index_content(&uri("/ISimpleLoadDataInteractor.kt"), "\
interface ISimpleLoadDataInteractor<out T> {
    suspend fun loadData(): T
}");
        idx.index_content(&uri("/SimpleLoadDataInteractor.kt"), "\
abstract class SimpleLoadDataInteractor<out T>(
    private val dispatcher: String
) : ISimpleLoadDataInteractor<T> {
    override suspend fun loadData(): T = withContext(dispatcher) { doLoad() }
    protected abstract suspend fun doLoad(): T
}");
        idx.index_content(&uri("/ContactLoader.kt"), "\
class ContactAddressInteractor(
    dispatcher: String
) : SimpleLoadDataInteractor<String>(dispatcher) {
    override suspend fun doLoad(): String = \"contacts\"
}");
        idx.index_content(&uri("/BalanceLoader.kt"), "\
class BalanceInteractor(
    dispatcher: String,
    private val repo: String
) : SimpleLoadDataInteractor<Int>(dispatcher) {
    override suspend fun doLoad(): Int = 42
}");

        // Direct subtypes of ISimpleLoadDataInteractor
        let direct = idx.subtypes.get("ISimpleLoadDataInteractor")
            .expect("ISimpleLoadDataInteractor should have direct subtypes");
        assert_eq!(direct.len(), 1, "should have 1 direct subtype (SimpleLoadDataInteractor)");

        // Direct subtypes of SimpleLoadDataInteractor
        let base_subs = idx.subtypes.get("SimpleLoadDataInteractor")
            .expect("SimpleLoadDataInteractor should have subtypes");
        assert_eq!(base_subs.len(), 2, "should have 2 direct subtypes");
    }

    #[test]
    fn subtypes_multiline_constructor() {
        // Multi-line constructor where supertype is on a continuation line:
        // class Foo(
        //     val x: Int,
        //     val y: String
        // ) : Bar(x) {
        let idx = Indexer::new();
        idx.index_content(&uri("/Base.kt"), "open class Base");
        idx.index_content(&uri("/Sub.kt"), "\
class Sub(
    val x: Int,
    val y: String
) : Base() {
    fun doStuff() {}
}");
        let subs = idx.subtypes.get("Base").expect("Base subtypes");
        assert_eq!(subs.len(), 1, "Base should have 1 subtype (Sub)");
    }

    #[test]
    fn subtypes_annotation_with_braces() {
        // Annotation on the class declaration that contains `{}`
        // should not stop header collection prematurely.
        let idx = Indexer::new();
        idx.index_content(&uri("/Mod.kt"), "\
@Module
@Provides({Foo::class, Bar::class})
class FooModule : BaseModule() {
    fun provide() {}
}");
        let subs = idx.subtypes.get("BaseModule")
            .expect("BaseModule should have subtypes");
        assert_eq!(subs.len(), 1, "annotation braces should not prevent supertype extraction");
    }

    #[test]
    fn subtypes_survive_cache_roundtrip() {
        // Simulate cache restore: index a file, save its FileData, create a
        // fresh indexer, restore from the saved data, check subtypes populated.
        let idx1 = Indexer::new();
        let u = uri("/Dog.kt");
        idx1.index_content(&u, "class Dog : IAnimal {\n    fun bark() {}\n}");

        // Grab the FileData that index_content produced.
        let data = idx1.files.get(u.as_str()).unwrap().clone();
        assert!(idx1.subtypes.get("IAnimal").is_some(), "subtypes populated after index_content");

        // Simulate loading from cache into a new indexer.
        let idx2 = Indexer::new();
        let entry = FileCacheEntry {
            mtime_secs: 0,
            file_size: 0,
            content_hash: 42,
            file_data: (*data).clone(),
        };
        // Use the pure pipeline: cache_entry_to_file_result → apply_file_result.
        let result = cache_entry_to_file_result(&u, &entry);
        idx2.apply_file_result(&result);

        // subtypes should be populated from cache restore.
        let subs = idx2.subtypes.get("IAnimal")
            .expect("subtypes should be populated after cache restore");
        assert_eq!(subs.len(), 1, "Dog should be a subtype of IAnimal after cache restore");
    }

    // ── real-world patterns from Moneta/android ──────────────────────────────

    #[test]
    fn real_sealed_interface_store_state() {
        let idx = Indexer::new();
        idx.index_content(&uri("/StoreState.kt"), "\
package cz.moneta.smartbanka.common.mvi.store

sealed interface StoreState<out S> : BusinessState {
  data object Uninitialized : StoreState<Nothing>
  data class Ready<S>(val state: S) : StoreState<S>

  fun readyOrNull(): S? {
    return when (this) {
      is Ready -> this.state
      Uninitialized -> null
    }
  }
}");
        let subs = idx.subtypes.get("StoreState")
            .expect("StoreState should have subtypes");
        let names: Vec<String> = subs.iter().filter_map(|l| {
            idx.files.get(l.uri.as_str()).and_then(|f|
                f.symbols.iter().find(|s| s.selection_range == l.range).map(|s| s.name.clone()))
        }).collect();
        assert!(names.contains(&"Uninitialized".to_string()), "Uninitialized missing: {names:?}");
        assert!(names.contains(&"Ready".to_string()), "Ready missing: {names:?}");
        assert_eq!(subs.len(), 2);
    }

    #[test]
    fn real_isimpleloaddatainteractor_chain() {
        let idx = Indexer::new();
        idx.index_content(&uri("/IInteractor.kt"), "\
package cz.moneta.smartbanka.shared_logic.product
interface IInteractor<Output>");
        idx.index_content(&uri("/ISimpleLoadDataInteractor.kt"), "\
package cz.moneta.smartbanka.shared_logic.product
interface ISimpleLoadDataInteractor<Output> : IInteractor<Output> {
  suspend fun loadData(): Output
}");
        idx.index_content(&uri("/ContactAddressInteractor.kt"), "\
package cz.moneta.smartbanka.feature.gold_conversion.model.goldcard
internal class ContactAddressInteractor @Inject constructor(
  private val repo: IGoldConversionRepository,
) : ISimpleLoadDataInteractor<PersonalAddress> {
  override suspend fun loadData(): PersonalAddress =
    requireNotNull(repo.contactAddressSetup().contactAddress)
}");
        idx.index_content(&uri("/PermanentAddressInteractor.kt"), "\
package cz.moneta.smartbanka.feature.gold_conversion.model.goldcard
internal class PermanentAddressInteractor @Inject constructor(
  private val repo: IGoldConversionRepository,
) : ISimpleLoadDataInteractor<PersonalAddress> {
  override suspend fun loadData(): PersonalAddress =
    requireNotNull(repo.permanentAddressSetup().permanentAddress)
}");

        // Direct subtypes of ISimpleLoadDataInteractor
        let subs = idx.subtypes.get("ISimpleLoadDataInteractor")
            .expect("ISimpleLoadDataInteractor should have subtypes");
        assert_eq!(subs.len(), 2, "should find both interactors");

        // ISimpleLoadDataInteractor itself is a subtype of IInteractor
        let iinteractor_subs = idx.subtypes.get("IInteractor")
            .expect("IInteractor should have subtypes");
        assert_eq!(iinteractor_subs.len(), 1, "ISimpleLoadDataInteractor is subtype of IInteractor");
    }

    // ─── Pure function tests ──────────────────────────────────────────────────

    fn make_result(uri_str: &str, pkg: &str, sym_name: &str, content: &str) -> FileIndexResult {
        let u = Url::parse(uri_str).unwrap();
        let mut result = Indexer::parse_file(&u, content);
        // Ensure package is set for qualified-key tests.
        result.data.package = Some(pkg.to_string());
        result
    }

    #[test]
    fn file_contributions_definitions() {
        let result = make_result(
            "file:///pkg/Foo.kt",
            "com.example",
            "Foo",
            "package com.example\nclass Foo",
        );
        let contrib = super::file_contributions(&result);
        assert!(contrib.definitions.contains_key("Foo"), "should have Foo in definitions");
        let locs = &contrib.definitions["Foo"];
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].uri.as_str(), "file:///pkg/Foo.kt");
    }

    #[test]
    fn file_contributions_qualified_both_keys() {
        // file stem = "Foo", class = "Bar" → both pkg.Bar and pkg.Foo.Bar inserted
        let result = make_result(
            "file:///pkg/Foo.kt",
            "com.example",
            "Bar",
            "package com.example\nclass Bar",
        );
        let contrib = super::file_contributions(&result);
        assert!(contrib.qualified.contains_key("com.example.Bar"), "pkg.Sym key missing");
        assert!(contrib.qualified.contains_key("com.example.Foo.Bar"), "pkg.Stem.Sym key missing");
    }

    #[test]
    fn file_contributions_qualified_stem_same_as_sym_no_alias() {
        // file stem = "Foo", class = "Foo" → only pkg.Foo, no pkg.Foo.Foo
        let result = make_result(
            "file:///pkg/Foo.kt",
            "com.example",
            "Foo",
            "package com.example\nclass Foo",
        );
        let contrib = super::file_contributions(&result);
        assert!(contrib.qualified.contains_key("com.example.Foo"), "pkg.Sym key missing");
        assert!(!contrib.qualified.contains_key("com.example.Foo.Foo"), "alias should not appear when stem == sym");
    }

    #[test]
    fn stale_keys_includes_both_qualified_aliases() {
        use crate::types::FileData;
        let uri = Url::parse("file:///pkg/Foo.kt").unwrap();
        let mut data = FileData::default();
        data.package = Some("com.example".to_string());
        let sym = crate::types::SymbolEntry {
            name: "Bar".to_string(),
            kind: tower_lsp::lsp_types::SymbolKind::CLASS,
            visibility: crate::types::Visibility::Public,
            range: Default::default(),
            selection_range: Default::default(),
            detail: String::new(),
        };
        data.symbols.push(sym);
        let stale = super::stale_keys_for(&uri, &data);
        assert!(stale.qualified_keys.contains(&"com.example.Bar".to_string()), "pkg.Sym missing");
        assert!(stale.qualified_keys.contains(&"com.example.Foo.Bar".to_string()), "pkg.Stem.Sym missing");
    }

    #[test]
    fn stale_keys_stem_equals_sym_no_alias() {
        use crate::types::FileData;
        let uri = Url::parse("file:///pkg/Foo.kt").unwrap();
        let mut data = FileData::default();
        data.package = Some("com.example".to_string());
        let sym = crate::types::SymbolEntry {
            name: "Foo".to_string(),
            kind: tower_lsp::lsp_types::SymbolKind::CLASS,
            visibility: crate::types::Visibility::Public,
            range: Default::default(),
            selection_range: Default::default(),
            detail: String::new(),
        };
        data.symbols.push(sym);
        let stale = super::stale_keys_for(&uri, &data);
        assert!(stale.qualified_keys.contains(&"com.example.Foo".to_string()), "pkg.Sym missing");
        assert!(!stale.qualified_keys.contains(&"com.example.Foo.Foo".to_string()), "alias should not appear");
    }

    #[test]
    fn build_bare_names_sorted_deduped() {
        let defs: DashMap<String, Vec<tower_lsp::lsp_types::Location>> = DashMap::new();
        defs.insert("Zebra".to_string(), vec![]);
        defs.insert("Apple".to_string(), vec![]);
        defs.insert("Apple".to_string(), vec![]); // duplicate key — DashMap replaces
        let names = super::build_bare_names(&defs);
        assert_eq!(names, vec!["Apple", "Zebra"]);
    }

    // ── apply_workspace_result ────────────────────────────────────────────────

    #[test]
    fn debug_super_chain() {
        let bar_src = "package com.example\nopen class Bar {\n  open fun doIt() {}\n}\n";
        let foo_src = "package com.example\nimport com.example.Bar\nclass Foo : Bar() {\n  override fun doIt() {\n    super.doIt()\n  }\n}";
        let (_, idx) = indexed("/Bar.kt", bar_src);
        let foo_uri = uri("/Foo.kt");
        idx.index_content(&foo_uri, foo_src);
        
        let bar_locs = idx.find_definition_qualified("Bar", None, &foo_uri);
        assert!(!bar_locs.is_empty(), "Bar should resolve via same-package or import");
    }

    // ── super / this go-to-def TDD tests ──────────────────────────────────────

    fn two_file_idx(a_path: &str, a_src: &str, b_path: &str, b_src: &str) -> (Url, Url, Indexer) {
        let (_, idx) = indexed(a_path, a_src);
        let b_uri = uri(b_path);
        idx.index_content(&b_uri, b_src);
        (uri(a_path), b_uri, idx)
    }

    /// `super` (standalone) resolves to the parent class declaration.
    #[test]
    fn goto_super_resolves_to_parent_class() {
        let bar_src = "package com.example\nopen class Bar\n";
        let foo_src = "package com.example\nclass Foo : Bar() {\n  fun test() {\n    super.toString()\n  }\n}";
        let (bar_uri, foo_uri, idx) = two_file_idx("/Bar.kt", bar_src, "/Foo.kt", foo_src);

        // Simulate `super` keyword lookup: find parent type names, then resolve them.
        let enclosing = idx.enclosing_class_at(&foo_uri, 3);
        assert_eq!(enclosing.as_deref(), Some("Foo"), "enclosing class");

        let locs = idx.find_definition_qualified("Bar", None, &foo_uri);
        assert!(!locs.is_empty(), "super should resolve to Bar");
        assert_eq!(locs[0].uri, bar_uri, "resolved to wrong file");
    }

    /// `super.method` resolves to the method in the parent class file.
    #[test]
    fn goto_super_method_resolves_in_parent() {
        let bar_src = "package com.example\nopen class Bar {\n  open fun onCleared() {}\n}\n";
        let foo_src = "package com.example\nclass Foo : Bar() {\n  override fun onCleared() {\n    super.onCleared()\n  }\n}";
        let (bar_uri, foo_uri, idx) = two_file_idx("/Bar.kt", bar_src, "/Foo.kt", foo_src);

        // `super.onCleared` → resolve_qualified("onCleared", "super", foo_uri)
        // should find onCleared defined in Bar.kt, NOT Foo.kt.
        let locs = idx.find_definition_qualified("onCleared", Some("super"), &foo_uri);
        assert!(!locs.is_empty(), "super.onCleared should resolve");
        assert_eq!(locs[0].uri, bar_uri, "super.onCleared should resolve to Bar.kt, not Foo.kt");
    }

    /// `this` (standalone) resolves to the enclosing class definition.
    #[test]
    fn goto_this_resolves_to_enclosing_class() {
        let src = "package com.example\nclass MyClass {\n  fun test() {\n    this.toString()\n  }\n}";
        let (u, idx) = indexed("/MyClass.kt", src);

        let enclosing = idx.enclosing_class_at(&u, 3);
        assert_eq!(enclosing.as_deref(), Some("MyClass"), "enclosing class for this");

        let locs = idx.find_definition_qualified("MyClass", None, &u);
        assert!(!locs.is_empty(), "this should resolve to MyClass");
        assert_eq!(locs[0].uri, u);
    }

    /// `super.method` where parent is not indexed must NOT resolve to the current
    /// class's override (which would be wrong). Should return empty or parent class.
    #[test]
    fn goto_super_method_no_fallthrough_to_override() {
        // Foo overrides doWork, but Base is NOT indexed.
        // super.doWork should NOT resolve to Foo.kt's override.
        let foo_src = "package com.example\nclass Foo : Base() {\n  override fun doWork() {\n    super.doWork()\n  }\n}";
        let (foo_uri, idx) = indexed("/Foo.kt", foo_src);

        // With super qualifier, result must NOT be in Foo.kt
        let locs = idx.find_definition_qualified("doWork", Some("super"), &foo_uri);
        for loc in &locs {
            assert_ne!(loc.uri, foo_uri, "super.doWork must not resolve to overriding file");
        }
    }

    /// `super.method` with multi-line constructor still resolves correctly.
    #[test]
    fn goto_super_method_multiline_constructor() {
        let bar_src = "package com.example\nopen class Bar {\n  open fun doWork() {}\n}\n";
        let foo_src = "package com.example
class Foo @Inject constructor(
  private val dep: String,
) : Bar() {
  override fun doWork() {
    super.doWork()
  }
}";
        let (bar_uri, foo_uri, idx) = two_file_idx("/Bar.kt", bar_src, "/Foo.kt", foo_src);

        // super.doWork at line 5 → should resolve to Bar.kt
        let locs = idx.find_definition_qualified("doWork", Some("super"), &foo_uri);
        assert!(!locs.is_empty(), "super.doWork should resolve");
        assert_eq!(locs[0].uri, bar_uri, "should resolve to Bar.kt");
    }

    // ── IgnoreMatcher ────────────────────────────────────────────────────────

    #[test]
    fn ignore_matcher_bare_pattern_matches_any_depth() {
        let root = Path::new("/workspace");
        let m = IgnoreMatcher::new(vec!["bazel-*".into()], root);
        assert!(m.matches(Path::new("bazel-bin/foo.kt")));
        assert!(m.matches(Path::new("sub/bazel-out/bar.kt")));
        assert!(!m.matches(Path::new("src/main.kt")));
    }

    #[test]
    fn ignore_matcher_path_pattern_matches_relative() {
        let root = Path::new("/workspace");
        let m = IgnoreMatcher::new(vec!["third-party/**".into()], root);
        assert!(m.matches(Path::new("third-party/lib/Foo.kt")));
        assert!(!m.matches(Path::new("src/third-party-util.kt")));
    }

    #[test]
    fn ignore_matcher_absolute_path_normalized() {
        let root = Path::new("/workspace");
        let m = IgnoreMatcher::new(vec!["/workspace/bazel-bin/**".into()], root);
        assert!(m.matches(Path::new("bazel-bin/foo.kt")));
        assert!(!m.matches(Path::new("src/main.kt")));
    }

    #[test]
    fn ignore_matcher_absolute_outside_root_skipped() {
        let root = Path::new("/workspace");
        // Pattern outside root should be skipped without panic.
        let m = IgnoreMatcher::new(vec!["/other/path/**".into()], root);
        assert!(!m.matches(Path::new("src/main.kt")));
    }

    #[test]
    fn ignore_matcher_empty_patterns() {
        let root = Path::new("/workspace");
        let m = IgnoreMatcher::new(vec![], root);
        assert!(m.is_empty());
        assert!(!m.matches(Path::new("src/main.kt")));
    }

    // ── E2E: ignorePatterns excludes files from the live index ───────────────

    /// Full indexing pipeline: build a real temp workspace, set ignore patterns,
    /// run `index_workspace_full`, and verify ignored symbols are absent.
    #[tokio::test]
    async fn e2e_ignore_patterns_excludes_symbols() {
        let dir = tempfile::TempDir::new().expect("create tempdir");
        let root = dir.path();

        // Normal source file.
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/Main.kt"),
            "package com.example\nclass MainClass {\n    fun hello(): String = \"world\"\n}\n"
        ).unwrap();

        // File inside a directory that should be ignored.
        std::fs::create_dir_all(root.join("bazel-bin/src")).unwrap();
        std::fs::write(root.join("bazel-bin/src/Generated.kt"),
            "package com.generated\nclass BazelGenerated {\n    fun run(): Int = 42\n}\n"
        ).unwrap();

        let indexer = Arc::new(Indexer::new());
        *indexer.ignore_matcher.write().unwrap() = Some(Arc::new(
            IgnoreMatcher::new(vec!["bazel-bin/**".to_owned()], root),
        ));

        Arc::clone(&indexer).index_workspace_full(root, None).await;

        assert!(
            indexer.definitions.contains_key("MainClass"),
            "MainClass (in src/) must be indexed"
        );
        assert!(
            !indexer.definitions.contains_key("BazelGenerated"),
            "BazelGenerated (in bazel-bin/) must be excluded by ignorePatterns"
        );
    }

    /// Bare pattern without path separator should exclude at any depth.
    #[tokio::test]
    async fn e2e_ignore_patterns_bare_pattern_any_depth() {
        let dir = tempfile::TempDir::new().expect("create tempdir");
        let root = dir.path();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/Keep.kt"),
            "package com.example\nclass KeepMe\n"
        ).unwrap();

        // Bare pattern "third-party" — should match nested dir at any depth.
        std::fs::create_dir_all(root.join("modules/third-party/lib")).unwrap();
        std::fs::write(root.join("modules/third-party/lib/Vendor.kt"),
            "package com.vendor\nclass VendorClass\n"
        ).unwrap();

        let indexer = Arc::new(Indexer::new());
        *indexer.ignore_matcher.write().unwrap() = Some(Arc::new(
            IgnoreMatcher::new(vec!["third-party".to_owned()], root),
        ));

        Arc::clone(&indexer).index_workspace_full(root, None).await;

        assert!(
            indexer.definitions.contains_key("KeepMe"),
            "KeepMe must be indexed"
        );
        assert!(
            !indexer.definitions.contains_key("VendorClass"),
            "VendorClass (under third-party/) must be excluded"
        );
    }

    // ── last_ident_in ────────────────────────────────────────────────────────

    #[test]
    fn last_ident_in_simple() {
        assert_eq!(crate::indexer::last_ident_in("foo.barBaz"), "barBaz");
    }
    #[test]
    fn last_ident_in_whole_string() {
        assert_eq!(crate::indexer::last_ident_in("identifier"), "identifier");
    }
    #[test]
    fn last_ident_in_empty() {
        assert_eq!(crate::indexer::last_ident_in(""), "");
    }
    #[test]
    fn last_ident_in_no_ident() {
        assert_eq!(crate::indexer::last_ident_in("foo.bar("), "");
    }
    #[test]
    fn last_ident_in_with_spaces() {
        assert_eq!(crate::indexer::last_ident_in("  someIdent"), "someIdent");
    }

    // ── completion helpers ───────────────────────────────────────────────────

    #[test]
    fn split_prefix_after_dot() {
        let (prefix, before_prefix) = split_prefix("foo.bar");
        assert_eq!(prefix, "bar");
        assert_eq!(before_prefix, "foo.");
    }
    #[test]
    fn split_prefix_bare() {
        let (prefix, before_prefix) = split_prefix("someIdent");
        assert_eq!(prefix, "someIdent");
        assert_eq!(before_prefix, "");
    }
    #[test]
    fn dot_receiver_simple() {
        assert_eq!(dot_receiver("foo."), Some("foo".to_string()));
    }
    #[test]
    fn dot_receiver_qualified() {
        assert_eq!(dot_receiver("Outer.Inner."), Some("Outer.Inner".to_string()));
    }
    #[test]
    fn dot_receiver_none() {
        assert_eq!(dot_receiver("foo"), None);
    }
}


