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
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use dashmap::DashMap;
use tower_lsp::lsp_types::*;

use super::{FileContributions, Indexer, StaleKeys};
use crate::indexer::cache::{build_qualified_keys, FileCacheEntry};
use crate::indexer::discover::find_source_files_unconstrained;
use crate::parser::parse_by_extension;
use crate::resolver::symbols_from_uri_as_completions_pub;
use crate::types::{FileData, FileIndexResult, Visibility, WorkspaceIndexResult};
use crate::StrExt;

// ─── Source-path scan helpers ─────────────────────────────────────────────────

/// Collected output from a slow-path source-path scan.
struct SourcePathScan {
    results: Vec<FileIndexResult>,
    new_library_uris: Vec<String>,
    cache_hits: usize,
}

/// Pure: check whether `path` matches a library cache entry (mtime + size).
/// Returns the matching entry when the on-disk file is unchanged, `None` otherwise.
fn try_cache_hit<'a>(
    lib_cache: Option<&'a HashMap<String, FileCacheEntry>>,
    path: &Path,
) -> Option<&'a FileCacheEntry> {
    let cache = lib_cache?;
    let path_str = path.to_string_lossy();
    let entry = cache.get(path_str.as_ref())?;
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    if entry.mtime_secs == mtime && entry.file_size == meta.len() {
        Some(entry)
    } else {
        None
    }
}

/// Resolve raw source-path strings against `workspace_root` at call time.
/// Relative paths are joined to `workspace_root`; absolute paths are kept as-is.
fn resolve_source_paths(raw_paths: &[String], workspace_root: &Path) -> Vec<PathBuf> {
    raw_paths
        .iter()
        .map(|s| {
            let p = PathBuf::from(s);
            if p.is_absolute() {
                p
            } else {
                workspace_root.join(s)
            }
        })
        .collect()
}

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

/// Strip private symbols from `results` whose URI appears in `library_uris`.
/// Private members of external dependencies are inaccessible from workspace code.
fn strip_library_private_symbols(
    results: &mut [FileIndexResult],
    library_uris: &std::collections::HashSet<&str>,
) {
    for result in results.iter_mut() {
        if library_uris.contains(result.uri.as_str()) {
            result
                .data
                .symbols
                .retain(|s| !matches!(s.visibility, Visibility::Private | Visibility::Internal));
        }
    }
}

/// Pure: compute what a parsed file contributes to each index map.
/// No side effects. Call [`Indexer::apply_contributions`] to commit.
pub(crate) fn file_contributions(result: &FileIndexResult) -> FileContributions {
    let uri_str = result.uri.to_string();
    let file_stem: Option<String> = result
        .uri
        .to_file_path()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()));

    let mut definitions: HashMap<String, Vec<Location>> = HashMap::new();
    let mut qualified: HashMap<String, Location> = HashMap::new();

    for sym in &result.data.symbols {
        let loc = Location {
            uri: result.uri.clone(),
            range: sym.selection_range,
        };
        definitions
            .entry(sym.name.clone())
            .or_default()
            .push(loc.clone());
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
        packages
            .entry(pkg.clone())
            .or_default()
            .push(uri_str.clone());
    }

    let mut subtypes: HashMap<String, Vec<Location>> = HashMap::new();
    for (super_name, class_loc) in &result.supertypes {
        subtypes
            .entry(super_name.clone())
            .or_default()
            .push(class_loc.clone());
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
    let file_stem: Option<String> = uri
        .to_file_path()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()));

    let definition_names: Vec<String> = old_data.symbols.iter().map(|s| s.name.clone()).collect();

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

/// Accumulator for the library index fast path.
///
/// Bundles all six HashMap contributions and the library-URI list so that
/// adding a new index field causes a compile error at `flush_into` rather
/// than a silent miss at an arbitrary call site.
struct LibraryBatch {
    files: HashMap<String, Arc<FileData>>,
    hashes: HashMap<String, u64>,
    definitions: HashMap<String, Vec<Location>>,
    qualified: HashMap<String, Location>,
    packages: HashMap<String, Vec<String>>,
    subtypes: HashMap<String, Vec<Location>>,
    library_uris: Vec<String>,
}

impl LibraryBatch {
    fn with_capacity(n: usize) -> Self {
        Self {
            files: HashMap::with_capacity(n),
            hashes: HashMap::with_capacity(n),
            definitions: HashMap::new(),
            qualified: HashMap::new(),
            packages: HashMap::new(),
            subtypes: HashMap::new(),
            library_uris: Vec::with_capacity(n),
        }
    }

