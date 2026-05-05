//! Workspace scanning — orchestrates file discovery, cache-partitioning,
//! concurrent parsing, and LSP progress notifications.
//!
//! Entry points (all `impl Indexer` methods):
//! - [`Indexer::index_workspace`]            — normal LSP startup (bounded).
//! - [`Indexer::index_workspace_full`]       — unbounded (CLI `--index-only` / reindex command).
//! - [`Indexer::index_workspace_prioritized`]— fast first-open: priority files first, then full scan.
//! - [`Indexer::save_cache_to_disk`]         — serialise current index to `~/.cache/kotlin-lsp/`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use tower_lsp::lsp_types::*;

/// Maximum number of file-read failures to log before suppressing further warnings.
const MAX_READ_FAILURES_LOGGED: usize = 5;

use crate::indexer::{
    cache::{cache_entry_to_file_result, save_cache, try_load_cache, write_status_file},
    discover::{find_source_files, warm_discover_files},
    Indexer, MAX_FILES_UNLIMITED,
};
use crate::rg::{regex_escape, IgnoreMatcher, SOURCE_EXTENSIONS};
use crate::task_runner::run_concurrent;
use crate::types::{FileIndexResult, IndexStats, WorkspaceIndexResult};

// ─── LSP progress notification ────────────────────────────────────────────────

/// Counters describing a workspace scan pass, used for progress notifications
/// and the status-file JSON.
#[derive(Copy, Clone)]
struct ProgressSummary {
    /// Number of files that need (re-)parsing (cache misses).
    parse_count: usize,
    /// Number of files discovered minus truncation (used in progress message).
    indexed_count: usize,
    /// Total files discovered before any truncation limit.
    total: usize,
    /// Number of files satisfied from disk cache.
    cache_hits: usize,
    /// Whether discovery was truncated by a file-count limit.
    truncated: bool,
}

mod progress {
    use tower_lsp::lsp_types::ProgressParams;

    /// `$/progress` notification — reports workspace indexing status to the editor.
    pub(super) enum KotlinProgress {}
    impl tower_lsp::lsp_types::notification::Notification for KotlinProgress {
        type Params = ProgressParams;
        const METHOD: &'static str = "$/progress";
    }
}

// ─── RAII guard ───────────────────────────────────────────────────────────────

/// Clears `indexing_in_progress` on drop (success, panic, or early return).
struct IndexingGuard {
    indexer: Arc<Indexer>,
}

impl Drop for IndexingGuard {
    fn drop(&mut self) {
        self.indexer
            .indexing_in_progress
            .store(false, std::sync::atomic::Ordering::Release);
        log::debug!("IndexingGuard: cleared indexing_in_progress flag");
    }
}

// ─── Max-files resolution ─────────────────────────────────────────────────────

/// Hard cap on workspace files indexed eagerly in LSP mode.
/// Override via `KOTLIN_LSP_MAX_FILES` environment variable.
/// Default is unlimited — the parse cost per file is low enough after
/// query/parser caching that indexing all files is the right default.
pub(super) const DEFAULT_MAX_INDEX_FILES: usize = MAX_FILES_UNLIMITED;

