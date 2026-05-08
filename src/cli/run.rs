//! CLI command runner.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tower_lsp::lsp_types::Location;

use crate::indexer::{Indexer, NoopReporter};
use crate::rg::{rg_find_definition, rg_word_search, RgSearchRequest};

use super::args::{CliArgs, Mode, OutputFmt, Subcommand};
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
    let root = resolve_root(args.root.as_deref());
    let json = args.fmt == OutputFmt::Json;
    let verbose = args.verbose;

    match args.subcommand {
        Subcommand::Index => run_index(&root, verbose).await,
        Subcommand::Find { name } => run_find(&root, args.mode, json, verbose, &name).await,
        Subcommand::Refs { name } => run_refs(&root, args.mode, json, verbose, &name).await,
        Subcommand::Hover { file, line, col } => {
            run_hover(&root, args.mode, json, verbose, &file, line, col).await
        }
        Subcommand::Tokens {
            file,
            cst_only,
            resolve,
            phases,
            show_tree,
        } => {
            let use_index = resolve && !cst_only;
            let index = if use_index {
                if verbose {
                    eprintln!("Loading index for Phase 2 resolution...");
                }
                Some(build_index(&root).await)
            } else {
                None
            };
            run_tokens(json, &file, index.as_ref(), cst_only, phases, show_tree)
        }
        Subcommand::Tree { file } => run_tree(&file),
        Subcommand::Sources => super::sources::run_sources(&root, json),
    }
}

async fn run_index(root: &Path, verbose: bool) {
    if verbose {
        eprintln!("Indexing workspace: {}", root.display());
    }
    let index = build_index(root).await;
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
            let index = build_index(root).await;
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
            let index = build_index(root).await;
            smart_refs(&index, name, root)
        }
    };
    exit_if_empty(&results, json, &format!("No references found for '{name}'"));
    print_results(&results, json);
}

async fn run_hover(root: &Path, mode: Mode, json: bool, verbose: bool, file: &Path, line: u32, col: u32) {
    if effective_mode(mode, root, "hover", verbose) == Mode::Fast {
        eprintln!("hover requires index; run `kotlin-lsp index` first or remove --fast");
        std::process::exit(1);
    }
    let index = build_index(root).await;
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

fn run_tokens(json: bool, file: &Path, index: Option<&Arc<Indexer>>, cst_only: bool, phases: bool, show_tree: bool) {
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