    /// Populate one cache entry into the batch.
    ///
    /// `path` is the filesystem path used to determine whether the file is
    /// outside `workspace_root` (library) or inside it (workspace source).
    fn collect_entry(
        &mut self,
        uri: &Url,
        uri_str: &str,
        path: &std::path::Path,
        entry: &FileCacheEntry,
        class_kinds: &[SymbolKind],
        workspace_root: &std::path::Path,
    ) {
        let is_library = !path.starts_with(workspace_root);

        // Library entries loaded from cache have private/internal symbols already
        // stripped at save time; workspace entries need no filtering.  Either way,
        // a plain Arc::clone is sufficient — no deep copy or retain() needed here.
        let file_data = Arc::clone(&entry.file_data);

        for sym in &file_data.symbols {
            let loc = Location {
                uri: uri.clone(),
                range: sym.selection_range,
            };
            self.definitions
                .entry(sym.name.clone())
                .or_default()
                .push(loc.clone());
        }

        // Fast path: use pre-computed qualified keys stored at save time.
        // Fall back to format!() for old cache entries that lack the field.
        if !entry.qualified_keys.is_empty() {
            for (key, range) in &entry.qualified_keys {
                self.qualified.insert(
                    key.clone(),
                    Location {
                        uri: uri.clone(),
                        range: *range,
                    },
                );
            }
        } else {
            let file_stem: Option<String> = uri
                .to_file_path()
                .ok()
                .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()));
            for (key, range) in build_qualified_keys(&file_data, file_stem.as_deref()) {
                self.qualified.insert(
                    key,
                    Location {
                        uri: uri.clone(),
                        range,
                    },
                );
            }
        }

        if let Some(ref pkg) = file_data.package {
            self.packages
                .entry(pkg.clone())
                .or_default()
                .push(uri_str.to_string());
        }

        for sym in &file_data.symbols {
            if !class_kinds.contains(&sym.kind) {
                continue;
            }
            let start_line = sym.selection_start();
            let class_loc = Location {
                uri: uri.clone(),
                range: sym.selection_range,
            };
            for (_, super_name, _) in file_data.supers.iter().filter(|(l, _, _)| *l == start_line) {
                self.subtypes
                    .entry(super_name.clone())
                    .or_default()
                    .push(class_loc.clone());
            }
        }

        self.files.insert(uri_str.to_string(), file_data);
        self.hashes.insert(uri_str.to_string(), entry.content_hash);

        if is_library {
            self.library_uris.push(uri_str.to_string());
        }
    }

    /// Bulk-extend the Indexer's DashMaps — one lock acquisition per unique key.
    ///
    /// All fields are consumed here. Adding a new index map to `LibraryBatch`
    /// will cause a compile error if `flush_into` is not updated.
    fn flush_into(self, indexer: &Indexer) {
        for (k, v) in self.hashes {
            indexer.content_hashes.insert(k, v);
        }
        for (k, v) in self.files {
            indexer.files.insert(k, v);
        }
        for (name, locs) in self.definitions {
            indexer.definitions.entry(name).or_default().extend(locs);
        }
        for (key, loc) in self.qualified {
            indexer.qualified.insert(key, loc);
        }
        for (pkg, uris) in self.packages {
            indexer.packages.entry(pkg).or_default().extend(uris);
        }
        for (super_name, locs) in self.subtypes {
            indexer.subtypes.entry(super_name).or_default().extend(locs);
        }
        for uri_str in self.library_uris {
            indexer.library_uris.insert(uri_str);
        }
    }
}

// ─── impl Indexer ─────────────────────────────────────────────────────────────

