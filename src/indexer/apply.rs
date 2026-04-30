//! Apply phase: parse files, compute contributions, apply results to the index.
//!
//! This module owns the "write path" of the indexer:
//!
//! - [`file_contributions`]       — pure: what a file adds to each map
//! - [`stale_keys_for`]           — pure: what a file previously owned
//! - [`build_bare_names`]         — pure: build sorted symbol-name list
//! - [`Indexer::parse_file`]      — run tree-sitter, extract symbols + supertypes
//! - [`Indexer::apply_file_result`]      — single-file delta (live edits, on_open)
//! - [`Indexer::apply_workspace_result`] — full-replace after workspace scan
//! - [`Indexer::apply_contributions`]    — primitive: drain FileContributions into DashMaps
//! - [`Indexer::index_content`]          — re-parse + apply + rebuild cache
//! - [`Indexer::prewarm_completion_cache`] — background warm for types in a file
//! - [`Indexer::rebuild_bare_name_cache`]  — rebuild completion name list
//! - [`Indexer::rebuild_importable_fqns`]  — rebuild simple_name → [FQN] map
//! - [`Indexer::index_source_paths`]       — additive scan of configured source paths

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use dashmap::DashMap;
use tower_lsp::lsp_types::*;

use crate::indexer::discover::find_source_files_unconstrained;
use crate::types::{FileData, FileIndexResult, WorkspaceIndexResult};
use super::{FileContributions, StaleKeys, Indexer};

// ─── hash helper ─────────────────────────────────────────────────────────────

/// Fast FNV-1a 64-bit hash used for content-change detection.
pub(super) fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ─── Pure functions ───────────────────────────────────────────────────────────

/// Pure: compute what a parsed file contributes to each index map.
/// No side effects. Call [`Indexer::apply_contributions`] to commit.
pub(crate) fn file_contributions(result: &FileIndexResult) -> FileContributions {
    let uri_str = result.uri.to_string();
    let file_stem: Option<String> = result.uri.to_file_path().ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()));

    let mut definitions: HashMap<String, Vec<Location>> = HashMap::new();
    let mut qualified:   HashMap<String, Location>      = HashMap::new();

    for sym in &result.data.symbols {
        let loc = Location { uri: result.uri.clone(), range: sym.selection_range };
        definitions.entry(sym.name.clone()).or_default().push(loc.clone());
        if let Some(ref pkg) = result.data.package {
            qualified.insert(format!("{pkg}.{}", sym.name), loc.clone());
            if let Some(ref stem) = file_stem {
                if *stem != sym.name {
                    qualified.insert(format!("{pkg}.{stem}.{}", sym.name), loc);
                }
            }
        }
    }

    let mut packages: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(ref pkg) = result.data.package {
        packages.entry(pkg.clone()).or_default().push(uri_str.clone());
    }

    let mut subtypes: HashMap<String, Vec<Location>> = HashMap::new();
    for (super_name, class_loc) in &result.supertypes {
        subtypes.entry(super_name.clone()).or_default().push(class_loc.clone());
    }

    FileContributions {
        definitions,
        qualified,
        packages,
        subtypes,
        file_data: (uri_str.clone(), Arc::new(result.data.clone())),
        content_hash: (uri_str, result.content_hash),
    }
}

/// Pure: compute which keys to remove from each index map when `uri` is re-indexed.
/// Requires the *old* `FileData` to know what the file previously contributed.
pub(crate) fn stale_keys_for(uri: &Url, old_data: &FileData) -> StaleKeys {
    let file_stem: Option<String> = uri.to_file_path().ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()));

    let definition_names: Vec<String> = old_data.symbols.iter()
        .map(|s| s.name.clone())
        .collect();

    let mut qualified_keys: Vec<String> = Vec::new();
    if let Some(ref pkg) = old_data.package {
        for sym in &old_data.symbols {
            qualified_keys.push(format!("{pkg}.{}", sym.name));
            if let Some(ref stem) = file_stem {
                if *stem != sym.name {
                    qualified_keys.push(format!("{pkg}.{stem}.{}", sym.name));
                }
            }
        }
    }

    StaleKeys {
        definition_names,
        qualified_keys,
        package: old_data.package.clone(),
    }
}

