//! On-disk index cache — persistence layer for the kotlin-lsp workspace index.
//!
//! # Contents
//! - [`FileCacheEntry`] / [`IndexCache`] — serialisable data types.
//! - [`workspace_cache_path`] — deterministic path derived from the workspace root.
//! - [`try_load_cache`] — load and validate the on-disk cache.
//! - [`cache_entry_to_file_result`] — pure: turn a cache entry into a [`FileIndexResult`].
//! - [`save_cache`] — build and write the cache from live index data.
//! - [`write_status_file`] — write `~/.cache/kotlin-lsp/status.json` for the skill extension.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::{DashMap, DashSet};
use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::Url;

use crate::types::{FileData, FileIndexResult, Visibility};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Bump when the serialized format changes; invalidates any older cache files.
pub(crate) const CACHE_VERSION: u32 = 23;

// ─── Types ───────────────────────────────────────────────────────────────────

/// Per-file entry stored in the on-disk index cache.
#[derive(Serialize, Deserialize)]
pub(crate) struct FileCacheEntry {
    /// File mtime (seconds since Unix epoch) — primary cache validity check.
    pub(crate) mtime_secs: u64,
    /// File size in bytes — secondary guard for same-second edits (1s mtime resolution).
    pub(crate) file_size: u64,
    /// FNV-1a content hash — tertiary guard for mtime collisions / FAT FS.
    pub(crate) content_hash: u64,
    /// Parsed symbol data for this file.
    ///
    /// Wrapped in `Arc` so that `collect_entry` can hand out cheap clones into
    /// the live index without deep-copying every `FileData` on each `complete`
    /// invocation.  `serde/rc` serialises `Arc<T>` identically to `T`, so
    /// no cache-version bump is needed.
    pub(crate) file_data: Arc<FileData>,
    /// Pre-computed qualified map keys for this file's symbols.
    ///
    /// Each entry is `(qualified_key, selection_range)` pre-built at save time
    /// so that fast-path loading skips all `format!("{pkg}.{name}")` calls.
    /// Old entries without this field deserialise as an empty `Vec`, falling
    /// back to the `format!()` path on first warm load.
    #[serde(default)]
    pub(crate) qualified_keys: Vec<(String, tower_lsp::lsp_types::Range)>,
}

/// Complete serialized index, written to `~/.cache/kotlin-lsp/<root-hash>/index.bin`.
#[derive(Serialize, Deserialize)]
pub(super) struct IndexCache {
    pub(super) version: u32,
    /// True when this cache was built from a complete (non-truncated) workspace scan.
    /// Only set to true when `total <= max` at index time.
    /// When false, the entries may be a partial subset of the workspace — warm-manifest
    /// mode is disabled to avoid hiding files that were never indexed.
    #[serde(default)]
    pub(super) complete_scan: bool,
    /// Absolute path string → per-file cached data.
    pub(super) entries: HashMap<String, FileCacheEntry>,
}

// ─── Path helpers ─────────────────────────────────────────────────────────────

fn xdg_cache_base() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = crate::util::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
            home.join(".cache")
        })
}

fn status_cache_path() -> PathBuf {
    xdg_cache_base().join("kotlin-lsp").join("status.json")
}

/// Returns the cache file path for the given workspace root.
///
/// Uses a SHA-256 hash of the canonicalized root path as the directory name so
/// equivalent roots always map to the same cache file regardless of symlinks.
pub(crate) fn workspace_cache_path(root: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let root_hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        u64::from_be_bytes(bytes)
    };
    xdg_cache_base()
        .join("kotlin-lsp")
        .join(format!("{root_hash:016x}"))
        .join("index.bin")
}

// ─── Status file ─────────────────────────────────────────────────────────────

/// Write a human-readable status blob to `~/.cache/kotlin-lsp/status.json`.
///
/// The skill extension reads this to report loading state with time estimates.
pub(super) fn write_status_file(content: &str) {
    let path = status_cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, content);
}

// ─── Load ─────────────────────────────────────────────────────────────────────

