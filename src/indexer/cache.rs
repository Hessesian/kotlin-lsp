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

use crate::types::{FileData, FileIndexResult};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Bump when the serialized format changes; invalidates any older cache files.
pub(crate) const CACHE_VERSION: u32 = 4;

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
    pub(crate) file_data: FileData,
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
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".cache")
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
    root:           &Path,
    files:          &DashMap<String, Arc<FileData>>,
    content_hashes: &DashMap<String, u64>,
    library_uris:   &DashSet<String>,
    complete_scan:  bool,
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
        if library_uris.contains(uri_str) { continue; }
        let data = file_ref.value();
        let hash = content_hashes.get(uri_str).map(|h| *h).unwrap_or(0);
        if let Ok(url) = uri_str.parse::<Url>() {
            if let Ok(path) = url.to_file_path() {
                let meta      = std::fs::metadata(&path);
                let mtime     = meta.as_ref().ok()
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
            // Don't overwrite a complete cache with an incomplete one.
            if !complete_scan {
                if let Ok(meta) = std::fs::metadata(&cache_path) {
                    if meta.len() > bytes.len() as u64 {
                        log::info!(
                            "Cache save skipped: existing cache ({} KB) is larger than \
                             incomplete new cache ({} KB)",
                            meta.len() / 1024, bytes.len() / 1024
                        );
                        return;
                    }
                }
            }
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

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