/// Pure: build sorted, deduplicated list of all symbol names from the definitions map.
pub(crate) fn build_bare_names(definitions: &DashMap<String, Vec<Location>>) -> Vec<String> {
    let mut names: Vec<String> = definitions.iter().map(|e| e.key().clone()).collect();
    names.sort_unstable();
    names.dedup();
    names
}

// ─── impl Indexer ─────────────────────────────────────────────────────────────

impl Indexer {
    /// Parse a single file via tree-sitter and extract symbols, supertypes, and a
    /// content hash.  Pure — no writes to any `Indexer` field.
    pub fn parse_file(uri: &Url, content: &str) -> FileIndexResult {
        let data = crate::parser::parse_by_extension(uri.path(), content);
        let hash = hash_str(content);

        // Extract supertype relationships for goToImplementation.
        let mut supertypes = Vec::new();
        let class_kinds = [
            SymbolKind::CLASS, SymbolKind::INTERFACE, SymbolKind::STRUCT,
            SymbolKind::ENUM, SymbolKind::OBJECT,
        ];

        for sym in &data.symbols {
            if !class_kinds.contains(&sym.kind) { continue; }
            let start_line = sym.selection_range.start.line;
            let class_loc = Location { uri: uri.clone(), range: sym.selection_range };
            for (_, super_name) in data.supers.iter().filter(|(l, _)| *l == start_line) {
                supertypes.push((super_name.clone(), class_loc.clone()));
            }
        }

        FileIndexResult {
            uri: uri.clone(),
            data,
            supertypes,
            content_hash: hash,
            error: None,
        }
    }

    /// Coordinator: apply a single file parse result to the index.
    ///
    /// Uses pure [`stale_keys_for`] to compute removals and [`file_contributions`]
    /// to compute insertions. This is the per-file delta path (live edits, on_open).
    pub fn apply_file_result(&self, result: &FileIndexResult) {
        let uri_str = result.uri.to_string();

        // ── Remove stale entries ──────────────────────────────────────────────
        if let Some(old) = self.files.get(&uri_str) {
            let stale = stale_keys_for(&result.uri, &old);
            for name in &stale.definition_names {
                if let Some(mut locs) = self.definitions.get_mut(name) {
                    locs.retain(|l| l.uri.as_str() != uri_str.as_str());
                }
            }
            for key in &stale.qualified_keys {
                self.qualified.remove(key);
            }
            if let Some(ref pkg) = stale.package {
                if let Some(mut uris) = self.packages.get_mut(pkg) {
                    uris.retain(|u| u != &uri_str);
                }
            }
            for mut entry in self.subtypes.iter_mut() {
                entry.value_mut().retain(|l| l.uri.as_str() != uri_str.as_str());
            }
        }

        // ── Insert fresh contributions ────────────────────────────────────────
        let contrib = file_contributions(result);
        self.apply_contributions(contrib);
    }

    /// Coordinator: apply workspace indexing results to the index.
    ///
    /// Full-replace path: resets all index maps first, then inserts all file
    /// contributions. Cache hits are already converted to `FileIndexResult` by
    /// `cache_entry_to_file_result` (supertypes included).
    pub fn apply_workspace_result(&self, result: &WorkspaceIndexResult) {
        log::info!(
            "Applying workspace results: {} files parsed, {} cache hits",
            result.stats.files_parsed, result.stats.cache_hits
        );

        // Full replace — clear stale state from any previous root or run.
        self.reset_index_state();

        for file_result in &result.files {
            let contrib = file_contributions(file_result);
            self.apply_contributions(contrib);
        }

        self.rebuild_bare_name_cache();

        log::info!(
            "Index ready: {} symbols from {} files",
            self.definitions.len(), self.files.len()
        );
    }