/// Load and validate the on-disk cache.  Returns `None` if absent / stale / corrupt.
pub(super) fn try_load_cache(root: &Path) -> Option<IndexCache> {
    let path = workspace_cache_path(root);
    let bytes = std::fs::read(&path).ok()?;
    let cache: IndexCache = match bincode::deserialize(&bytes) {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "Cache deserialize failed (struct layout changed?): {e} — will re-index. \
                 Delete {path} to suppress this warning.",
                path = path.display()
            );
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

// ─── Pure conversion ──────────────────────────────────────────────────────────

/// Pure: convert a disk-cache entry into a [`FileIndexResult`] ready for indexing.
///
/// Reconstructs `supertypes` from cached lines (not stored separately in cache) so
/// that `goToImplementation` works correctly on cache hits.
pub(crate) fn cache_entry_to_file_result(uri: &Url, entry: &FileCacheEntry) -> FileIndexResult {
    use tower_lsp::lsp_types::{Location, SymbolKind};
    let data = &entry.file_data;
    let class_kinds = [
        SymbolKind::CLASS,
        SymbolKind::INTERFACE,
        SymbolKind::STRUCT,
        SymbolKind::ENUM,
        SymbolKind::OBJECT,
    ];
    let mut supertypes: Vec<(String, Location)> = Vec::new();
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
        data: (**data).clone(),
        supertypes,
        content_hash: entry.content_hash,
        error: None,
    }
}

/// Compute qualified map keys for a single file.
///
/// Returns one or two `(key, selection_range)` pairs per symbol:
/// - `"pkg.SymName"`
/// - `"pkg.FileStem.SymName"` (only when file stem differs from the symbol name)
///
/// Used at save time so fast-path loading can skip `format!()` entirely.
pub(crate) fn build_qualified_keys(
    file_data: &FileData,
    file_stem: Option<&str>,
) -> Vec<(String, tower_lsp::lsp_types::Range)> {
    let Some(ref pkg) = file_data.package else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(file_data.symbols.len() * 2);
    for sym in &file_data.symbols {
        out.push((format!("{pkg}.{}", sym.name), sym.selection_range));
        if let Some(stem) = file_stem {
            if stem != sym.name {
                out.push((format!("{pkg}.{stem}.{}", sym.name), sym.selection_range));
            }
        }
    }
    out
}

// ─── Save ─────────────────────────────────────────────────────────────────────

/// Build and write the workspace index cache to disk.
///
/// Reads filesystem metadata (mtime, size) for each file, so this function
/// performs IO.  Library-source files (from `sourcePaths`) are excluded since
/// they are re-indexed on every startup.
///
/// Does **not** overwrite a larger complete cache with a smaller incomplete one,
/// to prevent an editor server (which may load only part of the workspace) from
/// truncating a cache built by `--index-only`.
pub(super) fn save_cache(
    root: &Path,
    files: &DashMap<String, Arc<FileData>>,
    content_hashes: &DashMap<String, u64>,
    library_uris: &DashSet<String>,
    complete_scan: bool,
) {
    let cache_path = workspace_cache_path(root);
    if let Some(parent) = cache_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn!("Cache: could not create directory: {e}");
            return;
        }
    }

    let mut entries: HashMap<String, FileCacheEntry> = HashMap::new();
    for file_ref in files.iter() {
        let uri_str = file_ref.key();
        // Skip library-source files — re-indexed from sourcePaths on each startup.
        if library_uris.contains(uri_str) {
            continue;
        }
        let data = file_ref.value();
        let hash = content_hashes.get(uri_str).map(|h| *h).unwrap_or(0);
        if let Ok(url) = uri_str.parse::<Url>() {
            if let Ok(path) = url.to_file_path() {
                let file_stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
                let meta = std::fs::metadata(&path);
                let mtime = meta
                    .as_ref()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let file_size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                let qualified_keys = build_qualified_keys(data, file_stem.as_deref());
                entries.insert(
                    path.to_string_lossy().to_string(),
                    FileCacheEntry {
                        mtime_secs: mtime,
                        file_size,
                        content_hash: hash,
                        file_data: Arc::clone(data),
                        qualified_keys,
                    },
                );
            }
        }
    }

    let cache = IndexCache {
        version: CACHE_VERSION,
        complete_scan,
        entries,
    };
    match bincode::serialize(&cache) {
        Ok(bytes) => {
            // Don't overwrite a complete cache with an incomplete one.
            if !complete_scan {
                if let Ok(meta) = std::fs::metadata(&cache_path) {
                    if meta.len() > bytes.len() as u64 {
                        log::info!(
                            "Cache save skipped: existing cache ({} KB) is larger than \
                             incomplete new cache ({} KB)",
                            meta.len() / 1024,
                            bytes.len() / 1024
                        );
                        return;
                    }
                }
            }
            // Write atomically: write to a sibling `.tmp` file then rename,
            // so a crash mid-write never leaves a truncated cache behind.
            let tmp_path = cache_path.with_extension("bin.tmp");
            let write_ok = std::fs::write(&tmp_path, &bytes)
                .and_then(|()| std::fs::rename(&tmp_path, &cache_path))
                .is_ok();
            if write_ok {
                log::info!(
                    "Cache saved ({} files, {} KB) → {}",
                    cache.entries.len(),
                    bytes.len() / 1024,
                    cache_path.display()
                );
            } else {
                let _ = std::fs::remove_file(&tmp_path);
                log::warn!("Cache write failed for {}", cache_path.display());
            }
        }
        Err(e) => log::warn!("Cache serialize failed: {e}"),
    }
}