/// Pure: resolve the maximum number of files to eagerly index.
///
/// Reads `KOTLIN_LSP_MAX_FILES` from the environment on each call.
/// Returns `default` when the variable is absent or not a valid integer.
///
/// - LSP mode callers pass `DEFAULT_MAX_INDEX_FILES` (unlimited).
/// - CLI `--index-only` callers pass `MAX_FILES_UNLIMITED`.
///   Note: setting `KOTLIN_LSP_MAX_FILES` in the environment will still cap the count.
pub fn resolve_max_files(default: usize) -> usize {
    std::env::var("KOTLIN_LSP_MAX_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ─── Helper: locate source files for given type names ─────────────────────────

/// Use `rg` to find the source files that declare any of the given type names.
/// Returns an empty vec when `names` is empty or `rg` fails.
pub(crate) fn find_files_for_types(
    names: &[String],
    root: &Path,
    matcher: Option<&IgnoreMatcher>,
) -> Vec<PathBuf> {
    if names.is_empty() {
        return vec![];
    }
    let alts = names
        .iter()
        .map(|n| regex_escape(n))
        .collect::<Vec<_>>()
        .join("|");
    let pattern = format!(
        r"(?:abstract\s+class|open\s+class|class|interface|object|struct|protocol)\s+(?:{alts})\b"
    );
    let mut cmd = Command::new("rg");
    cmd.args(["--no-heading", "--with-filename", "-l"]);
    for ext in SOURCE_EXTENSIONS {
        cmd.args(["--glob", &format!("*.{ext}")]);
    }
    cmd.args(["-e", &pattern]);
    cmd.arg(root);
    let out = match cmd.output() {
        Ok(o) if !o.stdout.is_empty() => o,
        _ => return vec![],
    };
    let paths: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    match matcher {
        None => paths,
        Some(m) => m.filter_paths(paths),
    }
}

#[derive(Clone)]
struct ParseWorkItem {
    path: PathBuf,
    key: String,
    start_gen: u64,
}

struct ScanSession<'a> {
    start_gen: u64,
    root_generation: &'a std::sync::atomic::AtomicU64,
    scheduled_paths: &'a dashmap::DashMap<String, u64>,
}

impl<'a> ScanSession<'a> {
    fn is_stale(&self) -> bool {
        self.root_generation
            .load(std::sync::atomic::Ordering::SeqCst)
            != self.start_gen
    }

    fn schedule_path(&self, path: &PathBuf) -> Option<ParseWorkItem> {
        let key = std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.clone())
            .to_string_lossy()
            .to_string();

        match self.scheduled_paths.entry(key.clone()) {
            dashmap::mapref::entry::Entry::Occupied(mut o) => {
                let existing_gen = *o.get();
                if existing_gen == self.start_gen {
                    log::debug!(
                        "Skipped scheduling parse for {} (already scheduled gen {})",
                        key,
                        existing_gen
                    );
                    return None;
                }
                o.insert(self.start_gen);
            }
            dashmap::mapref::entry::Entry::Vacant(v) => {
                v.insert(self.start_gen);
            }
        }

        Some(ParseWorkItem {
            path: path.clone(),
            key,
            start_gen: self.start_gen,
        })
    }
}

struct PartitionResult {
    cached: Vec<FileIndexResult>,
    to_parse: Vec<PathBuf>,
    cache_hits: usize,
    aborted: bool,
}

struct DiscoveredPaths {
    paths: Vec<PathBuf>,
    total: usize,
    indexed_count: usize,
    truncated: bool,
}

struct ScanSetup {
    guard: IndexingGuard,
    start_gen: u64,
    cache: Option<super::cache::IndexCache>,
    discovered: DiscoveredPaths,
}

#[derive(Clone, Default)]
struct ParseCounters {
    gen_skipped: Arc<std::sync::atomic::AtomicUsize>,
    read_failed: Arc<std::sync::atomic::AtomicUsize>,
    url_failed: Arc<std::sync::atomic::AtomicUsize>,
    panic_failed: Arc<std::sync::atomic::AtomicUsize>,
}

impl ParseCounters {
    fn log_summary(&self, total: usize) {
        let gen_skip_n = self.gen_skipped.load(std::sync::atomic::Ordering::Relaxed);
        let read_fail_n = self.read_failed.load(std::sync::atomic::Ordering::Relaxed);
        let url_fail_n = self.url_failed.load(std::sync::atomic::Ordering::Relaxed);
        let panic_n = self.panic_failed.load(std::sync::atomic::Ordering::Relaxed);
        log::info!(
            "All {} parse tasks done: gen_skipped={}, read_failed={}, url_failed={}, panics={}",
            total,
            gen_skip_n,
            read_fail_n,
            url_fail_n,
            panic_n
        );
    }
}

