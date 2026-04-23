//! Source-file discovery — finding Kotlin/Java/Swift files on disk.
//!
//! Three entry points:
//! - [`find_source_files`] — full workspace scan (fd with walkdir fallback).
//! - [`find_source_files_unconstrained`] — scan without hardcoded dir exclusions
//!   (used for explicit `sourcePaths` entries).
//! - [`warm_discover_files`] — warm-start optimisation: uses the cache manifest
//!   plus an incremental `fd --changed-within` pass.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::indexer::cache::{workspace_cache_path, IndexCache};
use crate::rg::{IgnoreMatcher, SOURCE_EXTENSIONS};

// ─── full scan ───────────────────────────────────────────────────────────────

pub(super) fn find_source_files(root: &Path, matcher: Option<&IgnoreMatcher>) -> Vec<PathBuf> {
    let root_str = root.to_string_lossy();

    let mut fd_args: Vec<String> = vec!["--type".into(), "f".into()];
    for ext in SOURCE_EXTENSIONS {
        fd_args.push("--extension".into());
        fd_args.push(ext.to_string());
    }
    let hardcoded: &[&str] = &[
        "--absolute-path",
        "--exclude", ".git",
        "--exclude", "build",
        "--exclude", "target",
        "--exclude", ".gradle",
        "--exclude", ".build",       // SwiftPM
        "--exclude", "DerivedData",  // Xcode
        "--exclude", "Generated",    // SwiftGen / R.swift codegen output
    ];
    for a in hardcoded {
        fd_args.push(a.to_string());
    }
    if let Some(m) = matcher {
        for pat in &m.patterns {
            fd_args.push("--exclude".into());
            fd_args.push(pat.clone());
        }
    }
    fd_args.push(".".into());
    fd_args.push(root_str.to_string());

    let fd_result = Command::new("fd").args(&fd_args).output();
    if let Ok(out) = fd_result {
        if out.status.success() {
            let paths: Vec<_> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect();
            if !paths.is_empty() {
                return paths;
            }
        }
    }

    log::info!("fd not available or found nothing; falling back to walkdir");
    walkdir_find(root, matcher)
}

fn walkdir_find(root: &Path, matcher: Option<&IgnoreMatcher>) -> Vec<PathBuf> {
    const EXCLUDED_DIRS: &[&str] = &[
        ".git", "build", "target", ".gradle", ".build", "DerivedData", "Generated",
    ];
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .standard_filters(true)
        .hidden(false)
        .parents(false);

    let root_owned = root.to_path_buf();
    let user_glob_set: Option<Arc<globset::GlobSet>> =
        matcher.filter(|m| !m.is_empty()).map(|m| m.glob_set());

    builder.filter_entry(move |entry| {
        let path = entry.path();
        if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                if EXCLUDED_DIRS.contains(&dir_name) {
                    return false;
                }
            }
            if let Some(gs) = &user_glob_set {
                let rel = path.strip_prefix(&root_owned).unwrap_or(path);
                if gs.is_match(rel) {
                    return false;
                }
                if let Some(name) = path.file_name() {
                    if gs.is_match(Path::new(name)) {
                        return false;
                    }
                }
            }
        }
        true
    });

    let user_glob_set_files: Option<Arc<globset::GlobSet>> =
        matcher.filter(|m| !m.is_empty()).map(|m| m.glob_set());
    let root_owned2 = root.to_path_buf();

    for result in builder.build() {
        if let Ok(entry) = result {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                    if SOURCE_EXTENSIONS.contains(&ext) {
                        if let Some(gs) = &user_glob_set_files {
                            let rel = path.strip_prefix(&root_owned2).unwrap_or(path);
                            if gs.is_match(rel) {
                                continue;
                            }
                        }
                        paths.push(path.to_path_buf());
                    }
                }
            }
        }
    }
    paths
}