// ─── Library cache (chunked) ─────────────────────────────────────────────────
//
// The library cache is split into fixed-size chunk files to allow incremental
// loading during the fast-path restore.  Loading 25k files one chunk at a time
// (each ~20 MB on disk, ~50 MB deserialized) avoids the ~1 GB peak that a
// single-file load causes — chunk + batch are dropped before the next chunk
// is read, keeping the instantaneous working set small.
//
// Atomicity: the manifest is written LAST.  A missing or corrupt manifest means
// the cache is invalid; callers fall back to a full re-scan.  Chunk files from a
// previous incomplete write with the same chunk index are harmless because they
// are overwritten at the start of the next save.

/// Max files per library cache chunk.
///
/// Each chunk deserialises to roughly `LIBRARY_CHUNK_SIZE × average_entry_size`.
/// With fields stripped (no `lines`, no `imports`, no RHS inference data),
/// average entry size is ~3–5 KB, so 2 000 files ≈ 6–10 MB per chunk.
const LIBRARY_CHUNK_SIZE: usize = 2000;

/// Tiny commit-point file written last; its presence signals a complete save.
#[derive(Serialize, Deserialize)]
struct LibraryManifest {
    version: u32,
    chunk_count: u32,
}

/// Directory that holds the library cache chunks + manifest.
///
/// Uses the same hash logic as the old `library_cache_path` so existing caches
/// do not collide: the old `library-{hash}.bin` file and the new
/// `library-{hash}-chunks/` directory have different names.
fn library_chunks_dir(source_paths: &[String]) -> PathBuf {
    let mut sorted = source_paths.to_vec();
    sorted.sort();
    let hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for p in &sorted {
            hasher.update(p.as_bytes());
            hasher.update(b"\0");
        }
        let digest = hasher.finalize();
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        u64::from_be_bytes(bytes)
    };
    xdg_cache_base()
        .join("kotlin-lsp")
        .join(format!("library-{hash:016x}-chunks"))
}

pub(super) fn library_manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest.bin")
}

pub(super) fn library_chunk_path(dir: &Path, idx: u32) -> PathBuf {
    dir.join(format!("chunk-{idx:04}.bin"))
}

/// Load the library cache manifest.  Returns `(chunks_dir, chunk_count)` or `None`
/// if the manifest is absent, corrupt, or version-mismatched.
///
/// Deliberately does NOT load any chunk data — callers decide whether they need
/// the data at all (freshness check loads only the first chunk; fast-path restore
/// loads chunks one at a time).
pub(super) fn try_load_library_manifest(source_paths: &[String]) -> Option<(PathBuf, u32)> {
    let dir = library_chunks_dir(source_paths);
    let bytes = std::fs::read(library_manifest_path(&dir)).ok()?;
    let manifest: LibraryManifest = bincode::deserialize(&bytes).ok()?;
    if manifest.version != CACHE_VERSION {
        return None;
    }
    Some((dir, manifest.chunk_count))
}

/// Load one library cache chunk.  Returns `None` if the file is missing, corrupt,
/// or has a mismatched CACHE_VERSION.
pub(super) fn load_library_chunk(dir: &Path, idx: u32) -> Option<HashMap<String, FileCacheEntry>> {
    let path = library_chunk_path(dir, idx);
    let file = std::fs::File::open(&path).ok()?;
    let reader = std::io::BufReader::new(file);
    let cache: IndexCache = bincode::deserialize_from(reader).ok()?;
    if cache.version != CACHE_VERSION {
        return None;
    }
    Some(cache.entries)
}

