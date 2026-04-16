use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::*;

// ─── LSP progress notification helper ────────────────────────────────────────

mod progress {
    use tower_lsp::lsp_types::ProgressParams;

    /// `$/progress` notification — used to report workspace indexing status.
    pub(super) enum KotlinProgress {}
    impl tower_lsp::lsp_types::notification::Notification for KotlinProgress {
        type Params = ProgressParams;
        const METHOD: &'static str = "$/progress";
    }
}

use crate::parser;
use crate::types::{FileData, SymbolEntry, FileIndexResult, WorkspaceIndexResult, IndexStats};

// ─── RAII guard for indexing_in_progress flag ─────────────────────────────────

/// RAII guard that clears `indexing_in_progress` on drop (success, panic, or early return).
struct IndexingGuard {
    indexer: Arc<Indexer>,
}

impl Drop for IndexingGuard {
    fn drop(&mut self) {
        self.indexer.indexing_in_progress.store(false, std::sync::atomic::Ordering::Release);
        log::debug!("IndexingGuard: cleared indexing_in_progress flag");
    }
}

/// Supported file extensions for indexing and rg/fd searches.
pub const SOURCE_EXTENSIONS: &[&str] = &["kt", "java", "swift"];

// ─── Indexing status file ─────────────────────────────────────────────────────

/// Human-readable status written to `~/.cache/kotlin-lsp/status.json`.
/// The skill extension reads this to report loading state with time estimates.
fn status_cache_path() -> PathBuf {
    let cache_base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".cache")
        });
    cache_base.join("kotlin-lsp").join("status.json")
}

fn write_status_file(content: &str) {
    let path = status_cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, content);
}

/// Hard cap on workspace files indexed eagerly in LSP mode.
/// Files beyond this limit are resolved on-demand via `rg` when first needed.
/// Override by setting the `KOTLIN_LSP_MAX_FILES` environment variable.
const DEFAULT_MAX_INDEX_FILES: usize = 2000;

/// Sentinel value meaning "no file-count limit" — index all discovered files.
/// Used by `--index-only` CLI mode which should always process the full workspace.
pub const MAX_FILES_UNLIMITED: usize = usize::MAX;

/// Pure: resolve the maximum number of files to eagerly index.
///
/// Reads `KOTLIN_LSP_MAX_FILES` from the environment once.
/// Returns `default` when the variable is absent or not a valid integer.
///
/// - LSP mode callers pass `DEFAULT_MAX_INDEX_FILES` (2000).
/// - CLI `--index-only` callers pass `MAX_FILES_UNLIMITED`.
pub fn resolve_max_files(default: usize) -> usize {
    std::env::var("KOTLIN_LSP_MAX_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ─── Disk cache ──────────────────────────────────────────────────────────────

/// Bump when the serialized format changes; invalidates any older cache files.
const CACHE_VERSION: u32 = 4;

/// Per-file entry stored in the on-disk index cache.
#[derive(Serialize, Deserialize)]
pub(crate) struct FileCacheEntry {
    /// File mtime (seconds since Unix epoch) — primary cache validity check.
    mtime_secs: u64,
    /// File size in bytes — secondary guard for same-second edits (1s mtime resolution).
    file_size: u64,
    /// FNV-1a content hash — tertiary guard for mtime collisions / FAT FS.
    content_hash: u64,
    /// Parsed symbol data for this file.
    file_data: FileData,
}

/// Complete serialized index, written to `~/.cache/kotlin-lsp/<root-hash>/index.bin`.
#[derive(Serialize, Deserialize)]
struct IndexCache {
    version: u32,
    /// True when this cache was built from a complete (non-truncated) workspace scan.
    /// Only set to true when `total <= max` at index time.
    /// When false, the entries may be a partial subset of the workspace — warm-manifest
    /// mode is disabled to avoid hiding files that were never indexed.
    #[serde(default)]
    complete_scan: bool,
    /// Absolute path string → per-file cached data.
    entries: HashMap<String, FileCacheEntry>,
}

/// Returns the cache file path for the given workspace root.
pub(crate) fn workspace_cache_path(root: &Path) -> PathBuf {
    // Use canonicalized absolute path so equivalent roots map to same cache.
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    // Hash canonical path for filesystem-friendly cache directory name.
    let root_hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        let digest = hasher.finalize();
        // take first 8 bytes as u64
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        u64::from_be_bytes(bytes)
    };
    let cache_base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".cache")
        });
    cache_base
        .join("kotlin-lsp")
        .join(format!("{root_hash:016x}"))
        .join("index.bin")
}

/// Load and validate the on-disk cache.  Returns `None` if absent / stale / corrupt.
fn try_load_cache(root: &Path) -> Option<IndexCache> {
    let path = workspace_cache_path(root);
    let bytes = std::fs::read(&path).ok()?;
    let cache: IndexCache = match bincode::deserialize(&bytes) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("Cache deserialize failed (struct layout changed?): {e} — will re-index. \
                        Delete {path} to suppress this warning.", path = path.display());
            return None;
        }
    };
    if cache.version != CACHE_VERSION {
        log::info!("Cache version mismatch — will re-index");
        return None;
    }
    log::info!(
        "Loaded index cache ({} files) from {}",
        cache.entries.len(),
        path.display()
    );
    Some(cache)
}

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

// ─── Pure functions ───────────────────────────────────────────────────────────

