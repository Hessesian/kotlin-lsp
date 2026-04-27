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

use crate::indexer::{
    Indexer, MAX_FILES_UNLIMITED,
    cache::{try_load_cache, save_cache, cache_entry_to_file_result, write_status_file},
    discover::{find_source_files, warm_discover_files},
};
use crate::rg::{IgnoreMatcher, SOURCE_EXTENSIONS, regex_escape};
use crate::types::{FileIndexResult, WorkspaceIndexResult, IndexStats};

// ─── LSP progress notification ────────────────────────────────────────────────

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
pub(super) const DEFAULT_MAX_INDEX_FILES: usize = 2000;

/// Pure: resolve the maximum number of files to eagerly index.
///
/// Reads `KOTLIN_LSP_MAX_FILES` from the environment on each call.
/// Returns `default` when the variable is absent or not a valid integer.
///
/// - LSP mode callers pass `DEFAULT_MAX_INDEX_FILES` (2000).
/// - CLI `--index-only` callers pass `MAX_FILES_UNLIMITED`.
///   Note: even with `MAX_FILES_UNLIMITED`, setting `KOTLIN_LSP_MAX_FILES`
///   in the environment will still cap the count.
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
        None    => paths,
        Some(m) => m.filter_paths(paths),
    }
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
        let result = Arc::clone(&self).index_workspace_impl(root, max, client.clone()).await;
        if !result.aborted {
            self.last_scan_complete
                .store(result.complete_scan, std::sync::atomic::Ordering::Release);
            self.apply_workspace_result(&result);
            Arc::clone(&self)
                .index_source_paths(root.to_path_buf())
                .await;
            self.save_cache_to_disk();
        }
        Arc::clone(&self).run_pending_reindex(max, client).await;
    }

    /// Normal LSP startup: bounded workspace scan.
    /// Sends LSP `$/progress` notifications so the editor shows a status spinner.
    /// On subsequent startups the on-disk cache is used for unchanged files so only
    /// modified or new files need to be re-parsed by tree-sitter.
    pub async fn index_workspace(
        self: Arc<Self>,
        root: &Path,
        client: Option<tower_lsp::Client>,
    ) {
        // workspace_root is updated inside index_workspace_impl after the
        // concurrency guard is acquired, so we never set a stale root here.
        let max = resolve_max_files(DEFAULT_MAX_INDEX_FILES);
        let result = Arc::clone(&self).index_workspace_impl(root, max, client.clone()).await;
        if !result.aborted {
            self.last_scan_complete
                .store(result.complete_scan, std::sync::atomic::Ordering::Release);
            self.apply_workspace_result(&result);
            Arc::clone(&self)
                .index_source_paths(root.to_path_buf())
                .await;
            self.save_cache_to_disk();
        }
        Arc::clone(&self).run_pending_reindex(max, client).await;
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
        if !self.indexing_in_progress.load(std::sync::atomic::Ordering::Acquire)
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
                // Expand priority set to include supertypes so cross-class navigation
                // (super, override resolution) works before the full scan completes.
                let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
                let supers = crate::resolver::extract_supers_from_lines(&lines);
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
        let result = Arc::clone(&self).index_workspace_impl(root, max, client.clone()).await;
        if !result.aborted {
            self.last_scan_complete
                .store(result.complete_scan, std::sync::atomic::Ordering::Release);
            self.apply_workspace_result(&result);
            Arc::clone(&self)
                .index_source_paths(root.to_path_buf())
                .await;
            self.save_cache_to_disk();
        }
        Arc::clone(&self).run_pending_reindex(max, client).await;
    }

    /// If a reindex was queued while a scan was in progress, run it now.
    ///
    /// Called at the end of every public scan function, after the full workflow
    /// (impl + apply + source_paths + save_cache) completes. Mirrors RA's
    /// `OpQueue` pattern: at most one pending request is retained (last wins).
    async fn run_pending_reindex(
        self: Arc<Self>,
        max: usize,
        client: Option<tower_lsp::Client>,
    ) {
        if !self.pending_reindex.swap(false, std::sync::atomic::Ordering::AcqRel) {
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
        log::info!("run_pending_reindex: starting queued reindex for {}", root.display());
        let result = Arc::clone(&self).index_workspace_impl(&root, max, client).await;
        if !result.aborted {
            self.last_scan_complete
                .store(result.complete_scan, std::sync::atomic::Ordering::Release);
            self.apply_workspace_result(&result);
            Arc::clone(&self).index_source_paths(root).await;
            self.save_cache_to_disk();
        }
        // Intentionally no recursive pending check: if more requests arrived
        // during this run they will re-queue and be picked up by the next caller.
    }

    /// Core workspace indexing: file discovery → cache partition → concurrent parse.
    /// Returns a [`WorkspaceIndexResult`] without mutating the index; callers apply it.
    async fn index_workspace_impl(
        self: Arc<Self>,
        root: &Path,
        max: usize,
        client: Option<tower_lsp::Client>,
    ) -> WorkspaceIndexResult {
        let already = self
            .indexing_in_progress
            .swap(true, std::sync::atomic::Ordering::AcqRel);

        if already {
            // Queue this request so the active scan's caller will re-run once done.
            // Last caller wins (RA OpQueue semantics): overwrite any earlier pending root.
            *self.pending_reindex_root.write().unwrap() = Some(root.to_path_buf());
            self.pending_reindex.store(true, std::sync::atomic::Ordering::Release);
            // Bump root_generation so the running scan aborts early on root change.
            self.root_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            log::warn!(
                "index_workspace_impl: scan in progress; queued reindex for {} \
                 and interrupted current run via root_generation bump.",
                root.display()
            );
            return WorkspaceIndexResult {
                files: Vec::new(),
                stats: IndexStats::default(),
                workspace_root: root.to_path_buf(),
                aborted: true,
                complete_scan: false,
            };
        }

        // RAII guard: clear indexing_in_progress on exit (success, panic, or early return).
        // Created AFTER the `already` check so we never clear the flag owned by a concurrent run.
        let _guard = IndexingGuard {
            indexer: Arc::clone(&self),
        };

        *self.workspace_root.write().unwrap() = Some(root.to_path_buf());
        let start_gen = self
            .root_generation
            .load(std::sync::atomic::Ordering::SeqCst);

        let cache = try_load_cache(root);

        let matcher: Option<Arc<IgnoreMatcher>> = self.ignore_matcher.read().unwrap().clone();
        let matcher_ref: Option<&IgnoreMatcher> = matcher.as_deref();

        // Warm start: use cache manifest to skip the O(total_dirs) fd scan.
        // Only when the cache was built from a complete (non-truncated) scan.
        let warm_start = cache.as_ref().map(|c| c.complete_scan).unwrap_or(false);
        let mut paths = if warm_start {
            warm_discover_files(root, cache.as_ref().unwrap(), matcher_ref)
        } else {
            find_source_files(root, matcher_ref)
        };
        let total = paths.len();

        // On warm start all paths are cache hits (pure deserialization, no parse overhead)
        // so bypass the file-count cap entirely.
        let effective_max = if warm_start { MAX_FILES_UNLIMITED } else { max };

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
            log::info!("Indexing {} source files under {}", indexed_count, root.display());
        }

        // ── Partition: cache hits → FileIndexResult, misses → need_parse ────────
        let mut need_parse: Vec<PathBuf> = Vec::new();
        let mut cached_results: Vec<FileIndexResult> = Vec::new();
        let mut cache_hits: usize = 0;
        let mut aborted_early = false;

        for path in &paths {
            if self
                .root_generation
                .load(std::sync::atomic::Ordering::SeqCst)
                != start_gen
            {
                log::info!("index_workspace_impl: generation changed, aborting partition");
                aborted_early = true;
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

            if let Some(ref c) = cache {
                if let Some(entry) = c.entries.get(&path_str) {
                    // Cache hit: mtime AND file size must both match to guard against
                    // same-second edits (1s mtime resolution on some filesystems).
                    if entry.mtime_secs == mtime && entry.file_size == on_disk_size {
                        if let Ok(uri) = Url::from_file_path(path) {
                            cached_results.push(cache_entry_to_file_result(&uri, entry));
                        }
                        cache_hits += 1;
                        continue;
                    }
                }
            }
            need_parse.push(path.clone());
        }

        let parse_count = need_parse.len();
        log::info!("Cache: {cache_hits} hits, {parse_count} files need (re-)parsing");
        log::debug!("About to spawn {} parse tasks", parse_count);

        self.parse_tasks_completed
            .store(0, std::sync::atomic::Ordering::Release);
        self.parse_tasks_total
            .store(parse_count, std::sync::atomic::Ordering::Release);

        // ── Status file: indexing started ────────────────────────────────────
        let index_start = std::time::Instant::now();
        let started_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let root_escaped = serde_json::to_string(&root.to_string_lossy().as_ref()).unwrap_or_default();
        write_status_file(&format!(
            r#"{{"phase":"indexing","workspace":{root_escaped},"indexed":0,"total":{parse_count},"cache_hits":{cache_hits},"symbols":0,"started_at":{started_unix},"elapsed_secs":0,"estimated_total_secs":null}}"#
        ));

        // ── LSP progress: begin ──────────────────────────────────────────────
        let token = NumberOrString::String("kotlin-lsp/indexing".into());
        if let Some(ref client) = client {
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

        let begin_msg = if cache_hits > 0 {
            format!("Indexing {parse_count}/{indexed_count} files ({cache_hits} cached)…")
        } else if truncated {
            format!("Indexing {indexed_count}/{total} Kotlin files (shallowest first)…")
        } else {
            format!("Indexing {total} Kotlin files…")
        };
        if let Some(ref client) = client {
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

        self.scheduled_paths.clear();

        #[derive(Clone)]
        struct ParseWorkItem {
            path: PathBuf,
            key: String,
            start_gen: u64,
        }

        let mut work_items = Vec::new();
        for path in need_parse {
            if self
                .root_generation
                .load(std::sync::atomic::Ordering::SeqCst)
                != start_gen
            {
                log::info!(
                    "index_workspace_impl: generation changed during scheduling, aborting remaining parses"
                );
                aborted_early = true;
                break;
            }

            let key = std::fs::canonicalize(&path)
                .unwrap_or_else(|_| path.clone())
                .to_string_lossy()
                .to_string();

            match self.scheduled_paths.entry(key.clone()) {
                dashmap::mapref::entry::Entry::Occupied(mut o) => {
                    let existing_gen = *o.get();
                    if existing_gen == start_gen {
                        log::debug!(
                            "Skipped scheduling parse for {} (already scheduled gen {})",
                            key,
                            existing_gen
                        );
                        continue;
                    } else {
                        o.insert(start_gen);
                    }
                }
                dashmap::mapref::entry::Entry::Vacant(v) => {
                    v.insert(start_gen);
                }
            }

            work_items.push(ParseWorkItem {
                path,
                key,
                start_gen,
            });
        }

        // ── Concurrent parse via task_runner ─────────────────────────────────
        log::info!("Parsing {} files concurrently", work_items.len());

        let idx_ref = Arc::clone(&self);
        let sem = Arc::clone(&self.parse_sem);

        let progress_idx = Arc::clone(&self);
        let progress_client = client.clone();
        let progress_token = token.clone();
        let progress_total = parse_count;
        let progress_handle = tokio::spawn(async move {
            if progress_client.is_none() || progress_total == 0 {
                return;
            }
            let mut interval =
                tokio::time::interval(std::time::Duration::from_millis(500));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let done = progress_idx
                    .parse_tasks_completed
                    .load(std::sync::atomic::Ordering::Relaxed);
                let pct = ((done * 100) / progress_total) as u32;
                if let Some(ref c) = progress_client {
                    c.send_notification::<progress::KotlinProgress>(ProgressParams {
                        token: progress_token.clone(),
                        value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(
                            WorkDoneProgressReport {
                                cancellable: Some(false),
                                message: Some(format!("{done}/{progress_total} files…")),
                                percentage: Some(pct),
                            },
                        )),
                    })
                    .await;
                }
                if done >= progress_total {
                    break;
                }
            }
        });

        let gen_skipped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let read_failed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let url_failed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let panic_failed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gen_skipped2 = Arc::clone(&gen_skipped);
        let read_failed2 = Arc::clone(&read_failed);
        let url_failed2 = Arc::clone(&url_failed);
        let panic_failed2 = Arc::clone(&panic_failed);

        let results = crate::task_runner::run_concurrent(
            work_items,
            sem,
            move |item, sem| {
                let idx = Arc::clone(&idx_ref);
                let gen_skipped = Arc::clone(&gen_skipped2);
                let read_failed = Arc::clone(&read_failed2);
                let url_failed = Arc::clone(&url_failed2);
                let panic_failed = Arc::clone(&panic_failed2);
                async move {
                    log::debug!("Parsing: {}", item.path.display());

                    if idx
                        .root_generation
                        .load(std::sync::atomic::Ordering::SeqCst)
                        != item.start_gen
                    {
                        gen_skipped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return None;
                    }

                    let content = match tokio::fs::read_to_string(&item.path).await {
                        Ok(c) => c,
                        Err(e) => {
                            let n =
                                read_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if n < 5 {
                                log::warn!(
                                    "Could not read {}: {}",
                                    item.path.display(),
                                    e
                                );
                            }
                            return None;
                        }
                    };

                    let uri = match Url::from_file_path(&item.path) {
                        Ok(u) => u,
                        Err(_) => {
                            url_failed
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            log::warn!("Invalid file path: {}", item.path.display());
                            return None;
                        }
                    };

                    let _permit = sem.acquire().await.unwrap();
                    let t0 = std::time::Instant::now();
                    let uri_clone = uri.clone();
                    let parse_result =
                        match tokio::task::spawn_blocking(move || {
                            Indexer::parse_file(&uri_clone, &content)
                        })
                        .await
                        {
                            Ok(result) => result,
                            Err(e) => {
                                panic_failed
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                log::warn!(
                                    "Parse task panicked for {}: {}",
                                    item.path.display(),
                                    e
                                );
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

                    let threshold: u128 =
                        std::env::var("KOTLIN_LSP_PARSE_LOG_MS")
                            .ok()
                            .and_then(|v| v.parse::<u128>().ok())
                            .unwrap_or(1000);
                    if took as u128 > threshold {
                        log::warn!(
                            "Slow parse: {} took {} ms",
                            item.path.display(),
                            took
                        );
                    }

                    idx.parse_tasks_completed
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                    Some(parse_result)
                }
            },
        )
        .await;

        progress_handle.abort();

        let gen_skip_n = gen_skipped.load(std::sync::atomic::Ordering::Relaxed);
        let read_fail_n = read_failed.load(std::sync::atomic::Ordering::Relaxed);
        let url_fail_n = url_failed.load(std::sync::atomic::Ordering::Relaxed);
        let panic_n = panic_failed.load(std::sync::atomic::Ordering::Relaxed);
        log::info!(
            "All {} parse tasks done: gen_skipped={}, read_failed={}, url_failed={}, panics={}",
            results.len(),
            gen_skip_n,
            read_fail_n,
            url_fail_n,
            panic_n
        );

        if self
            .root_generation
            .load(std::sync::atomic::Ordering::SeqCst)
            != start_gen
        {
            log::info!(
                "index_workspace_impl: generation changed after parse, discarding results"
            );
            aborted_early = true;
        }

        if aborted_early {
            return WorkspaceIndexResult {
                files: Vec::new(),
                stats: IndexStats::default(),
                workspace_root: root.to_path_buf(),
                aborted: true,
                complete_scan: false,
            };
        }

        let mut parsed_results: Vec<FileIndexResult> =
            results.into_iter().flatten().collect();
        let files_parsed = parsed_results.len();
        let parse_errors = parse_count - files_parsed;

        let mut all_results = cached_results;
        all_results.append(&mut parsed_results);

        let stats = IndexStats {
            files_discovered: total,
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

        // ── LSP progress: end ────────────────────────────────────────────────
        if let Some(ref client) = client {
            client
                .send_notification::<progress::KotlinProgress>(ProgressParams {
                    token: token.clone(),
                    value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                        WorkDoneProgressEnd {
                            message: Some(format!(
                                "Indexed {} files ({} cached, {} parsed)",
                                all_results.len(),
                                cache_hits,
                                files_parsed
                            )),
                        },
                    )),
                })
                .await;
        }

        // ── Status file: done ────────────────────────────────────────────────
        let elapsed = index_start.elapsed().as_secs();
        let root_escaped = serde_json::to_string(&root.to_string_lossy().as_ref()).unwrap_or_default();
        write_status_file(&format!(
            r#"{{"phase":"done","workspace":{root_escaped},"indexed":{files_parsed},"total":{actually_indexed},"cache_hits":{cache_hits},"symbols":{symbols},"elapsed_secs":{elapsed},"estimated_total_secs":null}}"#,
            actually_indexed = files_parsed + cache_hits,
            symbols = stats.symbols_extracted,
        ));

        WorkspaceIndexResult {
            files: all_results,
            stats,
            workspace_root: root.to_path_buf(),
            aborted: false,
            complete_scan: !truncated,
        }
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