/// Returns `true` if the library cache is likely still valid.
///
/// Two-tier check:
/// 1. Manifest mtime — catches additions/deletions in source directories (fast, 1 stat per dir).
/// 2. A random sample of up to 256 entries drawn from the first chunk AND the last chunk
///    (if more than one chunk exists) — catches in-place file edits.
///
/// Tier 1 covers the common case (Gradle re-extracts entire source directories).
/// Tier 2 covers in-place edits: modifying a file updates the file's own mtime but
/// not the parent directory mtime, so Tier 1 alone would miss it.  Sampling from
/// both ends of the corpus reduces the probability of missing a changed file.
///
/// Callers must have already loaded the first chunk; `cache_dir` and `chunk_count`
/// are used to optionally load the last chunk for the tail sample.
pub(super) fn library_cache_is_fresh(
    source_paths: &[PathBuf],
    manifest_path: &Path,
    first_chunk_entries: &HashMap<String, FileCacheEntry>,
    cache_dir: &Path,
    chunk_count: u32,
) -> bool {
    let cache_mtime = match std::fs::metadata(manifest_path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let dirs_fresh = source_paths.iter().all(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .map(|dir_mtime| cache_mtime >= dir_mtime)
            .unwrap_or(false)
    });
    if !dirs_fresh {
        return false;
    }

    if !sample_entries_fresh(first_chunk_entries, 128) {
        return false;
    }

    // Also sample the last chunk if it differs from the first.
    if chunk_count > 1 {
        if let Some(last_chunk) = load_library_chunk(cache_dir, chunk_count - 1) {
            if !sample_entries_fresh(&last_chunk, 128) {
                return false;
            }
        }
    }

    true
}

/// Sample up to `limit` entries from `chunk` and verify each file's mtime and size match.
fn sample_entries_fresh(chunk: &HashMap<String, FileCacheEntry>, limit: usize) -> bool {
    let sample_size = limit.min(chunk.len());
    if sample_size == 0 {
        return true;
    }
    let stride = (chunk.len() / sample_size).max(1);
    chunk
        .iter()
        .step_by(stride)
        .take(sample_size)
        .all(|(path_str, entry)| {
            std::fs::metadata(path_str)
                .ok()
                .map(|m| {
                    let mtime_ok = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() == entry.mtime_secs)
                        .unwrap_or(false);
                    mtime_ok && m.len() == entry.file_size
                })
                .unwrap_or(false)
        })
}

/// Save the library cache for the given source paths.
///
/// Splits entries into `LIBRARY_CHUNK_SIZE`-file chunks and writes them to a
/// dedicated directory.  The manifest is written **last** as a commit point:
/// a missing manifest means the cache is invalid regardless of what chunk files
/// exist.
pub(super) fn save_library_cache(
    source_paths: &[String],
    files: &DashMap<String, Arc<FileData>>,
    content_hashes: &DashMap<String, u64>,
    library_uris: &DashSet<String>,
) {
    let dir = library_chunks_dir(source_paths);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!("Library cache: could not create directory: {e}");
        return;
    }
    // Delete manifest first — marks save-in-progress.  A crash between here
    // and the final manifest write leaves no manifest, which callers treat
    // as an invalid cache and trigger a full re-scan.
    let manifest_path = library_manifest_path(&dir);
    let _ = std::fs::remove_file(&manifest_path);

    let entries = collect_library_entries(files, content_hashes, library_uris);
    let total_files = entries.len();
    let Some(total_bytes) = write_library_chunks(&dir, entries) else {
        // At least one chunk failed — manifest is absent, cache is invalid.
        return;
    };
    let chunk_count = total_files.div_ceil(LIBRARY_CHUNK_SIZE) as u32;

    commit_library_manifest(&manifest_path, chunk_count, total_files, total_bytes, &dir);
}