impl Indexer {
    /// Parse a single file via tree-sitter and extract symbols, supertypes, and a
    /// content hash.  Pure — no writes to any `Indexer` field.
    pub(crate) fn parse_file(uri: &Url, content: &str) -> FileIndexResult {
        let data = parse_by_extension(uri.path(), content);
        let hash = hash_str(content);

        // Extract supertype relationships for goToImplementation.
        let mut supertypes = Vec::new();
        let class_kinds = [
            SymbolKind::CLASS,
            SymbolKind::INTERFACE,
            SymbolKind::STRUCT,
            SymbolKind::ENUM,
            SymbolKind::OBJECT,
        ];

        for sym in &data.symbols {
            if !class_kinds.contains(&sym.kind) {
                continue;
            }
            let start_line = sym.selection_start();
            let class_loc = Location {
                uri: uri.clone(),
                range: sym.selection_range,
            };
            for (_, super_name, _) in data.supers.iter().filter(|(l, _, _)| *l == start_line) {
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
    pub(crate) fn apply_file_result(&self, result: &FileIndexResult) {
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
                entry
                    .value_mut()
                    .retain(|l| l.uri.as_str() != uri_str.as_str());
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
    pub(crate) fn apply_workspace_result(&self, result: &WorkspaceIndexResult) {
        log::info!(
            "Applying workspace results: {} files parsed, {} cache hits",
            result.stats.files_parsed,
            result.stats.cache_hits
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
            self.definitions.len(),
            self.files.len()
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
    pub(crate) async fn index_source_paths(self: Arc<Self>, workspace_root: PathBuf) {
        let raw_paths = self.source_paths_raw.read().unwrap().clone();
        if raw_paths.is_empty() {
            return;
        }

        let gen = self.workspace_root.generation();
        let source_paths = resolve_source_paths(&raw_paths, &workspace_root);

        let cache_path = crate::indexer::cache::library_cache_path(&raw_paths);
        let lib_cache = crate::indexer::cache::try_load_library_cache(&raw_paths);
        let cache_is_fresh = lib_cache.as_ref().is_some_and(|entries| {
            crate::indexer::cache::library_cache_is_fresh(&source_paths, &cache_path, entries)
        });

        // Fast path: library cache is fresh (source dirs haven't changed).
        if cache_is_fresh {
            let Some(lib_cache) = lib_cache else {
                return;
            };
            self.restore_from_library_cache(lib_cache, &workspace_root);
            return;
        }

        // Slow path: scan directories, validate per-file, parse changed files.
        let scan = self
            .scan_source_paths_slow(&source_paths, lib_cache.as_ref(), &workspace_root)
            .await;

        if self.workspace_root.generation() != gen {
            log::info!(
                "index_source_paths: generation changed during async I/O, discarding results"
            );
            return;
        }

        self.apply_source_path_scan(scan, &raw_paths);
    }

    /// Fast path: restore library index from a fresh on-disk cache without re-scanning.
    ///
    /// Batches all contributions into local HashMaps first (no DashMap overhead),
    /// then bulk-extends into DashMap in one pass. This avoids ~390K individual
    /// lock acquisitions + dedup scans that plague the per-file approach.
    fn restore_from_library_cache(
        &self,
        lib_cache: HashMap<String, FileCacheEntry>,
        workspace_root: &Path,
    ) {
        let total = lib_cache.len();
        log::debug!(
            "Library cache fresh: restoring {} entries without re-scanning",
            total
        );

        let mut batch = LibraryBatch::with_capacity(total);
        let class_kinds = [
            SymbolKind::CLASS,
            SymbolKind::INTERFACE,
            SymbolKind::STRUCT,
            SymbolKind::ENUM,
            SymbolKind::OBJECT,
        ];

        for (path_str, entry) in &lib_cache {
            let Ok(uri) = Url::from_file_path(path_str) else {
                continue;
            };
            let uri_str = uri.to_string();
            batch.collect_entry(
                &uri,
                &uri_str,
                Path::new(path_str.as_str()),
                entry,
                &class_kinds,
                workspace_root,
            );
        }

        batch.flush_into(self);
        self.rebuild_bare_name_cache();

        log::debug!(
            "Source paths restored from cache: {} library files, {} total indexed files",
            self.library_uris.len(),
            self.files.len()
        );
    }

    /// Slow path: scan source directories, use per-file cache where possible,
    /// and spawn async parse tasks for changed files.
    async fn scan_source_paths_slow(
        &self,
        source_paths: &[PathBuf],
        lib_cache: Option<&HashMap<String, FileCacheEntry>>,
        workspace_root: &Path,
    ) -> SourcePathScan {
        let sem = Arc::clone(&self.parse_sem);
        let mut new_library_uris: Vec<String> = Vec::new();
        let mut all_results: Vec<FileIndexResult> = Vec::new();
        let mut cache_hits: usize = 0;

        for source_path in source_paths {
            if !source_path.exists() {
                log::warn!("sourcePaths: {:?} does not exist, skipping", source_path);
                continue;
            }
            let files = find_source_files_unconstrained(source_path);
            log::info!(
                "Indexing source path: {} ({} files)",
                source_path.display(),
                files.len()
            );

            let mut tasks: Vec<tokio::task::JoinHandle<Option<FileIndexResult>>> = Vec::new();
            for path in files {
                let Ok(uri) = Url::from_file_path(&path) else {
                    continue;
                };
                // Only tag as library if the file is OUTSIDE the workspace root.
                if !path.starts_with(workspace_root) {
                    new_library_uris.push(uri.to_string());
                }

                if let Some(entry) = try_cache_hit(lib_cache, &path) {
                    all_results.push(crate::indexer::cache::cache_entry_to_file_result(
                        &uri, entry,
                    ));
                    cache_hits += 1;
                    continue;
                }

                let sem2 = Arc::clone(&sem);
                tasks.push(tokio::spawn(async move {
                    let _permit = sem2.acquire_owned().await.ok()?;
                    let content = tokio::fs::read_to_string(&path).await.ok()?;
                    Some(Indexer::parse_file(&uri, &content))
                }));
            }

            for task in tasks {
                if let Ok(Some(result)) = task.await {
                    all_results.push(result);
                }
            }
        }

        SourcePathScan {
            results: all_results,
            new_library_uris,
            cache_hits,
        }
    }

    /// Apply a completed slow-path scan: strip private symbols, apply contributions,
    /// rebuild caches, and persist the refreshed library cache.
    fn apply_source_path_scan(&self, mut scan: SourcePathScan, raw_paths: &[String]) {
        let newly_parsed = scan.results.len().saturating_sub(scan.cache_hits);
        let library_uri_set: std::collections::HashSet<&str> =
            scan.new_library_uris.iter().map(String::as_str).collect();
        strip_library_private_symbols(&mut scan.results, &library_uri_set);

        for result in scan.results {
            self.apply_contributions(file_contributions(&result));
        }
        for uri in scan.new_library_uris {
            self.library_uris.insert(uri);
        }

        self.rebuild_bare_name_cache();
        log::info!(
            "Source paths indexed: {} library files ({} cache hits), {} total indexed files",
            self.library_uris.len(),
            scan.cache_hits,
            self.files.len()
        );

        crate::indexer::cache::save_library_cache(
            raw_paths,
            &self.files,
            &self.content_hashes,
            &self.library_uris,
        );
        log::info!(
            "Library cache refreshed ({} hits, {} newly parsed files)",
            scan.cache_hits,
            newly_parsed
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
                if !entry
                    .iter()
                    .any(|l| l.uri == loc.uri && l.range == loc.range)
                {
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
                if !entry
                    .iter()
                    .any(|l| l.uri == loc.uri && l.range == loc.range)
                {
                    entry.push(loc);
                }
            }
        }
    }

    /// Coordinator: rebuild bare-name cache from current definitions map.
    pub(crate) fn rebuild_bare_name_cache(&self) {
        if let Ok(mut cache) = self.bare_name_cache.write() {
            *cache = build_bare_names(&self.definitions);
        }
        self.rebuild_importable_fqns();
        // Invalidate the single-entry last_completion cache so that the next
        // request re-runs against the updated symbol set (e.g. after library
        // source paths finish indexing).
        if let Ok(mut last) = self.last_completion.lock() {
            *last = None;
        }
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
            // Detect top-level symbols: a symbol is top-level if it has no container.
            let syms = &data.symbols;
            for sym in syms.iter() {
                if sym.container.is_some() {
                    continue;
                }
                let fqn = format!("{}.{}", pkg, sym.name);
                map.entry(sym.name.clone()).or_default().push(fqn);
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
    pub(crate) fn index_content(&self, uri: &Url, content: &str) -> Option<Arc<FileData>> {
        // Fast-path: skip re-parse if content hasn't changed since last index.
        let hash = hash_str(content);
        let uri_str = uri.to_string();
        if self
            .content_hashes
            .get(&uri_str)
            .map(|h| *h == hash)
            .unwrap_or(false)
        {
            return None;
        }

        self.parse_count.fetch_add(1, Ordering::Relaxed);
        // Invalidate cached completion items — the file is changing.
        self.completion_cache.remove(&uri_str);
        if let Ok(mut last) = self.last_completion.lock() {
            *last = None;
        }
        // Invalidate entire signature cache — a changed file may affect lookups from any caller.
        self.sig_cache.clear();

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
    pub(crate) fn prewarm_completion_cache(self: Arc<Self>, uri: &Url) {
        let Some(data) = self.files.get(uri.as_str()) else {
            return;
        };
        let from_uri = uri.clone();

        // Collect unique type names from this file's lines.
        let mut type_names: Vec<String> = Vec::new();
        {
            let mut seen = std::collections::HashSet::new();
            for line in data.lines.iter() {
                let t = line.trim_start();
                if t.starts_with("//") || t.starts_with('*') {
                    continue;
                }
                let mut rest = t;
                while let Some(ci) = rest.find(':') {
                    let after = rest[ci + 1..].trim_start();
                    let type_name = after.ident_prefix();
                    if !type_name.is_empty()
                        && type_name.starts_with_uppercase()
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
                        if idx.completion_cache.contains_key(&file_uri) {
                            return;
                        }
                        symbols_from_uri_as_completions_pub(&idx, &file_uri);
                    }
                })
                .await
                .ok();
            });
        }
    }
}

#[cfg(test)]
#[path = "apply_tests.rs"]
mod tests;
