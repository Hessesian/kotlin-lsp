//! CLI command runner.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tower_lsp::lsp_types::Location;

use crate::indexer::{Indexer, NoopReporter};
use crate::rg::{rg_find_definition, rg_word_search, RgSearchRequest};

use super::args::{CliArgs, Mode, OutputFmt, Subcommand};
use super::complete::completions_at;
use super::hover::hover_at;
use super::output::{attach_context, print_results, CliResult};
use super::tokens::{dump_tree, print_token_rows, token_rows, token_rows_phases};

/// Severity label strings used when printing diagnostics in text mode.
const SEVERITY_ERROR: &str = "error";
const SEVERITY_WARNING: &str = "warning";
const SEVERITY_INFO: &str = "info";
const SEVERITY_HINT: &str = "hint";
const SEVERITY_DIAG: &str = "diag";

// ── Root resolution ───────────────────────────────────────────────────────────

/// Resolve the workspace root: explicit --root, then nearest .git ancestor, then cwd.
fn resolve_root(explicit: Option<&Path>) -> PathBuf {
    let raw = if let Some(r) = explicit {
        r.to_path_buf()
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        find_git_root(&cwd).unwrap_or(cwd)
    };
    strip_unc_prefix(raw.canonicalize().unwrap_or(raw))
}

/// Strip the `\\?\` extended-length UNC prefix that `Path::canonicalize()` adds
/// on Windows. Paths with this prefix confuse external tools like `rg`.
fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    path
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
    let raw = if let Some(r) = explicit {
        r.to_path_buf()
    } else {
        let file_dir = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
        let file_dir = file_dir.parent().unwrap_or(&file_dir);
        let fallback = || {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            find_git_root(&cwd).unwrap_or(cwd)
        };
        find_git_root(file_dir).unwrap_or_else(fallback)
    };
    strip_unc_prefix(raw.canonicalize().unwrap_or(raw))
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
/// 2. `~/.kmp-lsp/sources` — the default `extract-sources` output dir
///    (skipped when `no_stdlib` is true)
pub(crate) async fn build_index(root: &Path, no_stdlib: bool) -> Arc<Indexer> {
    build_index_inner(root, collect_cli_source_paths(root, no_stdlib)).await
}

async fn build_index_inner(root: &Path, source_paths: Vec<String>) -> Arc<Indexer> {
    let idx = Arc::new(Indexer::new());
    // Canonicalize so relative roots (e.g. ".") don't confuse path.starts_with checks
    // in index_source_paths when comparing absolute fd output against workspace_root.
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    idx.workspace_root.set(canonical.clone());
    if !source_paths.is_empty() {
        *idx.source_paths_raw.write().unwrap() = source_paths;
    }
    // Populate workspace source roots from workspace.json so resolver/infer rg fallbacks
    // are scoped when the CLI is run in a project with configured module sourceRoots.
    // Uses `canonical`, not `root`: `<WORKSPACE>` substitution must produce absolute
    // paths so later `starts_with` checks against absolute `file://` URIs actually match
    // when the CLI is invoked with a relative root (e.g. `.`).
    let workspace_roots: Vec<String> = crate::workspace_json::load_source_paths(&canonical)
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    if !workspace_roots.is_empty() {
        *idx.workspace_source_roots.write().unwrap() = workspace_roots;
    }
    // Per-module real Gradle dependency data, for the hierarchy-walk
    // ambiguity-safe tail's module-scoped narrowing (see
    // `resolver::resolve::ambiguity_safe_tail_with_denylist`). Empty when no
    // real `workspace.json` module data is present. Also uses `canonical` for
    // the same reason as `load_source_paths` above.
    let module_dependencies = crate::workspace_json::load_module_dependencies(&canonical);
    if !module_dependencies.is_empty() {
        *idx.module_dependencies
            .write()
            .unwrap_or_else(|e| e.into_inner()) = module_dependencies;
    }
    Arc::clone(&idx)
        .index_workspace_full(&canonical, Arc::new(NoopReporter))
        .await;
    idx
}