/// Pure: compute what a parsed file contributes to each index map.
/// No side effects. Call `Indexer::apply_contributions` to commit.
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
/// Requires the *old* FileData to know what the file previously contributed.
pub(crate) fn stale_keys_for(uri: &Url, old_data: &crate::types::FileData) -> StaleKeys {
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

/// Pure: convert a disk-cache entry into a `FileIndexResult` ready for indexing.
/// Reconstructs `supertypes` from cached lines (not stored in cache) so that
/// `goToImplementation` works correctly on cache hits.
pub(crate) fn cache_entry_to_file_result(uri: &Url, entry: &FileCacheEntry) -> FileIndexResult {
    let data = &entry.file_data;
    let class_kinds = [
        SymbolKind::CLASS, SymbolKind::INTERFACE, SymbolKind::STRUCT,
        SymbolKind::ENUM, SymbolKind::OBJECT,
    ];
    let mut supertypes: Vec<(String, Location)> = Vec::new();
    for sym in &data.symbols {
        if !class_kinds.contains(&sym.kind) { continue; }
        let start = sym.selection_range.start.line as usize;
        let limit  = (start + 10).min(data.lines.len());
        let mut decl_lines: Vec<String> = Vec::new();
        for line in &data.lines[start..limit] {
            decl_lines.push(line.clone());
            if line.contains('{') { break; }
        }
        let class_loc = Location { uri: uri.clone(), range: sym.selection_range };
        for super_name in crate::resolver::extract_supers_from_lines(&decl_lines) {
            supertypes.push((super_name, class_loc.clone()));
        }
    }
    FileIndexResult {
        uri: uri.clone(),
        data: data.clone(),
        supertypes,
        content_hash: entry.content_hash,
        error: None,
    }
}

/// Pure: build sorted, deduplicated list of all symbol names from the definitions map.
pub(crate) fn build_bare_names(definitions: &DashMap<String, Vec<Location>>) -> Vec<String> {
    let mut names: Vec<String> = definitions.iter().map(|e| e.key().clone()).collect();
    names.sort_unstable();
    names.dedup();
    names
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
        if let Ok(mut cache) = self.bare_name_cache.write() {
            cache.clear();
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

    /// Discover and index *.kt / *.java files under `root`, bounded by MAX_INDEX_FILES.
    /// Sends LSP `$/progress` notifications so the editor shows a status bar spinner.
    /// On subsequent startups the on-disk cache is used for unchanged files so only
    /// modified or new files need to be re-parsed by tree-sitter.
    /// Full reindex with file-count limit from env var — used by --index-only and kotlin-lsp/reindex.
    pub async fn index_workspace_full(self: Arc<Self>, root: &Path, client: Option<tower_lsp::Client>) {
        let max = resolve_max_files(MAX_FILES_UNLIMITED);
        let result = Arc::clone(&self).index_workspace_impl(root, max, client).await;
        if !result.aborted {
            self.last_scan_complete.store(result.complete_scan, std::sync::atomic::Ordering::Release);
            self.apply_workspace_result(&result);
            self.save_cache_to_disk();
        }
    }
    
    pub async fn index_workspace(self: Arc<Self>, root: &Path, client: Option<tower_lsp::Client>) {
        *self.workspace_root.write().unwrap() = Some(root.to_path_buf());
        let max = resolve_max_files(DEFAULT_MAX_INDEX_FILES);
        let result = Arc::clone(&self).index_workspace_impl(root, max, client).await;
        if !result.aborted {
            self.last_scan_complete.store(result.complete_scan, std::sync::atomic::Ordering::Release);
            self.apply_workspace_result(&result);
            self.save_cache_to_disk();
        }
    }

    /// Prioritized indexing: parse `initial_paths` first (high-priority), then
    /// continue with the normal bounded workspace indexing. Useful for fast
    /// "in-and-out" responsiveness when an editor opens a file in a new root.
    pub async fn index_workspace_prioritized(self: Arc<Self>, root: &Path, initial_paths: Vec<PathBuf>, client: Option<tower_lsp::Client>) {
        // Set workspace root immediately so rg/fd work while priority parse runs.
        *self.workspace_root.write().unwrap() = Some(root.to_path_buf());

        // First, eagerly parse the prioritized files (if present) so their
        // symbols are available quickly for operations like go-to/hover.
        // Also expand priority set to include supertypes of each opened file so
        // that cross-class navigation (super, override resolution) works immediately.
        if !initial_paths.is_empty() {
            let sem = Arc::clone(&self.parse_sem);

            // Collect file contents first so we can extract supertypes synchronously.
            let mut priority_paths: Vec<PathBuf> = Vec::new();
            for path in initial_paths {
                if !path.exists() { continue; }
                let content = match tokio::fs::read_to_string(&path).await {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                // Extract supertypes declared in this file and find their source files
                // in the workspace root so they are indexed before the full scan.
                let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
                let supers = crate::resolver::extract_supers_from_lines(&lines);
                if !supers.is_empty() {
                    let super_paths = find_files_for_types(&supers, root);
                    for sp in super_paths {
                        if !priority_paths.contains(&sp) {
                            priority_paths.push(sp);
                        }
                    }
                }
                priority_paths.push(path);
            }
            // Deduplicate while preserving order (supertypes first).
            priority_paths.dedup();

            let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
            for path in priority_paths {
                let idx = Arc::clone(&self);
                let sem2 = Arc::clone(&sem);
                handles.push(tokio::spawn(async move {
                    let _permit = sem2.acquire_owned().await;
                    if path.exists() {
                        if let Ok(content) = tokio::fs::read_to_string(&path).await {
                            if let Ok(uri) = Url::from_file_path(&path) {
                                // index_content is blocking CPU work; run on blocking pool.
                                let _ = tokio::task::spawn_blocking(move || idx.index_content(&uri, &content)).await;
                            }
                        }
                    }
                }));
            }
            for h in handles { let _ = h.await; }
        }

        // Then proceed with the normal (bounded) workspace indexing in the background.
        let max = resolve_max_files(DEFAULT_MAX_INDEX_FILES);
        let result = Arc::clone(&self).index_workspace_impl(root, max, client).await;
        if !result.aborted {
            self.last_scan_complete.store(result.complete_scan, std::sync::atomic::Ordering::Release);
            self.apply_workspace_result(&result);
            self.save_cache_to_disk();
        }
    }

    async fn index_workspace_impl(self: Arc<Self>, root: &Path, max: usize, client: Option<tower_lsp::Client>) -> WorkspaceIndexResult {
        // Prevent concurrent indexing runs on same Indexer instance.
        let already = self.indexing_in_progress.swap(true, std::sync::atomic::Ordering::AcqRel);
        
        // RAII guard: always clear indexing_in_progress on exit (success, panic, or early return).
        // MUST be created before any early returns to ensure cleanup.
        let _guard = IndexingGuard { indexer: Arc::clone(&self) };
        
        if already {
            log::info!("index_workspace_impl: an indexing run is already in progress; skipping new start");
            return WorkspaceIndexResult {
                files: Vec::new(),
                stats: IndexStats::default(),
                workspace_root: root.to_path_buf(),
                aborted: true,
                complete_scan: false,
            };
        }

        *self.workspace_root.write().unwrap() = Some(root.to_path_buf());
        // Capture generation at start; background work should bail if this changes.
        let start_gen = self.root_generation.load(std::sync::atomic::Ordering::SeqCst);

        // Load cache first — drives both warm-manifest discovery and the mtime check below.
        let cache = try_load_cache(root);

        // Warm start: use cache as file manifest to skip the O(total_dirs) fd scan.
        // Only when the cache was built from a complete (non-truncated) scan — otherwise
        // the manifest may be a partial subset and we must fall back to the full scan.
        let mut paths = if cache.as_ref().map(|c| c.complete_scan).unwrap_or(false) {
            warm_discover_files(root, cache.as_ref().unwrap())
        } else {
            find_source_files(root)
        };
        let total = paths.len();

        // Shallower paths first.
        paths.sort_by_key(|p| p.components().count());
        let paths: Vec<_> = paths.into_iter().take(max).collect();
        let indexed_count = paths.len();

        let truncated = total > max && max != MAX_FILES_UNLIMITED;
        if truncated {
            log::warn!(
                "Large project: eagerly indexing {indexed_count}/{total} files \
                 (shallowest first). Deeper files resolved on-demand via rg. \
                 Set KOTLIN_LSP_MAX_FILES env var to raise the limit."
            );
        } else {
            log::info!("Indexing {} source files under {}", indexed_count, root.display());
        }

        // ── Partition: cache hits → FileIndexResult, misses → need_parse ────────
        // Pure partition: no DashMap mutations here.
        // Both halves are collected into file_results so apply_workspace_result
        // can do a single authoritative reset + full insert.
        let mut need_parse: Vec<PathBuf> = Vec::new();
        let mut cached_results: Vec<FileIndexResult> = Vec::new();
        let mut cache_hits: usize = 0;
        let mut aborted_early = false;

        for path in &paths {
            // Bail early if generation changed (root switch / explicit reindex).
            if self.root_generation.load(std::sync::atomic::Ordering::SeqCst) != start_gen {
                log::info!("index_workspace_impl: generation changed, aborting partition");
                aborted_early = true;
                break;
            }

            let path_str = path.to_string_lossy().to_string();
            let meta = std::fs::metadata(path);
            let mtime = meta.as_ref().ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let on_disk_size = meta.map(|m| m.len()).unwrap_or(u64::MAX);

            if let Some(ref c) = cache {
                if let Some(entry) = c.entries.get(&path_str) {
                    // Cache hit: mtime AND file size must both match to guard against
                    // same-second edits (1s mtime resolution on some filesystems).
                    if entry.mtime_secs == mtime && entry.file_size == on_disk_size {
                        if let Ok(uri) = Url::from_file_path(path) {
                            cached_results.push(cache_entry_to_file_result(&uri, entry));
                        }
                        cache_hits += 1;
                        continue;
                    }
                }
            }
            need_parse.push(path.clone());
        }

        let parse_count = need_parse.len();
        log::info!(
            "Cache: {cache_hits} hits, {parse_count} files need (re-)parsing"
        );
        log::debug!("About to spawn {} parse tasks", parse_count);
        
        // Reset and set parse task counters for progress tracking
        self.parse_tasks_completed.store(0, std::sync::atomic::Ordering::Release);
        self.parse_tasks_total.store(parse_count, std::sync::atomic::Ordering::Release);

        // ── Status file: indexing started ────────────────────────────────────
        let index_start = std::time::Instant::now();
        let started_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let root_str = root.to_string_lossy();
        write_status_file(&format!(
            r#"{{"phase":"indexing","workspace":"{root_str}","indexed":0,"total":{parse_count},"cache_hits":{cache_hits},"symbols":0,"started_at":{started_unix},"elapsed_secs":0,"estimated_total_secs":null}}"#
        ));

        // ── LSP progress: begin ──────────────────────────────────────────────
        let token = NumberOrString::String("kotlin-lsp/indexing".into());
        // Ask the client to create a progress token. Use a short timeout — some
        // editors (older Helix versions) never reply, which would stall indexing.
        if let Some(ref client) = client {
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                client.send_request::<tower_lsp::lsp_types::request::WorkDoneProgressCreate>(
                    WorkDoneProgressCreateParams { token: token.clone() }
                )
            ).await;
        }

        let begin_msg = if cache_hits > 0 {
            format!("Indexing {parse_count}/{indexed_count} files ({cache_hits} cached)…")
        } else if truncated {
            format!("Indexing {indexed_count}/{total} Kotlin files (shallowest first)…")
        } else {
            format!("Indexing {total} Kotlin files…")
        };
        if let Some(ref client) = client {
            client.send_notification::<progress::KotlinProgress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(WorkDoneProgressBegin {
                    title: "kotlin-lsp".into(),
                    cancellable: Some(false),
                    message: Some(begin_msg),
                    percentage: Some(0),
                })),
            }).await;
        }

        // Clear any previous scheduling state for this run so stale entries don't block new work.
        self.scheduled_paths.clear();

        // ── Prepare work items for concurrent parsing ─────────────────────────
        // Package each path with context needed by worker
        #[derive(Clone)]
        struct ParseWorkItem {
            path: PathBuf,
            key: String,
            start_gen: u64,
        }
        
        let mut work_items = Vec::new();
        for path in need_parse {
            // Abort early if generation changed while scheduling parses.
            if self.root_generation.load(std::sync::atomic::Ordering::SeqCst) != start_gen {
                log::info!("index_workspace_impl: generation changed during scheduling, aborting remaining parses");
                aborted_early = true;
                break;
            }

            // Deduplicate scheduling: compute canonical absolute path string as key
            let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone()).to_string_lossy().to_string();
            
            // Atomically insert scheduling entry if not already scheduled for this generation.
            match self.scheduled_paths.entry(key.clone()) {
                dashmap::mapref::entry::Entry::Occupied(mut o) => {
                    let existing_gen = *o.get();
                    if existing_gen == start_gen {
                        // Already scheduled for this generation; skip duplicate enqueue.
                        log::debug!("Skipped scheduling parse for {} (already scheduled gen {})", key, existing_gen);
                        continue;
                    } else {
                        // Different generation; update to current generation and allow schedule.
                        o.insert(start_gen);
                    }
                }
                dashmap::mapref::entry::Entry::Vacant(v) => {
                    v.insert(start_gen);
                }
            }
            
            work_items.push(ParseWorkItem { path, key, start_gen });
        }
        
        // ── concurrent parse using task_runner ────────────────────────────────
        log::info!("Parsing {} files concurrently", work_items.len());
        
        let idx_ref = Arc::clone(&self);
        let sem = Arc::clone(&self.parse_sem);

        // Spawn a task that sends WorkDoneProgress::Report every 500 ms so the
        // editor shows live progress instead of jumping straight from 0% to done.
        let progress_idx   = Arc::clone(&self);
        let progress_client = client.clone();
        let progress_token  = token.clone();
        let progress_total  = parse_count;
        let progress_handle = tokio::spawn(async move {
            if progress_client.is_none() || progress_total == 0 { return; }
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let done = progress_idx.parse_tasks_completed.load(std::sync::atomic::Ordering::Relaxed);
                let pct  = ((done * 100) / progress_total) as u32;
                if let Some(ref c) = progress_client {
                    c.send_notification::<progress::KotlinProgress>(ProgressParams {
                        token: progress_token.clone(),
                        value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(
                            WorkDoneProgressReport {
                                cancellable: Some(false),
                                message: Some(format!("{done}/{progress_total} files…")),
                                percentage: Some(pct),
                            }
                        )),
                    }).await;
                }
                if done >= progress_total { break; }
            }
        });

        let results = crate::task_runner::run_concurrent(
            work_items,
            sem,
            move |item, sem| {
                let idx = Arc::clone(&idx_ref);
                async move {
                    log::debug!("Parsing: {}", item.path.display());
                    
                    // Check generation before parsing
                    if idx.root_generation.load(std::sync::atomic::Ordering::SeqCst) != item.start_gen {
                        log::info!("Parse task: generation changed, skipping {}", item.path.display());
                        return None;
                    }
                    
                    // Read file
                    let content = match tokio::fs::read_to_string(&item.path).await {
                        Ok(c) => c,
                        Err(e) => {
                            log::warn!("Could not read {}: {}", item.path.display(), e);
                            return None;
                        }
                    };
                    
                    // Get URI
                    let uri = match Url::from_file_path(&item.path) {
                        Ok(u) => u,
                        Err(_) => {
                            log::warn!("Invalid file path: {}", item.path.display());
                            return None;
                        }
                    };
                    
                    // Parse (CPU-bound tree-sitter work)
                    // Acquire permit to throttle concurrent spawn_blocking calls
                    let _permit = sem.acquire().await.unwrap();
                    let t0 = std::time::Instant::now();
                    let uri_clone = uri.clone();
                    let content_clone = content.clone();
                    let parse_result = match tokio::task::spawn_blocking(move || {
                        Indexer::parse_file(&uri_clone, &content_clone)
                    }).await {
                        Ok(result) => result,
                        Err(e) => {
                            log::warn!("Parse task panicked for {}: {}", item.path.display(), e);
                            return None;
                        }
                    };
                    let took = t0.elapsed().as_millis();
                    
                    log::debug!("Parsed {} in {} ms", item.path.display(), took);
                    
                    // Remove scheduling marker - check then drop guard before remove
                    let should_remove = idx.scheduled_paths.get(&item.key)
                        .map(|gen| *gen == item.start_gen)
                        .unwrap_or(false);
                    if should_remove {
                        idx.scheduled_paths.remove(&item.key);
                    }
                    
                    // Log slow parses
                    let threshold: u128 = std::env::var("KOTLIN_LSP_PARSE_LOG_MS").ok()
                        .and_then(|v| v.parse::<u128>().ok())
                        .unwrap_or(1000);
                    if took as u128 > threshold {
                        log::warn!("Slow parse: {} took {} ms", item.path.display(), took);
                    }
                    
                    // Track completion
                    idx.parse_tasks_completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    
                    Some(parse_result)
                }
            }
        ).await;

        // Stop the progress reporter (it may still be sleeping on its interval).
        progress_handle.abort();

        log::info!("All {} parse tasks completed", results.len());
        
        // If generation changed while parse tasks ran, discard results — the new
        // root's indexing run will populate the index correctly.
        if self.root_generation.load(std::sync::atomic::Ordering::SeqCst) != start_gen {
            log::info!("index_workspace_impl: generation changed after parse, discarding results");
            aborted_early = true;
        }

        if aborted_early {
            return WorkspaceIndexResult {
                files: Vec::new(),
                stats: IndexStats::default(),
                workspace_root: root.to_path_buf(),
                aborted: true,
                complete_scan: false,
            };
        }

        // Combine cache hits + newly parsed into one list.
        // apply_workspace_result does a full reset + insert of ALL files.
        let mut parsed_results: Vec<FileIndexResult> = results.into_iter().flatten().collect();
        let files_parsed = parsed_results.len();
        let parse_errors = parse_count - files_parsed;

        let mut all_results = cached_results;
        all_results.append(&mut parsed_results);

        // Build stats
        let stats = IndexStats {
            files_discovered: total,
            cache_hits,
            files_parsed,
            symbols_extracted: all_results.iter().map(|f| f.data.symbols.len()).sum(),
            packages_found: all_results.iter().filter_map(|f| f.data.package.as_ref()).count(),
            errors: parse_errors,
        };
        
        log::info!(
            "Workspace indexing complete: {} parsed, {} cache hits, {} errors ({} total)",
            files_parsed, cache_hits, parse_errors, all_results.len()
        );

        // ── LSP progress: end ────────────────────────────────────────────────
        if let Some(ref client) = client {
            client.send_notification::<progress::KotlinProgress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: Some(format!(
                        "Indexed {} files ({} cached, {} parsed)",
                        all_results.len(), cache_hits, files_parsed
                    )),
                })),
            }).await;
        }

        // ── Status file: done ────────────────────────────────────────────────
        let elapsed = index_start.elapsed().as_secs();
        let root_str = root.to_string_lossy();
        write_status_file(&format!(
            r#"{{"phase":"done","workspace":"{root_str}","indexed":{files_parsed},"total":{total},"cache_hits":{cache_hits},"symbols":{symbols},"elapsed_secs":{elapsed},"estimated_total_secs":null}}"#,
            symbols = stats.symbols_extracted,
        ));

        WorkspaceIndexResult {
            files: all_results,
            stats,
            workspace_root: root.to_path_buf(),
            aborted: false,
            complete_scan: !truncated,
        }
    }

    /// Serialize the current index to `~/.cache/kotlin-lsp/<root-hash>/index.bin`.
    /// Safe to call from a background thread.  Logs warnings on error; never panics.
    pub fn save_cache_to_disk(&self) {
        let root_guard = self.workspace_root.read().unwrap();
        let root = match root_guard.as_ref() {
            Some(r) => r,
            None    => return,
        };
        let cache_path = workspace_cache_path(root);
        if let Some(parent) = cache_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::warn!("Cache: could not create directory: {e}");
                return;
            }
        }

        let complete_scan = self.last_scan_complete.load(std::sync::atomic::Ordering::Acquire);
        let mut entries: HashMap<String, FileCacheEntry> = HashMap::new();
        for file_ref in &self.files {
            let uri_str = file_ref.key();
            let data    = file_ref.value();
            let hash    = self.content_hashes.get(uri_str).map(|h| *h).unwrap_or(0);
            if let Ok(url) = uri_str.parse::<Url>() {
                if let Ok(path) = url.to_file_path() {
                    let meta = std::fs::metadata(&path);
                    let mtime = meta.as_ref().ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let file_size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                    entries.insert(
                        path.to_string_lossy().to_string(),
                        FileCacheEntry { mtime_secs: mtime, file_size, content_hash: hash, file_data: (**data).clone() },
                    );
                }
            }
        }

        let cache = IndexCache { version: CACHE_VERSION, complete_scan, entries };
        match bincode::serialize(&cache) {
            Ok(bytes) => {
                match std::fs::write(&cache_path, &bytes) {
                    Ok(()) => log::info!(
                        "Cache saved ({} files, {} KB) → {}",
                        cache.entries.len(), bytes.len() / 1024, cache_path.display()
                    ),
                    Err(e) => log::warn!("Cache write failed: {e}"),
                }
            }
            Err(e) => log::warn!("Cache serialize failed: {e}"),
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // Pure Parsing Functions (SOLID refactoring - no side effects)
    // ────────────────────────────────────────────────────────────────────────

    /// Parse a single file and return structured result data (pure function).
    /// 
    /// This is the testable, side-effect-free core of indexing.
    /// It takes content and returns data without mutating any shared state.
    /// 
    /// Use `apply_file_result()` to merge the result into the index.
    pub fn parse_file(uri: &Url, content: &str) -> FileIndexResult {
        let data = parser::parse_by_extension(uri.path(), content);
        let hash = hash_str(content);
        
        // Extract supertype relationships for goToImplementation
        let mut supertypes = Vec::new();
        let class_kinds = [
            SymbolKind::CLASS, SymbolKind::INTERFACE, SymbolKind::STRUCT,
            SymbolKind::ENUM, SymbolKind::OBJECT,
        ];
        
        for sym in &data.symbols {
            if !class_kinds.contains(&sym.kind) { continue; }
            let start = sym.selection_range.start.line as usize;
            let limit = (start + 10).min(data.lines.len());
            let mut decl_lines: Vec<String> = Vec::new();
            for line in &data.lines[start..limit] {
                decl_lines.push(line.clone());
                if line.contains('{') { break; }
            }
            let supers = crate::resolver::extract_supers_from_lines(&decl_lines);
            let class_loc = Location { uri: uri.clone(), range: sym.selection_range };
            for super_name in supers {
                supertypes.push((super_name, class_loc.clone()));
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
    /// Uses pure `stale_keys_for` to compute removals and `file_contributions`
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
    /// contributions. Cache hits are already converted to FileIndexResult by
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

    /// Primitive: drain a `FileContributions` into the DashMaps.
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
    }

    /// (Re-)parse and index a single file's content in-place.
    /// Returns immediately (no-op) when content is identical to the last indexed version.
    /// Parse and index a single file.  Returns `Some(data)` when the file was
    /// actually (re-)parsed, or `None` when the content-hash matched the previous
    /// parse (no work done).  Callers that need to publish diagnostics should
    /// read `data.syntax_errors` from the returned value.
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
        
        // Use pure parse function
        let result = Self::parse_file(uri, content);
        
        // Apply result to index
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
            // Skip if already cached or a primitive/stdlib type.
            let idx = Arc::clone(&self);
            let uri2 = from_uri.clone();
            let sem = Arc::clone(&limit);
            tokio::spawn(async move {
                let _permit = sem.acquire_owned().await;
                tokio::task::spawn_blocking(move || {
                    // Resolve the type to a file URI.
                    let locs = crate::resolver::resolve_symbol(&idx, &type_name, None, &uri2);
                    if let Some(loc) = locs.first() {
                        let file_uri = loc.uri.to_string();
                        // Only bother if not already cached.
                        if idx.completion_cache.contains_key(&file_uri) { return; }
                        // This call builds + caches the items.
                        crate::resolver::symbols_from_uri_as_completions_pub(&idx, &file_uri);
                    }
                }).await.ok();
            });
        }
    }
    ///
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

    /// Signature lookup with optional dot-receiver context.
    /// When `receiver` is given (e.g. `"oneYearOlderInteractor"`), resolves its
    /// type, finds that type's file, and looks up `name` there specifically.
    /// Falls back to the plain name-based search if receiver resolution fails.
    pub fn find_fun_signature_with_receiver(&self, uri: &Url, name: &str, receiver: Option<&str>) -> String {
        if let Some(recv) = receiver {
            // Infer the receiver's type and resolve to its file.
            if let Some(type_name) = crate::resolver::infer_variable_type_raw(self, recv, uri) {
                let outer = type_name.split('.').next().unwrap_or(&type_name);
                let locs = crate::resolver::resolve_symbol(self, outer, None, uri);
                for loc in &locs {
                    if let Some(data) = self.files.get(loc.uri.as_str()) {
                        if let Some(sig) = collect_fun_params_text(name, loc.uri.as_str(), self) {
                            return sig;
                        }
                        // Also search by line range within the type's body.
                        let type_end = data.symbols.iter()
                            .find(|s| s.name == outer)
                            .map(|s| s.range.end.line)
                            .unwrap_or(u32::MAX);
                        for sym in data.symbols.iter().filter(|s| s.name == name && s.range.start.line <= type_end) {
                            if let Some(sig) = collect_params_from_line(&data.lines, sym.range.start.line as usize) {
                                return sig;
                            }
                        }
                    }
                }
            }
        }
        find_fun_signature_full(name, self, uri).unwrap_or_default()
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
    pub fn completions(&self, uri: &Url, position: Position, snippets: bool) -> Vec<CompletionItem> {
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
            } else { return vec![]; }
        } else if let Some(data) = self.files.get(uri.as_str()) {
            if let Some(l) = data.lines.get(position.line as usize) {
                line_owned = l.clone();
                &line_owned
            } else { return vec![]; }
        } else { return vec![]; };

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
        // The candidate list only changes when the structural context changes
        // (different dot receiver, different enclosing call, different line).
        // Typing more characters in the same word doesn't change the candidates —
        // the client filters them. Cache keyed on (uri, before_prefix, line).
        let cache_key = format!("{}|{}|{}", uri.as_str(), before_prefix, position.line);
        if let Ok(guard) = self.last_completion.lock() {
            if let Some((ref k, _, ref cached)) = *guard {
                if k == &cache_key {
                    return cached.clone();
                }
            }
        }
        // (compute below, then store in cache at the end)
        let dot_receiver = if before_prefix.ends_with('.') {
            // Grab the identifier immediately preceding the dot.
            let recv: String = before_prefix[..before_prefix.len() - 1]
                .chars()
                .rev()
                .take_while(|&c| c.is_alphanumeric() || c == '_')
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if recv.is_empty() { None } else { Some(recv) }
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
                    let items = crate::resolver::complete_symbol(self, &prefix, Some(&elem_type), uri, snippets);
                    if items.is_empty() {
                        // Type name known (e.g. generic param `T`, `StateType`) but not
                        // indexed — show a single hint item so the user sees the inferred type.
                        use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind};
                        return vec![CompletionItem {
                            label: format!("{recv}: {elem_type}"),
                            kind: Some(CompletionItemKind::TYPE_PARAMETER),
                            detail: Some(format!("Inferred type: {elem_type}")),
                            sort_text: Some("~hint".into()),
                            ..Default::default()
                        }];
                    }
                    return items;
                }
                // Recognised lambda param but type unresolvable — return empty to
                // avoid showing members of a completely unrelated type.
                // For `this` we also return empty (don't fall through to unrelated symbols).
                return vec![];
            }
        }

        let mut items = crate::resolver::complete_symbol(
            self,
            &prefix,
            dot_receiver.as_deref(),
            uri,
            snippets,
        );

        // Add scope-aware lambda parameter names (bare-word completion only).
        // Uses brace-depth backward scan so only in-scope params appear —
        // a closed sibling lambda's params are never included.
        if dot_receiver.is_none() {
            let prefix_lower = prefix.to_lowercase();
            for param in self.lambda_params_at(uri, position.line as usize) {
                if param.to_lowercase().starts_with(&prefix_lower) {
                    use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind};
                    if !items.iter().any(|i| i.label == param) {
                        items.push(CompletionItem {
                            label:     param.clone(),
                            kind:      Some(CompletionItemKind::VARIABLE),
                            sort_text: Some(format!("1.5:{param}")),
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

        items
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
            format!("```{}\n{} {}\n```", lang, symbol_kw(sym.kind), name)
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

// ─── file discovery ──────────────────────────────────────────────────────────

fn find_source_files(root: &Path) -> Vec<std::path::PathBuf> {
    let root_str = root.to_string_lossy();

    // Build --extension args dynamically from SOURCE_EXTENSIONS.
    let mut fd_args: Vec<&str> = vec!["--type", "f"];
    for ext in SOURCE_EXTENSIONS {
        fd_args.push("--extension");
        fd_args.push(ext);
    }
    fd_args.extend_from_slice(&[
        "--absolute-path",
        "--exclude", ".git",
        "--exclude", "build",
        "--exclude", "target",
        "--exclude", ".gradle",
        "--exclude", ".build",       // SwiftPM
        "--exclude", "DerivedData",  // Xcode
        "--exclude", "Generated",    // SwiftGen / R.swift codegen output
        ".",
    ]);
    fd_args.push(root_str.as_ref());

    // Prefer `fd` — order of magnitude faster for large trees.
    let fd_result = Command::new("fd").args(&fd_args).output();

    if let Ok(out) = fd_result {
        if out.status.success() {
            let paths: Vec<_> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(std::path::PathBuf::from)
                .collect();
            if !paths.is_empty() {
                return paths;
            }
        }
    }

    log::info!("fd not available or found nothing; falling back to walkdir");
    walkdir_find(root)
}

fn walkdir_find(root: &Path) -> Vec<std::path::PathBuf> {
    // Use `ignore` crate's WalkBuilder which respects .gitignore and global git excludes.
    const EXCLUDED_DIRS: &[&str] = &[
        ".git", "build", "target", ".gradle", ".build", "DerivedData", "Generated",
    ];
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .standard_filters(true) // respects .gitignore, .git/info/exclude, and global excludes
        .hidden(false)
        .parents(false);
    let walker = builder.build();
    for result in walker {
        if let Ok(entry) = result {
            let path = entry.path();
            // Skip excluded directories.
            if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                    if EXCLUDED_DIRS.contains(&dir_name) {
                        continue;
                    }
                }
            }
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                    if SOURCE_EXTENSIONS.contains(&ext) {
                        paths.push(path.to_path_buf());
                    }
                }
            }
        }
    }
    paths
}

/// Find source files changed within the last `elapsed_secs` seconds.
/// Used for the warm-start incremental scan to discover new/modified files.
/// Falls back to empty list if fd is unavailable (warm start still works via cache manifest).
fn find_source_files_newer_than(root: &Path, elapsed_secs: u64) -> Vec<PathBuf> {
    let root_str = root.to_string_lossy();
    let window = format!("{}s", elapsed_secs);
    let mut fd_args: Vec<&str> = vec!["--type", "f", "--changed-within", window.as_str()];
    for ext in SOURCE_EXTENSIONS {
        fd_args.push("--extension");
        fd_args.push(ext);
    }
    fd_args.extend_from_slice(&[
        "--absolute-path",
        "--exclude", ".git",
        "--exclude", "build",
        "--exclude", "target",
        "--exclude", ".gradle",
        "--exclude", ".build",
        "--exclude", "DerivedData",
        "--exclude", "Generated",
        ".",
        root_str.as_ref(),
    ]);
    match Command::new("fd").args(&fd_args).output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect(),
        _ => vec![],
    }
}