fn aborted_scan_result(root: &Path) -> WorkspaceIndexResult {
    WorkspaceIndexResult {
        files: Vec::new(),
        stats: IndexStats::default(),
        workspace_root: root.to_path_buf(),
        aborted: true,
        complete_scan: false,
    }
}

fn queue_reindex_request(indexer: &Indexer, root: &Path, max: usize) {
    *indexer.pending_reindex_root.write().unwrap() = Some(root.to_path_buf());
    indexer
        .pending_reindex_max
        .store(max, std::sync::atomic::Ordering::Release);
    indexer
        .pending_reindex
        .store(true, std::sync::atomic::Ordering::Release);
    indexer
        .root_generation
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

fn prepare_scan(indexer: &Arc<Indexer>, root: &Path, max: usize) -> ScanSetup {
    let guard = IndexingGuard {
        indexer: Arc::clone(indexer),
    };
    *indexer.workspace_root.write().unwrap() = Some(root.to_path_buf());

    let start_gen = indexer
        .root_generation
        .load(std::sync::atomic::Ordering::SeqCst);
    let cache = try_load_cache(root);
    let matcher: Option<Arc<IgnoreMatcher>> = indexer.ignore_matcher.read().unwrap().clone();
    let discovered = discover_workspace_paths(root, max, &cache, matcher.as_deref());

    ScanSetup {
        guard,
        start_gen,
        cache,
        discovered,
    }
}

fn discover_workspace_paths(
    root: &Path,
    max: usize,
    cache: &Option<super::cache::IndexCache>,
    matcher_ref: Option<&IgnoreMatcher>,
) -> DiscoveredPaths {
    let mut paths = if let Some(cache) = cache.as_ref().filter(|c| c.complete_scan) {
        warm_discover_files(root, cache, matcher_ref)
    } else {
        find_source_files(root, matcher_ref)
    };
    let total = paths.len();
    let effective_max = if cache.as_ref().is_some_and(|c| c.complete_scan) {
        MAX_FILES_UNLIMITED
    } else {
        max
    };

    paths.sort_by_key(|p| p.components().count());
    let paths: Vec<_> = paths.into_iter().take(effective_max).collect();
    let indexed_count = paths.len();
    let truncated = total > effective_max && effective_max != MAX_FILES_UNLIMITED;

    if truncated {
        log::warn!(
            "Large project: eagerly indexing {indexed_count}/{total} files \
             (shallowest first). Deeper files resolved on-demand via rg. \
             Set KOTLIN_LSP_MAX_FILES env var to raise the limit."
        );
    } else {
        log::info!(
            "Indexing {} source files under {}",
            indexed_count,
            root.display()
        );
    }

    DiscoveredPaths {
        paths,
        total,
        indexed_count,
        truncated,
    }
}

fn partition_cache_hits(
    paths: &[PathBuf],
    cache: &Option<super::cache::IndexCache>,
    session: &ScanSession,
) -> PartitionResult {
    let mut to_parse = Vec::new();
    let mut cached = Vec::new();
    let mut cache_hits = 0;
    let mut aborted = false;

    for path in paths {
        if session.is_stale() {
            log::info!("index_workspace_impl: generation changed, aborting partition");
            aborted = true;
            break;
        }

        let path_str = path.to_string_lossy().to_string();
        let meta = std::fs::metadata(path);
        let mtime = meta
            .as_ref()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let on_disk_size = meta.map(|m| m.len()).unwrap_or(u64::MAX);

        if let Some(c) = cache.as_ref() {
            if let Some(entry) = c.entries.get(&path_str) {
                if entry.mtime_secs == mtime && entry.file_size == on_disk_size {
                    if let Ok(uri) = Url::from_file_path(path) {
                        cached.push(cache_entry_to_file_result(&uri, entry));
                        cache_hits += 1;
                        continue;
                    }
                }
            }
        }

        to_parse.push(path.clone());
    }

    PartitionResult {
        cached,
        to_parse,
        cache_hits,
        aborted,
    }
}

fn schedule_parse_work(
    to_parse: Vec<PathBuf>,
    session: &ScanSession,
) -> (Vec<ParseWorkItem>, bool) {
    let mut work_items = Vec::new();
    let mut aborted = false;

    for path in to_parse {
        if session.is_stale() {
            log::info!(
                "index_workspace_impl: generation changed during scheduling, aborting remaining parses"
            );
            aborted = true;
            break;
        }

        if let Some(item) = session.schedule_path(&path) {
            work_items.push(item);
        }
    }

    (work_items, aborted)
}

fn spawn_progress_reporter(
    idx: Arc<Indexer>,
    client: Option<tower_lsp::Client>,
    token: NumberOrString,
    total: usize,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if client.is_none() || total == 0 {
            return;
        }

        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let done = idx
                .parse_tasks_completed
                .load(std::sync::atomic::Ordering::Relaxed);
            let pct = ((done * 100) / total) as u32;
            if let Some(ref c) = client {
                c.send_notification::<progress::KotlinProgress>(ProgressParams {
                    token: token.clone(),
                    value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(
                        WorkDoneProgressReport {
                            cancellable: Some(false),
                            message: Some(format!("{done}/{total} files…")),
                            percentage: Some(pct),
                        },
                    )),
                })
                .await;
            }
            if done >= total {
                break;
            }
        }
    })
}

