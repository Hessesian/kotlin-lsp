use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};

use dashmap::{DashMap, DashSet};
use tower_lsp::lsp_types::*;

use crate::types::{CursorPos, FileData};
use crate::StrExt;

// Re-export rg-module items that existing callers reach via `crate::indexer::`.
pub(crate) use crate::rg::IgnoreMatcher;
pub(crate) use crate::rg::SOURCE_EXTENSIONS;
pub(crate) use self::scan::{NoopReporter, ProgressReporter};

mod doc;

mod infer;
pub(crate) mod resolution;
// Re-export pure helpers from submodules so existing callers within this file
// and the inline test module (`use super::*`) continue to resolve them by name.
#[allow(unused_imports)]
pub(crate) use self::infer::{
    collect_all_fun_params_texts,
    collect_params_from_line,
    // sig.rs
    collect_signature,
    extract_first_arg,
    extract_named_arg_name,
    // args.rs
    find_as_call_arg_type,
    find_fun_signature_full,
    find_fun_signature_with_receiver,
    // it_this.rs
    find_it_element_type,
    find_it_element_type_in_lines,
    find_last_dot_at_depth_zero,
    find_named_lambda_param_type,
    find_named_lambda_param_type_in_lines,
    find_named_param_type_in_sig,
    find_this_element_type_in_lines,
    has_named_params_not_it,
    is_inside_receiver_lambda,
    is_lambda_param,
    lambda_brace_pos_for_param,
    lambda_param_position_on_line,
    lambda_receiver_type_from_context,
    last_fun_param_type_str,
    line_has_lambda_param,
    nth_fun_param_type_str,
    strip_trailing_call_args,
};

mod cache;
pub(crate) use self::cache::workspace_cache_path;

mod discover;

mod scan;
pub(crate) const MAX_FILES_UNLIMITED: usize = usize::MAX;

mod apply;
#[allow(unused_imports)]
pub(crate) use self::apply::{build_bare_names, file_contributions, stale_keys_for};

pub(crate) mod lookup;
pub(crate) use lookup::apply_type_subst;

mod node_ext;
pub(crate) use node_ext::NodeExt;

mod scope;
pub(crate) use scope::find_enclosing_call_name;
pub(crate) use scope::is_id_char;
pub(crate) use scope::last_ident_in;

pub(crate) mod live_tree;
pub(crate) use live_tree::LiveDoc;
mod live_tree_impl;

// Re-export cache/scan items needed by the inline test module below.
#[cfg(test)]
use self::cache::{cache_entry_to_file_result, FileCacheEntry};
use crate::resolver::{
    complete_symbol, complete_symbol_with_context, infer_variable_type_raw, is_annotation_context,
};
#[cfg(test)]
use crate::rg::regex_escape;
#[cfg(test)]
#[allow(unused_imports)]
use crate::types::{FileIndexResult, IndexStats};
#[cfg(test)]
use std::path::Path;

#[cfg(test)]
pub(crate) mod test_helpers;

// ─── Pure helper types ────────────────────────────────────────────────────────

/// Everything a single file *adds* to the index. Pure value — no DashMaps.
pub(crate) struct FileContributions {
    pub definitions: HashMap<String, Vec<Location>>,
    /// Both `pkg.Sym` and `pkg.FileStem.Sym` keys.
    pub qualified: HashMap<String, Location>,
    pub packages: HashMap<String, Vec<String>>,
    pub subtypes: HashMap<String, Vec<Location>>,
    pub file_data: (String, Arc<crate::types::FileData>),
    pub content_hash: (String, u64),
}

/// Keys to remove from the index when a file is replaced.
pub(crate) struct StaleKeys {
    pub definition_names: Vec<String>,
    /// Both aliases: `pkg.Sym` AND `pkg.FileStem.Sym`.
    pub qualified_keys: Vec<String>,
    pub package: Option<String>,
}

pub(crate) struct Indexer {
    /// URI string → parsed file data.
    pub(crate) files: DashMap<String, Arc<FileData>>,
    /// Short name → definition locations  (fast first-pass lookup).
    pub(crate) definitions: DashMap<String, Vec<Location>>,
    /// Fully-qualified name → location   (e.g. "com.example.Foo" → …).
    pub(crate) qualified: DashMap<String, Location>,
    /// Package name → vec of URI strings (for same-package resolution).
    pub(crate) packages: DashMap<String, Vec<String>>,
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
        infer_variable_type_raw(self, var_name, uri)
    }
    fn find_field_type(&self, class_name: &str, field_name: &str) -> Option<String> {
        crate::resolver::infer::find_field_type_in_class(self, class_name, field_name)
    }
    fn find_fun_return_type(&self, fn_name: &str) -> Option<String> {
        crate::resolver::infer::find_fun_return_type_by_name(self, fn_name)
    }
}

impl Indexer {
    pub(crate) fn parse_sem(&self) -> Arc<tokio::sync::Semaphore> {
        Arc::clone(&self.parse_sem)
    }

