use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};

use dashmap::{DashMap, DashSet};
use tower_lsp::lsp_types::*;

use crate::types::FileData;

// Re-export rg-module items that existing callers reach via `crate::indexer::`.
pub(crate) use self::scan::{NoopReporter, ProgressReporter};
pub(crate) use crate::rg::IgnoreMatcher;

mod doc;

mod cst_folding;
pub(crate) use self::cst_folding::cst_folding_ranges;

mod infer;
pub(crate) mod resolution;
// Re-export pure helpers from submodules so existing callers within this file
// and the inline test module (`use super::*`) continue to resolve them by name.
#[allow(unused_imports)]
pub(crate) use self::infer::{
    collect_all_fun_params_texts,
    collect_params_from_line,
    collect_signature,
    cst_call_info,
    cst_cursor_is_local_var,
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
    // expr_type.rs
    infer_expr_type,
    is_import_reachable,
    is_inside_receiver_lambda,
    is_lambda_param,
    lambda_brace_pos_for_param,
    lambda_param_position_on_line,
    lambda_receiver_type_from_context,
    last_fun_param_type_str,
    line_has_lambda_param,
    nth_fun_param_type_str,
    resolve_call_signature,
    split_params_at_depth_zero,
    strip_trailing_call_args,
    // sig.rs
    CallInfo,
    CallSite,
    // deps.rs
    InferDeps,
    ResolutionScope,
    SignatureResult,
};

mod cache;
pub(crate) use self::cache::workspace_cache_path;

mod discover;

mod scan;
pub(crate) const MAX_FILES_UNLIMITED: usize = usize::MAX;

mod workspace_root;
pub(crate) use self::workspace_root::WorkspaceRoot;

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
use crate::resolver::infer_variable_type_raw;
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
    /// Workspace root path + monotonic staleness generation.
    /// The only write path is [`WorkspaceRoot::set`], which always bumps the
    /// generation — coupling enforced by the type, not by convention.
    /// Written only by [`crate::workspace::Actor`]; read-paths elsewhere observe it.
    pub(crate) workspace_root: WorkspaceRoot,
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
    /// Written only by [`crate::workspace::Actor`].
    pub(crate) workspace_pinned: std::sync::atomic::AtomicBool,
    /// Set to true after a non-truncated workspace scan; false after a truncated one.
    /// Drives `complete_scan` on the on-disk cache so warm-manifest mode is only
    /// used when the cache is known to be a full workspace snapshot.
    pub(crate) last_scan_complete: std::sync::atomic::AtomicBool,
    /// User-configured ignore patterns from LSP `initializationOptions`.
    /// Applied during file discovery to exclude matching paths.
    /// Written only by [`crate::workspace::Actor`]; tests configure it through actor events too.
    pub(crate) ignore_matcher: RwLock<Option<Arc<IgnoreMatcher>>>,
    /// Resolved source paths written by the workspace actor for `index_source_paths`.
    /// Populated from `Config::resolve_sources()`, which merges `initializationOptions.indexingOptions.sourcePaths`,
    /// auto-discovered `workspace.json` / build-layout paths, and the default extract-sources dir.
    /// Written only by [`crate::workspace::Actor`]; visibility stays `pub(crate)` for read-path consumers.
    pub(crate) source_paths_raw: RwLock<Vec<String>>,
    /// Workspace source roots for scoping rg searches to project source directories only.
    /// Populated exclusively from workspace.json JetBrains module sourceRoots
    /// (`workspace_json::load_source_paths`). LSP init `sourcePaths` is intentionally
    /// excluded — it is an additive indexing override for stubs/generated code, not a
    /// search-scope restriction. Auto-detected build-layout paths, Android SDK sources,
    /// and ~/.kotlin-lsp/sources are also excluded.
    pub(crate) workspace_source_roots: RwLock<Vec<String>>,
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
    /// Per-session cache for function signature lookups.
    /// Key: (fn_name, uri_string) → cached params text.
    /// Cleared on reindex to avoid stale results.
    pub(crate) sig_cache: DashMap<(String, String), Option<String>>,
}