fn write_indexing_started_status(root: &Path, summary: &ProgressSummary) {
    let started_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let root_escaped = serde_json::to_string(&root.to_string_lossy().as_ref()).unwrap_or_default();
    let (parse_count, cache_hits) = (summary.parse_count, summary.cache_hits);
    write_status_file(&format!(
        r#"{{"phase":"indexing","workspace":{root_escaped},"indexed":0,"total":{parse_count},"cache_hits":{cache_hits},"symbols":0,"started_at":{started_unix},"elapsed_secs":0,"estimated_total_secs":null}}"#
    ));
}

fn write_indexing_done_status(
    root: &Path,
    files_parsed: usize,
    cache_hits: usize,
    symbols: usize,
    elapsed: u64,
) {
    let root_escaped = serde_json::to_string(&root.to_string_lossy().as_ref()).unwrap_or_default();
    write_status_file(&format!(
        r#"{{"phase":"done","workspace":{root_escaped},"indexed":{files_parsed},"total":{actually_indexed},"cache_hits":{cache_hits},"symbols":{symbols},"elapsed_secs":{elapsed},"estimated_total_secs":null}}"#,
        actually_indexed = files_parsed + cache_hits,
    ));
}

async fn send_progress_begin(
    client: &Option<tower_lsp::Client>,
    token: &NumberOrString,
    summary: &ProgressSummary,
) {
    if let Some(client) = client.as_ref() {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            client.send_request::<tower_lsp::lsp_types::request::WorkDoneProgressCreate>(
                WorkDoneProgressCreateParams {
                    token: token.clone(),
                },
            ),
        )
        .await;
    }

    let (parse_count, indexed_count, total, cache_hits, truncated) = (
        summary.parse_count,
        summary.indexed_count,
        summary.total,
        summary.cache_hits,
        summary.truncated,
    );
    let begin_msg = if cache_hits > 0 {
        format!("Indexing {parse_count}/{indexed_count} files ({cache_hits} cached)…")
    } else if truncated {
        format!("Indexing {indexed_count}/{total} Kotlin files (shallowest first)…")
    } else {
        format!("Indexing {total} Kotlin files…")
    };

    if let Some(client) = client.as_ref() {
        client
            .send_notification::<progress::KotlinProgress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: "kotlin-lsp".into(),
                        cancellable: Some(false),
                        message: Some(begin_msg),
                        percentage: Some(0),
                    },
                )),
            })
            .await;
    }
}