/// Collect source paths for CLI indexing: workspace.json + default extract dir.
///
/// When `workspace.json` declares no JetBrains module source roots, Gradle/Maven
/// build-layout paths under `root` are included so CLI completions behave like
/// the full LSP path. External library paths (outside the workspace root) are
/// always included via the configured `sourcePaths` key or the default
/// `~/.kmp-lsp/sources` directory.
///
/// When `no_stdlib` is true, `~/.kmp-lsp/sources` is excluded regardless of
/// whether it appears in `workspace.json` or is auto-detected. Use this for fast
/// workspace-only completions (~2s vs ~10s).
fn collect_cli_source_paths(root: &Path, no_stdlib: bool) -> Vec<String> {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    #[allow(deprecated)]
    let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let default_sources = home.join(".kmp-lsp").join("sources");
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

    // When workspace files declare no source roots, try Gradle/Maven build
    // layout detection so `complete` behaves like the LSP path.
    // Only include paths outside the workspace root — internal paths are already
    // discovered and fully indexed by the workspace scan. Re-indexing them via
    // index_source_paths would double-parse ~11k files, doubling memory usage.
    if json_paths.is_empty() {
        for p in crate::workspace_json::detect_build_layout_source_paths(root) {
            if is_external(&p) {
                let s = p.to_string_lossy().into_owned();
                if !paths.contains(&s) {
                    paths.push(s);
                }
            }
        }
    }

    // `workspace.json` `sourcePaths` key — explicit library overrides.
    // When present (even as `[]`), it takes precedence over the default
    // `~/.kmp-lsp/sources` directory so a project can opt out entirely.
    if let Some(configured) = crate::workspace_json::load_configured_source_paths(root) {
        for p in configured {
            if is_external(&p) && !(no_stdlib && is_stdlib(&p)) {
                let s = p.to_string_lossy().into_owned();
                if !paths.contains(&s) {
                    paths.push(s);
                }
            }
        }
    } else if !no_stdlib {
        // Auto-include the well-known `extract-sources` output dir if present.
        if default_sources.is_dir() {
            let s = default_sources.to_string_lossy().into_owned();
            if !paths.contains(&s) {
                paths.push(s);
            }
        }
    }

    // Android SDK sources — always added when detectable, independent of
    // --no-stdlib (SDK sources are platform APIs, not stdlib).
    for p in crate::workspace_json::detect_android_sdk_source_paths(root) {
        let s = p.to_string_lossy().into_owned();
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

// ── Workspace source roots for CLI ───────────────────────────────────────────

/// Load workspace.json module sourceRoots to scope rg searches in the CLI.
/// Mirrors the subset of `Backend::collect_workspace_source_roots` relevant for CLI.
fn cli_workspace_source_roots(root: &Path) -> Vec<String> {
    crate::workspace_json::load_source_paths(root)
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

// ── Smart-mode find ───────────────────────────────────────────────────────────

fn smart_find(indexer: &Arc<Indexer>, name: &str, root: &Path) -> Vec<CliResult> {
    // Query definitions index for exact name match.
    let locs = indexer.definition_locations(name);
    if !locs.is_empty() {
        return locs_to_results(locs, name, "");
    }
    let source_roots = cli_workspace_source_roots(root);
    let locs = rg_find_definition(name, Some(root), &source_roots, None);
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

    let source_roots = cli_workspace_source_roots(root);
    let request = RgSearchRequest::new(name, None, None, Some(root), true, &dummy_uri, &decl_files)
        .with_source_paths(&source_roots);
    let locs = crate::rg::rg_find_references(&request, None);
    locs_to_results(locs, name, "")
}

// ── Fast-mode find ────────────────────────────────────────────────────────────

fn fast_find(name: &str, root: &Path) -> Vec<CliResult> {
    let source_roots = cli_workspace_source_roots(root);
    let locs = rg_find_definition(name, Some(root), &source_roots, None);
    locs_to_results(locs, name, "")
}

// ── Fast-mode refs ────────────────────────────────────────────────────────────

fn fast_refs(name: &str, root: &Path) -> Vec<CliResult> {
    let source_roots = cli_workspace_source_roots(root);
    let locs = rg_word_search(name, root, &source_roots);
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
        Subcommand::Refs {
            name,
            exclude_imports,
            context,
        } => {
            let root = resolve_root(args.root.as_deref());
            run_refs(
                &root,
                args.mode,
                json,
                verbose,
                &name,
                exclude_imports,
                context,
            )
            .await
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
        Subcommand::Diagnose { file, only } => {
            let root = resolve_root_for_file(args.root.as_deref(), &file);
            run_diagnose(&root, &file, verbose, only.as_deref()).await
        }
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
        Subcommand::Check { files } => {
            if files.is_empty() {
                eprintln!("check requires at least one FILE or DIR argument");
                std::process::exit(1);
            }
            let expanded = super::check::collect_files(&files);
            super::check::run_check(&expanded, json);
        }
        Subcommand::MissingImports { root } => {
            let root = resolve_root(root.as_deref().or(args.root.as_deref()));
            super::missing_import_poc::run_missing_imports(&root).await;
        }
        Subcommand::UnusedImports { root } => {
            let root = resolve_root(root.as_deref().or(args.root.as_deref()));
            super::unused_import_poc::run_unused_imports(&root).await;
        }
        Subcommand::ResolutionAccuracy { root } => {
            let root = resolve_root(root.as_deref().or(args.root.as_deref()));
            super::resolution_accuracy_poc::run_resolution_accuracy(&root).await;
        }
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

/// Extends a Gradle-cache JAR scan with the Android SDK's compiled
/// `android.jar`, for CLI paths (`find`, `diagnose`) that call
/// `scan_gradle_jars` directly and therefore bypass `ScanHandler`'s own
/// wiring of `detect_android_sdk_jar_path`. Reuses
/// `compiled_jar_paths_with_android_sdk` rather than duplicating the
/// detection logic — passes `true` for its JVM-source gate unconditionally
/// since the CLI's own Gradle-cache scan already runs unconditionally here
/// (no JVM-source gate at these call sites to mirror).
fn compiled_jar_paths_for_cli(root: &Path, gradle_paths: Vec<PathBuf>) -> Vec<PathBuf> {
    crate::workspace::scan_handler::compiled_jar_paths_with_android_sdk(
        Some(root.to_path_buf()),
        true,
        gradle_paths,
    )
}

async fn run_find(root: &Path, mode: Mode, json: bool, verbose: bool, name: &str) {
    let results = match effective_mode(mode, root, "find", verbose) {
        Mode::Fast => fast_find(name, root),
        _ => {
            let index = build_index(root, false).await;
            // Scan Gradle JARs (+ the Android SDK jar, if detected) so
            // stdlib/library/Android symbols resolve in find queries.
            let jars =
                compiled_jar_paths_for_cli(root, crate::indexer::jar::scan_gradle_jars(None));
            if !jars.is_empty() {
                if let Ok(mut sidecar_guard) = index.jar_sidecar.lock() {
                    crate::indexer::jar::clear_jar_maps(&index);
                    let total = crate::indexer::jar::index_jars(&index, &jars, &mut sidecar_guard);
                    if let Ok(mut phase) = index.jar_phase.lock() {
                        *phase = crate::indexer::jar_phase::JarPhase::Ready { count: total };
                    }
                }
            }
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

async fn run_refs(
    root: &Path,
    mode: Mode,
    json: bool,
    verbose: bool,
    name: &str,
    exclude_imports: bool,
    context: Option<u32>,
) {
    let mut results = match effective_mode(mode, root, "refs", verbose) {
        Mode::Fast => fast_refs(name, root),
        _ => {
            let index = build_index(root, false).await;
            smart_refs(&index, name, root)
        }
    };

    // Shared across the --exclude-imports filter and --context attachment below,
    // so a file matched by both passes is only read from disk once.
    let mut file_lines_cache: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    if exclude_imports {
        results.retain(|result| {
            // Smart-mode results may carry kind="import" directly.
            if result.kind == "import" {
                return false;
            }
            // For rg-based results with no kind, check the line on disk.
            if !result.kind.is_empty() {
                return true;
            }
            let lines = file_lines_cache
                .entry(result.file.clone())
                .or_insert_with(|| {
                    std::fs::read_to_string(&result.file)
                        .map(|src| src.lines().map(String::from).collect())
                        .unwrap_or_default()
                });
            lines
                .get(result.line.saturating_sub(1) as usize)
                .map(|line| !line.trim_start().starts_with("import "))
                .unwrap_or(true)
        });
    }

    exit_if_empty(&results, json, &format!("No references found for '{name}'"));
    if let Some(n) = context {
        attach_context(&mut results, n, &mut file_lines_cache);
    }
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
        eprintln!("hover requires index; run `kmp-lsp index` first or remove --fast");
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
            eprintln!("Loading workspace index (--no-stdlib, skipping ~/.kmp-lsp/sources)...");
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

/// Whether `name` should run, given the `--only` filter (`None` = run everything).
fn diagnostic_enabled(only: Option<&[String]>, name: &str) -> bool {
    only.is_none_or(|names| names.iter().any(|n| n == name))
}

async fn run_diagnose(root: &Path, file: &Path, _verbose: bool, only: Option<&[String]>) {
    use crate::features::call_arg_diagnostics::call_arg_diagnostics;
    use crate::features::fill_when::when_diagnostics;
    use crate::features::missing_import_diagnostics::missing_import_diagnostics;
    use crate::features::nullable_call_diagnostics::nullable_dot_call_diagnostics;
    use tower_lsp::lsp_types::Url;

    let syntax_enabled = diagnostic_enabled(only, "syntax");
    let call_arg_enabled = diagnostic_enabled(only, "call-arg");
    let nullable_enabled = diagnostic_enabled(only, "nullable");
    let when_enabled = diagnostic_enabled(only, "when");
    let missing_import_enabled = diagnostic_enabled(only, "missing-import");
    // Building the index + JAR scan is the expensive part of this command —
    // skip it entirely for a `--only syntax` request, same as `check`.
    let needs_index =
        call_arg_enabled || nullable_enabled || when_enabled || missing_import_enabled;

    let abs_path = if file.is_absolute() {
        file.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(file)
    };
    let source = std::fs::read_to_string(&abs_path).unwrap_or_else(|e| {
        eprintln!("error: cannot read file: {e}");
        std::process::exit(1);
    });
    let path_str = abs_path.to_string_lossy();
    if crate::indexer::live_tree::lang_for_path(&path_str).is_none() {
        eprintln!("error: unsupported file extension");
        std::process::exit(1);
    }

    // Emit syntax errors first (tree-sitter, no index required).
    let syntax_data = crate::parser::parse_by_extension(&path_str, &source);
    if syntax_enabled {
        for syntax_error in &syntax_data.syntax_errors {
            let line = syntax_error.range.start.line + 1;
            let col = syntax_error.range.start.character + 1;
            println!("{}:{} [error]: {}", line, col, syntax_error.message);
        }
    }

    let mut diagnostics = Vec::new();
    if needs_index {
        eprintln!("Indexing {}...", root.display());
        let index = build_index(root, true).await;
        eprintln!(
            "Indexed: {} files, {} symbols",
            index.files.len(),
            index.definitions.len()
        );

        // Index Gradle JARs and resolve `jar_phase` to a TERMINAL state (same
        // pattern as `run_find`). Without this, a machine with the sidecar
        // binary installed leaves the phase at its initial `Pending` forever —
        // and `call_arg_diagnostics`/`nullable_dot_call_diagnostics` suppress
        // themselves while the phase reads as loading, so `diagnose` printed NO
        // semantic diagnostics at all. (Machines WITHOUT the sidecar start at
        // `Unavailable`, which is terminal — which is why the gap only showed up
        // once a sidecar became present, e.g. in CI after the sidecar artifact
        // was wired into the test jobs.)
        let jars = compiled_jar_paths_for_cli(root, crate::indexer::jar::scan_gradle_jars(None));
        if let Ok(mut sidecar_guard) = index.jar_sidecar.lock() {
            let total = if jars.is_empty() {
                0
            } else {
                crate::indexer::jar::clear_jar_maps(&index);
                crate::indexer::jar::index_jars(&index, &jars, &mut sidecar_guard)
            };
            if let Ok(mut phase) = index.jar_phase.lock() {
                *phase = crate::indexer::jar_phase::JarPhase::Ready { count: total };
            }
        }

        let uri = Url::from_file_path(&abs_path).unwrap_or_else(|_| {
            eprintln!("error: cannot convert path to URI: {}", abs_path.display());
            std::process::exit(1);
        });

        // store_live_tree parses the file once; retrieve the result via live_doc()
        // so call_arg_diagnostics can use the same tree without a second parse.
        index.store_live_tree(&uri, &source);
        let doc = index.live_doc(&uri).unwrap_or_else(|| {
            eprintln!("error: failed to parse file");
            std::process::exit(1);
        });

        if call_arg_enabled {
            diagnostics.extend(call_arg_diagnostics(&index, &uri, &doc));
        }
        if nullable_enabled {
            diagnostics.extend(nullable_dot_call_diagnostics(&index, &uri, &doc));
        }
        if when_enabled {
            diagnostics.extend(when_diagnostics(&index, &uri));
        }
        if missing_import_enabled {
            diagnostics.extend(missing_import_diagnostics(&index, &uri, &doc));
        }
    }

    let printed_syntax_errors = syntax_enabled && !syntax_data.syntax_errors.is_empty();
    if !printed_syntax_errors && diagnostics.is_empty() {
        println!("No diagnostics.");
    } else {
        for diag in &diagnostics {
            let line = diag.range.start.line + 1;
            let col = diag.range.start.character + 1;
            let severity = diag
                .severity
                .map(|s| match s {
                    tower_lsp::lsp_types::DiagnosticSeverity::ERROR => SEVERITY_ERROR,
                    tower_lsp::lsp_types::DiagnosticSeverity::WARNING => SEVERITY_WARNING,
                    tower_lsp::lsp_types::DiagnosticSeverity::INFORMATION => SEVERITY_INFO,
                    tower_lsp::lsp_types::DiagnosticSeverity::HINT => SEVERITY_HINT,
                    _ => SEVERITY_DIAG,
                })
                .unwrap_or(SEVERITY_DIAG);
            println!("{}:{} [{}]: {}", line, col, severity, diag.message);
        }
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
                     Run `kmp-lsp index` first."
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
                         Run `kmp-lsp index` for precise results.",
                        root.display()
                    );
                }
                Mode::Fast
            }
        }
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
