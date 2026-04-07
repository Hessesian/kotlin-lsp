use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

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
use crate::types::{FileData, SymbolEntry};

/// Hard cap on workspace files indexed eagerly.
/// Files beyond this limit are resolved on-demand via `rg` when first needed.
/// Override by setting the `KOTLIN_LSP_MAX_FILES` environment variable.
const DEFAULT_MAX_INDEX_FILES: usize = 2000;

// ─── Disk cache ──────────────────────────────────────────────────────────────

/// Bump when the serialized format changes; invalidates any older cache files.
const CACHE_VERSION: u32 = 1;

/// Per-file entry stored in the on-disk index cache.
#[derive(Serialize, Deserialize)]
struct FileCacheEntry {
    /// File mtime (seconds since Unix epoch) — cheap cache validity check.
    mtime_secs: u64,
    /// FNV-1a content hash — secondary guard for mtime collisions / FAT FS.
    content_hash: u64,
    /// Parsed symbol data for this file.
    file_data: FileData,
}

/// Complete serialized index, written to `~/.cache/kotlin-lsp/<root-hash>/index.bin`.
#[derive(Serialize, Deserialize)]
struct IndexCache {
    version: u32,
    /// Absolute path string → per-file cached data.
    entries: HashMap<String, FileCacheEntry>,
}