/// Discover source files using a warm-start optimisation when a cache exists.
///
/// Cold start (no cache): full `fd`/walkdir scan — same as before.
///
/// Warm start (cache present): use the cache's file manifest to avoid
/// the O(total_dirs) `fd` scan.  Only runs an incremental `fd --changed-within`
/// pass to catch files added or modified since the cache was last saved.
/// This is inspired by the rust-analyzer VFS and gopls file-manifest patterns.
fn warm_discover_files(root: &Path, cache: &IndexCache) -> Vec<PathBuf> {
    let cache_path = workspace_cache_path(root);

    // Compute how many seconds ago the cache was saved.
    let elapsed_secs = std::fs::metadata(&cache_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| {
            std::time::SystemTime::now().duration_since(t).ok()
        })
        .map(|d| d.as_secs().saturating_add(2)) // +2 for clock skew / rounding
        .unwrap_or(u64::MAX);

    if elapsed_secs == u64::MAX {
        // Can't read cache mtime — fall back to full scan.
        log::info!("warm_discover_files: cannot stat cache file, falling back to full scan");
        return find_source_files(root);
    }

    // Phase 1: all previously cached files (deleted files are filtered by the
    // subsequent mtime check in index_workspace_impl — `path.exists()` is not
    // called here to avoid an extra stat per file).
    let cached_paths: HashSet<String> = cache.entries.keys().cloned().collect();
    let mut paths: Vec<PathBuf> = cached_paths.iter().map(PathBuf::from).collect();

    // Phase 2: find files created or modified since the cache was saved.
    // These are the only files `fd` needs to scan for.
    let newer = find_source_files_newer_than(root, elapsed_secs);
    let new_count = newer.iter()
        .filter(|f| !cached_paths.contains(f.to_string_lossy().as_ref()))
        .count();
    for f in newer {
        // Only add files not already covered by the cache manifest.
        // Modified files are already in Phase 1; they will fail the mtime check
        // in index_workspace_impl and be re-parsed.
        if !cached_paths.contains(f.to_string_lossy().as_ref()) {
            paths.push(f);
        }
    }

    log::info!(
        "Warm start: {} cached + {} new files (scanned last {}s window)",
        cached_paths.len(), new_count, elapsed_secs
    );
    paths
}

// ─── hash helper ─────────────────────────────────────────────────────────────

/// Fast non-cryptographic hash of a string (FNV-1a, no extra dependencies).
fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ─── rg cross-file fallback ──────────────────────────────────────────────────

/// Walk up from `file` until a `.git` directory is found, returning that
/// ancestor as the project root.  Returns `None` if no `.git` is found.
fn walk_to_git_root(file: &Path) -> Option<std::path::PathBuf> {
    let mut cur = file.parent()?;
    loop {
        if cur.join(".git").exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

/// Given a list of type names (e.g. `["MviViewModel", "ViewModel"]`), find the
/// source files in `root` that declare them using a fast `rg` search.
/// Returns deduplicated, existing paths (at most one file per type).
fn find_files_for_types(names: &[String], root: &Path) -> Vec<PathBuf> {
    if names.is_empty() { return vec![]; }
    // Build a single alternation pattern: `(class|abstract class|...) (TypeA|TypeB)`
    let alts = names.iter()
        .map(|n| regex_escape(n))
        .collect::<Vec<_>>()
        .join("|");
    let pattern = format!(
        r"(?:abstract\s+class|open\s+class|class|interface|object|struct|protocol)\s+(?:{alts})\b"
    );
    let mut cmd = Command::new("rg");
    cmd.args(["--no-heading", "--with-filename", "-l"]);
    for ext in SOURCE_EXTENSIONS { cmd.args(["--glob", &format!("*.{ext}")]); }
    cmd.args(["-e", &pattern]);
    cmd.arg(root);
    let out = match cmd.output() {
        Ok(o) if !o.stdout.is_empty() => o,
        _ => return vec![],
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect()
}

/// Return the best search root for rg/fd fallbacks given:
/// - `workspace_root` — the globally configured root (may point to a different project)
/// - `open_file`      — the file the user has open right now
///
/// If `open_file` lives inside `workspace_root`, use `workspace_root`.
/// Otherwise walk up from `open_file` to find a `.git` root and use that,
/// so rg searches the *actual* project even when the workspace config is stale.
pub(crate) fn effective_rg_root(
    workspace_root: Option<&Path>,
    open_file:      Option<&Path>,
) -> Option<std::path::PathBuf> {
    match (workspace_root, open_file) {
        (Some(root), Some(fp)) if fp.starts_with(root) => Some(root.to_path_buf()),
        (_, Some(fp)) => walk_to_git_root(fp)
            .or_else(|| fp.parent().map(|p| p.to_path_buf()))
            .or_else(|| std::env::current_dir().ok()),
        (Some(root), None) => Some(root.to_path_buf()),
        (None, None) => std::env::current_dir().ok(),
    }
}

/// Run `rg` to find definition sites for `name`, scoped to `root`.
///
/// When `root` is an absolute path, rg outputs absolute paths in results.
/// Passing workspace root here is essential; without it rg would search
/// from CWD which may not be the project when spawned by the editor.
pub(crate) fn rg_find_definition(name: &str, root: Option<&Path>) -> Vec<Location> {
    let pattern = crate::resolver::build_rg_pattern(name);

    // Use the provided root, or fall back to CWD (which editors like Helix
    // set to the workspace root when spawning the LSP server).
    let search_root: std::borrow::Cow<Path> = match root {
        Some(r) => std::borrow::Cow::Borrowed(r),
        None    => std::borrow::Cow::Owned(std::env::current_dir().unwrap_or_default()),
    };

    let mut cmd = Command::new("rg");
    cmd.args([
        "--no-heading",
        "--with-filename",
        "--line-number",
        "--column",
        // NOTE: rg has no --absolute-path flag; absolute output comes from
        // passing an absolute search root as the positional argument.
    ]);
    for ext in SOURCE_EXTENSIONS {
        cmd.args(["--glob", &format!("*.{ext}")]);
    }
    cmd.args(["-e", &pattern]);
    cmd.arg(search_root.as_ref());

    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| parse_rg_line_with_content_rooted(l, &search_root).map(|(loc, _)| loc))
        .collect()
}

/// Run `rg` to find all *usages* of `name` in the project.
///
/// Uses `--word-regexp` so only whole-word matches are returned.
/// If `include_decl` is false, declaration lines are filtered out by
/// excluding lines that contain declaration keywords before `name`.
/// If `from_uri` is provided, the source file is excluded when
/// `include_decl` is false (the definition is already known).
pub fn rg_find_references(
    name:         &str,
    parent_class: Option<&str>,
    declared_pkg: Option<&str>,
    root:         Option<&Path>,
    include_decl: bool,
    from_uri:     &Url,
    // Absolute file paths where `name` is declared — always included in bare-word
    // search so the declaration site itself is never missed (it uses bare `Name`,
    // not the qualified `Parent.Name` form that Pass A searches for).
    decl_files:   &[String],
) -> Vec<Location> {
    let search_root: std::borrow::Cow<Path> = match root {
        Some(r) => std::borrow::Cow::Borrowed(r),
        None    => std::borrow::Cow::Owned(std::env::current_dir().unwrap_or_default()),
    };

    let safe_name: String = regex_escape(name);
    let decl_kws = ["class ", "interface ", "object ", "fun ", "val ", "var ",
                    "typealias ", "enum class ", "enum ",
                    // Swift
                    "struct ", "protocol ", "func ", "let ", "extension "];

    let filter = |(loc, content): (Location, String)| -> Option<Location> {
        let trimmed = content.trim_start();
        // Import and package lines are never real references.
        if trimmed.starts_with("import ") || trimmed.starts_with("package ") {
            return None;
        }
        if !include_decl {
            let is_decl = decl_kws.iter().any(|kw| content.contains(kw))
                && loc.uri.as_str() == from_uri.as_str();
            if is_decl { return None; }
        }
        Some(loc)
    };

    if let Some(parent) = parent_class {
        // ── Scoped references: parent class is known ──────────────────────────
        //
        // Pass A: qualified form `ParentClass.Name` — works in any file.
        let safe_parent = regex_escape(parent);
        let qualified_pat = format!(r"\b{}\.\b{}\b", safe_parent, safe_name);
        let mut locs: Vec<Location> = rg_raw(&qualified_pat, &search_root)
            .into_iter()
            .filter_map(filter)
            .collect();
        eprintln!("[refs] parent={parent:?} Pass-A qualified={} locs", locs.len());

        // Pass B: bare `Name` restricted to files that directly import the inner
        // class itself (`import …ParentClass.Name` or `import …ParentClass.*`)
        // OR are in the same package.
        //
        // NOTE: we intentionally do NOT match files that only import the parent
        // class itself (`import …ParentClass`) — those files use the qualified
        // form `ParentClass.Name` which is already captured by Pass A, and
        // including them causes massive false-positive counts (e.g. every
        // ViewModel importing another ViewModel that also has a sealed `Effect`).
        //
        // Step B1 — files with explicit inner-class import.
        // Pattern must match the parent and name as ADJACENT dot-segments:
        //   import …ParentClass.Name   or   import …ParentClass.*
        // NOT files that merely mention both words (e.g. OtherContract.State).
        let direct_import_pat = format!(
            r"import[^\n]*\b{}\.({}|\*)\b",
            safe_parent, safe_name
        );
        let candidate_files = rg_files_with_matches(&direct_import_pat, &search_root);

        // Step B2 — files in the same package as the parent class declaration.
        // NOTE: for inner classes, same-package files use the QUALIFIED form
        // `ParentClass.Name` which is already caught by Pass A. Adding them to
        // the bare-name search causes false positives (e.g. AbilitiesSectionViewModel
        // in the same package has its own `State`). So we skip same-package here.

        // Merge candidate file sets.
        // Always include declaration files so the declaration site itself is
        // never missed (it uses bare `Name`, not the qualified `Parent.Name` form).
        let mut all_files: Vec<String> = candidate_files;
        for f in decl_files {
            if !all_files.contains(f) { all_files.push(f.clone()); }
        }

        if !all_files.is_empty() {
            let bare_hits = rg_word_in_files(&safe_name, &all_files);
            eprintln!("[refs] Pass-B candidate files={} bare_hits={}", all_files.len(), bare_hits.len());
            for (loc, content) in bare_hits {
                if let Some(loc) = filter((loc, content)) {
                    // Deduplicate against the qualified hits.
                    if !locs.iter().any(|l: &Location| l.uri == loc.uri && l.range.start == loc.range.start) {
                        locs.push(loc);
                    }
                }
            }
        }

        locs
    } else if let Some(dpkg) = declared_pkg {
        // ── Top-level symbol with known declared package ──────────────────────
        // Only search files that import `declared_pkg.Name` or `declared_pkg.*`
        // or are in the same package. This avoids the "13000 matches for Effect"
        // problem where every ViewModel has an inner class with the same name.
        let safe_pkg = regex_escape(dpkg);
        let import_pat = format!(
            r"import[^\n]*\b{safe_pkg}\b[^\n]*\b{safe_name}\b|import[^\n]*\b{safe_pkg}\b\.\*"
        );
        let pkg_pat = format!(r"^\s*package\s+{safe_pkg}\s*$");

        let mut candidate_files = rg_files_with_matches(&import_pat, &search_root);
        for f in rg_files_with_matches(&pkg_pat, &search_root) {
            if !candidate_files.contains(&f) { candidate_files.push(f); }
        }

        if candidate_files.is_empty() {
            return vec![];
        }
        rg_word_in_files(&safe_name, &candidate_files)
            .into_iter()
            .filter_map(filter)
            .collect()
    } else {
        // ── Fully unscoped: lowercase / unknown symbol ────────────────────────
        let mut cmd = Command::new("rg");
        cmd.args([
            "--no-heading", "--with-filename", "--line-number", "--column",
            "--word-regexp",
        ]);
        for ext in SOURCE_EXTENSIONS {
            cmd.args(["--glob", &format!("*.{ext}")]);
        }
        cmd.args(["-e", &safe_name]);
        cmd.arg(search_root.as_ref());

        let out = match cmd.output() {
            Ok(o) if !o.stdout.is_empty() => o,
            _ => return vec![],
        };

        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| parse_rg_line_with_content_rooted(l, &search_root))
            .filter_map(filter)
            .collect()
    }
}

fn regex_escape(s: &str) -> String {
    s.chars().flat_map(|c| {
        if c.is_alphanumeric() || c == '_' { vec![c] } else { vec!['\\', c] }
    }).collect()
}

/// Run rg with a regex pattern; return `(Location, line_content)` pairs.
fn rg_raw(pattern: &str, root: &Path) -> Vec<(Location, String)> {
    let mut cmd = Command::new("rg");
    cmd.args(["--no-heading", "--with-filename", "--line-number", "--column"]);
    for ext in SOURCE_EXTENSIONS {
        cmd.args(["--glob", &format!("*.{ext}")]);
    }
    cmd.args(["-e", pattern]).arg(root);
    let out = match cmd.output() {
        Ok(o) if !o.stdout.is_empty() => o,
        _ => return vec![],
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| parse_rg_line_with_content_rooted(l, root))
        .collect()
}

/// Run `rg -l` to get the list of files matching a pattern.
fn rg_files_with_matches(pattern: &str, root: &Path) -> Vec<String> {
    let mut cmd = Command::new("rg");
    cmd.arg("-l");
    for ext in SOURCE_EXTENSIONS {
        cmd.args(["--glob", &format!("*.{ext}")]);
    }
    cmd.args(["-e", pattern]).arg(root);
    let out = match cmd.output() {
        Ok(o) if !o.stdout.is_empty() => o,
        _ => return vec![],
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| {
            let p = std::path::Path::new(l);
            if p.is_absolute() {
                l.to_owned()
            } else {
                root.join(l).to_string_lossy().into_owned()
            }
        })
        .collect()
}

/// Run `rg --word-regexp NAME` restricted to specific files.
fn rg_word_in_files(safe_name: &str, files: &[String]) -> Vec<(Location, String)> {
    if files.is_empty() { return vec![]; }
    let out = match Command::new("rg")
        .args(["--no-heading", "--with-filename", "--line-number", "--column",
               "--word-regexp", "-e", safe_name, "--"])
        .args(files)
        .output()
    {
        Ok(o) if !o.stdout.is_empty() => o,
        _ => return vec![],
    };
    // Files passed to rg_word_in_files are already absolute (from rg_files_with_matches).
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| parse_rg_line_with_content_rooted(l, std::path::Path::new("/")))
        .collect()
}