impl crate::indexer::infer::InferDeps for Indexer {
    fn find_fun_params_text(&self, fn_name: &str, uri: &Url) -> Option<String> {
        find_fun_signature_full(fn_name, self, uri)
    }
    fn find_var_type(&self, var_name: &str, uri: &Url) -> Option<String> {
        infer_variable_type_raw(self, var_name, uri)
    }
    fn find_field_type(&self, class_name: &str, field_name: &str) -> Option<String> {
        if let Some(ty) = synthetic_enum_field(self, class_name, field_name) {
            return Some(ty);
        }
        crate::resolver::infer::find_field_type_in_class(self, class_name, field_name)
    }
    fn find_fun_return_type(&self, fn_name: &str) -> Option<String> {
        crate::resolver::infer::find_fun_return_type_by_name(self, fn_name)
    }
    fn find_class_type_params(&self, class_name: &str) -> Vec<String> {
        let Some(locations) = self.definitions.get(class_name) else {
            return Vec::new();
        };
        for loc in locations.iter() {
            if let Some(file_data) = self.files.get(loc.uri.as_str()) {
                if let Some(sym) = file_data
                    .symbols
                    .iter()
                    .find(|s| s.name == class_name && !s.type_params.is_empty())
                {
                    return sym.type_params.clone();
                }
            }
        }
        Vec::new()
    }
    fn find_method_return_type_for_type(
        &self,
        class_name: &str,
        method_name: &str,
    ) -> Option<String> {
        if let Some(ty) = synthetic_enum_method(self, class_name, method_name) {
            return Some(ty);
        }
        crate::resolver::infer::find_method_return_type(self, class_name, method_name)
    }
    fn find_method_params_text(&self, class_name: &str, method_name: &str) -> Option<String> {
        crate::indexer::infer::sig::find_method_params_in_class(self, class_name, method_name)
    }
    fn live_doc(&self, uri: &Url) -> Option<Arc<LiveDoc>> {
        self.live_doc(uri)
    }
}

// ─── Synthetic enum members ──────────────────────────────────────────────────
//
// Kotlin generates these on every enum class:
//   .entries  → EnumEntries<T>  (effectively List<T>)
//   .values() → Array<T>
//   .valueOf(String) → T
//   .name     → String  (instance)
//   .ordinal  → Int     (instance)

fn is_enum_class(indexer: &Indexer, class_name: &str) -> bool {
    let Some(locs) = indexer.definitions.get(class_name) else {
        return false;
    };
    for loc in locs.iter() {
        if let Some(fd) = indexer.files.get(loc.uri.as_str()) {
            if fd
                .symbols
                .iter()
                .any(|s| s.name == class_name && s.kind == SymbolKind::ENUM)
            {
                return true;
            }
        }
    }
    false
}

fn synthetic_enum_field(indexer: &Indexer, class_name: &str, field_name: &str) -> Option<String> {
    // Check name first to avoid expensive is_enum_class lookup for non-synthetic fields
    match field_name {
        "entries" | "name" | "ordinal" => {}
        _ => return None,
    }
    if !is_enum_class(indexer, class_name) {
        return None;
    }
    match field_name {
        "entries" => Some(format!("List<{class_name}>")),
        "name" => Some("String".to_string()),
        "ordinal" => Some("Int".to_string()),
        _ => None,
    }
}