async fn run_parse_phase(
    idx: Arc<Indexer>,
    need_parse: Vec<PathBuf>,
    session: &ScanSession<'_>,
    client: &Option<tower_lsp::Client>,
    token: &NumberOrString,
    parse_count: usize,
) -> (Vec<Option<FileIndexResult>>, bool) {
    idx.scheduled_paths.clear();
    let (work_items, aborted) = schedule_parse_work(need_parse, session);

    log::info!("Parsing {} files concurrently", work_items.len());
    let idx_ref = Arc::clone(&idx);
    let sem = Arc::clone(&idx.parse_sem);
    let progress_handle =
        spawn_progress_reporter(Arc::clone(&idx), client.clone(), token.clone(), parse_count);
    let counters = ParseCounters::default();
    let task_counters = counters.clone();
    let results = run_concurrent(work_items, sem, move |item, sem| {
        let idx = Arc::clone(&idx_ref);
        let counters = task_counters.clone();
        async move { parse_work_item(idx, item, sem, counters).await }
    })
    .await;

    progress_handle.abort();
    counters.log_summary(results.len());
    (results, aborted)
}

fn build_workspace_result(
    root: &Path,
    discovered: &DiscoveredPaths,
    cached_results: Vec<FileIndexResult>,
    results: Vec<Option<FileIndexResult>>,
    parse_count: usize,
    cache_hits: usize,
) -> WorkspaceIndexResult {
    let mut parsed_results: Vec<FileIndexResult> = results.into_iter().flatten().collect();
    let files_parsed = parsed_results.len();
    let parse_errors = parse_count - files_parsed;
    let mut all_results = cached_results;
    all_results.append(&mut parsed_results);

    let stats = IndexStats {
        files_discovered: discovered.total,
        cache_hits,
        files_parsed,
        symbols_extracted: all_results.iter().map(|f| f.data.symbols.len()).sum(),
        packages_found: all_results
            .iter()
            .filter_map(|f| f.data.package.as_ref())
            .count(),
        errors: parse_errors,
    };

    log::info!(
        "Workspace indexing complete: {} parsed, {} cache hits, {} errors ({} total)",
        files_parsed,
        cache_hits,
        parse_errors,
        all_results.len()
    );

    WorkspaceIndexResult {
        files: all_results,
        stats,
        workspace_root: root.to_path_buf(),
        aborted: false,
        complete_scan: !discovered.truncated,
    }
}

async fn send_progress_end(
    client: &Option<tower_lsp::Client>,
    token: &NumberOrString,
    result: &WorkspaceIndexResult,
) {
    if let Some(client) = client.as_ref() {
        client
            .send_notification::<progress::KotlinProgress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: Some(format!(
                        "Indexed {} files ({} cached, {} parsed)",
                        result.files.len(),
                        result.stats.cache_hits,
                        result.stats.files_parsed
                    )),
                })),
            })
            .await;
    }
}

async fn parse_work_item(
    idx: Arc<Indexer>,
    item: ParseWorkItem,
    sem: Arc<tokio::sync::Semaphore>,
    counters: ParseCounters,
) -> Option<FileIndexResult> {
    log::debug!("Parsing: {}", item.path.display());

    if idx
        .root_generation
        .load(std::sync::atomic::Ordering::SeqCst)
        != item.start_gen
    {
        counters
            .gen_skipped
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return None;
    }

    let content = match tokio::fs::read_to_string(&item.path).await {
        Ok(c) => c,
        Err(e) => {
            let n = counters
                .read_failed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < MAX_READ_FAILURES_LOGGED {
                log::warn!("Could not read {}: {}", item.path.display(), e);
            }
            return None;
        }
    };

    let uri = match Url::from_file_path(&item.path) {
        Ok(u) => u,
        Err(_) => {
            counters
                .url_failed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            log::warn!("Invalid file path: {}", item.path.display());
            return None;
        }
    };

    let _permit = sem.acquire().await.expect("parse semaphore closed unexpectedly");
    let t0 = std::time::Instant::now();
    let uri_clone = uri.clone();
    let parse_result = match tokio::task::spawn_blocking(move || {
        Indexer::parse_file(&uri_clone, &content)
    })
    .await
    {
        Ok(result) => result,
        Err(e) => {
            counters
                .panic_failed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            log::warn!("Parse task panicked for {}: {}", item.path.display(), e);
            return None;
        }
    };
    let took = t0.elapsed().as_millis();

    log::debug!("Parsed {} in {} ms", item.path.display(), took);

    let should_remove = idx
        .scheduled_paths
        .get(&item.key)
        .map(|gen| *gen == item.start_gen)
        .unwrap_or(false);
    if should_remove {
        idx.scheduled_paths.remove(&item.key);
    }

    let threshold: u128 = std::env::var("KOTLIN_LSP_PARSE_LOG_MS")
        .ok()
        .and_then(|v| v.parse::<u128>().ok())
        .unwrap_or(1000);
    if took > threshold {
        log::warn!("Slow parse: {} took {} ms", item.path.display(), took);
    }

    idx.parse_tasks_completed
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    Some(parse_result)
}