    pub(crate) fn new() -> Self {
        Self {
            files: DashMap::new(),
            definitions: DashMap::new(),
            qualified: DashMap::new(),
            packages: DashMap::new(),
            workspace_root: RwLock::new(None),
            content_hashes: DashMap::new(),
            // Allow configurable concurrent parse workers. Default to number of CPU cores.
            // Use env KOTLIN_LSP_PARSE_WORKERS to override.
            parse_sem: {
                // Default to half of available CPUs to avoid saturating system.
                let cpus = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4);
                let default = (cpus / 2).max(1);
                let configured = std::env::var("KOTLIN_LSP_PARSE_WORKERS")
                    .ok()
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(default);
                Arc::new(tokio::sync::Semaphore::new(configured))
            },
            parse_count: AtomicU64::new(0),
            completion_cache: DashMap::new(),
            live_lines: DashMap::new(),
            subtypes: DashMap::new(),
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
    pub(crate) fn reset_index_state(&self) {
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
    pub(crate) fn set_live_lines(&self, uri: &Url, content: &str) {
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
            let pos = CursorPos {
                line: cursor_line,
                utf16_col: cursor_col,
            };
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
            if param.to_lowercase().starts_with(prefix_lower.as_str())
                && !items.iter().any(|i| i.label == param)
            {
                items.push(CompletionItem {
                    label: param.clone(),
                    kind: Some(CompletionItemKind::VARIABLE),
                    sort_text: Some(format!("005:{param}")),
                    ..Default::default()
                });
            }
        }
    }

    ///
    /// Uses `live_lines` (updated synchronously on every keystroke) for the
    /// current file's line text, falling back to indexed lines or disk.
    pub(crate) fn completions(
        &self,
        uri: &Url,
        position: Position,
        snippets: bool,
    ) -> (Vec<CompletionItem>, bool) {
        self.ensure_indexed(uri);

        let Some(line) = self.line_for_position(uri, position.line) else {
            return (vec![], false);
        };
        let before = before_cursor(&line, position.character);
        let (prefix, before_prefix) = split_prefix(before);

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
            format!(
                "{}|{}|{}|{}",
                uri.as_str(),
                before_prefix,
                position.line,
                prefix
            )
        };
        if let Ok(guard) = self.last_completion.lock() {
            if let Some((ref k, _, ref cached)) = *guard {
                if k == &cache_key {
                    return (cached.clone(), false);
                }
            }
        }

        let dot_recv = dot_receiver(before_prefix);

        // `this.` / `it.` / named-param dot-completion.
        // `this` can mean: (a) scope-function receiver, (b) enclosing class.
        // `it` means: implicit lambda param.
        // Named lambda params are detected via `is_lambda_param`.
        if let Some(ref recv) = dot_recv {
            if recv == "it"
                || recv == "this"
                || is_lambda_param(recv, before, self, uri, position.line as usize)
            {
                let cursor_line = position.line as usize;
                let cursor_col = before.chars().count();
                let elem_type =
                    self.resolve_lambda_recv_type(recv, before, cursor_line, cursor_col, uri);
                if let Some(elem_type) = elem_type {
                    let (items, _) = complete_symbol(self, prefix, Some(&elem_type), uri, snippets);
                    if items.is_empty() {
                        // Type name known (e.g. generic param `T`, `StateType`) but not
                        // indexed — show a single hint item so the user sees the inferred type.
                        return (
                            vec![CompletionItem {
                                label: format!("{recv}: {elem_type}"),
                                kind: Some(CompletionItemKind::TYPE_PARAMETER),
                                detail: Some(format!("Inferred type: {elem_type}")),
                                sort_text: Some("~hint".into()),
                                ..Default::default()
                            }],
                            false,
                        );
                    }
                    return (items, false);
                }
                // Recognised lambda param but type unresolvable — return empty.
                return (vec![], false);
            }
        }

        let annotation_only = dot_recv.is_none() && is_annotation_context(before, prefix);
        let (mut items, hit_cap) = complete_symbol_with_context(
            self,
            prefix,
            dot_recv.as_deref(),
            uri,
            snippets,
            annotation_only,
        );

        // Add scope-aware lambda parameter names (bare-word completion only).
        if dot_recv.is_none() {
            self.add_lambda_param_completions(&mut items, uri, position.line as usize, prefix);
        }

        // Store in last_completion cache.
        if let Ok(mut guard) = self.last_completion.lock() {
            *guard = Some((cache_key, prefix.to_owned(), items.clone()));
        }

        (items, hit_cap)
    }
}

// ─── completion helpers (free functions) ─────────────────────────────────────

/// Returns a slice of `line` up to the UTF-16 column `utf16_col`.
fn before_cursor(line: &str, utf16_col: u32) -> &str {
    let target = utf16_col as usize;
    let mut utf16 = 0usize;
    let mut byte_end = line.len();
    for (bi, ch) in line.char_indices() {
        if utf16 >= target {
            byte_end = bi;
            break;
        }
        utf16 += ch.len_utf16();
    }
    &line[..byte_end]
}

/// Splits `before` into the trailing identifier fragment (`prefix`) and
/// everything that precedes it (`before_prefix`).
fn split_prefix(before: &str) -> (&str, &str) {
    let prefix = last_ident_in(before);
    let before_prefix = &before[..before.len() - prefix.len()];
    (prefix, before_prefix)
}

/// Returns the expression immediately before a trailing dot in `before_prefix`,
/// or `None` if `before_prefix` does not end with a dot.
///
/// Handles one level of qualification: `Outer.Inner.` → `"Outer.Inner"`.
fn dot_receiver(before_prefix: &str) -> Option<String> {
    let before_dot = before_prefix.strip_suffix('.')?;
    let inner = last_ident_in(before_dot);
    if inner.is_empty() {
        return None;
    }
    let remaining = &before_dot[..before_dot.len() - inner.len()];
    if remaining.ends_with('.') && inner.starts_with_uppercase() {
        let outer = last_ident_in(&remaining[..remaining.len() - 1]);
        if !outer.is_empty() && outer.starts_with_uppercase() {
            return Some(format!("{outer}.{inner}"));
        }
    }
    Some(inner.to_owned())
}

// ─── rg cross-file fallback ──────────────────────────────────────────────────

#[cfg(test)]
#[path = "indexer_tests.rs"]
mod tests;