fn parse_rg_line_with_content_rooted(line: &str, root: &Path) -> Option<(Location, String)> {
    let mut parts = line.splitn(4, ':');
    let file     = parts.next()?;
    let line_num: u32 = parts.next()?.trim().parse().ok()?;
    let col:      u32 = parts.next()?.trim().parse().ok()?;
    let content  = parts.next().unwrap_or("").to_string();

    let path = std::path::Path::new(file);
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };

    let uri = Url::from_file_path(&abs_path).ok()?;
    let pos = Position::new(line_num.saturating_sub(1), col.saturating_sub(1));
    Some((Location { uri, range: Range::new(pos, pos) }, content))
}

/// Quick heuristic rg-based implementor finder. Scans files that mention `name`
/// and returns locations where the line looks like a declaration/implementation
/// of that type (Kotlin/Java `class Foo : Interface`, `implements`, Swift
/// `class Foo: Protocol`, `struct Foo: Protocol`). This is a fallback when the
/// subtype index is empty during cold indexing.
pub fn rg_find_implementors(name: &str, root: Option<&Path>) -> Vec<Location> {
    let safe = name.to_string();
    let root = match root {
        Some(r) => r,
        None => return vec![],
    };
    // Search for the name in source files.
    let mut cmd = Command::new("rg");
    cmd.args(["--no-heading", "--with-filename", "--line-number", "--column", "-e", &safe]);
    for ext in SOURCE_EXTENSIONS { cmd.args(["--glob", &format!("*.{ext}")]); }
    cmd.arg(root);
    let out = match cmd.output() {
        Ok(o) if !o.stdout.is_empty() => o,
        _ => return vec![],
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| parse_rg_line_with_content_rooted(l, root))
        .filter_map(|(loc, content)| {
            let line = content.trim();
            // Heuristics: declaration-like lines
            // Kotlin/Java: class Foo, interface Foo, enum class Foo, class Foo : Interface
            // Java implements: class Foo implements Interface
            // Swift: class Foo: Protocol, struct Foo: Protocol, extension Foo: Protocol
            let lower = line.to_lowercase();
            if lower.contains("class ") || lower.contains("struct ") || lower.contains("interface") || lower.contains("enum") || lower.contains("extension ") {
                // Check that the name appears as a word and near a declaration keyword
                if line.contains(name) {
                    return Some(loc);
                }
            }
            None
        })
        .collect()
}

pub(crate) fn parse_rg_line(line: &str) -> Option<Location> {
    // format: /abs/path/to/File.kt:line:col:content
    let mut parts = line.splitn(4, ':');
    let file     = parts.next()?;
    let line_num: u32 = parts.next()?.trim().parse().ok()?;
    let col:      u32 = parts.next()?.trim().parse().ok()?;

    let path = std::path::Path::new(file);
    // Silently skip if rg somehow gave us a relative path.
    if !path.is_absolute() { return None; }

    let uri = Url::from_file_path(path).ok()?;
    let pos = Position::new(line_num.saturating_sub(1), col.saturating_sub(1));
    Some(Location { uri, range: Range::new(pos, pos) })
}

// ─── misc helpers ────────────────────────────────────────────────────────────

fn is_id_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Resolve the element type of `it` when inside a lambda.
///
/// Scans `before_cursor` (text from line start to cursor, ending with `it.`)
/// backward to find the lambda opening `{`, then the callee before it
/// (e.g. `users.forEach`), then the receiver (`users`).
///
/// Delegates to `lambda_receiver_type_from_context` for the actual inference.
fn find_it_element_type(before_cursor: &str, idx: &Indexer, uri: &Url) -> Option<String> {
    let brace_byte = before_cursor.rfind('{')?;
    let before_brace = &before_cursor[..brace_byte];
    lambda_receiver_type_from_context(before_brace, idx, uri)
}

/// Multi-line version of `find_it_element_type` for hover/goto-def contexts.
///
/// When hovering over `it`, the cursor is ON `it` in the lambda body — which
/// may be on a DIFFERENT line than the opening `{`.  The simple `rfind('{')` on
/// `before_cursor` would miss it.
///
/// Algorithm: scan backward from `cursor_line` tracking `{}` depth to find
/// the opening `{` of the immediately enclosing lambda.  Then inspect that
/// line for a receiver expression before the brace.
fn find_it_element_type_in_lines(
    lines:       &[String],
    cursor_line: usize,
    cursor_col:  usize,
    idx:         &Indexer,
    uri:         &Url,
) -> Option<String> {
    find_it_element_type_in_lines_impl(lines, cursor_line, cursor_col, idx, uri, false)
}

fn find_this_element_type_in_lines(
    lines:       &[String],
    cursor_line: usize,
    cursor_col:  usize,
    idx:         &Indexer,
    uri:         &Url,
) -> Option<String> {
    find_it_element_type_in_lines_impl(lines, cursor_line, cursor_col, idx, uri, true)
}

fn find_it_element_type_in_lines_impl(
    lines:       &[String],
    cursor_line: usize,
    cursor_col:  usize,
    idx:         &Indexer,
    uri:         &Url,
    for_this:    bool,
) -> Option<String> {
    // Scan right-to-left tracking brace depth.
    // Convention: depth starts at 0. `}` increments, `{` decrements.
    // When depth goes < 0, we've found the `{` that opens our enclosing lambda.
    //
    // IMPORTANT: On cursor_line, only scan characters *before* cursor_col.
    // Characters to the right of the cursor (e.g., closing `}`) must not affect
    // the depth; otherwise a balanced `{ it.name }` would never trigger depth < 0.
    let mut depth: i32 = 0;
    let scan_start = cursor_line.saturating_sub(15);

    for ln in (scan_start..=cursor_line).rev() {
        let line = match lines.get(ln) { Some(l) => l, None => continue };
        // On cursor_line restrict to chars at byte positions < cursor_col.
        let scan_slice: &str = if ln == cursor_line {
            let byte_bound = line.char_indices()
                .nth(cursor_col)
                .map(|(b, _)| b)
                .unwrap_or(line.len());
            &line[..byte_bound]
        } else {
            line.as_str()
        };

        for (bi, ch) in scan_slice.char_indices().rev() {
            match ch {
                '}' => depth += 1,
                '{' => {
                    depth -= 1;
                    if depth < 0 {
                        let before_brace = &scan_slice[..bi];
                        // Skip string interpolation `${`.
                        if before_brace.ends_with('$') { depth = 0; continue; }
                        // Skip named-param lambdas `{ name -> }` or `{ a, b -> }` — that's not `it`.
                        // Use depth-aware `->` detection to handle multi-param lambdas where
                        // `rest` starts with `,` not `->` (e.g. `{ loanId, isWustenrot ->`).
                        let after_brace = scan_slice[bi + 1..].trim_start();
                        if has_named_params_not_it(after_brace) {
                            depth = 0; continue;
                        }
                        if for_this {
                            // `this` only gets a hint from receiver lambdas (`T.() -> R`).
                            return lambda_receiver_this_type_from_context(before_brace, idx, uri);
                        }
                        let result = lambda_receiver_type_from_context(before_brace, idx, uri)
                            .or_else(|| lambda_receiver_type_named_arg_ml(
                                before_brace, 0, lines, ln, idx, uri,
                            ));
                        return result;
                    }
                }
                _ => {}
            }
        }
    }
    None
}



/// Returns true if `line` contains a lambda declaration that names `param_name`
/// as one of its parameters (handles single and multi-param patterns):
///   `{ param -> ... }`, `{ a, param, b -> ... }`
fn line_has_lambda_param(line: &str, param_name: &str) -> bool {
    // There may be multiple `->` on one line (e.g. inline + trailing lambda).
    // Iterate every `->` and check whether param_name is in the names before it.
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find("->") {
        let arrow_pos = search_from + rel;
        if let Some(brace_pos) = line[..arrow_pos].rfind('{') {
            let names_str = &line[brace_pos + 1..arrow_pos];
            for tok in names_str.split(',') {
                let tok = tok.trim();
                let n: String = tok.chars().take_while(|&c| c.is_alphanumeric() || c == '_').collect();
                if n == param_name { return true; }
            }
        }
        search_from = arrow_pos + 2;
    }
    false
}

/// Find the `{` byte position in `line` for the lambda that declares `param_name`.
/// Scans all `->` occurrences (a line may have multiple lambdas).
fn lambda_brace_pos_for_param(line: &str, param_name: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find("->") {
        let arrow_pos = search_from + rel;
        if let Some(brace_pos) = line[..arrow_pos].rfind('{') {
            let names_str = &line[brace_pos + 1..arrow_pos];
            for tok in names_str.split(',') {
                let tok = tok.trim();
                let n: String = tok.chars().take_while(|&c| c.is_alphanumeric() || c == '_').collect();
                if n == param_name { return Some(brace_pos); }
            }
        }
        search_from = arrow_pos + 2;
    }
    None
}

/// Multi-line version of `find_named_lambda_param_type` for hover/goto-def.
///
/// Scans the whole file (not just `before_cursor`) for `{ param_name ->`,
/// including the CURRENT line (needed when cursor is on the param name before
/// the `->` is written, or when scanning the declaration line itself).
///
/// Also handles multi-param lambdas `{ id, scan -> }`.
fn find_named_lambda_param_type_in_lines(
    lines:       &[String],
    param_name:  &str,
    cursor_line: usize,
    idx:         &Indexer,
    uri:         &Url,
) -> Option<String> {
    let scan_start = cursor_line.saturating_sub(40);
    // Include cursor_line itself (different from completion path which is exclusive).
    for ln in (scan_start..=cursor_line).rev() {
        let line = match lines.get(ln) { Some(l) => l, None => continue };
        if !line_has_lambda_param(line, param_name) { continue; }
        let brace_pos = lambda_brace_pos_for_param(line, param_name).unwrap_or(0);
        let before_brace = &line[..brace_pos];
        let pos = lambda_param_position_on_line(line, param_name);
        let result = lambda_receiver_type_from_context(before_brace, idx, uri)
            .or_else(|| lambda_receiver_type_named_arg_ml(before_brace, pos, lines, ln, idx, uri));
        if result.is_some() { return result; }
    }
    None
}

/// Resolve the element/receiver type for an EXPLICITLY NAMED lambda parameter.
///
/// Handles both same-line and multi-line lambda declarations:
///
/// Same-line:  `items.forEach { item -> item.`
/// Multi-line: `items.forEach { item ->\n    item.`  ← cursor on second line
///
/// Scans backward (up to 20 lines) for `{ param_name ->` to find where the lambda
/// was opened, then infers the element type from what's before the `{`.
fn find_named_lambda_param_type(
    before_cursor: &str,
    param_name:   &str,
    idx:          &Indexer,
    uri:          &Url,
    cursor_line:  usize,
) -> Option<String> {
    let lines = idx.live_lines.get(uri.as_str())
        .map(|ll| ll.clone())
        .or_else(|| idx.files.get(uri.as_str()).map(|f| f.lines.clone()));

    // 1. Check same line first — covers `items.forEach { item -> item.`
    //    Also handles multi-param: `items.map { a, b -> a.`
    if line_has_lambda_param(before_cursor, param_name) {
        if let Some(brace_pos) = lambda_brace_pos_for_param(before_cursor, param_name) {
            let before_brace = &before_cursor[..brace_pos];
            let pos = lambda_param_position_on_line(before_cursor, param_name);
            let result = lambda_receiver_type_from_context(before_brace, idx, uri)
                .or_else(|| lines.as_deref().and_then(|ls|
                    lambda_receiver_type_named_arg_ml(before_brace, pos, ls, cursor_line, idx, uri)
                ));
            if result.is_some() { return result; }
        }
    }

    // 2. Scan backward through previous lines.
    let lines = lines?;
    let scan_start = cursor_line.saturating_sub(20);
    for ln in (scan_start..cursor_line).rev() {
        let line = match lines.get(ln) { Some(l) => l, None => continue };
        if !line_has_lambda_param(line, param_name) { continue; }
        if let Some(brace_pos) = lambda_brace_pos_for_param(line, param_name) {
            let before_brace = &line[..brace_pos];
            let pos = lambda_param_position_on_line(line, param_name);
            let result = lambda_receiver_type_from_context(before_brace, idx, uri)
                .or_else(|| lambda_receiver_type_named_arg_ml(before_brace, pos, &lines, ln, idx, uri));
            if result.is_some() { return result; }
        }
    }
    None
}

/// Check whether `recv` looks like an explicitly-named lambda parameter
/// in the current editing context (same line or recent lines).
///
/// Used to avoid triggering lambda inference for ordinary local variables
/// that just happen to be lowercase.  Handles single and multi-param lambdas.
fn is_lambda_param(
    recv:        &str,
    before_cur:  &str,
    idx:         &Indexer,
    uri:         &Url,
    cursor_line: usize,
) -> bool {
    // Fast reject: if `recv` starts with uppercase or contains `.` it's a type/qualified
    // name, never a lambda parameter name.
    if recv.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) { return false; }
    if recv.contains('.') { return false; }

    if line_has_lambda_param(before_cur, recv) { return true; }

    let lines_opt = idx.live_lines.get(uri.as_str())
        .map(|ll| ll.clone())
        .or_else(|| idx.files.get(uri.as_str()).map(|f| f.lines.clone()));

    if let Some(lines) = lines_opt {
        // Only scan back up to 10 lines — lambda params declared further away
        // are practically out of scope for normal code.
        let scan_start = cursor_line.saturating_sub(10);
        for ln in (scan_start..cursor_line).rev() {
            if let Some(line) = lines.get(ln) {
                if line_has_lambda_param(line, recv) { return true; }
                // Stop early if we cross a closing brace at depth 0 — we've
                // left the enclosing lambda scope entirely.
                if line.trim_start().starts_with('}') { break; }
            }
        }
    }
    false
}

/// Shared core: given the text BEFORE the `{` that opens a lambda, infer
/// the element type that `it` / the named param will have.
///
/// Three cases:
///   A) `receiver.method { it }`          — infer element type from receiver
///   B) `plainFun(args) { it }`           — look up fun's last param type
///   C) `fn(arg1, { namedParam -> ... })` — look up fun's N-th param type
///   D) multi-line named-arg `name = {\n  it }` — resolved by callers via `_ml` variant
pub(crate) fn lambda_receiver_type_from_context(
    before_brace: &str,
    idx:          &Indexer,
    uri:          &Url,
) -> Option<String> {
    let trimmed = before_brace.trim_end();

    // Strip a trailing balanced `(args)` to expose the callee expression.
    let callee_raw = strip_trailing_call_args(trimmed).replace("?.", ".");
    let callee = callee_raw.trim(); // trim both ends — leading spaces from indentation matter

    // ── Case A: `receiver.method` ────────────────────────────────────────────
    // Use a depth-aware dot search so dots INSIDE argument lists are ignored
    // (e.g., `fn(Enum.VALUE, {` must not match the dot inside `Enum.VALUE`).
    if let Some(dot_pos) = find_last_dot_at_depth_zero(callee) {
        let receiver_expr = callee[..dot_pos].trim_end();
        let receiver_var: String = receiver_expr
            .chars().rev()
            .take_while(|&c| is_id_char(c))
            .collect::<String>()
            .chars().rev()
            .collect();
        // Extract method name (everything after the dot up to the first non-id char).
        let method: String = callee[dot_pos + 1..].trim_start()
            .chars().take_while(|&c| is_id_char(c))
            .collect();

        if !receiver_var.is_empty() {
            if let Some(raw) = crate::resolver::infer_variable_type_raw(idx, &receiver_var, uri) {
                if let Some(elem) = crate::resolver::extract_collection_element_type(&raw) {
                    return Some(elem);
                }
                // Non-collection receiver: prefer the method's own lambda param type when
                // the method is indexed (e.g. `flow.collectIn { it }` → T from `collectIn`'s
                // `block: suspend (T) -> Unit`).  Fall back to receiver type when the method
                // is not found (e.g. stdlib `run`, `apply`, `let` → receiver type is correct).
                if !method.is_empty() {
                    if let Some(ty) = fun_trailing_lambda_it_type(&method, idx, uri) {
                        return Some(ty);
                    }
                }
                let base: String = raw.chars().take_while(|&c| is_id_char(c)).collect();
                if !base.is_empty() && base.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                    return Some(base);
                }
            }
            if receiver_var.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                return Some(receiver_var);
            }
        }
    }

    // ── Case B: plain trailing lambda — `fnName(args) { it/this }` ─────────
    // Extract the trailing identifier from callee — handles cases where callee
    // is prefixed by outer-lambda context like `{ setState` (the `{` belongs
    // to an enclosing lambda, not this call).
    let trailing_fn: String = callee.chars().rev()
        .take_while(|&c| is_id_char(c))
        .collect::<String>()
        .chars().rev()
        .collect();
    if !trailing_fn.is_empty() {
        // Known stdlib scope function `with(receiver) { this }` — extract the
        // first argument as the receiver and infer its type directly.
        if trailing_fn == "with" {
            if let Some(recv_name) = extract_first_arg(trimmed) {
                if let Some(raw) = crate::resolver::infer_variable_type_raw(idx, recv_name, uri) {
                    let base: String = raw.chars().take_while(|&c| is_id_char(c)).collect();
                    if !base.is_empty() { return Some(base); }
                }
                // If recv_name starts uppercase it IS the type (companion / object ref).
                let base: String = recv_name.chars().take_while(|&c| is_id_char(c)).collect();
                if base.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                    return Some(base);
                }
            }
        }
        if let Some(ty) = fun_trailing_lambda_it_type(&trailing_fn, idx, uri) {
            return Some(ty);
        }
    }

    // ── Case C: inline lambda arg — `fn(arg, { param -> ... }, ...)` ─────────
    // `before_brace` ends inside an unclosed `(`, so scan backward to find
    // the function name and the positional index of this lambda argument.
    inline_lambda_param_type(trimmed, idx, uri)
}