/// Discover source files under `root` without hardcoded directory exclusions.
///
/// Used for explicit `sourcePaths` entries — the user chose the directory
/// deliberately, so we trust their intent and don't skip `build`, `.gradle`, etc.
pub(super) fn find_source_files_unconstrained(root: &Path) -> Vec<PathBuf> {
    let root_str = root.to_string_lossy();
    let mut fd_args: Vec<String> = vec!["--type".into(), "f".into()];
    for ext in SOURCE_EXTENSIONS {
        fd_args.push("--extension".into());
        fd_args.push(ext.to_string());
    }
    fd_args.push("--absolute-path".into());
    fd_args.push(".".into());
    fd_args.push(root_str.to_string());

    let fd_result = Command::new("fd").args(&fd_args).output();
    if let Ok(out) = fd_result {
        if out.status.success() {
            let paths: Vec<_> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect();
            if !paths.is_empty() {
                return paths;
            }
        }
    }

    log::info!("fd not available; falling back to walkdir for source path {}", root.display());
    let mut paths = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder.standard_filters(false).hidden(false).parents(false);
    for result in builder.build() {
        if let Ok(entry) = result {
            let path = entry.path();
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

// ─── incremental scan (used by warm-start) ───────────────────────────────────

/// Returns files created or modified within the last `elapsed_secs` seconds.
/// Falls back to empty list if fd is unavailable (warm start still works via cache manifest).
fn find_source_files_newer_than(
    root: &Path,
    elapsed_secs: u64,
    matcher: Option<&IgnoreMatcher>,
) -> Vec<PathBuf> {
    let root_str = root.to_string_lossy();
    let window = format!("{}s", elapsed_secs);
    let mut fd_args: Vec<String> = vec![
        "--type".into(), "f".into(),
        "--changed-within".into(), window,
    ];
    for ext in SOURCE_EXTENSIONS {
        fd_args.push("--extension".into());
        fd_args.push(ext.to_string());
    }
    let hardcoded: &[&str] = &[
        "--absolute-path",
        "--exclude", ".git",
        "--exclude", "build",
        "--exclude", "target",
        "--exclude", ".gradle",
        "--exclude", ".build",
        "--exclude", "DerivedData",
        "--exclude", "Generated",
    ];
    for a in hardcoded {
        fd_args.push(a.to_string());
    }
    if let Some(m) = matcher {
        for pat in &m.patterns {
            fd_args.push("--exclude".into());
            fd_args.push(pat.clone());
        }
    }
    fd_args.push(".".into());
    fd_args.push(root_str.to_string());

    match Command::new("fd").args(&fd_args).output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect(),
        _ => vec![],
    }
}

// ─── warm-start discovery ────────────────────────────────────────────────────

/// Discover source files using a warm-start optimisation when a cache exists.
///
/// **Cold start** (no cache): full `fd`/walkdir scan — same as `find_source_files`.
///
/// **Warm start** (cache present): use the cache's file manifest to avoid the
/// O(total_dirs) `fd` scan. Only runs an incremental `fd --changed-within` pass
/// to catch files added or modified since the cache was last saved.
///
/// Inspired by the rust-analyzer VFS and gopls file-manifest patterns.
pub(super) fn warm_discover_files(
    root: &Path,
    cache: &IndexCache,
    matcher: Option<&IgnoreMatcher>,
) -> Vec<PathBuf> {
    let cache_path = workspace_cache_path(root);

    let elapsed_secs = std::fs::metadata(&cache_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs().saturating_add(2))
        .unwrap_or(u64::MAX);

    if elapsed_secs == u64::MAX {
        log::info!("warm_discover_files: cannot stat cache file, falling back to full scan");
        return find_source_files(root, matcher);
    }

    // Phase 1: all previously cached files, filtered through the current ignore
    // matcher so newly-configured ignorePatterns take effect on warm start.
    // Skip paths no longer on disk (e.g. deleted by a branch switch).
    let cached_paths: HashSet<String> = cache.entries.keys()
        .filter(|p| {
            if let Some(m) = matcher {
                if !m.is_empty() {
                    let path = Path::new(p.as_str());
                    let rel = path.strip_prefix(root).unwrap_or(path);
                    if m.matches(rel) { return false; }
                }
            }
            true
        })
        .cloned()
        .collect();
    let mut paths: Vec<PathBuf> = cached_paths.iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    let on_disk_cached_count = paths.len();

    // Phase 2: incremental fd pass — only files newer than the cache.
    // Modified files are already in Phase 1; they will fail the mtime check
    // in the scan phase and be re-parsed then.
    let newer = find_source_files_newer_than(root, elapsed_secs, matcher);
    let new_count = newer.iter()
        .filter(|f| !cached_paths.contains(f.to_string_lossy().as_ref()))
        .count();
    for f in newer {
        if !cached_paths.contains(f.to_string_lossy().as_ref()) {
            paths.push(f);
        }
    }

    log::info!(
        "Warm start: {} cached (on-disk) + {} new files (scanned last {}s window)",
        on_disk_cached_count, new_count, elapsed_secs
    );
    paths
}

#[cfg(test)]
#[path = "discover_tests.rs"]
mod tests;