    /// Index all configured `sourcePaths` additively — without clearing the workspace index.
    ///
    /// Files outside the workspace root are marked as library sources in `library_uris`:
    /// they contribute to hover, definition, and autocomplete but are excluded from
    /// findReferences and rename. Files inside the workspace root are indexed but not
    /// marked as library (they are already covered by the workspace scan; sourcePaths
    /// can override ignorePatterns for those).
    ///
    /// Generation-safe: captures `root_generation` at the start and discards results
    /// if it changes during async I/O (root switch / explicit reindex).
    pub async fn index_source_paths(self: Arc<Self>, workspace_root: PathBuf) {
        let raw_paths = self.source_paths_raw.read().unwrap().clone();
        if raw_paths.is_empty() { return; }

        let gen = self.root_generation.load(Ordering::SeqCst);

        // Resolve raw paths against workspace root at call time.
        let source_paths: Vec<PathBuf> = raw_paths.iter().map(|s| {
            let p = PathBuf::from(s);
            if p.is_absolute() { p } else { workspace_root.join(s) }
        }).collect();

        let sem = Arc::clone(&self.parse_sem);
        let mut new_library_uris: Vec<String> = Vec::new();
        let mut all_results: Vec<FileIndexResult> = Vec::new();

        for source_path in &source_paths {
            if !source_path.exists() {
                log::warn!("sourcePaths: {:?} does not exist, skipping", source_path);
                continue;
            }
            log::info!("Indexing source path: {}", source_path.display());

            let files = find_source_files_unconstrained(source_path);
            log::info!("  Found {} source files in {}", files.len(), source_path.display());

            let mut tasks = Vec::new();
            for path in files {
                let uri = match Url::from_file_path(&path) {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                let uri_str = uri.to_string();
                // Only tag as library if the file is OUTSIDE the workspace root.
                // Files inside the workspace are already in the main index; sourcePaths
                // can be used to un-ignore them without misclassifying them as libraries.
                if !path.starts_with(&workspace_root) {
                    new_library_uris.push(uri_str.clone());
                }
                let sem2 = Arc::clone(&sem);
                let task: tokio::task::JoinHandle<Option<FileIndexResult>> = tokio::spawn(async move {
                    let _permit = sem2.acquire_owned().await.ok()?;
                    let content = tokio::fs::read_to_string(&path).await.ok()?;
                    Some(Indexer::parse_file(&uri, &content))
                });
                tasks.push(task);
            }

            for task in tasks {
                if let Ok(Some(result)) = task.await {
                    all_results.push(result);
                }
            }
        }

        // Bail if workspace switched during async I/O.
        if self.root_generation.load(Ordering::SeqCst) != gen {
            log::info!("index_source_paths: generation changed during async I/O, discarding results");
            return;
        }

        // Apply results additively (no reset_index_state).
        for result in all_results {
            let contrib = file_contributions(&result);
            self.apply_contributions(contrib);
        }

        for uri in new_library_uris {
            self.library_uris.insert(uri);
        }

        self.rebuild_bare_name_cache();
        log::info!(
            "Source paths indexed: {} library files, {} total indexed files",
            self.library_uris.len(), self.files.len()
        );
    }

    /// Primitive: drain a [`FileContributions`] into the DashMaps.
    /// Deduplicates before inserting (same behaviour as before).
    fn apply_contributions(&self, contrib: FileContributions) {
        let (uri_str, file_data) = contrib.file_data;
        let (hash_key, hash_val) = contrib.content_hash;

        self.content_hashes.insert(hash_key, hash_val);
        self.files.insert(uri_str.clone(), file_data);

        for (name, locs) in contrib.definitions {
            let mut entry = self.definitions.entry(name).or_default();
            for loc in locs {
                if !entry.iter().any(|l| l.uri == loc.uri && l.range == loc.range) {
                    entry.push(loc);
                }
            }
        }

        for (key, loc) in contrib.qualified {
            self.qualified.insert(key, loc);
        }

        for (pkg, uris) in contrib.packages {
            let mut entry = self.packages.entry(pkg).or_default();
            for u in uris {
                if !entry.contains(&u) {
                    entry.push(u);
                }
            }
        }

        for (super_name, locs) in contrib.subtypes {
            let mut entry = self.subtypes.entry(super_name).or_default();
            for loc in locs {
                if !entry.iter().any(|l| l.uri == loc.uri && l.range == loc.range) {
                    entry.push(loc);
                }
            }
        }
    }

    /// Coordinator: rebuild bare-name cache from current definitions map.
    pub fn rebuild_bare_name_cache(&self) {
        if let Ok(mut cache) = self.bare_name_cache.write() {
            *cache = build_bare_names(&self.definitions);
        }
        self.rebuild_importable_fqns();
    }

    /// Build importable_fqns: `simple_name → [FQN, …]` from real top-level symbols.
    /// Uses `files + package` rather than the `qualified` map to avoid synthetic FileStem keys.
    fn rebuild_importable_fqns(&self) {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for file_entry in self.files.iter() {
            let data = file_entry.value();
            let pkg = match &data.package {
                Some(p) if !p.is_empty() => p.clone(),
                _ => continue,
            };
            // Detect top-level symbols: a symbol is top-level if its range is not
            // wholly contained within any other symbol's range in the same file.
            let syms = &data.symbols;
            for (i, sym) in syms.iter().enumerate() {
                let is_nested = syms.iter().enumerate().any(|(j, other)| {
                    j != i
                        && other.range.start.line <= sym.range.start.line
                        && other.range.end.line >= sym.range.end.line
                        && !(other.range.start.line == sym.range.start.line
                            && other.range.end.line == sym.range.end.line)
                });
                if !is_nested {
                    let fqn = format!("{}.{}", pkg, sym.name);
                    map.entry(sym.name.clone()).or_default().push(fqn);
                }
            }
        }
        for fqns in map.values_mut() {
            fqns.sort_unstable();
            fqns.dedup();
        }
        if let Ok(mut guard) = self.importable_fqns.write() {
            *guard = map;
        }
    }

    /// (Re-)parse and index a single file's content in-place.
    ///
    /// Returns `Some(data)` when the file was actually (re-)parsed, or `None`
    /// when the content-hash matched the previous parse (no work done).
    /// Callers that need to publish diagnostics should read `data.syntax_errors`
    /// from the returned value.
    pub fn index_content(&self, uri: &Url, content: &str) -> Option<Arc<FileData>> {
        // Fast-path: skip re-parse if content hasn't changed since last index.
        let hash = hash_str(content);
        let uri_str = uri.to_string();
        if self.content_hashes.get(&uri_str).map(|h| *h == hash).unwrap_or(false) {
            return None;
        }

        self.parse_count.fetch_add(1, Ordering::Relaxed);
        // Invalidate cached completion items — the file is changing.
        self.completion_cache.remove(&uri_str);
        if let Ok(mut last) = self.last_completion.lock() {
            *last = None;
        }

        let result = Self::parse_file(uri, content);
        self.apply_file_result(&result);
        // Rebuild bare-name cache so complete_bare doesn't iterate definitions.
        self.rebuild_bare_name_cache();

        Some(Arc::new(result.data))
    }

    /// Spawn background tasks to pre-warm the completion cache for all types
    /// declared in `uri` as constructor parameters or properties.
    ///
    /// This runs after `index_content` so that when the user types `repo.` the
    /// cache is already populated and the response is instant.
    pub fn prewarm_completion_cache(self: Arc<Self>, uri: &Url) {
        let Some(data) = self.files.get(uri.as_str()) else { return };
        let from_uri = uri.clone();

        // Collect unique type names from this file's lines.
        let mut type_names: Vec<String> = Vec::new();
        {
            let mut seen = std::collections::HashSet::new();
            for line in data.lines.iter() {
                let t = line.trim_start();
                if t.starts_with("//") || t.starts_with('*') { continue; }
                let mut rest = t;
                while let Some(ci) = rest.find(':') {
                    let after = rest[ci + 1..].trim_start();
                    let type_name: String = after.chars()
                        .take_while(|&c| c.is_alphanumeric() || c == '_')
                        .collect();
                    if !type_name.is_empty()
                        && type_name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
                        && seen.insert(type_name.clone())
                    {
                        type_names.push(type_name);
                    }
                    rest = &rest[ci + 1..];
                }
            }
        }
        drop(data);

        // Spawn a background task per type (capped to avoid bursts).
        let limit = Arc::new(tokio::sync::Semaphore::new(4));
        for type_name in type_names {
            let idx = Arc::clone(&self);
            let uri2 = from_uri.clone();
            let sem = Arc::clone(&limit);
            tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.expect("semaphore closed");
                tokio::task::spawn_blocking(move || {
                    let locs = idx.resolve_symbol(&type_name, None, &uri2);
                    if let Some(loc) = locs.first() {
                        let file_uri = loc.uri.to_string();
                        if idx.completion_cache.contains_key(&file_uri) { return; }
                        crate::resolver::symbols_from_uri_as_completions_pub(&idx, &file_uri);
                    }
                }).await.ok();
            });
        }
    }
}

#[cfg(test)]
#[path = "apply_tests.rs"]
mod tests;
