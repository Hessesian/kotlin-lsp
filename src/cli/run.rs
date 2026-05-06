//! CLI command runner.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tower_lsp::lsp_types::Location;

use crate::indexer::{Indexer, NoopReporter};
use crate::rg::{rg_find_definition, rg_word_search, RgSearchRequest};

use super::args::{CliArgs, Mode, OutputFmt, Subcommand};
use super::hover::hover_at;
use super::output::{print_results, CliResult};

// ── Root resolution ───────────────────────────────────────────────────────────

/// Resolve the workspace root: explicit --root, then nearest .git ancestor, then cwd.
fn resolve_root(explicit: Option<&Path>) -> PathBuf {
    if let Some(r) = explicit {
        return r.to_path_buf();
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut cur = cwd.as_path();
    loop {
        if cur.join(".git").exists() {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
    cwd
}

// ── Cache probe ───────────────────────────────────────────────────────────────

fn cache_exists(root: &Path) -> bool {
    crate::indexer::workspace_cache_path(root).exists()
}

// ── Indexer bootstrap ─────────────────────────────────────────────────────────

/// Build (or load from cache) a full workspace index.  Reports progress to stderr.
async fn build_index(root: &Path) -> Arc<Indexer> {
    let idx = Arc::new(Indexer::new());
    Arc::clone(&idx)
        .index_workspace_full(root, Arc::new(NoopReporter))
        .await;
    idx
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

    let dummy_uri: tower_lsp::lsp_types::Url =
        tower_lsp::lsp_types::Url::from_file_path(root)
            .unwrap_or_else(|_| "file:///".parse().unwrap());

    let request = RgSearchRequest::new(
        name,
        None,
        None,
        Some(root),
        true,
        &dummy_uri,
        &decl_files,
    );
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
    let root = resolve_root(args.root.as_deref());
    let json = args.fmt == OutputFmt::Json;

    match args.subcommand {
        // ── index ─────────────────────────────────────────────────────────────
        Subcommand::Index => {
            eprintln!("Indexing workspace: {}", root.display());
            let idx = build_index(&root).await;
            eprintln!(
                "Done: {} files, {} symbols",
                idx.files.len(),
                idx.definitions.len()
            );
        }

        // ── find ──────────────────────────────────────────────────────────────
        Subcommand::Find { name } => {
            let effective_mode = effective_mode(args.mode, &root, "find");
            let results = match effective_mode {
                Mode::Fast => fast_find(&name, &root),
                _ => {
                    let idx = build_index(&root).await;
                    smart_find(&idx, &name, &root)
                }
            };
            if results.is_empty() {
                if !json {
                    eprintln!("No declarations found for '{name}'");
                }
                std::process::exit(1);
            }
            print_results(&results, json);
        }

        // ── refs ──────────────────────────────────────────────────────────────
        Subcommand::Refs { name } => {
            let effective_mode = effective_mode(args.mode, &root, "refs");
            let results = match effective_mode {
                Mode::Fast => fast_refs(&name, &root),
                _ => {
                    let idx = build_index(&root).await;
                    smart_refs(&idx, &name, &root)
                }
            };
            if results.is_empty() {
                if !json {
                    eprintln!("No references found for '{name}'");
                }
                std::process::exit(1);
            }
            print_results(&results, json);
        }

        // ── hover ─────────────────────────────────────────────────────────────
        Subcommand::Hover { file, line, col } => {
            let effective_mode = effective_mode(args.mode, &root, "hover");
            if effective_mode == Mode::Fast {
                eprintln!("hover requires index; run `kotlin-lsp index` first or remove --fast");
                std::process::exit(1);
            }
            let idx = build_index(&root).await;
            match hover_at(&idx, &file, line, col) {
                Some(text) => {
                    if json {
                        let obj = serde_json::json!({ "signature": text });
                        println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_default());
                    } else {
                        println!("{text}");
                    }
                }
                None => {
                    eprintln!("No symbol found at {}:{}:{}", file.display(), line, col);
                    std::process::exit(1);
                }
            }
        }
    }
}

// ── Mode resolution ───────────────────────────────────────────────────────────

fn effective_mode(requested: Mode, root: &Path, subcommand: &str) -> Mode {
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
                eprintln!(
                    "note: no index cache found for {}; using rg/fd (fast mode). \
                     Run `kotlin-lsp index` for precise results.",
                    root.display()
                );
                Mode::Fast
            }
        }
    }
}