// ─── impl Indexer ─────────────────────────────────────────────────────────────

impl Indexer {
    /// Full reindex passing `MAX_FILES_UNLIMITED` as the default file cap —
    /// used by `--index-only` CLI mode and the `kotlin-lsp/reindex` workspace command.
    /// The `KOTLIN_LSP_MAX_FILES` environment variable can still override the count.
    pub async fn index_workspace_full(
        self: Arc<Self>,
        root: &Path,
        client: Option<tower_lsp::Client>,
    ) {
        let max = resolve_max_files(MAX_FILES_UNLIMITED);
        let (result, guard_opt) = Arc::clone(&self)
            .index_workspace_impl(root, max, client.clone())
            .await;
        if let Some(guard) = guard_opt {
            if !result.aborted {
                Arc::clone(&self)
                    .finalize_workspace_scan(result, guard, client.clone())
                    .await;
            }
        }
        Arc::clone(&self).run_pending_reindex(client).await;
    }

    /// Normal LSP startup: bounded workspace scan.
    /// Sends LSP `$/progress` notifications so the editor shows a status spinner.
    /// On subsequent startups the on-disk cache is used for unchanged files so only
    /// modified or new files need to be re-parsed by tree-sitter.
    pub async fn index_workspace(self: Arc<Self>, root: &Path, client: Option<tower_lsp::Client>) {
        // workspace_root is updated inside index_workspace_impl after the
        // concurrency guard is acquired, so we never set a stale root here.
        let max = resolve_max_files(DEFAULT_MAX_INDEX_FILES);
        let (result, guard_opt) = Arc::clone(&self)
            .index_workspace_impl(root, max, client.clone())
            .await;
        if let Some(guard) = guard_opt {
            if !result.aborted {
                Arc::clone(&self)
                    .finalize_workspace_scan(result, guard, client.clone())
                    .await;
            }
        }
        Arc::clone(&self).run_pending_reindex(client).await;
    }

