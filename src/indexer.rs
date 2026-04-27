use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use dashmap::{DashMap, DashSet};
use tower_lsp::lsp_types::*;

use crate::parser;
use crate::types::{FileData, SymbolEntry, FileIndexResult, WorkspaceIndexResult};

// Re-export rg-module items that existing callers reach via `crate::indexer::`.
pub(crate) use crate::rg::IgnoreMatcher;
pub use crate::rg::SOURCE_EXTENSIONS;

mod doc;
use doc::extract_doc_comment;

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

pub(super) mod cache;
pub(crate) use self::cache::workspace_cache_path;

mod discover;
use self::discover::find_source_files_unconstrained;

mod scan;
pub use self::scan::resolve_max_files;
pub const MAX_FILES_UNLIMITED: usize = usize::MAX;

mod apply;
pub(crate) use self::apply::{file_contributions, stale_keys_for, build_bare_names};
use self::apply::hash_str;

// Re-export cache/scan items needed by the inline test module below.
#[cfg(test)]
use self::cache::{FileCacheEntry, cache_entry_to_file_result};
#[cfg(test)]
#[allow(unused_imports)]
use crate::types::IndexStats;
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
            parse_tasks_completed: std::sync::atomic::AtomicUsize::new(0),
            parse_tasks_total: std::sync::atomic::AtomicUsize::new(0),
            scheduled_paths: DashMap::new(),
            workspace_pinned: std::sync::atomic::AtomicBool::new(false),
            last_scan_complete: std::sync::atomic::AtomicBool::new(false),
            ignore_matcher: RwLock::new(None),
            source_paths_raw: RwLock::new(Vec::new()),
            library_uris: DashSet::new(),
            importable_fqns: std::sync::RwLock::new(std::collections::HashMap::new()),
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

    /// LSP positions are UTF-16; for ASCII-heavy Kotlin/Java identifiers the
    /// character offset is identical to the UTF-16 unit offset.
    pub fn word_at(&self, uri: &Url, position: Position) -> Option<String> {
        self.word_and_qualifier_at(uri, position).map(|(w, _)| w)
    }

    /// Like `word_at` but also returns the `Range` of the word in LSP (UTF-16) coordinates.
    pub fn word_and_range_at(&self, uri: &Url, position: Position) -> Option<(String, Range)> {
        let lines = self.lines_for(uri)?;
        let line_text = lines.get(position.line as usize)?;
        let target_utf16 = position.character as usize;
        let mut utf16_acc = 0usize;
        let mut char_idx  = 0usize;
        for ch in line_text.chars() {
            if utf16_acc >= target_utf16 { break; }
            utf16_acc += ch.len_utf16();
            char_idx  += 1;
        }
        let chars: Vec<char> = line_text.chars().collect();
        let effective = if char_idx < chars.len() && is_id_char(chars[char_idx]) {
            char_idx
        } else if char_idx > 0 && is_id_char(chars[char_idx - 1]) {
            char_idx - 1
        } else {
            return None;
        };
        let start_char = (0..=effective).rev()
            .find(|&i| !is_id_char(chars[i])).map(|i| i + 1).unwrap_or(0);
        let end_char = (effective..chars.len())
            .find(|&i| !is_id_char(chars[i])).unwrap_or(chars.len());
        if start_char >= end_char { return None; }
        let word: String = chars[start_char..end_char].iter().collect();
        if word == "_" { return None; }
        // Compute UTF-16 columns for start and end.
        let start_utf16 = chars[..start_char].iter().map(|c| c.len_utf16() as u32).sum::<u32>();
        let end_utf16   = start_utf16 + chars[start_char..end_char].iter().map(|c| c.len_utf16() as u32).sum::<u32>();
        let range = Range {
            start: Position::new(position.line, start_utf16),
            end:   Position::new(position.line, end_utf16),
        };
        Some((word, range))
    }

    /// Returns a clone of the live (possibly unsaved) lines for a URI.
    pub fn lines_for(&self, uri: &Url) -> Option<Arc<Vec<String>>> {
        // Prefer live (unsaved) lines, fall back to indexed file.
        if let Some(live) = self.live_lines.get(uri.as_str()) {
            return Some(live.clone());
        }
        if let Some(f) = self.files.get(uri.as_str()) {
            return Some(f.lines.clone());
        }
        // File not indexed yet (cold start / indexing in progress) — read from disk
        // so that word_at / word_and_qualifier_at work and rg fallbacks can fire.
        if let Ok(path) = uri.to_file_path() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
                return Some(Arc::new(lines));
            }
        }
        None
    }

    /// Returns true if `name` has at least one definition location inside `uri`.
    pub fn is_declared_in(&self, uri: &Url, name: &str) -> bool {
        self.definitions.get(name)
            .map(|locs| locs.iter().any(|l| l.uri == *uri))
            .unwrap_or(false)
    }

    /// Like `word_at` but also returns the single dot-qualifier immediately
    /// preceding the word, if any.
    ///
    /// `AccountPickerMapper.Content`  cursor on `Content`
    ///   → `Some(("Content", Some("AccountPickerMapper")))`
    ///
    /// `List<StaticDocument>` cursor on `StaticDocument`
    ///   → `Some(("StaticDocument", None))`
    pub fn word_and_qualifier_at(
        &self,
        uri: &Url,
        position: Position,
    ) -> Option<(String, Option<String>)> {
        let lines = self.lines_for(uri)?;
        let line = lines.get(position.line as usize)?;

        // UTF-16 → char index
        let target_utf16 = position.character as usize;
        let mut utf16_acc = 0usize;
        let mut char_idx  = 0usize;
        for ch in line.chars() {
            if utf16_acc >= target_utf16 { break; }
            utf16_acc += ch.len_utf16();
            char_idx  += 1;
        }

        let chars: Vec<char> = line.chars().collect();
        let effective = if char_idx < chars.len() && is_id_char(chars[char_idx]) {
            char_idx
        } else if char_idx > 0 && is_id_char(chars[char_idx - 1]) {
            char_idx - 1
        } else {
            return None;
        };

        let start = (0..=effective)
            .rev()
            .find(|&i| !is_id_char(chars[i]))
            .map(|i| i + 1)
            .unwrap_or(0);

        let end = (effective..chars.len())
            .find(|&i| !is_id_char(chars[i]))
            .unwrap_or(chars.len());

        if start >= end { return None; }
        let word: String = chars[start..end].iter().collect();
        if word == "_" { return None; }

        // Scan back over the full dot-chain preceding the word.
        // `A.B.C.D` cursor on `D` → qualifier `"A.B.C"`, not just `"C"`.
        // `resolve_qualified` then uses the ROOT segment ("A") to locate the file
        // and searches that file for the word ("D"), handling arbitrary nesting depth.
        let qualifier = if start >= 2 && chars[start - 1] == '.' {
            let mut scan = start - 1; // pointing at the final dot
            while scan > 0 && (is_id_char(chars[scan - 1]) || chars[scan - 1] == '.') {
                scan -= 1;
            }
            let q: String = chars[scan..start - 1].iter().collect();
            let q = q.trim_start_matches('.').to_string();
            if !q.is_empty() && q != "_" { Some(q) } else { None }
        } else {
            // No dot-qualifier. Check if this looks like a named argument: `word = value`
            // (but NOT `word ==`). If so, scan backward for the enclosing call's name
            // and use that as the qualifier so we search the constructor/function's params.
            let after: String = chars[end..].iter().collect();
            let after_trimmed = after.trim_start();
            let is_named_arg = after_trimmed.starts_with('=')
                && !after_trimmed.starts_with("==");
            if is_named_arg {
                find_enclosing_call_name(&lines, position.line as usize, start)
                    .and_then(|callee| callee_to_qualifier(&callee))
            } else {
                None
            }
        };

        Some((word, qualifier))
    }

    /// Resolve definition locations for `name` (with optional dot-qualifier).
    #[allow(dead_code)]
    pub fn find_definition(&self, name: &str, from_uri: &Url) -> Vec<Location> {
        crate::resolver::resolve_symbol(self, name, None, from_uri)
    }

    pub fn find_definition_qualified(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
    ) -> Vec<Location> {
        crate::resolver::resolve_symbol(self, name, qualifier, from_uri)
    }

    /// If `name` at `position` is `it` or a named lambda parameter, return the
    /// inferred element/receiver type name (e.g. `"Product"`, `"User"`).
    ///
    /// Used by hover and go-to-definition to provide useful info for lambda params.
    /// Handles both same-line and multi-line lambda declarations by scanning
    /// backward through file lines (not just the text before the cursor).
    pub fn infer_lambda_param_type_at(
        &self,
        name:     &str,
        uri:      &Url,
        position: Position,
    ) -> Option<String> {
        let line_no = position.line as usize;

        // Prefer live_lines (current editor content, updated synchronously on
        // did_change) over files.lines (refreshed after debounced reindex).
        // Type resolution still uses the index (definitions, files) by name —
        // that data remains valid even before reindex completes.
        let lines: Arc<Vec<String>> = self.live_lines.get(uri.as_str())
            .map(|ll| ll.clone())
            .or_else(|| {
                self.files.get(uri.as_str()).map(|f| f.lines.clone())
            })?;

        if name == "it" || name == "this" {
            let col = position.character as usize;
            let lambda_type = if name == "this" {
                find_this_element_type_in_lines(&lines, line_no, col, self, uri)
            } else {
                find_it_element_type_in_lines(&lines, line_no, col, self, uri)
            };
            if lambda_type.is_some() { return lambda_type; }
            // Type-directed fallback: if `it`/`this` is a call argument (named or
            // positional), look up the expected parameter type from the function signature.
            // Mimics Kotlin's type-directed implicit-receiver / lambda-param resolution.
            if let Some(ty) = find_as_call_arg_type(&lines, line_no, col, self, uri) {
                return Some(ty);
            }
            // Fallback for `this` in a regular class method body (not a lambda):
            // scan backward for the enclosing class/object declaration.
            if name == "this" {
                return self.enclosing_class_at(uri, position.line);
            }
            None
        } else {
            // For named params: scan backward for `{ name ->` pattern.
            // Also check the CURRENT line (needed when cursor is ON the param
            // at its declaration line, before `->` — before_cursor wouldn't
            // contain the arrow).
            find_named_lambda_param_type_in_lines(&lines, name, line_no, self, uri)
        }
    }

    /// Lambda parameter names that are **in scope** at `(cursor_line, cursor_col)`.
    ///
    /// Uses the same brace-depth backward-scan algorithm as
    /// `find_it_element_type_in_lines`: `}` increments depth, `{` decrements;
    /// when depth < 0 we've found an *enclosing* `{` lambda.  Sibling/inner lambdas
    /// whose closing `}` appears before their `{` in the backward scan self-balance
    /// and never trigger depth < 0, so they are correctly excluded.
    ///
    /// Example — cursor inside `{ resultState -> … }`:
    ///   `reloadableProduct(…, { isRefresh -> … }) { resultState -> │ }`
    ///   → returns `["resultState"]`,  NOT `["isRefresh", "resultState"]`
    pub fn lambda_params_at(&self, uri: &Url, cursor_line: usize) -> Vec<String> {
        self.lambda_params_at_col(uri, cursor_line, usize::MAX)
    }

    /// Like `lambda_params_at` but also respects `cursor_col` when scanning the
    /// cursor line.  Passing `usize::MAX` is equivalent to `lambda_params_at`.
    ///
    /// The column limit prevents the closing `}` of an inline lambda from being
    /// seen when the cursor is inside that lambda on the same line:
    ///   `loan = { loanId, isWustenrot -> setEvent(...) },`
    ///                                                  ^ cursor here
    /// Without the limit, the scan hits `}` first (depth→1), then `{` resets to 0
    /// (not <0), so the lambda params are never collected.
    pub fn lambda_params_at_col(&self, uri: &Url, cursor_line: usize, cursor_col: usize) -> Vec<String> {
        let lines = self.live_lines.get(uri.as_str())
            .map(|ll| ll.clone())
            .or_else(|| self.files.get(uri.as_str()).map(|f| f.lines.clone()))
            .unwrap_or_default();

        let mut params: Vec<String> = Vec::new();
        let mut depth: i32 = 0;
        let scan_start = cursor_line.saturating_sub(50);

        for ln in (scan_start..=cursor_line).rev() {
            let line = match lines.get(ln) { Some(l) => l, None => continue };
            // On the cursor line only consider chars up to the cursor column so
            // that the closing `}` of an inline lambda (which comes AFTER the
            // cursor) does not inflate the depth counter.
            let scan_line: &str = if ln == cursor_line && cursor_col < line.len() {
                // cursor_col is a UTF-16 character offset; find the byte boundary.
                let mut utf16 = 0usize;
                let mut byte_end = line.len();
                for (bi, ch) in line.char_indices() {
                    if utf16 >= cursor_col { byte_end = bi; break; }
                    utf16 += ch.len_utf16();
                }
                &line[..byte_end]
            } else {
                line
            };
            for (bi, ch) in scan_line.char_indices().rev() {
                match ch {
                    '}' => depth += 1,
                    '{' => {
                        depth -= 1;
                        if depth < 0 {
                            // Skip string interpolation.
                            if line[..bi].ends_with('$') { depth = 0; continue; }
                            // Check for named params `{ a, b -> }`.
                            let after = line[bi + 1..].trim_start();
                            if let Some(arrow_pos) = after.find("->") {
                                let names_str = &after[..arrow_pos];
                                for tok in names_str.split(',') {
                                    let name: String = tok.trim()
                                        .chars().take_while(|&c| c.is_alphanumeric() || c == '_')
                                        .collect();
                                    if !name.is_empty() && name != "it" && name != "_"
                                        && name.chars().next().map(|c| c.is_lowercase()).unwrap_or(false)
                                    {
                                        if !params.contains(&name) { params.push(name.clone()); }
                                    }
                                }
                            }
                            // Reset so outer lambdas can also be found.
                            depth = 0;
                        }
                    }
                    _ => {}
                }
            }
        }
        params
    }

    /// Find the `{ name ->` declaration line for a lambda parameter in scope at
    /// `cursor_line`.  Returns a `Location` pointing to the opening `{` of the
    /// enclosing lambda (the parameter's declaration site).
    pub fn find_lambda_param_decl(&self, uri: &Url, param_name: &str, cursor_line: usize) -> Option<Location> {
        let lines = self.live_lines.get(uri.as_str())
            .map(|ll| ll.clone())
            .or_else(|| self.files.get(uri.as_str()).map(|f| f.lines.clone()))?;

        let scan_start = cursor_line.saturating_sub(50);
        for ln in (scan_start..=cursor_line).rev() {
            let line = match lines.get(ln) { Some(l) => l, None => continue };
            if !line_has_lambda_param(line, param_name) { continue; }
            if let Some(brace_pos) = lambda_brace_pos_for_param(line, param_name) {
                let char_col = line[..brace_pos].chars().count() as u32;
                return Some(Location {
                    uri: uri.clone(),
                    range: tower_lsp::lsp_types::Range {
                        start: tower_lsp::lsp_types::Position { line: ln as u32, character: char_col },
                        end:   tower_lsp::lsp_types::Position { line: ln as u32, character: char_col + 1 },
                    },
                });
            }
        }
        None
    }

    ///
    /// Uses `live_lines` (updated synchronously on every keystroke) for the
    /// current file's line text, falling back to indexed lines or disk.
    pub fn completions(&self, uri: &Url, position: Position, snippets: bool) -> (Vec<CompletionItem>, bool) {
        // Ensure the file is indexed — on first open, did_open's spawn_blocking
        // may not have finished by the time the first completion request arrives.
        if !self.files.contains_key(uri.as_str()) {
            if let Ok(path) = uri.to_file_path() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    self.index_content(uri, &content);
                }
            }
        }

        // Use live_lines (no debounce delay) for the current line so dot-detection
        // is always accurate even when the user types faster than the 120ms debounce.
        let line_owned: String;
        let line: &str = if let Some(ll) = self.live_lines.get(uri.as_str()) {
            if let Some(l) = ll.get(position.line as usize) {
                line_owned = l.clone();
                &line_owned
            } else { return (vec![], false); }
        } else if let Some(data) = self.files.get(uri.as_str()) {
            if let Some(l) = data.lines.get(position.line as usize) {
                line_owned = l.clone();
                &line_owned
            } else { return (vec![], false); }
        } else { return (vec![], false); };

        // Slice line up to cursor (UTF-16 aware).
        let target = position.character as usize;
        let mut utf16 = 0usize;
        let mut byte_end = line.len();
        for (bi, ch) in line.char_indices() {
            if utf16 >= target { byte_end = bi; break; }
            utf16 += ch.len_utf16();
        }
        let before = &line[..byte_end];

        // Extract the identifier fragment the user is currently typing.
        let prefix: String = before.chars()
            .rev()
            .take_while(|&c| c.is_alphanumeric() || c == '_')
            .collect::<String>()
            .chars()
            .rev()
            .collect();

        // Check if the char before the prefix is a dot.
        let before_prefix = &before[..before.len() - prefix.len()];

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
        // (compute below, then store in cache at the end)
        let dot_receiver = if before_prefix.ends_with('.') {
            // Grab the expression immediately preceding the dot.
            // For simple identifiers: `foo.` → "foo"
            // For qualified nested types: `DPSCoordinator.Kind.` → "DPSCoordinator.Kind"
            // We walk backwards collecting identifier chars; if we then see a dot followed
            // by another identifier, include that outer segment too (one level only).
            let before_dot = &before_prefix[..before_prefix.len() - 1];
            let inner: String = before_dot
                .chars()
                .rev()
                .take_while(|&c| c.is_alphanumeric() || c == '_')
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if inner.is_empty() {
                None
            } else {
                // Check whether there is an `Outer.` prefix immediately before `inner`.
                let remaining = &before_dot[..before_dot.len() - inner.len()];
                if remaining.ends_with('.')
                    && inner.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
                {
                    let outer: String = remaining[..remaining.len() - 1]
                        .chars()
                        .rev()
                        .take_while(|&c| c.is_alphanumeric() || c == '_')
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect();
                    if !outer.is_empty()
                        && outer.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
                    {
                        Some(format!("{}.{}", outer, inner))
                    } else {
                        Some(inner)
                    }
                } else {
                    Some(inner)
                }
            }
        } else {
            None
        };

        // `this.` / `it.` / named-param dot-completion.
        // `this` can mean: (a) scope-function receiver, (b) enclosing class.
        // `it` means: implicit lambda param.
        // Named lambda params are detected via `is_lambda_param`.
        if let Some(ref recv) = dot_receiver {
            if recv == "it" || recv == "this"
                || is_lambda_param(recv, before, self, uri, position.line as usize)
            {
                let cursor_line = position.line as usize;
                let cursor_col  = before.chars().count();
                let elem_type = if recv == "it" || recv == "this" {
                    // Try single-line first (fast path: `obj.run { this. }` on same line).
                    let t = find_it_element_type(before, self, uri);
                    if t.is_some() && recv == "it" {
                        t
                    } else {
                        // Multi-line fallback: lambda opened on a previous line.
                        let lines = self.live_lines.get(uri.as_str())
                            .map(|ll| ll.clone())
                            .or_else(|| self.files.get(uri.as_str()).map(|f| f.lines.clone()));
                        let ml = lines.and_then(|ls| {
                            if recv == "this" {
                                find_this_element_type_in_lines(&ls, cursor_line, cursor_col, self, uri)
                            } else {
                                find_it_element_type_in_lines(&ls, cursor_line, cursor_col, self, uri)
                            }
                        });
                        if ml.is_some() {
                            ml
                        } else if recv == "this" {
                            self.enclosing_class_at(uri, position.line)
                        } else {
                            None
                        }
                    }
                } else {
                    find_named_lambda_param_type(before, recv, self, uri, position.line as usize)
                };
                if let Some(elem_type) = elem_type {
                    let (items, _) = crate::resolver::complete_symbol(self, &prefix, Some(&elem_type), uri, snippets);
                    if items.is_empty() {
                        // Type name known (e.g. generic param `T`, `StateType`) but not
                        // indexed — show a single hint item so the user sees the inferred type.
                        use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind};
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

        let (mut items, hit_cap) = {
            // Detect @AnnotationName context — suppress functions/variables.
            let annotation_only = dot_receiver.is_none()
                && crate::resolver::is_annotation_context(before, &prefix);
            crate::resolver::complete_symbol_with_context(
                self, &prefix, dot_receiver.as_deref(), uri, snippets, annotation_only,
            )
        };

        // Add scope-aware lambda parameter names (bare-word completion only).
        if dot_receiver.is_none() {
            let prefix_lower = prefix.to_lowercase();
            for param in self.lambda_params_at(uri, position.line as usize) {
                if param.to_lowercase().starts_with(&prefix_lower) {
                    use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind};
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

        // Store in last_completion cache.
        if let Ok(mut guard) = self.last_completion.lock() {
            *guard = Some((cache_key, prefix.clone(), items.clone()));
        }

        (items, hit_cap)
    }

    /// Build a Markdown hover snippet for a symbol name.
    pub fn hover_info(&self, name: &str) -> Option<String> {
        // Check stdlib first so well-known symbols (run, apply, map, …) get
        // proper signatures even when no project source contains them.
        if let Some(md) = crate::stdlib::hover(name) { return Some(md); }

        // Drop the dashmap ref before taking the second one.
        let loc: Location = {
            let r = self.definitions.get(name)?;
            r.first()?.clone()
        };
        self.hover_info_at_location(&loc, name)
    }

    /// Build hover markdown for `name` at a specific resolved `Location`.
    /// Used by the hover handler so it shows the same symbol as go-to-definition.
    pub fn hover_info_at_location(&self, loc: &Location, name: &str) -> Option<String> {
        // On-demand index: the file may have been found by rg but not yet indexed.
        if !self.files.contains_key(loc.uri.as_str()) {
            if let Ok(path) = loc.uri.to_file_path() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    self.index_content(&loc.uri, &content);
                }
            }
        }
        let data = self.files.get(loc.uri.as_str())?;
        // Prefer exact match by resolved location range; fall back to name match
        // for symbols found via rg where the range may not align exactly.
        let sym = data.symbols.iter().find(|s| s.selection_range == loc.range)
            .or_else(|| data.symbols.iter().find(|s| s.name == name))?;

        let start_line = sym.selection_range.start.line as usize;
        let sig = collect_signature(&data.lines, start_line);

        let lang = if loc.uri.path().ends_with(".kt") { "kotlin" }
                   else if loc.uri.path().ends_with(".swift") { "swift" }
                   else { "java" };

        let code_block = if sig.is_empty() {
            format!("```{}\n{} {}\n```", lang, symbol_kw_for_lang(sym.kind, lang), name)
        } else {
            format!("```{}\n{}\n```", lang, sig)
        };

        // Prepend KDoc / Javadoc comment if one immediately precedes the declaration.
        if let Some(doc) = extract_doc_comment(&data.lines, start_line) {
            Some(format!("{doc}\n\n---\n\n{code_block}"))
        } else {
            Some(code_block)
        }
    }

    /// All symbols declared in the given file (for `documentSymbol`).
    pub fn file_symbols(&self, uri: &Url) -> Vec<SymbolEntry> {
        self.files
            .get(uri.as_str())
            .map(|d| d.symbols.clone())
            .unwrap_or_default()
    }

    /// Find the name of the innermost enclosing class/interface/object
    /// that contains `row` in the given file.
    ///
    /// Used by `references` to scope a short symbol name (e.g. `Loading`) to
    /// its parent sealed class so we can filter out unrelated `Loading` classes
    /// in other sealed hierarchies.
    pub fn enclosing_class_at(&self, uri: &Url, row: u32) -> Option<String> {
        let file = self.files.get(uri.as_str())?;
        let row = row as usize;

        // Walk backward tracking brace depth to find the innermost class/object
        // declaration that encloses the cursor.  We ignore indentation because
        // the cursor may sit on an import or top-level line (indent 0) where the
        // indentation heuristic always returns None.
        let mut depth = 0i32;
        let end = row.min(file.lines.len().saturating_sub(1));
        for i in (0..=end).rev() {
            let line = match file.lines.get(i) { Some(l) => l, None => continue };
            // Count braces right-to-left on this line.
            for ch in line.chars().rev() {
                match ch {
                    '}' => depth += 1,
                    '{' => depth -= 1,
                    _ => {}
                }
            }
            // depth < 0: we just crossed the opening brace of a scope that
            // starts on this line and isn't closed before the cursor.
            // The cursor line itself is excluded (it can't enclose itself).
            if depth < 0 && i < row {
                let t = line.trim();
                if let Some(name) = extract_class_decl_name(t) {
                    return Some(name);
                }
                // The `{` may be on a different line than the `class` keyword, e.g.:
                //   line N:   class Foo @Inject constructor(
                //   ...params...
                //   line i:   ) : Bar() {
                // Scan up to 15 lines backward from `i` to find the class keyword.
                let scan_up = i.saturating_sub(15);
                for j in (scan_up..i).rev() {
                    if let Some(prev) = file.lines.get(j) {
                        if let Some(name) = extract_class_decl_name(prev.trim()) {
                            return Some(name);
                        }
                        // Stop if we hit a line that is clearly a different scope
                        // (another `}` or a function/lambda body closer).
                        let pt = prev.trim();
                        if pt.starts_with('}') || pt.ends_with('}') { break; }
                    }
                }
                // Opening brace belongs to a function/lambda — keep searching.
                depth = 0;
            }
        }
        None
    }

    /// Return the package declared in the given file, if any.
    pub fn package_of(&self, uri: &Url) -> Option<String> {
        self.files.get(uri.as_str())?.package.clone()
    }

    /// Return the package in which `name` is declared, by looking up its
    /// definition locations and reading the `package` field of those files.
    /// If `prefer_uri` is set, prefer definitions from that file first.
    pub fn declared_package_of(&self, name: &str) -> Option<String> {
        let locs = self.definitions.get(name)?;
        for loc in locs.iter() {
            if let Some(f) = self.files.get(loc.uri.as_str()) {
                if let Some(pkg) = &f.package {
                    return Some(pkg.clone());
                }
            }
        }
        None
    }

    /// If `name` is declared as an inner/nested class, return the name of its
    /// enclosing class at the declaration site in `preferred_uri` (if found there),
    /// otherwise the first definition site.
    pub fn declared_parent_class_of(&self, name: &str, preferred_uri: &Url) -> Option<String> {
        let locs = self.definitions.get(name)?;
        // Try declaration in the preferred (current) file first.
        for loc in locs.iter() {
            if loc.uri == *preferred_uri {
                return self.enclosing_class_at(&loc.uri, loc.range.start.line);
            }
        }
        // Fall back to first definition in any file.
        for loc in locs.iter() {
            if let Some(parent) = self.enclosing_class_at(&loc.uri, loc.range.start.line) {
                return Some(parent);
            }
        }
        None
    }

    /// Scan imports in `uri` for `name` and return (parent_class, declared_pkg)
    /// as resolved from the import statement.  E.g.:
    ///   `import com.example.DashboardViewModel.Effect`
    ///   → parent_class = Some("DashboardViewModel"), pkg = Some("com.example.DashboardViewModel")
    pub fn resolve_symbol_via_import(
        &self,
        uri: &Url,
        name: &str,
    ) -> (Option<String>, Option<String>) {
        let file = match self.files.get(uri.as_str()) {
            Some(f) => f,
            None    => return (None, None),
        };
        for line in file.lines.iter() {
            let t = line.trim();
            if !t.starts_with("import ") { continue; }
            // Handle `import a.b.c.Name` and `import a.b.c.Name as Alias`
            let import_path = t["import ".len()..].split_whitespace().next().unwrap_or("");
            let segments: Vec<&str> = import_path.split('.').collect();
            // Last segment should match `name` (or be `*`).
            let last = *segments.last().unwrap_or(&"");
            if last != name && last != "*" { continue; }

            // Found a matching import. The declared package is everything up to (not incl.) `name`.
            // The parent class is the segment immediately before `name` if it starts uppercase.
            if last == name && segments.len() >= 2 {
                let pkg = segments[..segments.len() - 1].join(".");
                let parent = segments.get(segments.len() - 2)
                    .filter(|s| s.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
                    .map(|s| s.to_string());
                return (parent, Some(pkg));
            }
        }
        (None, None)
    }
}

/// If `line` is a class/interface/object/sealed declaration, return the type name.
fn extract_class_decl_name(line: &str) -> Option<String> {
    // Strip common modifiers: Kotlin + Java + Swift
    let mut rest = line;
    let modifiers = [
        "abstract ", "sealed ", "data ", "open ", "inner ", "private ",
        "protected ", "public ", "internal ", "inline ", "value ", "enum ",
        "companion ", "override ", "final ",
        // Swift-specific
        "fileprivate ", "@objc ", "static ", "final ",
    ];
    loop {
        let before = rest;
        for m in &modifiers { rest = rest.strip_prefix(m).unwrap_or(rest).trim_start(); }
        // Skip @Annotations (Kotlin) and @attributes (Swift)
        if rest.starts_with('@') {
            if let Some(after) = rest.find(' ') { rest = rest[after..].trim_start(); }
        }
        if rest == before { break; }
    }
    // Now rest should start with a type keyword
    let rest = rest.strip_prefix("class ")
        .or_else(|| rest.strip_prefix("interface "))
        .or_else(|| rest.strip_prefix("object "))
        .or_else(|| rest.strip_prefix("struct "))
        .or_else(|| rest.strip_prefix("protocol "))
        .or_else(|| rest.strip_prefix("extension "))?;
    // Extract the identifier
    let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    if name.is_empty() || !name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
        return None;
    }
    Some(name)
}

// ─── rg cross-file fallback ──────────────────────────────────────────────────

// ─── misc helpers ────────────────────────────────────────────────────────────

pub(crate) fn is_id_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Scan backward from `(line_no, col)` — where `col` is the START of the cursor
/// word — to find the name of the enclosing function/constructor call.
///
/// Used to resolve named arguments: `User(name = "Alice")` with cursor on `name`
/// → scan back past the `(` → return `"User"`.
///
/// Returns the FULL dotted callee name (e.g. `"BottomSheetState.empty"`, `"User"`).
/// The caller converts this to a qualifier via `callee_to_qualifier`.
///
/// Scans at most 20 lines backward to avoid runaway on deeply nested expressions.
/// Tracks `()` and `[]` depth; lambda `{}` bodies are transparent (their inner
/// `()` still balance) so we don't need special-case brace handling.
pub(crate) fn find_enclosing_call_name(lines: &[String], line_no: usize, col: usize) -> Option<String> {
    let mut depth: i32 = 0;
    let scan_range_start = line_no.saturating_sub(20);

    for ln in (scan_range_start..=line_no).rev() {
        let line_chars: Vec<char> = lines[ln].chars().collect();
        let scan_to = if ln == line_no { col } else { line_chars.len() };

        for i in (0..scan_to).rev() {
            match line_chars[i] {
                ')' | ']' => depth += 1,
                '(' | '[' => {
                    depth -= 1;
                    if depth < 0 {
                        // This `(` opened the call we're inside.
                        if i == 0 { return None; }
                        // Extract the identifier (possibly dotted) just before `(`.
                        let mut end = i;
                        while end > 0 && (is_id_char(line_chars[end - 1]) || line_chars[end - 1] == '.') {
                            end -= 1;
                        }
                        if end >= i { return None; }
                        let name: String = line_chars[end..i].iter().collect();
                        let name = name.trim_matches('.').to_string();
                        return if name.is_empty() { None } else { Some(name) };
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Convert a raw callee name (from `find_enclosing_call_name`) to the qualifier
/// to use when resolving a named argument parameter.
///
/// Rules:
/// - Last segment uppercase → constructor call, qualifier = last segment.
///   `"User"` → `"User"`, `"com.example.User"` → `"User"`
/// - Last segment lowercase (method call) → look for the rightmost uppercase
///   segment in the receiver chain as the owner type.
///   `"BottomSheetState.empty"` → `"BottomSheetState"`
///   `"SomeClass.companion.build"` → `"SomeClass"` (last uppercase before method)
/// - Pure lowercase, no uppercase anywhere → `None` (can't resolve statically).
fn callee_to_qualifier(full_callee: &str) -> Option<String> {
    let segments: Vec<&str> = full_callee.split('.').collect();
    let last = *segments.last()?;

    // Constructor call: last segment is a type name (uppercase first char).
    if last.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
        return Some(last.to_string());
    }

    // Method call: find rightmost uppercase segment in the receiver chain.
    // `BottomSheetState.empty` → segments[..-1] = ["BottomSheetState"] → "BottomSheetState"
    // `viewModel.state.copy`   → no uppercase in receiver → None
    let receiver = &segments[..segments.len() - 1];
    receiver.iter().rev()
        .find(|s| s.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
        .map(|s| s.to_string())
}

fn symbol_kw(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::CLASS          => "class",
        SymbolKind::INTERFACE      => "interface",
        SymbolKind::FUNCTION       => "fun",
        SymbolKind::METHOD         => "fun",
        SymbolKind::VARIABLE       => "var",
        SymbolKind::CONSTANT       => "val",
        SymbolKind::OBJECT         => "object",
        SymbolKind::TYPE_PARAMETER => "typealias",
        SymbolKind::ENUM           => "enum class",
        SymbolKind::FIELD          => "field",
        _                          => "symbol",
    }
}

fn symbol_kw_for_lang(kind: SymbolKind, lang: &str) -> &'static str {
    let kw = symbol_kw(kind);
    // Swift uses `func`, not `fun`.
    if lang == "swift" && kw == "fun" { "func" } else { kw }
}

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

    // ── Swift hover uses "func" not "fun" ────────────────────────────────────

    #[test]
    fn swift_hover_uses_func_keyword() {
        // Swift function with no signature detail should show "func", not "fun".
        let src = "func greet() {}";
        let (u, idx) = indexed("/Greeting.swift", src);
        let hover = idx.hover_info_at_location(
            &Location { uri: u.clone(), range: Default::default() },
            "greet",
        ).unwrap_or_default();
        assert!(
            hover.contains("func"),
            "Swift hover should say 'func', got: {hover}"
        );
        assert!(
            !hover.contains("```kotlin\nfun ") && !hover.contains("```swift\nfun "),
            "Swift hover must not emit 'fun', got: {hover}"
        );
    }

    #[test]
    fn kotlin_hover_still_uses_fun_keyword() {
        let src = "fun greet() {}";
        let (u, idx) = indexed("/Greeting.kt", src);
        let hover = idx.hover_info_at_location(
            &Location { uri: u.clone(), range: Default::default() },
            "greet",
        ).unwrap_or_default();
        assert!(
            hover.contains("fun"),
            "Kotlin hover should say 'fun', got: {hover}"
        );
    }

    // ── word_at ──────────────────────────────────────────────────────────────

    #[test]
    fn word_at_middle() {
        let (u, idx) = indexed("/t.kt", "val foo = 1");
        assert_eq!(idx.word_at(&u, Position::new(0, 5)), Some("foo".into()));
    }

    #[test]
    fn word_at_end_of_word() {
        let (u, idx) = indexed("/t.kt", "val foo = 1");
        // character=7 is just past the 'o'; should still return "foo"
        assert_eq!(idx.word_at(&u, Position::new(0, 7)), Some("foo".into()));
    }

    #[test]
    fn word_at_operator_returns_none() {
        let (u, idx) = indexed("/t.kt", "val foo = 1");
        assert_eq!(idx.word_at(&u, Position::new(0, 8)), None); // '='
    }

    #[test]
    fn word_at_angle_bracket_steps_back_to_word() {
        let (u, idx) = indexed("/t.kt", "List<String>");
        // '<' at position 4 is not an id char, but 't' at position 3 is.
        // word_at steps back and returns the word ending there.
        assert_eq!(idx.word_at(&u, Position::new(0, 4)), Some("List".into()));
    }

    #[test]
    fn word_at_space_between_operator_and_number_returns_none() {
        // "val foo = 1"
        //  0123456789A
        // pos 9 = ' ' between '=' and '1'; prev char[8]='=' is also non-ident → None
        let (u, idx) = indexed("/t.kt", "val foo = 1");
        assert_eq!(idx.word_at(&u, Position::new(0, 9)), None);
    }

    // ── word_and_qualifier_at ────────────────────────────────────────────────

    #[test]
    fn qualifier_none_plain_word() {
        let (u, idx) = indexed("/t.kt", "val x: Bar = y");
        // cursor on 'B' of 'Bar'
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 7)),
            Some(("Bar".into(), None))
        );
    }

    #[test]
    fn qualifier_dot_access() {
        let (u, idx) = indexed("/t.kt", "val x = Outer.Inner");
        // cursor on 'I' of 'Inner'
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 14)),
            Some(("Inner".into(), Some("Outer".into())))
        );
    }

    #[test]
    fn qualifier_in_type_param() {
        let (u, idx) = indexed("/t.kt", "val x: List<Outer.Content>");
        // cursor on 'C' of 'Content' (position 18) — full chain captured
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 18)),
            Some(("Content".into(), Some("Outer".into())))
        );
    }

    #[test]
    fn qualifier_full_chain() {
        // A.B.C with cursor on C → full chain "A.B" not just "B"
        let (u, idx) = indexed("/t.kt", "A.B.C");
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 4)),
            Some(("C".into(), Some("A.B".into())))
        );
    }

    #[test]
    fn qualifier_deep_chain() {
        // A.B.C.D.E cursor on E → qualifier is the full "A.B.C.D"
        let (u, idx) = indexed("/t.kt", "A.B.C.D.E");
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 8)),
            Some(("E".into(), Some("A.B.C.D".into())))
        );
    }

    // ── named argument detection ──────────────────────────────────────────────

    #[test]
    fn named_arg_simple_constructor() {
        // `User(name = "Alice")` cursor on `name` → qualifier should be "User"
        let (u, idx) = indexed("/t.kt", "User(name = \"Alice\")");
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 5)),
            Some(("name".into(), Some("User".into())))
        );
    }

    #[test]
    fn named_arg_not_equality() {
        // `if (x == foo)` — `==` is NOT a named arg
        let (u, idx) = indexed("/t.kt", "val r = x == foo");
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 9)),
            Some(("x".into(), None))  // plain word, no qualifier
        );
    }

    #[test]
    fn named_arg_assignment_not_arg() {
        // `val x = y` — `=` after a `val` binding is NOT a named arg (not inside a call)
        let (u, idx) = indexed("/t.kt", "val x = someValue");
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 4)),
            Some(("x".into(), None))  // no enclosing `(` → no qualifier
        );
    }

    #[test]
    fn named_arg_multiline_ctor() {
        // Constructor split across lines:
        //   User(
        //       name = "Alice",  ← cursor on name (col 4)
        //   )
        let src = "User(\n    name = \"Alice\",\n)";
        let (u, idx) = indexed("/t.kt", src);
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(1, 4)),
            Some(("name".into(), Some("User".into())))
        );
    }

    #[test]
    fn named_arg_method_with_uppercase_receiver() {
        // BottomSheetState.empty(onBottomSheetClose = handler)
        // → qualifier should be "BottomSheetState" (the receiver type)
        let src = "BottomSheetState.empty(onBottomSheetClose = handler)";
        let (u, idx) = indexed("/t.kt", src);
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 23)),
            Some(("onBottomSheetClose".into(), Some("BottomSheetState".into())))
        );
    }

    #[test]
    fn named_arg_fully_qualified_ctor() {
        // com.example.User(name = "Alice") → qualifier "User" (last uppercase segment)
        let src = "com.example.User(name = \"Alice\")";
        let (u, idx) = indexed("/t.kt", src);
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 17)),
            Some(("name".into(), Some("User".into())))
        );
    }

    #[test]
    fn named_arg_lowercase_method_no_receiver() {
        // someFunction(param = value) — pure lowercase, no type info → None qualifier
        let src = "someFunction(param = value)";
        let (u, idx) = indexed("/t.kt", src);
        // qualifier should be None (we can't resolve this without type inference)
        let result = idx.word_and_qualifier_at(&u, Position::new(0, 13));
        assert_eq!(result.as_ref().map(|(w, _)| w.as_str()), Some("param"));
        assert_eq!(result.as_ref().and_then(|(_, q)| q.as_deref()), None);
    }

    #[test]
    fn named_arg_state_multiline_with_method_receiver() {
        // Simulates the real-world pattern:
        //   State(
        //     sheetState = BottomSheetState.empty(SheetType.Empty, onBottomSheetClose = cb),
        //     ...                                                   ^^^^^^^^^^^^^^^^^^
        let src = "State(\n  sheetState = BottomSheetState.empty(SheetType.Empty, onBottomSheetClose = cb),\n)";
        let (u, idx) = indexed("/t.kt", src);
        // cursor on onBottomSheetClose (inside the inner .empty() call on line 1)
        let line1 = &src.lines().collect::<Vec<_>>()[1];
        let col = line1.find("onBottomSheetClose").unwrap() as u32;
        let result = idx.word_and_qualifier_at(&u, Position::new(1, col));
        assert_eq!(result.as_ref().map(|(w, _)| w.as_str()), Some("onBottomSheetClose"));
        assert_eq!(result.as_ref().and_then(|(_, q)| q.as_deref()), Some("BottomSheetState"));
    }

    // ── it-completion ─────────────────────────────────────────────────────────

    #[test]
    fn it_element_type_list() {
        // val items: List<Product>
        // items.forEach { it.  ← element type should be "Product"
        let src = "val items: List<Product> = emptyList()";
        let (u, idx) = indexed("/t.kt", src);
        let before = "items.forEach { it.";
        let result = find_it_element_type(before, &idx, &u);
        assert_eq!(result.as_deref(), Some("Product"));
    }

    #[test]
    fn it_element_type_flow() {
        let src = "val events: Flow<Event> = emptyFlow()";
        let (u, idx) = indexed("/t.kt", src);
        let before = "events.collect { it.";
        assert_eq!(find_it_element_type(before, &idx, &u).as_deref(), Some("Event"));
    }

    #[test]
    fn it_element_type_state_flow() {
        let src = "    private val _state: StateFlow<UiState>";
        let (u, idx) = indexed("/t.kt", src);
        let before = "_state.value.let { it."; // `value` is lowercase → chain, falls back
        // _state itself is StateFlow, but we ask about `value` which isn't typed here.
        // Just ensure no panic.
        let _ = find_it_element_type(before, &idx, &u);
    }

    #[test]
    fn it_scope_fn_let() {
        // val user: User — `user.let { it.` — it IS the User type
        let src = "val user: User = User()";
        let (u, idx) = indexed("/t.kt", src);
        let before = "user.let { it.";
        // User is not a collection, so returns the base type directly
        assert_eq!(find_it_element_type(before, &idx, &u).as_deref(), Some("User"));
    }

    #[test]
    fn it_element_type_nullable_call() {
        // val user: User? — `user?.let { it.`
        let src = "val user: User? = null";
        let (u, idx) = indexed("/t.kt", src);
        let before = "user?.let { it.";
        // `?` in `?.` is normalised away — should still find "User"
        // `infer_type_in_lines_raw` for `user: User?` → "User" (? stripped at type boundary)
        let result = find_it_element_type(before, &idx, &u);
        assert_eq!(result.as_deref(), Some("User"));
    }

    #[test]
    fn it_element_type_with_call_args() {
        // items.map(transform) { it.  → strip `(transform)` first
        let src = "val items: List<Order> = emptyList()";
        let (u, idx) = indexed("/t.kt", src);
        let before = "items.mapNotNull(::transform) { it.";
        // strip `(::transform)` → callee = `items.mapNotNull` → receiver = `items` → List<Order>
        assert_eq!(find_it_element_type(before, &idx, &u).as_deref(), Some("Order"));
    }

    #[test]
    fn it_unknown_var_returns_none() {
        let (u, idx) = indexed("/t.kt", "");
        assert_eq!(find_it_element_type("unknown.forEach { it.", &idx, &u), None);
    }

    // ── named lambda parameter type inference ─────────────────────────────────

    #[test]
    fn named_lambda_param_same_line() {
        // items.forEach { item -> item.  ← same line
        let src = "val items: List<Product> = emptyList()";
        let (u, idx) = indexed("/t.kt", src);
        let before = "items.forEach { item -> item.";
        let result = find_named_lambda_param_type(before, "item", &idx, &u, 0);
        assert_eq!(result.as_deref(), Some("Product"));
    }

    #[test]
    fn named_lambda_param_multiline() {
        // items.forEach { item ->
        //     item.  ← cursor here
        let src = "val items: List<Order> = emptyList()\nitems.forEach { order ->\n    order.x\n}";
        let (u, idx) = indexed("/t.kt", src);
        // cursor on line 2 ("    order.x"), scanning back to line 1 for `{ order ->`
        let result = find_named_lambda_param_type("    order.", "order", &idx, &u, 2);
        assert_eq!(result.as_deref(), Some("Order"));
    }

    #[test]
    fn named_lambda_param_scope_fn() {
        // val user: User — `user.also { u -> u.` — `u` is User itself
        let src = "val user: User = User()";
        let (u, idx) = indexed("/t.kt", src);
        let before = "user.also { u -> u.";
        let result = find_named_lambda_param_type(before, "u", &idx, &u, 0);
        assert_eq!(result.as_deref(), Some("User"));
    }

    #[test]
    fn is_lambda_param_detects_same_line() {
        let src = "";
        let (u, idx) = indexed("/t.kt", src);
        assert!(is_lambda_param("item", "items.forEach { item -> item.", &idx, &u, 0));
        assert!(!is_lambda_param("item", "val item = something()", &idx, &u, 0));
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
    fn enclosing_class_simple() {
        let src = "\
sealed interface NewsFeedUiState {
    data object Loading : NewsFeedUiState
    data class Success(val items: List<String>) : NewsFeedUiState
}";
        let (u, idx) = indexed("/NewsFeed.kt", src);
        // Line 1 = "    data object Loading ..."  → enclosing = NewsFeedUiState
        assert_eq!(
            idx.enclosing_class_at(&u, 1),
            Some("NewsFeedUiState".into()),
        );
    }

    #[test]
    fn enclosing_class_top_level_returns_none() {
        let src = "sealed interface NewsFeedUiState {\n    data object Loading : NewsFeedUiState\n}";
        let (u, idx) = indexed("/NewsFeed.kt", src);
        // Line 0 = the sealed interface itself — no enclosure
        assert_eq!(idx.enclosing_class_at(&u, 0), None);
    }

    #[test]
    fn enclosing_class_nested_two_levels() {
        let src = "\
class Outer {
    sealed class Inner {
        data object Loading : Inner
    }
}";
        let (u, idx) = indexed("/Outer.kt", src);
        // Line 2 = "        data object Loading..." → enclosing = Inner (closer one)
        assert_eq!(idx.enclosing_class_at(&u, 2), Some("Inner".into()));
    }

    #[test]
    fn enclosing_class_multiline_constructor() {
        // Simulates DashboardProductsViewModel: `class` keyword on line 0,
        // closing `)` + `{` on line 3. Cursor at line 5 (inside the body).
        let src = "\
class Foo @Inject constructor(
  private val a: A,
  private val b: B,
) : Bar() {
  override fun doIt() {
    super.doIt()
  }
}";
        let (u, idx) = indexed("/Foo.kt", src);
        // Line 5 = "    super.doIt()" → enclosing class = Foo
        assert_eq!(idx.enclosing_class_at(&u, 5), Some("Foo".into()));
    }

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
        let start = locs[0].range.start.line as usize;
        let end = (start + 10).min(file.lines.len());
        let mut decl_lines: Vec<String> = vec![];
        for line in &file.lines[start..end] {
            decl_lines.push(line.clone());
            if line.contains('{') { break; }
        }
        let supers = crate::resolver::extract_supers_from_lines(&decl_lines);
        assert!(supers.contains(&"Bar".to_string()), "supers={supers:?}");

        // 3. find_definition_qualified finds Bar (same package)
        let bar_locs = idx.find_definition_qualified("Bar", None, &foo_uri);
        assert!(!bar_locs.is_empty(), "Bar must resolve via same-package");
    }

    #[test]
    fn extract_class_decl_name_variants() {
        assert_eq!(extract_class_decl_name("sealed interface Foo {"), Some("Foo".into()));
        assert_eq!(extract_class_decl_name("data class Bar(val x: Int)"), Some("Bar".into()));
        assert_eq!(extract_class_decl_name("object Baz"), Some("Baz".into()));
        assert_eq!(extract_class_decl_name("fun doSomething() {}"), None);
        assert_eq!(extract_class_decl_name("val x: Int = 0"), None);
        assert_eq!(extract_class_decl_name("// class NotReal"), None);
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
    fn hover_includes_kdoc() {
        let src = r#"package com.example

/**
 * Represents a user account.
 */
class Account(val name: String)"#;
        let (u, idx) = indexed("/Account.kt", src);
        let hover = idx.hover_info("Account").unwrap();
        assert!(hover.contains("Represents a user account"), "got: {hover}");
        assert!(hover.contains("```kotlin"), "got: {hover}");
        assert!(hover.contains("---"), "separator missing: {hover}");
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

    // ── multi-param lambda detection ─────────────────────────────────────────

    #[test]
    fn multi_param_lambda_params_at() {
        let src = "items.zip(other) { a, b ->\n    a.id\n}";
        let (u, idx) = indexed("/t.kt", src);
        // Simulate live_lines for lambda_params_at
        idx.set_live_lines(&u, src);
        let params = idx.lambda_params_at(&u, 1);
        assert!(params.contains(&"a".to_string()), "expected a, got: {params:?}");
        assert!(params.contains(&"b".to_string()), "expected b, got: {params:?}");
    }

    #[test]
    fn lambda_params_at_excludes_sibling_lambda() {
        // `isRefresh` is in a CLOSED sibling lambda; cursor is in `resultState` body.
        let src = concat!(
            "reload({ isRefresh -> doSomething(isRefresh) }) { resultState ->\n",
            "    resultState.value\n",
            "}",
        );
        let (u, idx) = indexed("/t.kt", src);
        idx.set_live_lines(&u, src);
        let params = idx.lambda_params_at(&u, 1);
        assert!(params.contains(&"resultState".to_string()),
            "resultState should be in scope, got: {params:?}");
        assert!(!params.contains(&"isRefresh".to_string()),
            "isRefresh is a closed sibling — must NOT appear, got: {params:?}");
    }

    #[test]
    fn lambda_params_at_col_inline_lambda() {
        // Cursor is inside the body of an inline lambda on the SAME line.
        // `lambda_params_at` (without col) would see the closing `}` first and
        // NOT collect the params.  `lambda_params_at_col` limits the scan to
        // the cursor column, so it correctly identifies `loanId` and `isWustenrot`.
        let src = "    loan = { loanId, isWustenrot -> setEvent(OnSecondaryActionClick(loanId, isWustenrot)) },";
        let (u, idx) = indexed("/t.kt", src);
        idx.set_live_lines(&u, src);
        // Cursor on the second `loanId` (inside the setEvent call).
        // Column ~= position of second "loanId".
        let col = src.rfind("loanId").unwrap();  // byte offset ≈ UTF-16 col for ASCII
        let params = idx.lambda_params_at_col(&u, 0, col);
        assert!(params.contains(&"loanId".to_string()),
            "loanId should be in scope (col-aware), got: {params:?}");
        assert!(params.contains(&"isWustenrot".to_string()),
            "isWustenrot should be in scope (col-aware), got: {params:?}");
    }

    #[test]
    fn find_named_param_on_line_with_multiple_arrows() {
        // `resultState` is the SECOND lambda on the same line — the first `->` belongs
        // to `{ isRefresh -> ... }`.  `line_has_lambda_param` must scan all arrows.
        let line = "reloadableProduct(ProductKey.FAMILY, { isRefresh -> getFamilyAccount(isRefresh) }) { resultState ->";
        assert!(line_has_lambda_param(line, "resultState"),
            "must find resultState even when isRefresh arrow comes first");
        assert!(line_has_lambda_param(line, "isRefresh"),
            "must still find isRefresh");
        assert!(!line_has_lambda_param(line, "other"),
            "must NOT find unknown name");

        let brace = lambda_brace_pos_for_param(line, "resultState");
        assert!(brace.is_some(), "must find brace for resultState");
        // The brace for resultState is the LAST `{` on the line.
        let last_brace = line.rfind('{').unwrap();
        assert_eq!(brace.unwrap(), last_brace,
            "brace pos should be the last {{ on the line");
    }

    #[test]
    fn multi_param_lambda_is_detected() {
        let src = "items.zip(other) { a, b ->\n    a.id\n}";
        let (u, idx) = indexed("/t.kt", src);
        idx.set_live_lines(&u, src);
        // Both `a` and `b` should be recognised as lambda params
        assert!(is_lambda_param("a", "items.zip(other) { a, b ->", &idx, &u, 0));
        assert!(is_lambda_param("b", "items.zip(other) { a, b ->", &idx, &u, 0));
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

    // ── hover on val bindings ────────────────────────────────────────────────

    #[test]
    fn hover_val_binding_constructor_param() {
        // Constructor parameter: `private val repo: IGoldConversionRepository`
        let idx = Indexer::new();
        let u = uri("/Foo.kt");
        idx.index_content(&u, "\
class Foo(
    private val repo: IGoldConversionRepository
) {
    fun doStuff() {}
}");
        // 1. repo should be captured as a symbol
        let data = idx.files.get(u.as_str()).unwrap();
        let repo_sym = data.symbols.iter().find(|s| s.name == "repo");
        assert!(repo_sym.is_some(), "repo should be in symbols; got: {:?}",
            data.symbols.iter().map(|s| &s.name).collect::<Vec<_>>());

        // 2. find_definition_qualified should find it
        let locs = idx.find_definition_qualified("repo", None, &u);
        assert!(!locs.is_empty(), "repo should be found via find_definition_qualified");

        // 3. hover_info_at_location should return something
        let hover = idx.hover_info_at_location(locs.first().unwrap(), "repo");
        assert!(hover.is_some(), "hover on val repo should produce result");
        let md = hover.unwrap();
        assert!(md.contains("repo"), "hover should mention 'repo', got: {md}");
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

    #[test]
    fn real_hover_constructor_val_binding() {
        // From report: hover on `repo` in constructor param returns nothing
        let idx = Indexer::new();
        let u = uri("/ContactAddressInteractor.kt");
        idx.index_content(&u, "\
package cz.moneta.smartbanka.feature.gold_conversion.model.goldcard
internal class ContactAddressInteractor @Inject constructor(
  private val repo: IGoldConversionRepository,
) : ISimpleLoadDataInteractor<PersonalAddress> {
  override suspend fun loadData(): PersonalAddress =
    requireNotNull(repo.contactAddressSetup().contactAddress)
}");
        // hover on `repo` (line 2, col ~14)
        let locs = idx.find_definition_qualified("repo", None, &u);
        assert!(!locs.is_empty(), "repo should be found");
        let hover = idx.hover_info_at_location(locs.first().unwrap(), "repo");
        assert!(hover.is_some(), "hover on val repo should work");
        let md = hover.unwrap();
        assert!(md.contains("repo"), "hover should mention repo: {md}");
        assert!(md.contains("IGoldConversionRepository"), "hover should show type: {md}");
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
}