/// Type-directed inference for `it` or `this` used as a call argument.
///
/// When `it`/`this` appears as an argument to a function — either as a **named arg**
/// (`param = it`) or as a **positional arg** (`fn(a, it)`) — look up the expected
/// parameter type and return it as the hint.
///
/// This mimics Kotlin's implicit-receiver / lambda-param resolution by type:
/// the compiler picks the in-scope `it` or `this` whose type satisfies the
/// expected parameter type.
///
/// Examples:
///   `.send(channel = this)` → `channel: SendChannel<...>` → `SendChannel`
///   `process(it)`           → first param of `process` → e.g. `Item`
///   `fn(a, it)`             → second param of `fn` → e.g. `String`
fn find_as_call_arg_type(
    lines:       &[String],
    cursor_line: usize,
    cursor_col:  usize,
    idx:         &Indexer,
    uri:         &Url,
) -> Option<String> {
    let line = lines.get(cursor_line)?;
    // Slice the line up to (but not including) the cursor position.
    let before_cursor = {
        let mut byte_end = line.len();
        let mut utf16 = 0usize;
        for (bi, ch) in line.char_indices() {
            if utf16 >= cursor_col { byte_end = bi; break; }
            utf16 += ch.len_utf16();
        }
        &line[..byte_end]
    };
    let col = before_cursor.chars().count();

    // ── Named arg: `param = ` just before cursor ─────────────────────────────
    let s = before_cursor.trim_end();
    if let Some(s) = s.strip_suffix('=') {
        if !s.ends_with(|c: char| "!<>=".contains(c)) {
            let s = s.trim_end();
            let ident_start = s.rfind(|c: char| !c.is_alphanumeric() && c != '_')
                .map(|i| i + 1).unwrap_or(0);
            let named_arg = &s[ident_start..];
            if !named_arg.is_empty()
                && named_arg.chars().next().map(|c| !c.is_uppercase()).unwrap_or(false)
            {
                let preceding = s[..ident_start].trim_end().chars().last();
                if matches!(preceding, Some('(') | Some(',')) {
                    if let Some(fn_full) = find_enclosing_call_name(lines, cursor_line, col) {
                        if let Some(fn_name) = fn_full.split('.').last().filter(|n| !n.is_empty()) {
                            if let Some(sig) = find_fun_signature_full(fn_name, idx, uri) {
                                if let Some(param_type) = find_named_param_type_in_sig(&sig, named_arg) {
                                    let base: String = param_type.trim()
                                        .chars().take_while(|&c| is_id_char(c)).collect();
                                    if !base.is_empty() { return Some(base); }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Positional arg: `fn(a, keyword)` ────────────────────────────────────
    // Scan backward tracking paren/bracket depth; count top-level commas to
    // determine which argument position the cursor is in.
    let mut depth: i32 = 0;
    let mut arg_pos: usize = 0;
    let scan_start = cursor_line.saturating_sub(20);

    for ln in (scan_start..=cursor_line).rev() {
        let chars: Vec<char> = lines[ln].chars().collect();
        let scan_to = if ln == cursor_line { col.min(chars.len()) } else { chars.len() };

        for i in (0..scan_to).rev() {
            match chars[i] {
                ')' | ']' => depth += 1,
                '(' | '[' => {
                    depth -= 1;
                    if depth < 0 {
                        if i == 0 { return None; }
                        // Extract function name (possibly dotted) before `(`.
                        let mut end = i;
                        while end > 0 && (is_id_char(chars[end - 1]) || chars[end - 1] == '.') {
                            end -= 1;
                        }
                        if end >= i { return None; }
                        let full_name: String = chars[end..i].iter().collect();
                        let fn_name = full_name.trim_matches('.')
                            .split('.').last().filter(|n| !n.is_empty())?;
                        let sig = find_fun_signature_full(fn_name, idx, uri)?;
                        let param_type = nth_fun_param_type_str(&sig, arg_pos)?;
                        let base: String = param_type.trim()
                            .chars().take_while(|&c| is_id_char(c)).collect();
                        return if base.is_empty() { None } else { Some(base) };
                    }
                }
                ',' if depth == 0 => arg_pos += 1,
                _ => {}
            }
        }
    }
    None
}


///
/// `this` in Kotlin refers to the implicit receiver only inside a **receiver lambda**
/// (`T.() -> R`).  In a regular lambda (`(T) -> R`) `this` is the enclosing class —
/// we must NOT emit a hint from the lambda in that case.
///
/// Rules:
///  - Case A `receiver.method { this }`: check if `method` has a receiver-lambda last
///    param (`T.() -> R`) — if so return `T`.  If method not indexed but is a known
///    stdlib scope function (`run`, `apply`, `also`, `let`), return the receiver type.
///  - Case B `with(receiver) { this }`: return the receiver's type (special-cased).
///  - Everything else: return `None` (don't hint `this` from the lambda).
fn lambda_receiver_this_type_from_context(
    before_brace: &str,
    idx:          &Indexer,
    uri:          &Url,
) -> Option<String> {
    let trimmed = before_brace.trim_end();
    let callee_raw = strip_trailing_call_args(trimmed).replace("?.", ".");
    let callee = callee_raw.trim();

    // ── Case A: `receiver.method` ────────────────────────────────────────────
    if let Some(dot_pos) = find_last_dot_at_depth_zero(callee) {
        let receiver_expr = callee[..dot_pos].trim_end();
        let receiver_var: String = receiver_expr
            .chars().rev()
            .take_while(|&c| is_id_char(c))
            .collect::<String>()
            .chars().rev()
            .collect();
        let method: String = callee[dot_pos + 1..].trim_start()
            .chars().take_while(|&c| is_id_char(c))
            .collect();

        if !receiver_var.is_empty() && !method.is_empty() {
            // Prefer the method's own receiver-lambda type (only for indexed fns).
            if let Some(ty) = fun_trailing_lambda_this_type(&method, idx, uri) {
                return Some(ty);
            }
            // Stdlib scope functions are receiver lambdas but not indexed.
            if SCOPE_FUNCTIONS.contains(&method.as_str()) {
                if let Some(raw) = crate::resolver::infer_variable_type_raw(idx, &receiver_var, uri) {
                    let base: String = raw.chars().take_while(|&c| is_id_char(c)).collect();
                    if !base.is_empty() { return Some(base); }
                }
                if receiver_var.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                    return Some(receiver_var);
                }
            }
        }
        return None; // Non-scope, non-receiver-lambda: `this` is enclosing class.
    }

    // ── Case B: `with(receiver) { this }` ───────────────────────────────────
    let trailing_fn: String = callee.chars().rev()
        .take_while(|&c| is_id_char(c))
        .collect::<String>()
        .chars().rev()
        .collect();
    if trailing_fn == "with" {
        if let Some(recv_name) = extract_first_arg(trimmed) {
            if let Some(raw) = crate::resolver::infer_variable_type_raw(idx, recv_name, uri) {
                let base: String = raw.chars().take_while(|&c| is_id_char(c)).collect();
                if !base.is_empty() { return Some(base); }
            }
            let base: String = recv_name.chars().take_while(|&c| is_id_char(c)).collect();
            if base.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                return Some(base);
            }
        }
    }

    None
}


/// opener like `  buildingSavings = ` or `  loan = ` spread across multiple
/// lines (the enclosing `(` is on a previous line).
///
/// Returns `Some(type_name)` for the Nth input type of the parameter's functional
/// type, where N = `lambda_param_pos` (0-based position of the named param in the
/// multi-param lambda, e.g. `{ loanId, isWustenrot -> }` → loanId=0, isWustenrot=1).
fn lambda_receiver_type_named_arg_ml(
    before_brace:      &str,
    lambda_param_pos:  usize,
    lines:             &[String],
    line_no:           usize,
    idx:               &Indexer,
    uri:               &Url,
) -> Option<String> {
    let named_arg = extract_named_arg_name(before_brace)?;

    // Find the enclosing function/constructor call by scanning backward.
    let callee_full = find_enclosing_call_name(lines, line_no, before_brace.len())?;

    // Use the LAST segment of a dotted callee as the function name to look up.
    // `DashboardProductsReducer.SheetReloadActions` → `SheetReloadActions`
    let fn_name = callee_full.split('.').last()?;

    // If callee is qualified (e.g. `DashboardProductsReducer.SheetReloadActions`),
    // resolve the outer class to its file and search only there.  This prevents
    // picking a same-named class from a different file when multiple classes share
    // the same short name (e.g. two `SheetReloadActions` in the same project).
    let sig = if let Some(dot) = callee_full.rfind('.') {
        let outer = &callee_full[..dot];
        // Find outer class file; try indexed files first (no rg), then rg fallback.
        let outer_file: Option<String> = {
            let locs = crate::resolver::resolve_symbol_no_rg(idx, outer, uri);
            locs.first().map(|l| l.uri.to_string())
                .or_else(|| {
                    // On-demand: use rg to find and index the outer class.
                    let rg_locs = rg_find_definition(
                        outer, idx.workspace_root.read().unwrap().as_deref()
                    );
                    for loc in &rg_locs {
                        if !idx.files.contains_key(loc.uri.as_str()) {
                            if let Ok(path) = loc.uri.to_file_path() {
                                if let Ok(content) = std::fs::read_to_string(&path) {
                                    idx.index_content(&loc.uri, &content);
                                }
                            }
                        }
                    }
                    rg_locs.first().map(|l| l.uri.to_string())
                })
        };
        if let Some(file_uri) = outer_file {
            // Try ALL symbols named `fn_name` in the outer-class file — the file
            // may have multiple same-named nested classes (e.g. two `SheetReloadActions`
            // in different reducers).  Pick the first one whose params contain `named_arg`.
            let sigs = collect_all_fun_params_texts(fn_name, &file_uri, idx);
            let found = sigs.into_iter()
                .find_map(|s| find_named_param_type_in_sig(&s, named_arg).map(|ty| (s, ty)));
            if let Some((_sig, param_type)) = found {
                return lambda_type_nth_input(&param_type, lambda_param_pos);
            }
            find_fun_signature_full(fn_name, idx, uri)
        } else {
            find_fun_signature_full(fn_name, idx, uri)
        }
    } else {
        find_fun_signature_full(fn_name, idx, uri)
    }?;

    let param_type = find_named_param_type_in_sig(&sig, named_arg)?;
    lambda_type_nth_input(&param_type, lambda_param_pos)
}

/// Detect the `IDENT =` named-arg pattern at the end of `before_brace`.
/// Returns the identifier if found (must be lowercase-first, not `!=`, `<=`, `>=`).
///
/// Also requires that the text BEFORE the identifier is only whitespace (or
/// comma + whitespace for same-line multi-arg calls), so that patterns like
/// `(isRefresh = { resultState ->` are NOT falsely matched as named args
/// (the `(` before `isRefresh` disqualifies it).
fn extract_named_arg_name(before_brace: &str) -> Option<&str> {
    let s = before_brace.trim_end();
    let s = s.strip_suffix('=')?;
    // Guard against `!=`, `<=`, `>=`, `==`
    if s.ends_with(|c: char| "!<>=".contains(c)) { return None; }
    let s = s.trim_end();
    // Extract trailing identifier
    let ident_start = s.rfind(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|i| i + 1)
        .unwrap_or(0);
    let ident = &s[ident_start..];
    if ident.is_empty() { return None; }
    // Named args start with a lowercase letter
    if ident.chars().next().map(|c| c.is_uppercase()).unwrap_or(true) { return None; }
    // Require the prefix to be only whitespace (optionally preceded by a comma).
    // This prevents `(isRefresh = {` from matching — the `(` before `isRefresh`
    // makes the prefix non-empty after stripping commas and whitespace.
    let prefix = s[..ident_start].trim_start().trim_start_matches(',').trim_start();
    if !prefix.is_empty() { return None; }
    Some(ident)
}

/// Find the type string of a named parameter `param_name` inside a
/// comma-separated parameter list text (output of `collect_fun_params_text`).
///
/// Handles `val`/`var` prefixes, strips them. Returns the full type string
/// (may be a functional type like `(String, Boolean) -> Unit`).
fn find_named_param_type_in_sig(sig: &str, param_name: &str) -> Option<String> {
    // Split by comma at depth 0 (respecting `()` only — NOT `<>` because `->` contains `>`
    // which would falsely decrement a `<>` depth counter).
    let mut parts: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    for (i, ch) in sig.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => { parts.push(&sig[start..i]); start = i + 1; }
            _ => {}
        }
    }
    if start < sig.len() { parts.push(&sig[start..]); }

    let colon_pat = format!("{param_name}:");
    for part in parts {
        let part = part.trim().trim_start_matches("val ").trim_start_matches("var ");
        // Exact param_name match (no suffix)
        let Some(col_pos) = part.find(&colon_pat) else { continue };
        let before = &part[..col_pos];
        if before.chars().last().map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false) {
            continue; // suffix match like `otherParam:`
        }
        let after = part[col_pos + colon_pat.len()..].trim();
        if !after.is_empty() { return Some(after.to_owned()); }
    }
    None
}

/// Return the Nth (0-based) input type from a functional type expression.
///
/// `lambda_type_nth_input("(String, Boolean) -> Unit", 0)` → `Some("String")`
/// `lambda_type_nth_input("(String, Boolean) -> Unit", 1)` → `Some("Boolean")`
/// `lambda_type_nth_input("() -> Unit", 0)` → `None`
fn lambda_type_nth_input(ty: &str, n: usize) -> Option<String> {
    let ty = ty.trim();
    // Strip `suspend` keyword — Kotlin allows `suspend (T) -> Unit` as a type.
    let ty = strip_suspend(ty);
    if !ty.starts_with('(') { return None; }
    // Find matching `)` using separate paren/angle depth so `>` in `->` is
    // never mistaken for a closing angle bracket.
    let mut paren_depth: i32 = 0;
    let mut _angle_depth: i32 = 0;
    let mut close = None;
    for (i, ch) in ty.char_indices() {
        match ch {
            '(' => paren_depth += 1,
            ')' => { paren_depth -= 1; if paren_depth == 0 { close = Some(i); break; } }
            '<' => _angle_depth += 1,
            '>' => _angle_depth -= 1,
            _ => {}
        }
    }
    let close = close?;
    let inner = ty[1..close].trim();
    if inner.is_empty() { return None; }

    // If `inner` is itself a function type (outer parens were just wrapping:
    // `((T) -> R)` → inner = `(T) -> R`), recurse into it.
    if inner.starts_with('(') && inner.contains("->") {
        if let Some(t) = lambda_type_nth_input(inner, n) {
            return Some(t);
        }
    }

    // Split inner by comma at depth 0.
    let mut args: Vec<&str> = Vec::new();
    let mut start = 0;
    let mut d: i32 = 0;
    for (i, ch) in inner.char_indices() {
        match ch {
            '(' | '<' | '[' => d += 1,
            ')' | '>' | ']' => d -= 1,
            ',' if d == 0 => { args.push(&inner[start..i]); start = i + 1; }
            _ => {}
        }
    }
    args.push(&inner[start..]);

    let arg = args.get(n).map(|s| s.trim())?;
    // Strip named-param prefix `name:`.
    let arg = if let Some(c) = arg.find(':') { arg[c + 1..].trim() } else { arg };
    // Strip `suspend` keyword from function-type args like `suspend (T) -> Unit`.
    let arg = strip_suspend(arg);
    // Allow dots for qualified types like `CreditCardDashboardInteractor.CardProduct`.
    let base: String = arg.chars().take_while(|&c| is_id_char(c) || c == '.').collect();
    // Trim any trailing dots.
    let base = base.trim_end_matches('.');
    if base.is_empty() || !base.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
        return None;
    }
    Some(base.to_owned())
}

/// 0-based index of `param_name` in a multi-param lambda opening `{ a, b, c ->`.
/// Returns 0 for single-param lambdas.
fn lambda_param_position_on_line(line: &str, param_name: &str) -> usize {
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find("->") {
        let arrow_pos = search_from + rel;
        if let Some(brace_pos) = line[..arrow_pos].rfind('{') {
            let names_str = &line[brace_pos + 1..arrow_pos];
            for (i, tok) in names_str.split(',').enumerate() {
                let tok = tok.trim();
                let n: String = tok.chars().take_while(|&c| c.is_alphanumeric() || c == '_').collect();
                if n == param_name { return i; }
            }
        }
        search_from = arrow_pos + 2;
    }
    0
}

/// Returns true if `after_open_brace` looks like the opening of an explicitly
/// named parameter lambda — single-param `{ name ->` or multi-param `{ a, b ->`.
///
/// Handles multi-param correctly by finding `->` via a depth-aware scan
/// (not just checking whether the text after the first word starts with `->`).
///
/// Returns false for:
///   - `{ it }`               — implicit single param
///   - `{ }` / `{`            — empty / block
///   - `{ setEvent(...)` }    — starts with a function call
fn has_named_params_not_it(after_open_brace: &str) -> bool {
    let s = after_open_brace.trim_start();
    // Find the first `->` at brace-depth 0 (ignoring `->` inside nested lambdas).
    let mut depth: i32 = 0;
    let bytes = s.as_bytes();
    let mut arrow_pos: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => { depth += 1; i += 1; }
            b'}' => { depth -= 1; i += 1; }
            b'-' if depth == 0 && i + 1 < bytes.len() && bytes[i + 1] == b'>' => {
                arrow_pos = Some(i); break;
            }
            _ => { i += 1; }
        }
    }
    let Some(ap) = arrow_pos else { return false; };
    let before_arrow = s[..ap].trim_end();
    // All tokens before `->` must be valid identifiers.
    // If any non-`it`, non-`_` identifier is present, it's a named-param lambda.
    for tok in before_arrow.split(',') {
        let tok = tok.trim();
        let name: String = tok.chars()
            .take_while(|&c| c.is_alphanumeric() || c == '_')
            .collect();
        if !name.is_empty() && name != "it" && name != "_" {
            return true;
        }
    }
    false
}