fn synthetic_enum_method(indexer: &Indexer, class_name: &str, method_name: &str) -> Option<String> {
    match method_name {
        "values" | "valueOf" => {}
        _ => return None,
    }
    if !is_enum_class(indexer, class_name) {
        return None;
    }
    match method_name {
        "values" => Some(format!("Array<{class_name}>")),
        "valueOf" => Some(class_name.to_string()),
        _ => None,
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
            workspace_root: WorkspaceRoot::new(),
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
            workspace_source_roots: RwLock::new(Vec::new()),
            library_uris: DashSet::new(),
            importable_fqns: std::sync::RwLock::new(std::collections::HashMap::new()),
            live_trees: DashMap::new(),
            sig_cache: DashMap::new(),
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
        self.sig_cache.clear();
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

    pub(crate) fn definition_locations(&self, name: &str) -> Vec<Location> {
        self.definitions
            .get(name)
            .map(|locations| locations.clone())
            .unwrap_or_default()
    }

    /// Returns parsed file data for `uri`, or `None` if not yet indexed.
    pub(crate) fn file_data_for(&self, uri: &str) -> Option<Arc<FileData>> {
        self.files.get(uri).map(|r| Arc::clone(&*r))
    }

    /// Returns all known direct subtypes of `name` (empty if none).
    pub(crate) fn subtypes_of(&self, name: &str) -> Vec<Location> {
        self.subtypes
            .get(name)
            .map(|r| r.value().clone())
            .unwrap_or_default()
    }

    /// Calls `f(uri, file_data)` for every indexed file.
    /// Return `false` from the callback to stop iteration early.
    pub(crate) fn for_each_indexed_file(&self, mut f: impl FnMut(&str, &Arc<FileData>) -> bool) {
        for entry in self.files.iter() {
            if !f(entry.key(), entry.value()) {
                break;
            }
        }
    }

    pub(crate) fn is_library_uri(&self, uri: &Url) -> bool {
        self.library_uris.contains(uri.as_str())
    }

    /// Return `(effective_root, scoped_source_paths, matcher)` for an rg search
    /// whose context file is `open_file`.
    ///
    /// `effective_root` is derived via `effective_rg_root`: when `open_file` lives
    /// outside the configured workspace root, it walks up to the nearest `.git` root
    /// so rg searches the *actual* project of that file.
    ///
    /// `scoped_source_paths` is non-empty only when `effective_root` matches the
    /// configured workspace root — when the file belongs to a different project,
    /// workspace source roots don't apply and we fall back to a full-root search.
    ///
    /// Pass `None` for `open_file` to get workspace-level scope (no file context).
    pub(crate) fn rg_scope_for_path(
        &self,
        open_file: Option<&std::path::Path>,
    ) -> (
        Option<std::path::PathBuf>,
        Vec<String>,
        Option<Arc<crate::rg::IgnoreMatcher>>,
    ) {
        let workspace_root = self.workspace_root.get();
        let source_roots = self
            .workspace_source_roots
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let matcher = self
            .ignore_matcher
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let effective_root = crate::rg::effective_rg_root(workspace_root.as_deref(), open_file);

        // source_roots belong to the configured workspace — when rg switches to
        // an external project (effective_root != workspace_root), they must not
        // leak into the search.
        let scoped_source_roots = match (&effective_root, &workspace_root) {
            (Some(effective_root), Some(workspace_root)) if effective_root == workspace_root => {
                source_roots
            }
            _ => vec![],
        };

        (effective_root, scoped_source_roots, matcher)
    }

    pub(crate) fn remove_live_lines(&self, uri: &Url) {
        self.live_lines.remove(uri.as_str());
    }

    pub(crate) fn remove_indexed_file(&self, uri: &Url) {
        self.files.remove(uri.as_str());
    }

    // ─── completion helpers (methods on Indexer) ─────────────────────────────

    /// Ensures the file at `uri` is indexed, loading from disk if needed.
    /// Called on the completion hot-path before the debounced re-index finishes.
    pub(crate) fn ensure_indexed(&self, uri: &Url) {
        if !self.files.contains_key(uri.as_str()) {
            if let Ok(path) = uri.to_file_path() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    self.index_content(uri, &content);
                }
            }
        }
    }

    /// Uses `live_lines` (updated synchronously on every keystroke) for the
    /// current file's line text, falling back to indexed lines or disk.
    pub(crate) fn completions(
        &self,
        uri: &Url,
        position: Position,
        snippets: bool,
    ) -> (Vec<CompletionItem>, bool) {
        crate::features::completion::run_completions(self, uri, position, snippets)
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "indexer_tests.rs"]
mod tests;