/// Returns the cache file path for the given workspace root.
fn workspace_cache_path(root: &Path) -> PathBuf {
    let root_hash = hash_str(&root.to_string_lossy());
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

/// Returns file mtime as seconds since Unix epoch, or `None` on error.
fn file_mtime(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Load and validate the on-disk cache.  Returns `None` if absent / stale / corrupt.
fn try_load_cache(root: &Path) -> Option<IndexCache> {
    let path = workspace_cache_path(root);
    let bytes = std::fs::read(&path).ok()?;
    let cache: IndexCache = bincode::deserialize(&bytes).ok()?;
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
    pub(crate) workspace_root: OnceLock<PathBuf>,
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
    pub(crate) live_lines: DashMap<String, Vec<String>>,
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
            workspace_root: OnceLock::new(),
            content_hashes: DashMap::new(),
            // Allow at most 4 concurrent parse workers; workspace batch indexing
            // uses a dedicated await loop so this mainly guards did_change bursts.
            parse_sem:      Arc::new(tokio::sync::Semaphore::new(4)),
            parse_count:    AtomicU64::new(0),
            completion_cache: DashMap::new(),
            live_lines:     DashMap::new(),
        }
    }

    /// Update the live-lines cache for `uri` without any debounce.
    /// Called from `did_change` before the debounced re-index so that
    /// `completions()` always sees the current document text.
    pub fn set_live_lines(&self, uri: &Url, content: &str) {
        let lines: Vec<String> = content.lines().map(String::from).collect();
        self.live_lines.insert(uri.to_string(), lines);
    }

    /// Discover and index *.kt / *.java files under `root`, bounded by MAX_INDEX_FILES.
    /// Sends LSP `$/progress` notifications so the editor shows a status bar spinner.
    /// On subsequent startups the on-disk cache is used for unchanged files so only
    /// modified or new files need to be re-parsed by tree-sitter.
    pub async fn index_workspace(self: Arc<Self>, root: &Path, client: tower_lsp::Client) {
        // Record workspace root so rg/fd always search within the project.
        let _ = self.workspace_root.set(root.to_path_buf());

        let max = std::env::var("KOTLIN_LSP_MAX_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_INDEX_FILES);

        let mut paths = find_source_files(root);
        let total = paths.len();

        // Shallower paths first.
        paths.sort_by_key(|p| p.components().count());
        let paths: Vec<_> = paths.into_iter().take(max).collect();
        let indexed_count = paths.len();

        let truncated = total > max;
        if truncated {
            log::warn!(
                "Large project: eagerly indexing {indexed_count}/{total} files \
                 (shallowest first). Deeper files resolved on-demand via rg. \
                 Set KOTLIN_LSP_MAX_FILES env var to raise the limit."
            );
        } else {
            log::info!("Indexing {total} source files under {}", root.display());
        }

        // ── Try disk cache ────────────────────────────────────────────────────
        // Restore unchanged files from the on-disk cache (mtime check, no parse).
        // Only files whose mtime has changed (or are new) go through tree-sitter.
        let cache = try_load_cache(root);
        let mut need_parse: Vec<PathBuf> = Vec::new();
        let mut cache_hits: usize = 0;

        for path in &paths {
            let path_str = path.to_string_lossy().to_string();
            let mtime = file_mtime(path).unwrap_or(0);

            if let Some(ref c) = cache {
                if let Some(entry) = c.entries.get(&path_str) {
                    if entry.mtime_secs == mtime {
                        // Cache hit: restore into all index maps without tree-sitter.
                        if let Ok(uri) = Url::from_file_path(path) {
                            self.restore_from_cache_entry(&uri, entry);
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

        // ── LSP progress: begin ──────────────────────────────────────────────
        let token = NumberOrString::String("kotlin-lsp/indexing".into());
        // Ask the client to create a progress token. Use a short timeout — some
        // editors (older Helix versions) never reply, which would stall indexing.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client.send_request::<tower_lsp::lsp_types::request::WorkDoneProgressCreate>(
                WorkDoneProgressCreateParams { token: token.clone() }
            )
        ).await;

        let begin_msg = if cache_hits > 0 {
            format!("Indexing {parse_count}/{indexed_count} files ({cache_hits} cached)…")
        } else if truncated {
            format!("Indexing {indexed_count}/{total} Kotlin files (shallowest first)…")
        } else {
            format!("Indexing {total} Kotlin files…")
        };
        client.send_notification::<progress::KotlinProgress>(ProgressParams {
            token: token.clone(),
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: "kotlin-lsp".into(),
                cancellable: Some(false),
                message: Some(begin_msg),
                percentage: Some(0),
            })),
        }).await;

        // ── concurrent parse (up to 8 workers) ──────────────────────────────
        let sem          = Arc::new(tokio::sync::Semaphore::new(8));
        let done_count   = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::with_capacity(parse_count);

        for path in need_parse {
            let sem        = Arc::clone(&sem);
            let idx        = Arc::clone(&self);
            let done       = Arc::clone(&done_count);
            let client2    = client.clone();
            let token2     = token.clone();
            let total_cnt  = parse_count.max(1);

            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await;
                match tokio::fs::read_to_string(&path).await {
                    Ok(content) => {
                        if let Ok(uri) = Url::from_file_path(&path) {
                            tokio::task::spawn_blocking(move || idx.index_content(&uri, &content))
                                .await
                                .ok();
                        }
                    }
                    Err(e) => log::warn!("Could not read {}: {e}", path.display()),
                }

                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                // Report progress every ~5% (but at least every 50 files).
                // Never send percentage=100 here — some editors (Helix) treat that
                // as "done" and clear the spinner before the End notification fires.
                // The End notification below carries the final summary.
                let report_every = (total_cnt / 20).max(50);
                if n % report_every == 0 && n < total_cnt {
                    let pct = ((n * 100) / total_cnt).min(99) as u32;
                    client2.send_notification::<progress::KotlinProgress>(ProgressParams {
                        token: token2,
                        value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(
                            WorkDoneProgressReport {
                                cancellable: Some(false),
                                message: Some(format!("{n}/{total_cnt} files")),
                                percentage: Some(pct),
                            }
                        )),
                    }).await;
                }
            }));
        }

        for h in handles { h.await.ok(); }

        // ── Persist updated index to disk ────────────────────────────────────
        // Spawn as a blocking task so we don't hold up the progress End notification.
        let idx_for_save = Arc::clone(&self);
        tokio::task::spawn_blocking(move || idx_for_save.save_cache_to_disk());

        // ── LSP progress: end ────────────────────────────────────────────────
        let sym_count = self.definitions.len();
        let cache_note = if cache_hits > 0 {
            format!(", {cache_hits} from cache")
        } else {
            String::new()
        };
        client.send_notification::<progress::KotlinProgress>(ProgressParams {
            token,
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                message: Some(format!(
                    "Indexed {} files, {} symbols{}{}",
                    self.files.len(), sym_count, cache_note,
                    if truncated { format!(" (+{} lazy)", total - indexed_count) } else { String::new() }
                )),
            })),
        }).await;

        log::info!(
            "Done — {} files indexed ({} cached), {} distinct symbols, {} packages",
            self.files.len(),
            cache_hits,
            self.definitions.len(),
            self.packages.len(),
        );
    }

    /// Restore a single file from the on-disk cache into all index maps.
    /// This is the cache-hit path: no tree-sitter, no disk read.
    fn restore_from_cache_entry(&self, uri: &Url, entry: &FileCacheEntry) {
        let uri_str = uri.to_string();
        let data = &entry.file_data;

        // Mark as already-hashed so index_content skips it if called again.
        self.content_hashes.insert(uri_str.clone(), entry.content_hash);

        let file_stem: Option<String> = uri.to_file_path().ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()));

        for sym in &data.symbols {
            let loc = Location { uri: uri.clone(), range: sym.selection_range };

            let mut locs = self.definitions.entry(sym.name.clone()).or_default();
            if !locs.iter().any(|l| l.uri == loc.uri && l.range == loc.range) {
                locs.push(loc.clone());
            }
            drop(locs);

            if let Some(ref pkg) = data.package {
                self.qualified.insert(format!("{pkg}.{}", sym.name), loc.clone());
                if let Some(ref stem) = file_stem {
                    if *stem != sym.name {
                        self.qualified.insert(format!("{pkg}.{stem}.{}", sym.name), loc);
                    }
                }
            }
        }

        if let Some(ref pkg) = data.package {
            let mut uris = self.packages.entry(pkg.clone()).or_default();
            if !uris.contains(&uri_str) {
                uris.push(uri_str.clone());
            }
        }

        self.files.insert(uri_str, Arc::new(data.clone()));
    }

    /// Serialize the current index to `~/.cache/kotlin-lsp/<root-hash>/index.bin`.
    /// Safe to call from a background thread.  Logs warnings on error; never panics.
    pub fn save_cache_to_disk(&self) {
        let root = match self.workspace_root.get() {
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

        let mut entries: HashMap<String, FileCacheEntry> = HashMap::new();
        for file_ref in &self.files {
            let uri_str = file_ref.key();
            let data    = file_ref.value();
            let hash    = self.content_hashes.get(uri_str).map(|h| *h).unwrap_or(0);
            if let Ok(url) = uri_str.parse::<Url>() {
                if let Ok(path) = url.to_file_path() {
                    let mtime = file_mtime(&path).unwrap_or(0);
                    entries.insert(
                        path.to_string_lossy().to_string(),
                        FileCacheEntry { mtime_secs: mtime, content_hash: hash, file_data: (**data).clone() },
                    );
                }
            }
        }

        let cache = IndexCache { version: CACHE_VERSION, entries };
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

    /// (Re-)parse and index a single file's content in-place.
    /// Returns immediately (no-op) when content is identical to the last indexed version.
    pub fn index_content(&self, uri: &Url, content: &str) {
        // Fast-path: skip re-parse if content hasn't changed since last index.
        let hash = hash_str(content);
        let uri_str = uri.to_string();
        if self.content_hashes.get(&uri_str).map(|h| *h == hash).unwrap_or(false) {
            return;
        }
        self.content_hashes.insert(uri_str.clone(), hash);
        self.parse_count.fetch_add(1, Ordering::Relaxed);
        // Invalidate cached completion items — the file is changing.
        self.completion_cache.remove(&uri_str);
        let is_kotlin = uri.path().ends_with(".kt");
        let data = if is_kotlin {
            parser::parse_kotlin(content)
        } else {
            parser::parse_java(content)
        };



        // ── Remove stale entries for this URI ─────────────────────────────────
        if let Some(old) = self.files.get(&uri_str) {
            for sym in &old.symbols {
                if let Some(mut locs) = self.definitions.get_mut(&sym.name) {
                    locs.retain(|l| l.uri.as_str() != uri.as_str());
                }
                if let Some(ref pkg) = old.package {
                    self.qualified.remove(&format!("{pkg}.{}", sym.name));
                }
            }
            if let Some(ref pkg) = old.package {
                if let Some(mut uris) = self.packages.get_mut(pkg) {
                    uris.retain(|u| u != &uri_str);
                }
            }
        }

        // ── Register fresh definitions ────────────────────────────────────────
        // Derive the file stem (e.g. "AccountContract" from "AccountContract.kt")
        // so we can also store nested-class qualified keys like
        // "com.example.AccountContract.State" in addition to "com.example.State".
        let file_stem: Option<String> = uri.to_file_path().ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()));

        for sym in &data.symbols {
            let loc = Location { uri: uri.clone(), range: sym.selection_range };

            // short-name index (guarded against duplicates)
            let mut locs = self.definitions.entry(sym.name.clone()).or_default();
            if !locs.iter().any(|l| l.uri == loc.uri && l.range == loc.range) {
                locs.push(loc.clone());
            }
            drop(locs);

            // qualified index: "com.example.Foo" → Location
            if let Some(ref pkg) = data.package {
                // Primary key: pkg.SymbolName
                self.qualified.insert(format!("{pkg}.{}", sym.name), loc.clone());

                // Secondary key: pkg.FileStem.SymbolName — covers nested/inner classes
                // whose import path includes the outer class name, e.g.
                // `import com.example.AccountContract.State` where State is nested
                // inside AccountContract.kt.
                if let Some(ref stem) = file_stem {
                    if *stem != sym.name {
                        self.qualified.insert(format!("{pkg}.{stem}.{}", sym.name), loc);
                    }
                }
            }
        }

        // ── Package membership ────────────────────────────────────────────────
        if let Some(ref pkg) = data.package {
            let mut uris = self.packages.entry(pkg.clone()).or_default();
            if !uris.contains(&uri_str) {
                uris.push(uri_str.clone());
            }
        }

        self.files.insert(uri_str, Arc::new(data));
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
            for line in &data.lines {
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
        let data = self.files.get(uri.as_str())?;
        let line = data.lines.get(position.line as usize)?;

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
                find_enclosing_call_name(&data.lines, position.line as usize, start)
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

    /// Completion candidates at `position` in `uri`.
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

        // Special case: `it` is Kotlin's implicit single-parameter lambda argument.
        // Scan backward through `before` to find the enclosing lambda's collection
        // receiver (e.g. `users.forEach { it.` → element type of `users`).
        if dot_receiver.as_deref() == Some("it") {
            if let Some(elem_type) = find_it_element_type(before, self, uri) {
                return crate::resolver::complete_symbol(self, &prefix, Some(&elem_type), uri, snippets);
            }
            return vec![];
        }

        crate::resolver::complete_symbol(
            self,
            &prefix,
            dot_receiver.as_deref(),
            uri,
            snippets,
        )
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
        let data = self.files.get(loc.uri.as_str())?;
        let sym  = data.symbols.iter().find(|s| s.name == name)?;

        let start_line = sym.selection_range.start.line as usize;
        let sig = collect_signature(&data.lines, start_line);

        let lang = if loc.uri.path().ends_with(".kt") { "kotlin" } else { "java" };

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

        // Walk backward from the cursor line looking for the nearest class-like
        // declaration that is lexically at a lower indentation level than `row`.
        // We stop at the first matching line rather than tracking brace depth
        // (brace counting is unreliable with Kotlin multi-line lambdas).
        let target_indent = file.lines.get(row).map(|l| leading_spaces(l)).unwrap_or(0);

        for i in (0..row).rev() {
            let line = match file.lines.get(i) { Some(l) => l, None => continue };
            let indent = leading_spaces(line);
            // Only consider lines at strictly lower indent — they could be the enclosure.
            if indent >= target_indent { continue; }

            let t = line.trim();
            if t.is_empty() || t.starts_with("//") || t.starts_with('*') { continue; }

            if let Some(name) = extract_class_decl_name(t) {
                return Some(name);
            }
        }
        None
    }

    /// Return the package declared in the given file, if any.
    pub fn package_of(&self, uri: &Url) -> Option<String> {
        self.files.get(uri.as_str())?.package.clone()
    }
}