fn extract_first_arg(call_expr: &str) -> Option<&str> {
    let paren = call_expr.find('(')?;
    let rest = &call_expr[paren + 1..];
    let mut depth: i32 = 0;
    let mut end = rest.len();
    for (i, ch) in rest.char_indices() {
        match ch {
            '(' | '<' | '[' => depth += 1,
            ')' | ']' => { if depth == 0 { end = i; break; } depth -= 1; }
            '>' => depth -= 1,
            ',' if depth == 0 => { end = i; break; }
            _ => {}
        }
    }
    let arg = rest[..end].trim();
    if arg.is_empty() { None } else { Some(arg) }
}

/// Find the position of the last `.` that is at parenthesis/bracket depth 0
/// (scanning left-to-right so that `fn(Enum.VALUE,` returns None — the dot
/// is at depth 1 inside the argument list).
fn find_last_dot_at_depth_zero(s: &str) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut last_dot: Option<usize> = None;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '.' if depth == 0 => last_dot = Some(i),
            _ => {}
        }
    }
    last_dot
}

/// For an INLINE lambda argument `fn(a, b, { param -> ... })`:
/// find the enclosing function name and the 0-based position of this lambda,
/// then look up that function parameter's type.
fn inline_lambda_param_type(before_brace: &str, idx: &Indexer, uri: &Url) -> Option<String> {
    // Scan right-to-left to find the nearest unclosed `(`.
    // Convention: `)` increments depth, `(` decrements.  depth < 0 → found it.
    let mut depth: i32 = 0;
    let mut open_paren_byte = None;
    let mut comma_count: usize = 0;

    for (bi, ch) in before_brace.char_indices().rev() {
        match ch {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth < 0 { open_paren_byte = Some(bi); break; }
            }
            ',' if depth == 0 => comma_count += 1,
            _ => {}
        }
    }

    let open_pos = open_paren_byte?;
    let fn_name: String = before_brace[..open_pos]
        .trim_end()
        .chars().rev()
        .take_while(|&c| is_id_char(c))
        .collect::<String>()
        .chars().rev()
        .collect();

    if fn_name.is_empty() { return None; }

    let sig = find_fun_signature_full(&fn_name, idx, uri)?;
    let param_type = nth_fun_param_type_str(&sig, comma_count)?;
    lambda_type_first_input(&param_type)
}

/// Look up a function by name, find its last parameter's type, and return the
/// first input type if that parameter is a lambda/function type.
///
/// Example: `fun loadProduct(key: K, flow: Flow<T>, map: (ResultState<T>) -> Model)`
/// returns `Some("ResultState")` so that `it` in `loadProduct(...) { it }` resolves.
fn fun_trailing_lambda_it_type(fn_name: &str, idx: &Indexer, uri: &Url) -> Option<String> {
    let sig = find_fun_signature_full(fn_name, idx, uri)?;
    let last_type = last_fun_param_type_str(&sig)?;
    lambda_type_first_input(&last_type)
}

/// Kotlin stdlib scope functions whose lambda parameter is a receiver lambda `T.() -> R`.
/// For these, `this` inside the lambda refers to `T` (the receiver), so a type hint is valid.
const SCOPE_FUNCTIONS: &[&str] = &["run", "apply", "also", "let"];

/// Given a Kotlin function/lambda type, extracts the receiver type if it is a **receiver
/// lambda** (`T.() -> R` or `T.(Params) -> R`).  Returns `None` for regular lambdas
/// (`(T) -> R`) since `this` in those refers to the enclosing class, not the param.
fn lambda_type_receiver(ty: &str) -> Option<String> {
    let ty = strip_suspend(ty.trim());
    if let Some(dot_paren) = ty.find(".(") {
        let receiver = ty[..dot_paren].trim();
        let base: String = receiver.chars().take_while(|&c| is_id_char(c) || c == '.').collect();
        let base = base.trim_end_matches('.');
        if !base.is_empty() {
            return Some(base.to_owned());
        }
    }
    None
}

/// Like `fun_trailing_lambda_it_type` but for `this`: only returns a type when
/// the trailing lambda parameter is a **receiver lambda** `T.() -> R`.
fn fun_trailing_lambda_this_type(fn_name: &str, idx: &Indexer, uri: &Url) -> Option<String> {
    let sig = find_fun_signature_full(fn_name, idx, uri)?;
    let last_type = last_fun_param_type_str(&sig)?;
    lambda_type_receiver(&last_type)
}

/// Collect the full parameter-list text for a function named `fn_name`.
/// Fast path only — no rg, no disk I/O, no index mutations.
/// Used by signature help (fires on every keystroke).
fn find_fun_signature(fn_name: &str, idx: &Indexer, uri: &Url) -> Option<String> {
    // 1. Import-aware resolution using only already-indexed files (no rg/disk).
    let locs = crate::resolver::resolve_symbol_no_rg(idx, fn_name, uri);
    for loc in &locs {
        let file_uri_str = loc.uri.as_str();
        if let Some(data) = idx.files.get(file_uri_str) {
            let start_line = loc.range.start.line as usize;
            if let Some(sig) = collect_params_from_line(&data.lines, start_line) {
                return Some(sig);
            }
        }
    }

    // 2. Fallback: current file → all already-indexed files (name-only scan).
    if let Some(sig) = collect_fun_params_text(fn_name, uri.as_str(), idx) {
        return Some(sig);
    }
    for entry in idx.files.iter() {
        if entry.key() == uri.as_str() { continue; }
        if let Some(sig) = collect_fun_params_text(fn_name, entry.key(), idx) {
            return Some(sig);
        }
    }
    None
}

/// Full signature lookup including rg + on-demand indexing.
/// Used by hover and lambda type inference where latency is acceptable.
fn find_fun_signature_full(fn_name: &str, idx: &Indexer, uri: &Url) -> Option<String> {
    if let Some(sig) = find_fun_signature(fn_name, idx, uri) {
        return Some(sig);
    }
    // Slow path: rg to locate the definition, index on-demand.
    let locs = rg_find_definition(fn_name, idx.workspace_root.read().unwrap().as_deref());
    for loc in &locs {
        let file_uri_str = loc.uri.as_str();
        if !idx.files.contains_key(file_uri_str) {
            if let Ok(path) = loc.uri.to_file_path() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    idx.index_content(&loc.uri, &content);
                }
            }
        }
        if let Some(sig) = collect_fun_params_text(fn_name, file_uri_str, idx) {
            return Some(sig);
        }
    }
    None
}

/// Collect everything between the outer `(…)` of a function's parameter list.
/// Scans the symbol's start line and up to 20 following lines.
/// Matches both top-level `fun` (FUNCTION) and class methods (METHOD).
fn collect_fun_params_text(fn_name: &str, uri_str: &str, idx: &Indexer) -> Option<String> {
    collect_all_fun_params_texts(fn_name, uri_str, idx).into_iter().next()
}

/// Like `collect_fun_params_text` but returns ALL params texts for every symbol
/// named `fn_name` in the file (a file may have multiple same-named nested classes).
fn collect_all_fun_params_texts(fn_name: &str, uri_str: &str, idx: &Indexer) -> Vec<String> {
    let data = match idx.files.get(uri_str) { Some(d) => d, None => return vec![] };
    let start_lines: Vec<usize> = data.symbols.iter()
        .filter(|s| s.name == fn_name
               && (s.kind == SymbolKind::FUNCTION
                   || s.kind == SymbolKind::METHOD
                   || s.kind == SymbolKind::CLASS
                   || s.kind == SymbolKind::STRUCT))  // data class → STRUCT
        .map(|s| s.range.start.line as usize)
        .collect();

    start_lines.into_iter().filter_map(|start_line| collect_params_from_line(&data.lines, start_line)).collect()
}

fn collect_params_from_line(lines: &[String], start_line: usize) -> Option<String> {
    // Walk forward from the `fun` line accumulating chars until the outermost
    // `)` closes — that ends the parameter list.
    // We only track `()` depth (NOT `<>`) to avoid false-triggers on `->` arrows.
    let mut paren_depth: i32 = 0;
    let mut found_open = false;
    let mut params = String::new();

    'outer: for ln in start_line..start_line + 20 {
        let line = match lines.get(ln) { Some(l) => l, None => break };
        let mut chars = line.char_indices().peekable();
        while let Some((_, ch)) = chars.next() {
            match ch {
                '(' => {
                    paren_depth += 1;
                    if paren_depth == 1 { found_open = true; continue; }
                    if found_open { params.push(ch); }
                }
                ')' => {
                    paren_depth -= 1;
                    if found_open && paren_depth == 0 { break 'outer; }
                    if found_open { params.push(ch); }
                }
                _ if found_open => params.push(ch),
                _ => {}
            }
        }
        if found_open { params.push('\n'); }
    }

    if params.is_empty() { None } else { Some(params) }
}

/// Split the flattened parameter list by `,` at depth-0 (respecting `()`, `<>`).
/// Returns the type string of the parameter at position `n` (0-based).
/// Falls back to the last parameter if `n` is out of range.
///
/// NOTE: `->` in Kotlin functional types (e.g. `(Boolean) -> Flow<T>`) contains
/// `>` which would falsely decrement `<>` depth.  We skip the `>` of any `->` by
/// tracking the previous character.
fn nth_fun_param_type_str(params_text: &str, n: usize) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    let mut prev = '\0';
    for (i, ch) in params_text.char_indices() {
        match ch {
            '(' | '<' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            // Skip `>` of `->` so lambda return arrows don't upset `<>` depth.
            '>' if prev != '-' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&params_text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        prev = ch;
    }
    parts.push(&params_text[start..]);
    // Drop trailing-comma empty parts (Kotlin allows `fun f(a: A, b: B,) {}`).
    parts.retain(|p| !p.trim().is_empty());
    if parts.is_empty() { return None; }

    let param = parts.get(n).unwrap_or_else(|| parts.last().unwrap()).trim();
    // Strip leading modifiers (`vararg`, `crossinline`, `noinline`).
    let param = param.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_');
    let colon = param.find(':')?;
    Some(param[colon + 1..].trim().to_owned())
}

fn last_fun_param_type_str(params_text: &str) -> Option<String> {
    // Count top-level parameters (same `->` skip logic as nth_fun_param_type_str).
    let count = {
        let mut n = 1usize;
        let mut depth: i32 = 0;
        let mut prev = '\0';
        for ch in params_text.chars() {
            match ch {
                '(' | '<' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                '>' if prev != '-' => depth -= 1,
                ',' if depth == 0 => n += 1,
                _ => {}
            }
            prev = ch;
        }
        n
    };
    nth_fun_param_type_str(params_text, count.saturating_sub(1))
}

/// Given a Kotlin function/lambda type `(A, B, ...) -> R`, return the base name
/// of the first input type `A`.  Returns `None` for `() -> Unit` (no `it`).
///
/// Examples:
///   `(ResultState<T>) -> Model`         → `Some("ResultState")`
///   `(String, Int) -> Unit`             → `Some("String")`
///   `() -> Unit`                        → `None`
///   `((T) -> ProductDetailSheetModel)`  → `Some("T")`  (strips outer wrapping parens)
fn lambda_type_first_input(ty: &str) -> Option<String> {
    let ty = ty.trim();
    // Strip `suspend` keyword — Kotlin allows `suspend (T) -> Unit` as a type.
    let ty = strip_suspend(ty);
    // Receiver lambda: `State.() -> State` or `State.(Param) -> R`
    // The implicit receiver is the `it`/`this`-equivalent inside the lambda.
    if let Some(dot_paren) = ty.find(".(") {
        let receiver = ty[..dot_paren].trim();
        let base: String = receiver.chars().take_while(|&c| is_id_char(c) || c == '.').collect();
        let base = base.trim_end_matches('.');
        if !base.is_empty() && base.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            return Some(base.to_owned());
        }
    }
    // Must start with `(` to be a function type.
    if !ty.starts_with('(') { return None; }
    // Find matching `)` using separate paren/angle depth counters so `>` in `->`
    // is never mistaken for a closing angle bracket.
    let mut paren_depth: i32 = 0;
    let mut _angle_depth: i32 = 0;
    let mut close = None;
    for (i, ch) in ty.char_indices() {
        match ch {
            '(' => paren_depth += 1,
            ')' => { paren_depth -= 1; if paren_depth == 0 { close = Some(i); break; } }
            '<' => _angle_depth += 1,
            '>' => _angle_depth -= 1,
            _ => {}
        }
    }
    let close = close?;
    let inner = ty[1..close].trim();
    if inner.is_empty() { return None; }

    // If `inner` is itself a function type (outer parens were just wrapping:
    // `((T) -> R)` → inner = `(T) -> R`), recurse into it.
    if inner.starts_with('(') && inner.contains("->") {
        if let Some(t) = lambda_type_first_input(inner) {
            return Some(t);
        }
    }

    // Take the first type argument (before the first `,` at depth 0).
    let mut first = inner;
    let mut d: i32 = 0;
    for (i, ch) in inner.char_indices() {
        match ch {
            '(' | '<' | '[' => d += 1,
            ')' | '>' | ']' => d -= 1,
            ',' if d == 0 => { first = &inner[..i]; break; }
            _ => {}
        }
    }

    // Strip any named-param prefix `name:` (Kotlin allows `(name: Type) -> R`)
    let first = if let Some(colon) = first.find(':') {
        first[colon + 1..].trim()
    } else {
        first.trim()
    };

    // Return the base type name (allow qualified names like `Outer.Inner`, strip generics).
    let base: String = first.chars().take_while(|&c| is_id_char(c) || c == '.').collect();
    let base = base.trim_end_matches('.');
    if base.is_empty() || !base.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
        return None;
    }
    Some(base.to_owned())
}

/// Strip a leading `suspend` keyword from a Kotlin function type string.
/// `"suspend (T) -> Unit"` → `"(T) -> Unit"`;  anything else unchanged.
/// Only strips when followed by `(` or `.` (receiver lambdas like `suspend T.() -> R`).
#[inline]
fn strip_suspend(ty: &str) -> &str {
    let rest = ty.strip_prefix("suspend").unwrap_or(ty);
    if rest.len() == ty.len() { return ty; } // no prefix stripped
    let rest = rest.trim_start();
    if rest.starts_with('(') || rest.starts_with('.') { rest } else { ty }
}