    /// Prioritized indexing: parse `initial_paths` first (high-priority files such as the
    /// currently-open document and its supertypes), then continue with normal bounded indexing.
    /// This gives fast symbol availability for the files the user is actually working on.
    pub async fn index_workspace_prioritized(
        self: Arc<Self>,
        root: &Path,
        initial_paths: Vec<PathBuf>,
        client: Option<tower_lsp::Client>,
    ) {
        // workspace_root is updated inside index_workspace_impl; don't set it
        // here to avoid leaving a stale root if the impl aborts early.

        // Guard priority parsing: if a scan is already running, skip it to
        // avoid mutating the shared index concurrently.
        if !self
            .indexing_in_progress
            .load(std::sync::atomic::Ordering::Acquire)
            && !initial_paths.is_empty()
        {
            let sem = Arc::clone(&self.parse_sem);

            let mut priority_paths: Vec<PathBuf> = Vec::new();
            for path in initial_paths {
                if !path.exists() {
                    continue;
                }
                let content = match tokio::fs::read_to_string(&path).await {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let Ok(uri) = Url::from_file_path(&path) else {
                    continue;
                };
                // Index now so that (a) we can read FileData.supers directly and
                // (b) the priority-loop's index_content call hash-skips this file —
                // avoiding a double parse for each initial_paths entry.
                let idx_c = Arc::clone(&self);
                let uri_c = uri.clone();
                let cont_c = content.clone();
                let supers: Vec<String> = tokio::task::spawn_blocking(move || {
                    idx_c
                        .index_content(&uri_c, &cont_c)
                        .map(|d| d.supers.iter().map(|(_, n, _)| n.clone()).collect())
                        .unwrap_or_default()
                })
                .await
                .unwrap_or_default();
                // Expand priority set to include supertypes so cross-class navigation
                // (super, override resolution) works before the full scan completes.
                if !supers.is_empty() {
                    let m = self.ignore_matcher.read().unwrap().clone();
                    let super_paths = find_files_for_types(&supers, root, m.as_deref());
                    for sp in super_paths {
                        if !priority_paths.contains(&sp) {
                            priority_paths.push(sp);
                        }
                    }
                }
                priority_paths.push(path);
            }
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
                                let _ = tokio::task::spawn_blocking(move || {
                                    idx.index_content(&uri, &content)
                                })
                                .await;
                            }
                        }
                    }
                }));
            }
            for h in handles {
                let _ = h.await;
            }
        }

        let max = resolve_max_files(DEFAULT_MAX_INDEX_FILES);
        let (result, guard_opt) = Arc::clone(&self)
            .index_workspace_impl(root, max, client.clone())
            .await;
        if let Some(guard) = guard_opt {
            if !result.aborted {
                Arc::clone(&self)
                    .finalize_workspace_scan(result, guard, client.clone())
                    .await;
            }
        }
        Arc::clone(&self).run_pending_reindex(client).await;
    }

    /// If a reindex was queued while a scan was in progress, run it now.
    ///
    /// Called at the end of every public scan function, after the full workflow
    /// (impl + apply + source_paths + save_cache) completes. Mirrors RA's
    /// `OpQueue` pattern: at most one pending request is retained (last wins).
    async fn run_pending_reindex(self: Arc<Self>, client: Option<tower_lsp::Client>) {
        loop {
            // Never consume the pending flag while another scan is still active.
            // The finishing scan will call run_pending_reindex itself and drain it.
            if self
                .indexing_in_progress
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return;
            }
            // Atomically claim the queued request; if nothing is pending, we're done.
            if self
                .pending_reindex
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_err()
            {
                return;
            }
            let root_opt = self.pending_reindex_root.write().unwrap().take();
            let root = match root_opt {
                Some(r) => r,
                None => match self.workspace_root.read().unwrap().clone() {
                    Some(r) => r,
                    None => return,
                },
            };
            // Use the max stored when the request was queued so a full (unbounded) reindex
            // that was queued during a bounded scan keeps its unlimited cap.
            let max = self
                .pending_reindex_max
                .load(std::sync::atomic::Ordering::Acquire);
            log::info!(
                "run_pending_reindex: starting queued reindex for {}",
                root.display()
            );
            let (result, guard_opt) = Arc::clone(&self)
                .index_workspace_impl(&root, max, client.clone())
                .await;
            if result.aborted {
                // Lost the scan guard to a concurrent caller — restore the queued
                // request so that caller's run_pending_reindex will drain it.
                {
                    let mut pending_root = self.pending_reindex_root.write().unwrap();
                    if pending_root.is_none() {
                        *pending_root = Some(root);
                    }
                }
                self.pending_reindex
                    .store(true, std::sync::atomic::Ordering::Release);
                return;
            }
            if let Some(guard) = guard_opt {
                Arc::clone(&self)
                    .finalize_workspace_scan(result, guard, client.clone())
                    .await;
            }
            // Loop: drain any request that arrived while this queued reindex was running.
        }
    }

    /// Apply scan results, index source paths, and save the cache — while keeping
    /// `indexing_in_progress` true for the full duration via `_guard`.
    async fn finalize_workspace_scan(
        self: Arc<Self>,
        result: WorkspaceIndexResult,
        _guard: IndexingGuard,
        _client: Option<tower_lsp::Client>,
    ) {
        self.last_scan_complete
            .store(result.complete_scan, std::sync::atomic::Ordering::Release);
        let root = result.workspace_root.clone();
        self.apply_workspace_result(&result);
        Arc::clone(&self).index_source_paths(root).await;
        self.save_cache_to_disk();
        // _guard dropped here → indexing_in_progress cleared
    }

    /// Core workspace indexing: file discovery → cache partition → concurrent parse.
    /// Returns `(result, guard)`. `guard` is `Some` iff this call successfully acquired
    /// `indexing_in_progress`; callers must hold it alive until the full workflow completes
    /// (apply + source_paths + save_cache). `result.aborted` may be true with either
    /// `Some` or `None` guard depending on the abort reason; callers should skip finalization
    /// whenever `result.aborted` is true.
    async fn index_workspace_impl(
        self: Arc<Self>,
        root: &Path,
        max: usize,
        client: Option<tower_lsp::Client>,
    ) -> (WorkspaceIndexResult, Option<IndexingGuard>) {
        if self
            .indexing_in_progress
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            queue_reindex_request(&self, root, max);
            log::warn!(
                "index_workspace_impl: scan in progress; queued reindex for {} \
                 and interrupted current run via root_generation bump.",
                root.display()
            );
            return (aborted_scan_result(root), None);
        }

        let ScanSetup {
            guard,
            start_gen,
            cache,
            discovered,
        } = prepare_scan(&self, root, max);
        let session = ScanSession {
            start_gen,
            root_generation: &self.root_generation,
            scheduled_paths: &self.scheduled_paths,
        };
        let PartitionResult {
            cached: cached_results,
            to_parse: need_parse,
            cache_hits,
            aborted: mut aborted_early,
        } = partition_cache_hits(&discovered.paths, &cache, &session);
        let parse_count = need_parse.len();

        log::info!("Cache: {cache_hits} hits, {parse_count} files need (re-)parsing");
        log::debug!("About to spawn {} parse tasks", parse_count);
        self.parse_tasks_completed
            .store(0, std::sync::atomic::Ordering::Release);
        self.parse_tasks_total
            .store(parse_count, std::sync::atomic::Ordering::Release);

        let index_start = std::time::Instant::now();
        let token = NumberOrString::String("kotlin-lsp/indexing".into());
        let progress = ProgressSummary {
            parse_count,
            indexed_count: discovered.indexed_count,
            total: discovered.total,
            cache_hits,
            truncated: discovered.truncated,
        };
        write_indexing_started_status(root, &progress);
        send_progress_begin(&client, &token, &progress).await;

        let (results, scheduling_aborted) = run_parse_phase(
            Arc::clone(&self),
            need_parse,
            &session,
            &client,
            &token,
            parse_count,
        )
        .await;
        aborted_early |= scheduling_aborted;
        if session.is_stale() {
            log::info!("index_workspace_impl: generation changed after parse, discarding results");
            aborted_early = true;
        }
        if aborted_early {
            return (aborted_scan_result(root), Some(guard));
        }

        let result = build_workspace_result(
            root,
            &discovered,
            cached_results,
            results,
            parse_count,
            cache_hits,
        );
        send_progress_end(&client, &token, &result).await;
        write_indexing_done_status(
            root,
            result.stats.files_parsed,
            result.stats.cache_hits,
            result.stats.symbols_extracted,
            index_start.elapsed().as_secs(),
        );

        (result, Some(guard))
    }

    /// Serialize the current index to `~/.cache/kotlin-lsp/<root-hash>/index.bin`.
    /// Safe to call from a background thread. Logs warnings on error; never panics.
    pub fn save_cache_to_disk(&self) {
        let root_guard = self.workspace_root.read().unwrap();
        let root = match root_guard.as_ref() {
            Some(r) => r,
            None => return,
        };
        let complete_scan = self
            .last_scan_complete
            .load(std::sync::atomic::Ordering::Acquire);
        save_cache(
            root,
            &self.files,
            &self.content_hashes,
            &self.library_uris,
            complete_scan,
        );
    }
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