/// Collect all library files into a flat vec of (path, cache-entry) pairs.
/// Strips runtime-unneeded fields to minimise serialised size.
fn collect_library_entries(
    files: &DashMap<String, Arc<FileData>>,
    content_hashes: &DashMap<String, u64>,
    library_uris: &DashSet<String>,
) -> Vec<(String, FileCacheEntry)> {
    let mut entries: Vec<(String, FileCacheEntry)> = Vec::new();
    for file_ref in files.iter() {
        let uri_str = file_ref.key();
        if !library_uris.contains(uri_str) {
            continue;
        }
        let data = file_ref.value();
        let hash = content_hashes.get(uri_str).map(|h| *h).unwrap_or(0);
        if let Ok(url) = uri_str.parse::<Url>() {
            if let Ok(path_buf) = url.to_file_path() {
                let file_stem = path_buf
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned());
                let meta = std::fs::metadata(&path_buf);
                let mtime = meta
                    .as_ref()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let file_size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                let filtered = strip_library_file_data(data);
                let qualified_keys = build_qualified_keys(&filtered, file_stem.as_deref());
                entries.push((
                    path_buf.to_string_lossy().to_string(),
                    FileCacheEntry {
                        mtime_secs: mtime,
                        file_size,
                        content_hash: hash,
                        file_data: filtered,
                        qualified_keys,
                    },
                ));
            }
        }
    }
    entries
}

/// Drop fields not needed for library symbols at runtime.
///
/// `lines` is stripped intentionally: restoring ~25k library files' source text
/// would add ~300 MB of steady-state heap.  Callers that fall back to
/// `data.lines` (e.g. `collect_params_from_line` in `infer/sig.rs`) silently
/// return `None` for library symbols — this is acceptable since `SymbolEntry.params`
/// and `detail` carry the same information for well-formed library classes.
fn strip_library_file_data(data: &FileData) -> Arc<FileData> {
    let mut filtered = (*data).clone();
    filtered
        .symbols
        .retain(|s| !matches!(s.visibility, Visibility::Private | Visibility::Internal));
    // lines/declared_names/imports: only used for in-file completion.
    // rhs/method_call/field_access RHS: only for workspace-file inference.
    filtered.lines = Arc::new(Vec::new());
    filtered.declared_names = Vec::new();
    filtered.imports = Vec::new();
    filtered.rhs_types = Vec::new();
    filtered.method_call_rhs = Vec::new();
    filtered.field_access_rhs = Vec::new();
    Arc::new(filtered)
}

/// Write entries as sequential chunks.
///
/// Returns `Some(total_bytes)` if all chunks were written successfully.
/// Returns `None` if any chunk failed; the manifest must NOT be written in that case.
fn write_library_chunks(dir: &Path, entries: Vec<(String, FileCacheEntry)>) -> Option<usize> {
    let chunk_count = entries.len().div_ceil(LIBRARY_CHUNK_SIZE) as u32;
    let mut total_bytes = 0_usize;
    let mut entries_iter = entries.into_iter();

    for idx in 0..chunk_count {
        let chunk_entries: HashMap<String, FileCacheEntry> =
            entries_iter.by_ref().take(LIBRARY_CHUNK_SIZE).collect();
        let cache = IndexCache {
            version: CACHE_VERSION,
            complete_scan: true,
            entries: chunk_entries,
        };
        match bincode::serialize(&cache) {
            Ok(bytes) => {
                total_bytes += bytes.len();
                let chunk_path = library_chunk_path(dir, idx);
                if let Err(e) = std::fs::write(&chunk_path, &bytes) {
                    log::warn!("Library cache chunk {idx} write failed: {e}");
                    return None; // manifest absent → cache invalid on next load
                }
            }
            Err(e) => {
                log::warn!("Library cache chunk {idx} serialize failed: {e}");
                return None;
            }
        }
    }
    Some(total_bytes)
}

/// Write the manifest (commit point).  No manifest → cache invalid.
fn commit_library_manifest(
    manifest_path: &Path,
    chunk_count: u32,
    total_files: usize,
    total_bytes: usize,
    dir: &Path,
) {
    let manifest = LibraryManifest {
        version: CACHE_VERSION,
        chunk_count,
    };
    match bincode::serialize(&manifest) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(manifest_path, &bytes) {
                log::warn!("Library cache manifest write failed: {e}");
                return;
            }
        }
        Err(e) => {
            log::warn!("Library cache manifest serialize failed: {e}");
            return;
        }
    }
    log::info!(
        "Library cache saved ({total_files} files in {chunk_count} chunks, {} KB total) → {}",
        total_bytes / 1024,
        dir.display()
    );
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