/// Strip a balanced trailing `(…)` argument list from the end of `s`.
/// `"collection.method(arg1, arg2)"` → `"collection.method"`
/// `"collection.forEach"`           → `"collection.forEach"`  (unchanged)
fn strip_trailing_call_args(s: &str) -> &str {
    if !s.ends_with(')') { return s; }
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 { return &s[..i]; }
            }
            _ => {}
        }
    }
    s
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
fn find_enclosing_call_name(lines: &[String], line_no: usize, col: usize) -> Option<String> {
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

/// Collect the declaration signature starting at `start_line`, spanning
/// multiple lines if the declaration continues (e.g. multiline constructor).
///
/// Walk backward from `decl_line` (exclusive) skipping blank lines, annotations
/// (`@…`), and visibility/modifier keywords, then collect either a `/** … */`
/// block-doc comment or a run of `//` line-doc comments.
///
/// Returns cleaned Markdown text, or `None` when no doc comment is found.
///
/// Handles:
/// - Kotlin: `/** ... */` (KDoc) and `//` line comments above annotations
/// - Java:   `/** ... */` (Javadoc)
/// - Strips leading `*` and `/** ` / ` */` markers
/// - Converts `@param`, `@return`, `@throws` tags to bold Markdown headings
/// - Skips `@suppress`, `@hide`, `@internal` — not user-facing
/// - Strips `[LinkText](url)` Markdown links from KDoc `[Symbol]` references
fn extract_doc_comment(lines: &[String], decl_line: usize) -> Option<String> {
    if decl_line == 0 { return None; }

    // Find the end of the doc comment block by scanning backward over
    // annotations, blank lines, and modifier-only lines.
    let mut search_end = decl_line;
    loop {
        if search_end == 0 { return None; }
        search_end -= 1;
        let trimmed = lines[search_end].trim();
        if trimmed.is_empty()
            || trimmed.starts_with('@')
            || is_modifier_line(trimmed)
        {
            if search_end == 0 { return None; }
            continue;
        }
        break;
    }

    let end_line = &lines[search_end];
    let end_trim = end_line.trim();

    // ── Block doc comment `/** ... */` ───────────────────────────────────────
    if end_trim.ends_with("*/") {
        // Find the opening `/**`
        let mut start = search_end;
        loop {
            let t = lines[start].trim();
            if t.starts_with("/**") || t.starts_with("/*") { break; }
            if start == 0 { return None; }
            start -= 1;
        }

        let raw_lines: Vec<&str> = lines[start..=search_end]
            .iter()
            .map(|l| l.as_str())
            .collect();
        return Some(render_block_doc(&raw_lines));
    }

    // ── Line doc comments `// …` ──────────────────────────────────────────────
    if end_trim.starts_with("//") {
        let mut start = search_end;
        while start > 0 && lines[start - 1].trim().starts_with("//") {
            start -= 1;
        }
        let text = lines[start..=search_end]
            .iter()
            .map(|l| {
                let t = l.trim();
                let stripped = if t.starts_with("/// ") {
                    &t[4..]
                } else if t.starts_with("//! ") {
                    &t[4..]
                } else if t.starts_with("// ") {
                    &t[3..]
                } else if t.starts_with("//") {
                    &t[2..]
                } else {
                    t
                };
                stripped.to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = format_doc_tags(&text);
        return if rendered.trim().is_empty() { None } else { Some(rendered) };
    }

    None
}

/// Returns `true` for lines that contain only Kotlin/Java modifiers/keywords
/// (e.g. `override`, `public final`) — we skip these when hunting for docs.
fn is_modifier_line(s: &str) -> bool {
    const MODIFIERS: &[&str] = &[
        "public", "private", "protected", "internal", "override", "open",
        "abstract", "sealed", "final", "static", "inline", "tailrec",
        "external", "suspend", "operator", "infix", "data", "inner",
        "companion", "lateinit", "const",
    ];
    s.split_whitespace().all(|w| MODIFIERS.contains(&w))
}

/// Strip `/** … */` markers and leading `*` from each line, then format tags.
fn render_block_doc(raw_lines: &[&str]) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in raw_lines {
        let t = line.trim();
        let t = t.strip_prefix("/**").unwrap_or(t);
        let t = t.strip_suffix("*/").unwrap_or(t);
        let t = t.strip_prefix("/*").unwrap_or(t);
        let t = if let Some(rest) = t.strip_prefix('*') { rest } else { t };
        let t = t.trim();
        // Skip the lone opening/closing marker lines that become empty
        if !t.is_empty() {
            out.push(t.to_owned());
        }
    }
    let joined = out.join("\n");
    format_doc_tags(&joined)
}

/// Convert KDoc/Javadoc tags to readable Markdown.
///
/// - `@param name desc`   → `**Parameters**\n- \`name\` desc`
/// - `@return desc`       → `**Returns**\n desc`
/// - `@throws T desc`     → `**Throws**\n- \`T\` desc`
/// - `@see ref`           → `**See also:** ref`
/// - `@since ver`         → `**Since:** ver`
/// - `[Symbol]` (KDoc)    → `` `Symbol` ``
/// - `{@code …}` (Java)   → `` `…` ``
/// - `{@link T}` (Java)   → `` `T` ``
/// - Suppressed: `@suppress`, `@hide`, `@internal`
fn format_doc_tags(text: &str) -> String {
    // Split on Javadoc/KDoc tag boundaries (lines starting with @).
    // We need to preserve multi-line tag bodies.
    let mut description: Vec<String> = Vec::new();
    let mut params:  Vec<(String, String)> = Vec::new();
    let mut returns: Option<String> = None;
    let mut throws:  Vec<(String, String)> = Vec::new();
    let mut see:     Vec<String> = Vec::new();
    let mut since:   Option<String> = None;

    // Accumulate current tag body across newlines.
    let mut cur_tag: Option<String>  = None;
    let mut cur_body: Vec<String>    = Vec::new();

    let flush = |cur_tag: &Option<String>, cur_body: &Vec<String>,
                  params: &mut Vec<(String, String)>,
                  returns: &mut Option<String>,
                  throws: &mut Vec<(String, String)>,
                  see: &mut Vec<String>,
                  since: &mut Option<String>| {
        let body = cur_body.join(" ").trim().to_owned();
        if let Some(tag) = cur_tag {
            match tag.as_str() {
                "param" | "property" => {
                    let (name, rest) = split_first_word(&body);
                    params.push((name.to_owned(), rest.trim().to_owned()));
                }
                "return" | "returns" => *returns = Some(body),
                "throws" | "exception" => {
                    let (name, rest) = split_first_word(&body);
                    throws.push((name.to_owned(), rest.trim().to_owned()));
                }
                "see"   => see.push(body),
                "since" => *since = Some(body),
                _ => {} // suppress, hide, internal, author, etc.
            }
        }
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('@') {
            // Flush previous tag
            flush(&cur_tag, &cur_body, &mut params, &mut returns,
                  &mut throws, &mut see, &mut since);
            cur_body.clear();

            let (tag, body) = split_first_word(rest);
            cur_tag = Some(tag.to_lowercase());
            if !body.is_empty() { cur_body.push(body.trim().to_owned()); }
        } else if cur_tag.is_some() {
            if !trimmed.is_empty() { cur_body.push(trimmed.to_owned()); }
        } else {
            description.push(trimmed.to_owned());
        }
    }
    flush(&cur_tag, &cur_body, &mut params, &mut returns,
          &mut throws, &mut see, &mut since);

    // Reassemble as Markdown.
    let mut md = description.join("\n").trim().to_owned();

    // Inline substitutions (KDoc links + Java {@code} / {@link})
    md = inline_doc_markup(&md);

    if !params.is_empty() {
        md.push_str("\n\n**Parameters**");
        for (name, desc) in &params {
            let desc = inline_doc_markup(desc);
            if desc.is_empty() {
                md.push_str(&format!("\n- `{name}`"));
            } else {
                md.push_str(&format!("\n- `{name}` — {desc}"));
            }
        }
    }
    if let Some(ret) = returns {
        md.push_str(&format!("\n\n**Returns** {}", inline_doc_markup(&ret)));
    }
    if !throws.is_empty() {
        md.push_str("\n\n**Throws**");
        for (ty, desc) in &throws {
            let desc = inline_doc_markup(desc);
            if desc.is_empty() {
                md.push_str(&format!("\n- `{ty}`"));
            } else {
                md.push_str(&format!("\n- `{ty}` — {desc}"));
            }
        }
    }
    if !see.is_empty() {
        let refs = see.iter().map(|s| format!("`{}`", s.trim())).collect::<Vec<_>>().join(", ");
        md.push_str(&format!("\n\n**See also:** {refs}"));
    }
    if let Some(s) = since {
        md.push_str(&format!("\n\n**Since:** {s}"));
    }

    md.trim().to_owned()
}

/// Apply inline markup substitutions.
fn inline_doc_markup(s: &str) -> String {
    // `{@code expr}` and `{@link Type}` → `expr` / `Type`
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('{') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        if let Some(end) = rest.find('}') {
            let inner = &rest[1..end]; // strip braces
            let inner = inner.trim_start_matches("@code").trim_start_matches("@link").trim();
            out.push('`');
            out.push_str(inner);
            out.push('`');
            rest = &rest[end + 1..];
        } else {
            out.push('{');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);

    // KDoc `[Symbol]` → `Symbol`
    // Avoid matching Markdown links `[text](url)` — only bare `[Word]`
    let out = regex_replace_kdoc_links(&out);
    out
}

/// Replace KDoc `[SymbolName]` (not followed by `(`) with `` `SymbolName` ``.
fn regex_replace_kdoc_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            // Find the closing `]`
            if let Some(rel) = bytes[i + 1..].iter().position(|&b| b == b']') {
                let end = i + 1 + rel;
                let inner = &s[i + 1..end];
                // Only treat as KDoc link if inner has no spaces (symbol name)
                // and is NOT followed by `(` (which would be a Markdown link)
                let next = bytes.get(end + 1).copied();
                if !inner.contains(' ') && next != Some(b'(') {
                    out.push('`');
                    out.push_str(inner);
                    out.push('`');
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Split `"word rest of string"` → `("word", "rest of string")`.
fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], &s[i..]),
        None    => (s, ""),
    }
}

