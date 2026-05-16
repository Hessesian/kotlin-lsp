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
pub(crate) const CACHE_VERSION: u32 = 19;

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

// ─── Library cache ────────────────────────────────────────────────────────────

/// Deterministic cache path for a set of library source paths.
///
/// Keyed by a hash of the (sorted) source path strings so the same
/// `~/.kotlin-lsp/sources` directory shares one cache file across all workspaces.
pub(super) fn library_cache_path(source_paths: &[String]) -> PathBuf {
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
        .join(format!("library-{hash:016x}.bin"))
}

/// Returns `true` if the library cache is likely still valid.
///
/// Two-tier check:
/// 1. Directory mtime — catches file additions/deletions (fast, 1 stat per dir).
/// 2. A random sample of up to 256 cached entries — catches in-place edits, which
///    on most filesystems do NOT update the parent directory mtime.
///
/// Limitation: edited files not in the random sample are still missed.
/// For `~/.kotlin-lsp/sources` (populated by `extract-sources`) this is
/// acceptable because those files are not directly edited by users.
pub(super) fn library_cache_is_fresh(
    source_paths: &[PathBuf],
    cache_path: &Path,
    cached_entries: &HashMap<String, FileCacheEntry>,
) -> bool {
    let cache_mtime = match std::fs::metadata(cache_path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };
    // Tier 1: directory mtime.  Detects files added or removed directly inside each source
    // path on most filesystems (the immediate parent directory mtime changes on add/remove).
    // Note: on Linux, modifying a file's contents does NOT update its parent directory mtime,
    // so this tier cannot detect modifications — Tier 2 handles that case.
    let dirs_fresh = source_paths.iter().all(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .map(|dir_mtime| cache_mtime >= dir_mtime)
            .unwrap_or(false)
    });
    if !dirs_fresh {
        return false;
    }
    // Tier 2: validate a spread sample of cached entries against on-disk mtime+size.
    // This catches file modifications that Tier 1 misses.  We use up to 256 samples with
    // a uniform stride so we cover the full entry set at roughly 1-in-(N/256) granularity.
    // Library files (Gradle caches, extracted sources) are stable in practice, so this
    // probabilistic check is sufficient; a full O(N) scan would be prohibitively slow for
    // caches with tens of thousands of entries.
    let sample_size = 256_usize.min(cached_entries.len());
    if sample_size == 0 {
        return true;
    }
    let stride = (cached_entries.len() / sample_size).max(1);
    cached_entries
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

/// Load the library cache for the given source paths.
/// Returns `None` if absent, corrupt, or version-mismatched.
pub(super) fn try_load_library_cache(
    source_paths: &[String],
) -> Option<HashMap<String, FileCacheEntry>> {
    let path = library_cache_path(source_paths);
    let bytes = std::fs::read(&path).ok()?;
    let cache: IndexCache = bincode::deserialize(&bytes).ok()?;
    if cache.version != CACHE_VERSION {
        return None;
    }
    log::info!(
        "Loaded library cache ({} files) from {}",
        cache.entries.len(),
        path.display()
    );
    Some(cache.entries)
}

/// Save the library cache for the given source paths.
///
/// Only writes entries whose URI is in `library_uris`.
pub(super) fn save_library_cache(
    source_paths: &[String],
    files: &DashMap<String, Arc<FileData>>,
    content_hashes: &DashMap<String, u64>,
    library_uris: &DashSet<String>,
) {
    let cache_path = library_cache_path(source_paths);
    if let Some(parent) = cache_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn!("Library cache: could not create directory: {e}");
            return;
        }
    }

    let mut entries: HashMap<String, FileCacheEntry> = HashMap::new();
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
                // Strip private/internal symbols before writing to disk: library
                // members with restricted visibility are never accessible from
                // workspace code, so there is no point caching them.  Filtering
                // here means fast-path loads can use a plain Arc::clone.
                let mut filtered = (**data).clone();
                filtered.symbols.retain(|s| {
                    !matches!(s.visibility, Visibility::Private | Visibility::Internal)
                });
                let filtered = Arc::new(filtered);
                let qualified_keys = build_qualified_keys(&filtered, file_stem.as_deref());
                entries.insert(
                    path_buf.to_string_lossy().to_string(),
                    FileCacheEntry {
                        mtime_secs: mtime,
                        file_size,
                        content_hash: hash,
                        file_data: filtered,
                        qualified_keys,
                    },
                );
            }
        }
    }

    let cache = IndexCache {
        version: CACHE_VERSION,
        complete_scan: true,
        entries,
    };
    match bincode::serialize(&cache) {
        Ok(bytes) => {
            let tmp_path = cache_path.with_extension("bin.tmp");
            let write_ok = std::fs::write(&tmp_path, &bytes)
                .and_then(|()| std::fs::rename(&tmp_path, &cache_path))
                .is_ok();
            if write_ok {
                log::info!(
                    "Library cache saved ({} files, {} KB) → {}",
                    cache.entries.len(),
                    bytes.len() / 1024,
                    cache_path.display()
                );
            } else {
                let _ = std::fs::remove_file(&tmp_path);
                log::warn!("Library cache write failed for {}", cache_path.display());
            }
        }
        Err(e) => log::warn!("Library cache serialize failed: {e}"),
    }
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