/// Count leading ASCII spaces (used for indentation-based enclosing-class detection).
fn leading_spaces(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

/// If `line` is a class/interface/object/sealed declaration, return the type name.
fn extract_class_decl_name(line: &str) -> Option<String> {
    // Strip common modifiers: abstract sealed data open inner enum @Annotation etc.
    let mut rest = line;
    let modifiers = [
        "abstract ", "sealed ", "data ", "open ", "inner ", "private ",
        "protected ", "public ", "internal ", "inline ", "value ", "enum ",
        "companion ", "override ", "final ",
    ];
    loop {
        let before = rest;
        for m in &modifiers { rest = rest.strip_prefix(m).unwrap_or(rest).trim_start(); }
        if rest == before { break; }
    }
    // Now rest should start with "class", "interface", or "object"
    let rest = if let Some(r) = rest.strip_prefix("class ").or_else(|| rest.strip_prefix("interface ")).or_else(|| rest.strip_prefix("object ")) {
        r
    } else {
        return None;
    };
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

    // Prefer `fd` — order of magnitude faster for large trees.
    let fd_result = Command::new("fd")
        .args([
            "--type", "f",
            "--extension", "kt",
            "--extension", "java",
            "--absolute-path",
            "--exclude", ".git",
            "--exclude", "build",
            "--exclude", "target",
            "--exclude", ".gradle",
            ".",
            root_str.as_ref(),
        ])
        .output();

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
    walkdir::WalkDir::new(root)
        .into_iter()
        // filter_entry prunes directories — don't descend into VCS / build dirs.
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            if e.file_type().is_dir() {
                !matches!(
                    name.as_ref(),
                    ".git" | "build" | "target" | "node_modules"
                        | ".gradle" | ".idea" | ".kotlin"
                )
            } else {
                true
            }
        })
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && matches!(
                    e.path().extension().and_then(|s| s.to_str()),
                    Some("kt") | Some("java")
                )
        })
        .map(|e| e.into_path())
        .collect()
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
        "--glob", "*.kt",
        "--glob", "*.java",
        "-e", &pattern,
    ]);
    cmd.arg(search_root.as_ref());

    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_rg_line)
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
    same_pkg:     Option<&str>,
    root:         Option<&Path>,
    include_decl: bool,
    from_uri:     &Url,
) -> Vec<Location> {
    let search_root: std::borrow::Cow<Path> = match root {
        Some(r) => std::borrow::Cow::Borrowed(r),
        None    => std::borrow::Cow::Owned(std::env::current_dir().unwrap_or_default()),
    };

    let safe_name: String = regex_escape(name);
    let decl_kws = ["class ", "interface ", "object ", "fun ", "val ", "var ",
                    "typealias ", "enum class ", "enum "];

    let filter = |(loc, content): (Location, String)| -> Option<Location> {
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

        // Pass B: bare `Name` restricted to files that either…
        //   (a) import the parent directly:  import .*.ParentClass
        //   (b) import Name directly:        import .*.ParentClass.Name  or  .ParentClass.*
        //   (c) are in the same package as the declaration (no import needed)
        //
        // Step B1 — find candidate files via a fast rg file-list pass.
        let import_pat = format!(r"import.*\b{}\b", safe_parent);
        let candidate_files = rg_files_with_matches(&import_pat, &search_root);

        // Step B2 — also add files in the same package (package-private visibility).
        let same_pkg_prefix = same_pkg.unwrap_or("__no_package__");
        // (same-package files will have the same package declaration; gather via a
        // second file-list search rather than scanning the index — this function
        // does not have access to the Indexer.)
        let pkg_pat = format!(r"^\s*package\s+{}\s*$", regex_escape(same_pkg_prefix));
        let pkg_files = if same_pkg.is_some() {
            rg_files_with_matches(&pkg_pat, &search_root)
        } else {
            vec![]
        };

        // Merge candidate file sets.
        let mut all_files: Vec<String> = candidate_files;
        for f in pkg_files {
            if !all_files.contains(&f) { all_files.push(f); }
        }

        if !all_files.is_empty() {
            let bare_hits = rg_word_in_files(&safe_name, &all_files);
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
    } else {
        // ── Unscoped: fall back to the original word-boundary search ──────────
        let mut cmd = Command::new("rg");
        cmd.args([
            "--no-heading", "--with-filename", "--line-number", "--column",
            "--word-regexp", "--glob", "*.kt", "--glob", "*.java",
            "-e", &safe_name,
        ]);
        cmd.arg(search_root.as_ref());

        let out = match cmd.output() {
            Ok(o) if !o.stdout.is_empty() => o,
            _ => return vec![],
        };

        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(parse_rg_line_with_content)
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
    let out = match Command::new("rg")
        .args(["--no-heading", "--with-filename", "--line-number", "--column",
               "--glob", "*.kt", "--glob", "*.java", "-e", pattern])
        .arg(root)
        .output()
    {
        Ok(o) if !o.stdout.is_empty() => o,
        _ => return vec![],
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_rg_line_with_content)
        .collect()
}

/// Run `rg -l` to get the list of files matching a pattern.
fn rg_files_with_matches(pattern: &str, root: &Path) -> Vec<String> {
    let out = match Command::new("rg")
        .args(["-l", "--glob", "*.kt", "--glob", "*.java", "-e", pattern])
        .arg(root)
        .output()
    {
        Ok(o) if !o.stdout.is_empty() => o,
        _ => return vec![],
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
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
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_rg_line_with_content)
        .collect()
}

/// Like `parse_rg_line` but also returns the matched line content.
fn parse_rg_line_with_content(line: &str) -> Option<(Location, String)> {
    let mut parts = line.splitn(4, ':');
    let file     = parts.next()?;
    let line_num: u32 = parts.next()?.trim().parse().ok()?;
    let col:      u32 = parts.next()?.trim().parse().ok()?;
    let content  = parts.next().unwrap_or("").to_string();

    let path = std::path::Path::new(file);
    if !path.is_absolute() { return None; }

    let uri = Url::from_file_path(path).ok()?;
    let pos = Position::new(line_num.saturating_sub(1), col.saturating_sub(1));
    Some((Location { uri, range: Range::new(pos, pos) }, content))
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
/// - If the receiver has a collection-like generic type (`List<Product>`,
///   `Flow<Event>`, …) → returns the inner type (`Product`, `Event`).
/// - Otherwise (scope function: `user.let { it.`) → returns the receiver's
///   base type directly (`User`).
///
/// Returns `None` when the type cannot be determined statically.
fn find_it_element_type(before_cursor: &str, idx: &Indexer, uri: &Url) -> Option<String> {
    // Find the last `{` that opens the lambda we're inside.
    let brace_byte = before_cursor.rfind('{')?;
    let before_brace = before_cursor[..brace_byte].trim_end();

    // Strip a trailing `method(...)` call arg list so we're left with the
    // dotted chain `receiver.method`.
    let callee_str = strip_trailing_call_args(before_brace);

    // Handle `?.` null-safe calls by normalising to `.` for the split.
    let callee_normalised = callee_str.replace("?.", ".");
    let callee_trimmed = callee_normalised.trim_end();

    // Split at the last `.` to separate receiver expression from method name.
    let dot_pos = callee_trimmed.rfind('.')?;
    let receiver_expr = callee_trimmed[..dot_pos].trim_end();

    // Extract the last identifier in the receiver expression.
    // `viewModel.items` → `items`;  `items` → `items`
    let receiver_var: String = receiver_expr
        .chars()
        .rev()
        .take_while(|&c| is_id_char(c))
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    if receiver_var.is_empty() { return None; }

    // Try raw type with generics first (covers List<T>, Flow<T>, StateFlow<T> …).
    if let Some(raw) = crate::resolver::infer_variable_type_raw(idx, &receiver_var, uri) {
        // Collection-like type → return element type.
        if let Some(elem) = crate::resolver::extract_collection_element_type(&raw) {
            return Some(elem);
        }
        // Scope function (let, also, apply …) → `it` IS the receiver.
        let base: String = raw.chars().take_while(|&c| is_id_char(c)).collect();
        if !base.is_empty() && base.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            return Some(base);
        }
    }

    // Uppercase bare name used as object receiver: `MyObject.let { it.`
    if receiver_var.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
        return Some(receiver_var);
    }

    None
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

/// Cap on number of lines to walk backward when searching for a doc comment.
/// (Used as the maximum `search_end` starting point inside `extract_doc_comment`.)
const DOC_SEARCH_LIMIT: usize = 40;

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
}