/// Rules:
/// - Track `(` / `)` depth.
/// - Once depth is back to 0, the signature ends at the current line.
/// - A line ending with `{` signals the start of the body — strip the `{`
///   and stop (we don't want the body in the hover).
/// - Lines ending with `,` inside balanced parens always continue.
/// - Cap at 15 lines to avoid runaway on pathological files.
fn collect_signature(lines: &[String], start_line: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut depth: i32 = 0;

    for i in start_line..(start_line + 15).min(lines.len()) {
        let raw   = lines[i].trim();

        // Count parens in this line.
        for ch in raw.chars() {
            match ch { '(' => depth += 1, ')' => depth -= 1, _ => {} }
        }

        if raw.ends_with('{') {
            // Body starts — include the line without the brace (shows inheritance).
            let trimmed = raw.trim_end_matches('{').trim_end();
            if !trimmed.is_empty() { parts.push(trimmed.to_owned()); }
            break;
        }

        parts.push(raw.to_owned());

        // Signature ends when parens are balanced and the line doesn't
        // look like a continuation (trailing comma means more params follow).
        if depth <= 0 && !raw.ends_with(',') {
            break;
        }
    }

    parts.join("\n")
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

    // ── Pure parsing (SOLID refactoring tests) ──────────────────────────────

    #[test]
    fn parse_file_returns_symbols() {
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
        assert_eq!(result.data.symbols.len(), 2); // class + fun
        assert!(result.data.symbols.iter().any(|s| s.name == "Foo"));
        assert!(result.data.symbols.iter().any(|s| s.name == "bar"));
        assert!(result.error.is_none());
    }

    #[test]
    fn parse_file_extracts_supertypes() {
        let src = r#"
interface Base
class Child : Base
"#;
        let u = uri("/Child.kt");
        let result = Indexer::parse_file(&u, src);
        
        // Debug: print what we found
        eprintln!("Supertypes found: {:?}", result.supertypes);
        
        // Should find Child implements Base (interface Base has no supertypes)
        assert!(result.supertypes.iter().any(|(name, _)| name == "Base"));
    }

    #[test]
    fn apply_file_result_updates_index() {
        let src = "class TestClass";
        let u = uri("/Test.kt");
        let result = Indexer::parse_file(&u, src);
        
        let idx = Indexer::new();
        idx.apply_file_result(&result);
        
        // Verify symbol indexed
        assert!(idx.definitions.contains_key("TestClass"));
        let locs = idx.definitions.get("TestClass").unwrap();
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].uri, u);
    }

    #[test]
    fn apply_file_result_clears_stale_entries() {
        let u = uri("/Test.kt");
        
        // First parse: class OldName
        let result1 = Indexer::parse_file(&u, "class OldName");
        let idx = Indexer::new();
        idx.apply_file_result(&result1);
        assert!(idx.definitions.contains_key("OldName"));
        
        // Second parse: class NewName (same file)
        let result2 = Indexer::parse_file(&u, "class NewName");
        idx.apply_file_result(&result2);
        
        // OldName should be gone
        assert!(!idx.definitions.contains_key("OldName") || 
                idx.definitions.get("OldName").unwrap().is_empty());
        assert!(idx.definitions.contains_key("NewName"));
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

    // ── parse_rg_line ────────────────────────────────────────────────────────

    #[test]
    fn rg_line_absolute_path_parsed() {
        let line = "/home/user/project/Foo.kt:10:5:class Foo {";
        let loc = parse_rg_line(line).unwrap();
        assert_eq!(loc.range.start.line,      9); // 1-indexed → 0-indexed
        assert_eq!(loc.range.start.character, 4);
        assert_eq!(loc.uri.path(), "/home/user/project/Foo.kt");
    }

    #[test]
    fn rg_line_relative_path_ignored() {
        // Before the fix this would panic / produce a wrong URI
        let line = "src/Foo.kt:10:5:class Foo {";
        assert!(parse_rg_line(line).is_none(), "relative paths must be ignored");
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
        let items = idx.completions(&vm_uri, Position::new(4, dot_col), true);
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
        let items = idx.completions(&vm_uri, Position::new(4, col), true);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"findAll"), "findAll missing; got: {labels:?}");
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
        let items = idx2.completions(&u2, Position::new(3, col), false);
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

    // ── KDoc / Javadoc extraction ─────────────────────────────────────────────

    fn lines(src: &str) -> Vec<String> {
        src.lines().map(String::from).collect()
    }

    #[test]
    fn kdoc_simple_block_comment() {
        let src = r#"
/**
 * Does something useful.
 */
fun doThing() {}"#;
        let ls = lines(src);
        let decl = ls.iter().position(|l| l.contains("fun doThing")).unwrap();
        let doc = extract_doc_comment(&ls, decl).unwrap();
        assert!(doc.contains("Does something useful"), "got: {doc}");
        // extract_doc_comment returns plain text; no code block here
        assert!(!doc.contains("```"), "got: {doc}");
    }

    #[test]
    fn kdoc_with_params_and_return() {
        let src = r#"
/**
 * Fetches the widget.
 *
 * @param id The widget identifier.
 * @param flag Whether to refresh.
 * @return The widget or null.
 */
fun getWidget(id: Int, flag: Boolean): Widget? = null"#;
        let ls = lines(src);
        let decl = ls.iter().position(|l| l.contains("fun getWidget")).unwrap();
        let doc = extract_doc_comment(&ls, decl).unwrap();
        assert!(doc.contains("Fetches the widget"), "got: {doc}");
        assert!(doc.contains("**Parameters**"), "got: {doc}");
        assert!(doc.contains("`id`"), "got: {doc}");
        assert!(doc.contains("`flag`"), "got: {doc}");
        assert!(doc.contains("**Returns**"), "got: {doc}");
    }

    #[test]
    fn kdoc_skips_annotations() {
        let src = r#"
/**
 * Annotated function.
 */
@Suppress("unused")
@JvmStatic
fun annotated() {}"#;
        let ls = lines(src);
        let decl = ls.iter().position(|l| l.contains("fun annotated")).unwrap();
        let doc = extract_doc_comment(&ls, decl).unwrap();
        assert!(doc.contains("Annotated function"), "got: {doc}");
    }

    #[test]
    fn kdoc_no_comment_returns_none() {
        let src = "fun plain() {}";
        let ls = lines(src);
        assert!(extract_doc_comment(&ls, 0).is_none());
    }

    #[test]
    fn kdoc_line_comments() {
        let src = r#"// Short description.
// More detail.
fun withLineDoc() {}"#;
        let ls = lines(src);
        let decl = 2;
        let doc = extract_doc_comment(&ls, decl).unwrap();
        assert!(doc.contains("Short description"), "got: {doc}");
        assert!(doc.contains("More detail"), "got: {doc}");
    }

    #[test]
    fn kdoc_inline_code_and_links() {
        let src = r#"
/**
 * Use {@code Foo.bar()} or [Baz] to achieve this.
 */
fun example() {}"#;
        let ls = lines(src);
        let decl = ls.iter().position(|l| l.contains("fun example")).unwrap();
        let doc = extract_doc_comment(&ls, decl).unwrap();
        assert!(doc.contains("`Foo.bar()`"), "got: {doc}");
        assert!(doc.contains("`Baz`"), "got: {doc}");
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
    fn lambda_type_first_input_parses_correctly() {
        assert_eq!(lambda_type_first_input("(ResultState<T>) -> Model"), Some("ResultState".into()));
        assert_eq!(lambda_type_first_input("(String, Int) -> Unit"), Some("String".into()));
        assert_eq!(lambda_type_first_input("() -> Unit"), None);
        assert_eq!(lambda_type_first_input("(id: String, scan: String) -> Unit"), Some("String".into()));
        // Double-wrapped parens (Kotlin allows `((T) -> R)` as a type annotation):
        assert_eq!(lambda_type_first_input("((T) -> ProductDetailSheetModel)"), Some("T".into()));
        assert_eq!(lambda_type_first_input("((LoanDetail) -> Model)"), Some("LoanDetail".into()));
        // `->` arrow must not confuse angle-bracket depth tracking:
        assert_eq!(lambda_type_first_input("(Flow<T>) -> Unit"), Some("Flow".into()));
        // `suspend` prefix — Kotlin suspend function types like `suspend (T) -> Unit`:
        assert_eq!(lambda_type_first_input("suspend (T) -> Unit"), Some("T".into()));
        assert_eq!(lambda_type_first_input("suspend (value: LoanDetail) -> Unit"), Some("LoanDetail".into()));
        assert_eq!(lambda_type_first_input("suspend (String, Int) -> Unit"), Some("String".into()));
        assert_eq!(lambda_type_first_input("suspend () -> Unit"), None);
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
    fn lambda_type_nth_input_test() {
        assert_eq!(super::lambda_type_nth_input("(String, Boolean) -> Unit", 0), Some("String".into()));
        assert_eq!(super::lambda_type_nth_input("(String, Boolean) -> Unit", 1), Some("Boolean".into()));
        assert_eq!(super::lambda_type_nth_input("() -> Unit", 0), None);
        assert_eq!(super::lambda_type_nth_input("(SaveInfo) -> Unit", 0), Some("SaveInfo".into()));
        // suspend function type as whole outer type:
        assert_eq!(super::lambda_type_nth_input("suspend (T) -> Unit", 0), Some("T".into()));
        assert_eq!(super::lambda_type_nth_input("suspend (LoanDetail) -> Unit", 0), Some("LoanDetail".into()));
        assert_eq!(super::lambda_type_nth_input("suspend () -> Unit", 0), None);
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

    // ── rg_find_references scoping ───────────────────────────────────────────

    /// Write `content` to `dir/rel_path` and return the absolute path as String.
    fn write_temp(dir: &std::path::Path, rel_path: &str, content: &str) -> String {
        let p = dir.join(rel_path);
        if let Some(parent) = p.parent() { std::fs::create_dir_all(parent).unwrap(); }
        std::fs::write(&p, content).unwrap();
        p.to_str().unwrap().to_owned()
    }

    /// `rg_find_references` must not bleed references across sealed interfaces
    /// that share the same inner name (`Event`) but belong to different contracts.
    ///
    /// Layout:
    ///   activate_contract.kt   — declares interface ActivateUpdateAppContract { sealed interface Event }
    ///   other_contract.kt      — declares interface OtherContract             { sealed interface Event }
    ///   activate_vm.kt         — imports ActivateUpdateAppContract.Event, uses bare `Event`
    ///   other_vm.kt            — imports OtherContract.Event,             uses bare `Event`
    ///
    /// Finding refs for ActivateUpdateAppContract.Event must return hits in
    /// activate_contract.kt and activate_vm.kt ONLY — not other_vm.kt.
    #[test]
    fn refs_inner_class_does_not_bleed_across_contracts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        write_temp(root, "activate_contract.kt", concat!(
            "package com.example.activate\n",
            "interface ActivateUpdateAppContract {\n",
            "  sealed interface Event\n",
            "}\n",
        ));
        write_temp(root, "other_contract.kt", concat!(
            "package com.example.other\n",
            "interface OtherContract {\n",
            "  sealed interface Event\n",
            "}\n",
        ));
        write_temp(root, "activate_vm.kt", concat!(
            "package com.example.activate\n",
            "import com.example.activate.ActivateUpdateAppContract.Event\n",
            "class ActivateViewModel {\n",
            "  fun handle(e: Event) {}\n",
            "}\n",
        ));
        write_temp(root, "other_vm.kt", concat!(
            "package com.example.other\n",
            "import com.example.other.OtherContract.Event\n",
            "class OtherViewModel {\n",
            "  fun handle(e: Event) {}\n",
            "}\n",
        ));

        let activate_uri = Url::from_file_path(root.join("activate_contract.kt")).unwrap();
        let activate_decl = root.join("activate_contract.kt").to_str().unwrap().to_owned();

        // Simulate: cursor on declaration of Event inside ActivateUpdateAppContract.
        // parent_class = "ActivateUpdateAppContract", declared_pkg = "com.example.activate"
        let locs = super::rg_find_references(
            "Event",
            Some("ActivateUpdateAppContract"),
            Some("com.example.activate"),   // declared_pkg
            Some(root),
            true,  // include_declaration
            &activate_uri,
            &[activate_decl],
        );

        let hit_files: std::collections::HashSet<String> = locs.iter()
            .map(|l| l.uri.to_file_path().unwrap().file_name().unwrap().to_str().unwrap().to_owned())
            .collect();

        assert!(hit_files.contains("activate_contract.kt"),
            "should include declaration file; got: {hit_files:?}");
        assert!(hit_files.contains("activate_vm.kt"),
            "should include file that imports ActivateUpdateAppContract.Event; got: {hit_files:?}");
        assert!(!hit_files.contains("other_vm.kt"),
            "must NOT include file that only imports OtherContract.Event; got: {hit_files:?}");
        assert!(!hit_files.contains("other_contract.kt"),
            "must NOT include OtherContract declaration; got: {hit_files:?}");
    }

    /// When cursor is on `Event` inside a file that imports `OtherContract.Event`,
    /// refs must not include files that only import `ActivateUpdateAppContract.Event`.
    #[test]
    fn refs_inner_class_resolved_from_import_in_reference_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        write_temp(root, "activate_contract.kt", concat!(
            "package com.example.activate\n",
            "interface ActivateUpdateAppContract {\n",
            "  sealed interface Event\n",
            "}\n",
        ));
        write_temp(root, "other_contract.kt", concat!(
            "package com.example.other\n",
            "interface OtherContract {\n",
            "  sealed interface Event\n",
            "}\n",
        ));
        write_temp(root, "activate_vm.kt", concat!(
            "package com.example.activate\n",
            "import com.example.activate.ActivateUpdateAppContract.Event\n",
            "class ActivateViewModel {\n",
            "  fun handle(e: Event) {}\n",
            "}\n",
        ));
        write_temp(root, "other_vm.kt", concat!(
            "package com.example.other\n",
            "import com.example.other.OtherContract.Event\n",
            "class OtherViewModel {\n",
            "  fun handle(e: Event) {}\n",
            "}\n",
        ));

        // Simulate: cursor on `Event` inside other_vm.kt (a reference, not declaration).
        // resolve_symbol_via_import on other_vm.kt → parent=OtherContract, pkg=com.example.other
        let other_vm_uri = Url::from_file_path(root.join("other_vm.kt")).unwrap();
        let other_decl = root.join("other_contract.kt").to_str().unwrap().to_owned();

        let locs = super::rg_find_references(
            "Event",
            Some("OtherContract"),
            Some("com.example.other"),
            Some(root),
            true,
            &other_vm_uri,
            &[other_decl],
        );

        let hit_files: std::collections::HashSet<String> = locs.iter()
            .map(|l| l.uri.to_file_path().unwrap().file_name().unwrap().to_str().unwrap().to_owned())
            .collect();

        assert!(hit_files.contains("other_contract.kt"),
            "should include OtherContract declaration; got: {hit_files:?}");
        assert!(hit_files.contains("other_vm.kt"),
            "should include file importing OtherContract.Event; got: {hit_files:?}");
        assert!(!hit_files.contains("activate_vm.kt"),
            "must NOT include file importing ActivateUpdateAppContract.Event; got: {hit_files:?}");
    }

    /// Regression: when `decl_files` is unfiltered it includes ALL contracts that
    /// declare a `sealed interface Event`, causing every consumer ViewModel to appear
    /// in results for an unrelated contract's Event.
    ///
    /// Layout: two contracts each with `sealed interface Event`, two ViewModels each
    /// importing their own contract's Event.  Finding refs for DashboardContract.Event
    /// must NOT return VisitBranchViewModel even though both are in `decl_files` when
    /// unfiltered by enclosing-class.
    #[test]
    fn refs_decl_files_filtered_by_enclosing_class() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        write_temp(root, "DashboardContract.kt", concat!(
            "package com.example.dashboard\n",
            "interface DashboardContract {\n",
            "  sealed interface Event\n",
            "}\n",
        ));
        write_temp(root, "VisitBranchContract.kt", concat!(
            "package com.example.visitbranch\n",
            "interface VisitBranchContract {\n",
            "  sealed interface Event\n",
            "}\n",
        ));
        write_temp(root, "DashboardViewModel.kt", concat!(
            "package com.example.dashboard\n",
            "import com.example.dashboard.DashboardContract.Event\n",
            "class DashboardViewModel {\n",
            "  fun handle(e: Event) {}\n",
            "}\n",
        ));
        write_temp(root, "VisitBranchViewModel.kt", concat!(
            "package com.example.visitbranch\n",
            "import com.example.visitbranch.VisitBranchContract.Event\n",
            "class VisitBranchViewModel {\n",
            "  fun handle(e: Event) {}\n",
            "}\n",
        ));

        let dashboard_uri = Url::from_file_path(root.join("DashboardContract.kt")).unwrap();
        // decl_files filtered to only DashboardContract.kt (enclosing = DashboardContract)
        let dashboard_decl = root.join("DashboardContract.kt").to_str().unwrap().to_owned();

        let locs = super::rg_find_references(
            "Event",
            Some("DashboardContract"),
            Some("com.example.dashboard"),
            Some(root),
            true,
            &dashboard_uri,
            &[dashboard_decl],  // NOT including VisitBranchContract.kt
        );

        let hit_files: std::collections::HashSet<String> = locs.iter()
            .map(|l| l.uri.to_file_path().unwrap().file_name().unwrap().to_str().unwrap().to_owned())
            .collect();

        assert!(hit_files.contains("DashboardContract.kt"),
            "should include DashboardContract declaration; got: {hit_files:?}");
        assert!(hit_files.contains("DashboardViewModel.kt"),
            "should include DashboardViewModel; got: {hit_files:?}");
        assert!(!hit_files.contains("VisitBranchViewModel.kt"),
            "must NOT include VisitBranchViewModel; got: {hit_files:?}");
        assert!(!hit_files.contains("VisitBranchContract.kt"),
            "must NOT include VisitBranchContract; got: {hit_files:?}");
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

    #[test]
    fn apply_file_result_removes_both_stale_qualified_keys() {
        let idx = Indexer::new();
        let u = Url::parse("file:///pkg/Foo.kt").unwrap();
        // First index: class Bar in file Foo.kt
        idx.index_content(&u, "package com.example\nclass Bar {}");
        assert!(idx.qualified.contains_key("com.example.Bar"), "initial pkg.Sym missing");
        assert!(idx.qualified.contains_key("com.example.Foo.Bar"), "initial pkg.Stem.Sym missing");
        // Re-index with Bar removed
        idx.index_content(&u, "package com.example\n// empty");
        assert!(!idx.qualified.contains_key("com.example.Bar"), "stale pkg.Sym not removed");
        assert!(!idx.qualified.contains_key("com.example.Foo.Bar"), "stale pkg.Stem.Sym not removed");
    }

    #[test]
    fn resolve_max_files_uses_default_when_unset() {
        // Ensure env var is unset for this test.
        std::env::remove_var("KOTLIN_LSP_MAX_FILES");
        assert_eq!(super::resolve_max_files(2000), 2000);
        assert_eq!(super::resolve_max_files(super::MAX_FILES_UNLIMITED), super::MAX_FILES_UNLIMITED);
    }

    #[test]
    fn resolve_max_files_reads_env_var() {
        std::env::set_var("KOTLIN_LSP_MAX_FILES", "500");
        let result = super::resolve_max_files(2000);
        std::env::remove_var("KOTLIN_LSP_MAX_FILES");
        assert_eq!(result, 500);
    }

    #[test]
    fn resolve_max_files_invalid_env_falls_back_to_default() {
        std::env::set_var("KOTLIN_LSP_MAX_FILES", "not_a_number");
        let result = super::resolve_max_files(2000);
        std::env::remove_var("KOTLIN_LSP_MAX_FILES");
        assert_eq!(result, 2000);
    }

    // ── cache_entry_to_file_result ────────────────────────────────────────────

    /// cache_entry_to_file_result must reconstruct supertypes from FileData.lines
    /// even when the FileCacheEntry was loaded from disk (lines are always cached).
    #[test]
    fn cache_entry_to_file_result_supertypes_extracted() {
        let u = uri("/Cat.kt");
        let mut data = crate::types::FileData::default();
        data.lines = Arc::new(vec![
            "class Cat : IAnimal {".into(),
            "    fun meow() {}".into(),
            "}".into(),
        ]);
        data.symbols.push(crate::types::SymbolEntry {
            name: "Cat".into(),
            kind: tower_lsp::lsp_types::SymbolKind::CLASS,
            visibility: crate::types::Visibility::Public,
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

        let result = super::cache_entry_to_file_result(&u, &entry);
        let super_names: Vec<&str> = result.supertypes.iter().map(|(n, _)| n.as_str()).collect();
        assert!(super_names.contains(&"IAnimal"), "IAnimal missing from supertypes: {super_names:?}");
    }

    // ── apply_workspace_result ────────────────────────────────────────────────

    /// apply_workspace_result must include files from cache hits (not just parsed files).
    #[test]
    fn apply_workspace_result_includes_cached_files() {
        let u = uri("/Cached.kt");
        // Build cache entry from a prior parse.
        let parsed = Indexer::parse_file(&u, "class CachedClass");
        let entry = FileCacheEntry {
            mtime_secs: 0,
            file_size: 0,
            content_hash: 0,
            file_data: parsed.data.clone(),
        };
        let cached = super::cache_entry_to_file_result(&u, &entry);

        let workspace_result = WorkspaceIndexResult {
            files: vec![cached],
            stats: IndexStats { cache_hits: 1, ..Default::default() },
            workspace_root: std::path::PathBuf::from("/"),
            aborted: false,
            complete_scan: true,
        };

        let idx = Indexer::new();
        idx.apply_workspace_result(&workspace_result);

        assert!(idx.definitions.contains_key("CachedClass"),
            "CachedClass from cache hit should be in definitions index");
        assert!(idx.files.contains_key(u.as_str()),
            "CachedClass file should be in files map");
    }

    /// apply_workspace_result must do a full reset — switching workspaces removes
    /// all symbols from the previous workspace.
    #[test]
    fn apply_workspace_result_clears_stale_workspace() {
        let idx = Indexer::new();

        let u1 = uri("/A.kt");
        idx.apply_workspace_result(&WorkspaceIndexResult {
            files: vec![Indexer::parse_file(&u1, "class ClassA")],
            stats: IndexStats::default(),
            workspace_root: std::path::PathBuf::from("/workspace_a"),
            aborted: false,
            complete_scan: true,
        });
        assert!(idx.definitions.contains_key("ClassA"), "ClassA should be present after first apply");

        let u2 = uri("/B.kt");
        idx.apply_workspace_result(&WorkspaceIndexResult {
            files: vec![Indexer::parse_file(&u2, "class ClassB")],
            stats: IndexStats::default(),
            workspace_root: std::path::PathBuf::from("/workspace_b"),
            aborted: false,
            complete_scan: true,
        });

        assert!(!idx.definitions.contains_key("ClassA"),
            "ClassA should be gone after workspace switch");
        assert!(idx.definitions.contains_key("ClassB"),
            "ClassB should be present in new workspace");
    }

    /// apply_workspace_result must combine both cache hits and freshly parsed files
    /// into the final index — neither should be silently dropped.
    #[test]
    fn apply_workspace_result_mixed_cache_and_parsed() {
        let u_cached = uri("/Cached.kt");
        let u_parsed  = uri("/Parsed.kt");

        let cached_parse = Indexer::parse_file(&u_cached, "class CachedClass");
        let entry = FileCacheEntry {
            mtime_secs: 0, file_size: 0, content_hash: 0, file_data: cached_parse.data.clone()
        };
        let cached_result = super::cache_entry_to_file_result(&u_cached, &entry);

        let parsed_result = Indexer::parse_file(&u_parsed, "class ParsedClass");

        let idx = Indexer::new();
        idx.apply_workspace_result(&WorkspaceIndexResult {
            files: vec![cached_result, parsed_result],
            stats: IndexStats { cache_hits: 1, files_parsed: 1, ..Default::default() },
            workspace_root: std::path::PathBuf::from("/"),
            aborted: false,
            complete_scan: true,
        });

        assert!(idx.definitions.contains_key("CachedClass"),
            "CachedClass (from cache) should be in index");
        assert!(idx.definitions.contains_key("ParsedClass"),
            "ParsedClass (freshly parsed) should be in index");
        assert_eq!(idx.files.len(), 2, "exactly 2 files in index");
    }

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
}


