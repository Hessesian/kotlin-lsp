//! CLI command runner.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tower_lsp::lsp_types::Location;

use crate::indexer::{Indexer, NoopReporter};
use crate::rg::{rg_find_definition, rg_word_search, RgSearchRequest};

use super::args::{CliArgs, Mode, OutputFmt, Subcommand};
use super::complete::completions_at;
use super::hover::hover_at;
use super::output::{print_results, CliResult};
use super::tokens::{dump_tree, print_token_rows, token_rows, token_rows_phases};

// ── Root resolution ───────────────────────────────────────────────────────────

/// Resolve the workspace root: explicit --root, then nearest .git ancestor, then cwd.
fn resolve_root(explicit: Option<&Path>) -> PathBuf {
    if let Some(r) = explicit {
        return r.to_path_buf();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    find_git_root(&cwd).unwrap_or(cwd)
}

/// Walk up from `start` looking for a `.git` directory.
fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut cur = start;
    loop {
        if cur.join(".git").exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

/// Resolve workspace root for file-centric commands: tries explicit root first,
/// then walks up from the file's directory, then falls back to CWD-based detection.
fn resolve_root_for_file(explicit: Option<&Path>, file: &Path) -> PathBuf {
    if let Some(r) = explicit {
        return r.to_path_buf();
    }
    let file_dir = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let file_dir = file_dir.parent().unwrap_or(&file_dir);
    if let Some(root) = find_git_root(file_dir) {
        return root;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    find_git_root(&cwd).unwrap_or(cwd)
}

// ── Column resolution helpers ─────────────────────────────────────────────────

/// Resolve a 1-based UTF-16 column for `complete`, applying `--dot` / `--eol`
/// when an explicit col is absent or when the flags are set.
///
/// - `--dot` (`dot=true`): position just after the last `.` on the line.
///   Returns `Err` if the line contains no `.`.
/// - `--eol` (`eol=true`): position after the last non-whitespace character.
///   Returns `Err` if the line is blank/whitespace-only.
/// - explicit col: used as-is.
/// - fallback (no flags, no col): col 1 (beginning of line).
fn resolve_col(
    file: &Path,
    line: u32,
    col: Option<u32>,
    dot: bool,
    eol: bool,
) -> Result<u32, String> {
    if !dot && !eol {
        return Ok(col.unwrap_or(1));
    }
    let line_text = read_line(file, line)?;
    if dot {
        col_after_last_dot(&line_text).ok_or_else(|| format!("no '.' found on line {line}"))
    } else {
        col_after_last_nonws(&line_text)
            .ok_or_else(|| format!("line {line} is blank — cannot use --eol"))
    }
}

/// Read line `line` (1-based) from `file` using a buffered reader —
/// stops at the target line without loading the whole file.
/// Returns `Err` on I/O error or when `line` is out of range.
fn read_line(file: &Path, line: u32) -> Result<String, String> {
    use std::io::BufRead;
    let f =
        std::fs::File::open(file).map_err(|e| format!("cannot open {}: {e}", file.display()))?;
    let reader = std::io::BufReader::new(f);
    let target = (line as usize).saturating_sub(1);
    reader
        .lines()
        .nth(target)
        .ok_or_else(|| format!("line {line} is out of range in {}", file.display()))?
        .map_err(|e| format!("cannot read line {line} from {}: {e}", file.display()))
}

/// Return 1-based UTF-16 column just after the last `.` in `text`, or `None`
/// if there is no dot.
fn col_after_last_dot(text: &str) -> Option<u32> {
    // byte index of last '.'
    let dot_byte = text.rfind('.')?;
    // UTF-16 length up to and including the dot, then +1 for "after the dot"
    let utf16_before: usize = text[..dot_byte].encode_utf16().count();
    // +2: +1 for the dot itself, +1 for 1-based
    Some((utf16_before + 2) as u32)
}

/// Return 1-based UTF-16 column just after the last non-whitespace character,
/// or `None` if the line is blank.
fn col_after_last_nonws(text: &str) -> Option<u32> {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    let utf16_len = trimmed.encode_utf16().count();
    Some((utf16_len + 1) as u32)
}

// ── Cache probe ───────────────────────────────────────────────────────────────

fn cache_exists(root: &Path) -> bool {
    crate::indexer::workspace_cache_path(root).exists()
}

// ── Indexer bootstrap ─────────────────────────────────────────────────────────

/// Build (or load from cache) a full workspace index.  Reports progress to stderr.
///
/// Source paths are collected from:
/// 1. `workspace.json` (JetBrains IDE format) `sourcePaths` field at the workspace root
/// 2. `~/.kotlin-lsp/sources` — the default `extract-sources` output dir
///    (skipped when `no_stdlib` is true)
async fn build_index(root: &Path, no_stdlib: bool) -> Arc<Indexer> {
    build_index_inner(root, collect_cli_source_paths(root, no_stdlib)).await
}

/// Build a full workspace index with explicitly provided source paths.
/// Bypasses all workspace.json / global-default discovery — for tests.
#[cfg(test)]
pub(crate) async fn build_index_with_sources(
    root: &Path,
    source_paths: Vec<std::path::PathBuf>,
) -> Arc<Indexer> {
    let strs: Vec<String> = source_paths
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    build_index_inner(root, strs).await
}

async fn build_index_inner(root: &Path, source_paths: Vec<String>) -> Arc<Indexer> {
    let idx = Arc::new(Indexer::new());
    if !source_paths.is_empty() {
        *idx.source_paths_raw.write().unwrap() = source_paths;
    }
    Arc::clone(&idx)
        .index_workspace_full(root, Arc::new(NoopReporter))
        .await;
    idx
}

/// Collect source paths for CLI indexing: workspace.json + default extract dir.
///
/// Build-layout paths auto-detected under `root` are intentionally excluded —
/// those files are already covered by `index_workspace_full`'s workspace scan.
/// Only paths that live *outside* the workspace root need a separate indexing pass.
///
/// When `no_stdlib` is true, `~/.kotlin-lsp/sources` is excluded regardless of
/// whether it appears in `workspace.json` or is auto-detected. Use this for fast
/// workspace-only completions (~2s vs ~10s).
fn collect_cli_source_paths(root: &Path, no_stdlib: bool) -> Vec<String> {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    #[allow(deprecated)]
    let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let default_sources = home.join(".kotlin-lsp").join("sources");
    let canonical_default_sources = default_sources
        .canonicalize()
        .unwrap_or_else(|_| default_sources.clone());

    let is_external = |p: &std::path::PathBuf| -> bool {
        let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
        !canonical.starts_with(&canonical_root)
    };
    let is_stdlib = |p: &std::path::PathBuf| -> bool {
        let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
        canonical == canonical_default_sources
    };

    let mut paths: Vec<String> = Vec::new();

    let json_paths = crate::workspace_json::load_source_paths(root);
    for p in &json_paths {
        if is_external(p) && !(no_stdlib && is_stdlib(p)) {
            let s = p.to_string_lossy().into_owned();
            if !paths.contains(&s) {
                paths.push(s);
            }
        }
    }

    // If workspace.json declares explicit sourcePaths, use those and skip the
    // global default.  An absent key (None) falls through to the global default.
    if let Some(configured) = crate::workspace_json::load_configured_source_paths(root) {
        for p in configured {
            if is_external(&p) && !(no_stdlib && is_stdlib(&p)) {
                let s = p.to_string_lossy().into_owned();
                if !paths.contains(&s) {
                    paths.push(s);
                }
            }
        }
        return paths;
    }

    if no_stdlib {
        return paths;
    }

    // Auto-include the well-known `extract-sources` output dir if present.
    if default_sources.is_dir() {
        let s = default_sources.to_string_lossy().into_owned();
        if !paths.contains(&s) {
            paths.push(s);
        }
    }

    paths
}

// ── Location helpers ─────────────────────────────────────────────────────────

fn locs_to_results(locs: Vec<Location>, name: &str, kind: &str) -> Vec<CliResult> {
    locs.iter()
        .filter_map(|l| CliResult::from_location(l, name, kind))
        .collect()
}

// ── Smart-mode find ───────────────────────────────────────────────────────────

fn smart_find(indexer: &Arc<Indexer>, name: &str, root: &Path) -> Vec<CliResult> {
    // Query definitions index for exact name match.
    let locs = indexer.definition_locations(name);
    if !locs.is_empty() {
        return locs_to_results(locs, name, "");
    }
    // Fallback to rg so smart mode still covers edge cases (generics, type aliases).
    let locs = rg_find_definition(name, Some(root), None);
    locs_to_results(locs, name, "")
}

// ── Smart-mode refs ───────────────────────────────────────────────────────────

fn smart_refs(indexer: &Arc<Indexer>, name: &str, root: &Path) -> Vec<CliResult> {
    let decl_locs = indexer.definition_locations(name);
    let decl_files: Vec<String> = decl_locs
        .iter()
        .filter_map(|l| l.uri.to_file_path().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    let dummy_uri: tower_lsp::lsp_types::Url = tower_lsp::lsp_types::Url::from_file_path(root)
        .unwrap_or_else(|_| "file:///".parse().unwrap());

    let request = RgSearchRequest::new(name, None, None, Some(root), true, &dummy_uri, &decl_files);
    let locs = crate::rg::rg_find_references(&request, None);
    locs_to_results(locs, name, "")
}

// ── Fast-mode find ────────────────────────────────────────────────────────────

fn fast_find(name: &str, root: &Path) -> Vec<CliResult> {
    let locs = rg_find_definition(name, Some(root), None);
    locs_to_results(locs, name, "")
}

// ── Fast-mode refs ────────────────────────────────────────────────────────────

fn fast_refs(name: &str, root: &Path) -> Vec<CliResult> {
    let locs = rg_word_search(name, root);
    locs_to_results(locs, name, "")
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub(crate) async fn run(args: CliArgs) {
    let json = args.fmt == OutputFmt::Json;
    let verbose = args.verbose;

    match args.subcommand {
        Subcommand::Index => {
            let root = resolve_root(args.root.as_deref());
            run_index(&root, verbose).await
        }
        Subcommand::Find { name } => {
            let root = resolve_root(args.root.as_deref());
            run_find(&root, args.mode, json, verbose, &name).await
        }
        Subcommand::Refs { name } => {
            let root = resolve_root(args.root.as_deref());
            run_refs(&root, args.mode, json, verbose, &name).await
        }
        Subcommand::Hover { file, line, col } => {
            let root = resolve_root_for_file(args.root.as_deref(), &file);
            run_hover(&root, args.mode, json, verbose, &file, line, col).await
        }
        Subcommand::Complete {
            file,
            line,
            col,
            dot,
            eol,
            no_stdlib,
        } => {
            let root = resolve_root_for_file(args.root.as_deref(), &file);
            let resolved_col = match resolve_col(&file, line, col, dot, eol) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
            run_complete(&root, json, verbose, &file, line, resolved_col, no_stdlib).await
        }
        Subcommand::Tokens {
            file,
            cst_only,
            resolve,
            phases,
            show_tree,
        } => {
            let root = resolve_root_for_file(args.root.as_deref(), &file);
            let use_index = resolve && !cst_only;
            let index = if use_index {
                if verbose {
                    eprintln!("Loading index for Phase 2 resolution...");
                }
                Some(build_index(&root, false).await)
            } else {
                None
            };
            run_tokens(json, &file, index.as_ref(), cst_only, phases, show_tree)
        }
        Subcommand::Tree { file } => run_tree(&file),
        Subcommand::Sources => {
            let root = resolve_root(args.root.as_deref());
            super::sources::run_sources(&root, json)
        }
        Subcommand::ExtractSources {
            gradle_home,
            output,
            dry_run,
            patterns,
        } => super::extract_sources::run_extract_sources(super::extract_sources::ExtractOptions {
            gradle_home,
            output,
            dry_run,
            patterns,
        }),
    }
}

async fn run_index(root: &Path, verbose: bool) {
    if verbose {
        eprintln!("Indexing workspace: {}", root.display());
    }
    let index = build_index(root, false).await;
    if verbose {
        eprintln!(
            "Done: {} files, {} symbols",
            index.files.len(),
            index.definitions.len()
        );
    }
}

async fn run_find(root: &Path, mode: Mode, json: bool, verbose: bool, name: &str) {
    let results = match effective_mode(mode, root, "find", verbose) {
        Mode::Fast => fast_find(name, root),
        _ => {
            let index = build_index(root, false).await;
            smart_find(&index, name, root)
        }
    };
    exit_if_empty(
        &results,
        json,
        &format!("No declarations found for '{name}'"),
    );
    print_results(&results, json);
}

async fn run_refs(root: &Path, mode: Mode, json: bool, verbose: bool, name: &str) {
    let results = match effective_mode(mode, root, "refs", verbose) {
        Mode::Fast => fast_refs(name, root),
        _ => {
            let index = build_index(root, false).await;
            smart_refs(&index, name, root)
        }
    };
    exit_if_empty(&results, json, &format!("No references found for '{name}'"));
    print_results(&results, json);
}

async fn run_hover(
    root: &Path,
    mode: Mode,
    json: bool,
    verbose: bool,
    file: &Path,
    line: u32,
    col: u32,
) {
    if effective_mode(mode, root, "hover", verbose) == Mode::Fast {
        eprintln!("hover requires index; run `kotlin-lsp index` first or remove --fast");
        std::process::exit(1);
    }
    let index = build_index(root, false).await;
    let Some(text) = hover_at(&index, file, line, col) else {
        eprintln!("No symbol found at {}:{}:{}", file.display(), line, col);
        std::process::exit(1);
    };
    if json {
        let object = serde_json::json!({ "signature": text });
        println!(
            "{}",
            serde_json::to_string_pretty(&object).unwrap_or_default()
        );
    } else {
        println!("{text}");
    }
}

async fn run_complete(
    root: &Path,
    json: bool,
    verbose: bool,
    file: &Path,
    line: u32,
    col: u32,
    no_stdlib: bool,
) {
    if verbose {
        if no_stdlib {
            eprintln!("Loading workspace index (--no-stdlib, skipping ~/.kotlin-lsp/sources)...");
        } else {
            eprintln!("Loading index for completion...");
        }
    }
    let index = build_index(root, no_stdlib).await;
    let rows = completions_at(&index, file, line, col);
    if rows.is_empty() {
        eprintln!("No completions at {}:{}:{}", file.display(), line, col);
        std::process::exit(1);
    }
    if json {
        let arr: Vec<_> = rows
            .iter()
            .map(|r| {
                let mut obj = serde_json::json!({
                    "label": r.label,
                    "kind": r.kind,
                });
                if !r.detail.is_empty() {
                    obj["detail"] = serde_json::Value::String(r.detail.clone());
                }
                if let Some(ref import) = r.import {
                    obj["import"] = serde_json::Value::String(import.clone());
                }
                obj
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
    } else {
        for row in &rows {
            let import_hint = row
                .import
                .as_deref()
                .map(|i| format!("  [{i}]"))
                .unwrap_or_default();
            if row.detail.is_empty() {
                println!("{:<40} {}{}", row.label, row.kind, import_hint);
            } else {
                println!(
                    "{:<40} {}  {}{}",
                    row.label, row.kind, row.detail, import_hint
                );
            }
        }
        eprintln!("({} items)", rows.len());
    }
}

fn run_tokens(
    json: bool,
    file: &Path,
    index: Option<&Arc<Indexer>>,
    cst_only: bool,
    phases: bool,
    show_tree: bool,
) {
    if phases {
        match token_rows_phases(file, index) {
            Ok(output) => print!("{output}"),
            Err(error) => {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        return;
    }
    match token_rows(file, index, cst_only) {
        Ok(rows) => {
            print_token_rows(&rows, json);
            if show_tree {
                eprintln!();
                if let Err(error) = dump_tree(file) {
                    eprintln!("tree: {error}");
                }
            }
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}

fn run_tree(file: &Path) {
    if let Err(error) = dump_tree(file) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn exit_if_empty(results: &[CliResult], json: bool, message: &str) {
    if results.is_empty() {
        if !json {
            eprintln!("{message}");
        }
        std::process::exit(1);
    }
}

// ── Mode resolution ───────────────────────────────────────────────────────────

fn effective_mode(requested: Mode, root: &Path, subcommand: &str, verbose: bool) -> Mode {
    match requested {
        Mode::Fast => Mode::Fast,
        Mode::Smart => {
            if !cache_exists(root) {
                eprintln!(
                    "error: --smart requires a pre-built index. \
                     Run `kotlin-lsp index` first."
                );
                std::process::exit(1);
            }
            Mode::Smart
        }
        Mode::Auto => {
            if cache_exists(root) {
                Mode::Smart
            } else {
                if subcommand == "hover" {
                    // hover can't work without index; report clearly
                    return Mode::Smart; // will build index
                }
                if verbose {
                    eprintln!(
                        "note: no index cache found for {}; using rg/fd (fast mode). \
                         Run `kotlin-lsp index` for precise results.",
                        root.display()
                    );
                }
                Mode::Fast
            }
        }
    }
}
