//! [`Actor`] — the single serialised writer of workspace state.
//!
//! All workspace-level mutations (root, source paths, ignore patterns, scans)
//! are processed here, one at a time, in arrival order. Request handlers that
//! only read index data continue to run concurrently via `Arc<Indexer>`.
//!
//! # Invariants
//!
//! * Only `Actor` event handlers may call `resolve_sources()` and write
//!   `Indexer::source_paths_raw` or `Indexer::ignore_matcher`.
//! * The `Indexer` is long-lived; it is never replaced, so live-document state
//!   accumulated in `live_lines`, `live_trees`, etc. survives reindex/root-switch.
//! * The actor's `phase` field is the authoritative lifecycle state. Before
//!   `Initialize` fires it is `State::Uninitialized`; after it is
//!   `State::Ready(ReadyState)`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::task::AbortHandle;
use tower_lsp::lsp_types::Url;
use tower_lsp::Client;

use crate::backend::helpers::syntax_diagnostics;
use crate::indexer::{Indexer, ProgressReporter};
use crate::rg::IgnoreMatcher;

use super::contract::{ReadyState, State};
use super::scan_queue::{ScanArgs, ScanKind, ScanQueue};
use super::{Config, Event};

// ─── Actor ──────────────────────────────────────────────────────────

/// MVI-style actor that owns all workspace write operations.
///
/// Generic over `R` (the progress reporter) so that LSP mode uses
/// [`LspProgressReporter`](crate::backend::LspProgressReporter) and CLI / tests
/// use [`NoopReporter`](crate::indexer::NoopReporter) — no heap allocation or
/// vtable dispatch needed at the actor level.
///
/// Construct with [`Actor::new`] and drive with [`Actor::run`].
pub(crate) struct Actor<R: ProgressReporter + 'static> {
    indexer: Arc<Indexer>,
    reporter: Arc<R>,
    rx: mpsc::Receiver<Event>,
    client: Option<Client>,
    pending_reindex: HashMap<String, AbortHandle>,
    /// Lifecycle phase — `Uninitialized` until the first `Initialize` event.
    /// Shared so that read-path consumers can observe workspace state without
    /// touching Indexer's internal lock fields directly.
    phase: Arc<RwLock<State>>,
    /// Coalescing queue for full-workspace scans (Initialize / Reindex / ChangeRoot).
    /// At most one scan runs at a time; newer requests replace pending ones.
    scan_queue: ScanQueue,
    /// Signals the actor's `run()` loop when a scan task finishes (or panics).
    scan_done_tx: mpsc::Sender<()>,
    scan_done_rx: mpsc::Receiver<()>,
    /// Single-slot pushback for the FileChanged drain loop.
    /// When a non-FileChanged event is encountered while draining, it is stored
    /// here so it is processed at the start of the next loop iteration.
    /// Invariant: at most one event is stored at any time.
    pushback: Option<Event>,
}

impl<R: ProgressReporter + 'static> Actor<R> {
    /// Create a new actor.
    ///
    /// `reporter` is used for every workspace scan triggered by this actor.
    /// For LSP mode, pass `Arc::new(LspProgressReporter(client.clone()))`.
    /// For CLI mode or tests, pass `Arc::new(NoopReporter)`.
    pub(crate) fn new(
        indexer: Arc<Indexer>,
        reporter: Arc<R>,
        rx: mpsc::Receiver<Event>,
        client: Option<Client>,
    ) -> Self {
        let (scan_done_tx, scan_done_rx) = mpsc::channel(8);
        Self {
            indexer,
            reporter,
            rx,
            client,
            pending_reindex: HashMap::new(),
            phase: Arc::new(RwLock::new(State::Uninitialized)),
            scan_queue: ScanQueue::new(),
            scan_done_tx,
            scan_done_rx,
            pushback: None,
        }
    }

    /// Expose the shared state handle for read-path consumers introduced in Wave 3.
    #[allow(dead_code)]
    pub(crate) fn state_handle(&self) -> Arc<RwLock<State>> {
        Arc::clone(&self.phase)
    }

    /// Run the event loop until the sender side is dropped.
    ///
    /// The exhaustive `match` is the architectural guarantee: every new
    /// [`Event`] variant must be handled here or the code will not
    /// compile.
    pub(crate) async fn run(mut self) {
        loop {
            // Drain the pushback buffer before waiting on channels.
            let event = if let Some(ev) = self.pushback.take() {
                ev
            } else {
                tokio::select! {
                    maybe_ev = self.rx.recv() => match maybe_ev {
                        None => break,
                        Some(ev) => ev,
                    },
                    Some(()) = self.scan_done_rx.recv() => {
                        self.scan_queue.completed();
                        if let Some(args) = self.scan_queue.try_start() {
                            self.execute_scan(args);
                        }
                        continue;
                    },
                }
            };

            match event {
                Event::Initialize {
                    config,
                    completion_tx,
                } => self.handle_initialize(config, completion_tx).await,
                Event::Reindex => self.handle_reindex().await,
                Event::ChangeRoot { root } => self.handle_change_root(root).await,
                Event::FileOpened {
                    uri,
                    language_id,
                    content,
                } => self.handle_file_opened(uri, language_id, content).await,
                Event::FileChanged { uri, changes } => {
                    // Coalesce: drain all immediately available FileChanged events.
                    // Last change per URI wins (HashMap deduplication).
                    let mut batch: HashMap<
                        String,
                        (Url, Vec<tower_lsp::lsp_types::TextDocumentContentChangeEvent>),
                    > = HashMap::new();
                    batch.insert(uri.to_string(), (uri, changes));
                    loop {
                        match self.rx.try_recv() {
                            Ok(Event::FileChanged { uri, changes }) => {
                                batch.insert(uri.to_string(), (uri, changes));
                            }
                            Ok(other) => {
                                // Non-FileChanged event — push back and stop draining.
                                self.pushback = Some(other);
                                break;
                            }
                            Err(_) => break, // channel empty or closed
                        }
                    }
                    for (_, (uri, changes)) in batch {
                        self.handle_file_changed(uri, changes).await;
                    }
                }
                Event::FileSaved { uri } => self.handle_file_saved(uri).await,
                Event::FileClosed { uri } => self.handle_file_closed(uri).await,
            }
        }
    }

    // ── Event handlers ────────────────────────────────────────────────────────

    async fn handle_initialize(
        &mut self,
        config: Config,
        completion_tx: Option<oneshot::Sender<()>>,
    ) {
        let data = ReadyState::from_config(&config);
        let root = data.root.clone();

        // Set the root immediately so read-path handlers can see it without
        // waiting for index_workspace_impl to run. The scan will overwrite
        // this with the same value once it acquires the indexing guard.
        self.set_root(root.clone());
        self.apply_ignore_patterns(&config.ignore_patterns, &root);

        // Always write source paths — even when empty — to clear any prior state.
        let source_paths = data.source_paths.clone();
        self.write_source_paths(source_paths.clone());
        self.set_state(data).await;

        let args = ScanArgs {
            root,
            kind: ScanKind::Prioritized {
                initial_paths: Vec::new(),
            },
            source_paths,
            completion_tx,
        };
        self.enqueue_and_start_scan(args);
    }

    async fn handle_reindex(&mut self) {
        let Some(root) = self.current_root() else {
            log::warn!("Actor: Reindex received but no workspace root is set");
            return;
        };
        self.indexer.reset_index_state();
        let source_paths = self
            .indexer
            .source_paths_raw
            .read()
            .ok()
            .map(|g| g.clone())
            .unwrap_or_default();
        let args = ScanArgs {
            root,
            kind: ScanKind::Full,
            source_paths,
            completion_tx: None,
        };
        self.enqueue_and_start_scan(args);
    }

    async fn handle_change_root(&mut self, root: PathBuf) {
        // Clear stale ignore patterns from the previous root, then re-resolve
        // source paths for the new root (workspace.json, build layout, etc.).
        // Explicit source paths from initialization are intentionally dropped
        // because they were relative to the old root and are editor-session-scoped.
        let config = Config {
            root: root.clone(),
            explicit_source_paths: Vec::new(),
            ignore_patterns: Vec::new(),
        };
        let data = ReadyState::from_config(&config);

        self.apply_ignore_patterns(&config.ignore_patterns, &root);
        self.write_source_paths(data.source_paths.clone());
        self.set_root(root.clone());
        self.set_state(data.clone()).await;

        self.indexer.reset_index_state();
        let args = ScanArgs {
            root,
            kind: ScanKind::Full,
            source_paths: data.source_paths,
            completion_tx: None,
        };
        self.enqueue_and_start_scan(args);
    }

    async fn handle_file_opened(&mut self, uri: Url, _language_id: String, content: String) {
        let opened_file_path = uri.to_file_path().ok();
        let workspace_pinned = self.indexer.workspace_pinned.load(Ordering::Relaxed);

        if let Some(workspace_root) =
            self.detect_workspace_root_switch(workspace_pinned, opened_file_path.as_deref())
        {
            self.switch_workspace_root_for_opened_document(
                workspace_root,
                opened_file_path.clone(),
            )
            .await;
        }

        if self.is_outside_pinned_workspace_root(workspace_pinned, opened_file_path.as_deref()) {
            log::info!(
                "Outside-root file — indexing content only: {}",
                opened_file_path
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            );
            self.store_live_document_state(&uri, &content).await;
            self.spawn_outside_root_document_indexing(uri, content);
            return;
        }

        self.store_live_document_state(&uri, &content).await;
        self.spawn_open_document_indexing(uri, content);
    }

    async fn handle_file_changed(
        &mut self,
        uri: Url,
        changes: Vec<tower_lsp::lsp_types::TextDocumentContentChangeEvent>,
    ) {
        let Some(change) = changes.into_iter().last() else {
            return;
        };
        let text = change.text;
        let indexer = Arc::clone(&self.indexer);

        self.indexer.set_live_lines(&uri, &text);
        {
            let indexer = Arc::clone(&self.indexer);
            let uri = uri.clone();
            let text = text.clone();
            // Fire-and-forget: live-tree parse runs on a blocking thread but we
            // do not await it in the actor loop to avoid blocking subsequent events.
            // The 300 ms debounce below provides ample time for the parse to finish
            // before index_content consumes the updated live tree.
            // Dropping the JoinHandle detaches from the task; the blocking thread
            // continues and cannot be cancelled.
            drop(tokio::task::spawn_blocking(move || {
                indexer.store_live_tree(&uri, &text);
            }));
        }

        let key = uri.to_string();
        if let Some(handle) = self.pending_reindex.remove(&key) {
            handle.abort();
        }

        let client = self.client.clone();
        let sem = indexer.parse_sem();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
            let Ok(permit) = sem.acquire_owned().await else {
                return;
            };
            let diagnostics_uri = uri.clone();
            let result = tokio::task::spawn_blocking(move || {
                let data = indexer.index_content(&uri, &text);
                drop(permit); // release semaphore after index_content completes
                data
            })
            .await;

            if let (Some(client), Ok(Some(data))) = (client, result) {
                let diagnostics = syntax_diagnostics(&data.syntax_errors);
                client
                    .publish_diagnostics(diagnostics_uri, diagnostics, None)
                    .await;
            }
        });
        self.pending_reindex.insert(key, handle.abort_handle());
    }

    async fn handle_file_saved(&mut self, uri: Url) {
        let indexer = Arc::clone(&self.indexer);
        let sem = indexer.parse_sem();
        tokio::task::spawn(async move {
            let Ok(path) = uri.to_file_path() else {
                return;
            };
            let Ok(content) = tokio::fs::read_to_string(&path).await else {
                return;
            };
            let Ok(permit) = sem.acquire_owned().await else {
                return;
            };
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                indexer.index_content(&uri, &content);
            })
            .await
            .ok();
        });
    }

    async fn handle_file_closed(&mut self, uri: Url) {
        let key = uri.to_string();
        if let Some(handle) = self.pending_reindex.remove(&key) {
            handle.abort();
        }

        self.indexer.remove_live_tree(&uri);
        self.indexer.remove_live_lines(&uri);
        if let Some(client) = &self.client {
            client.publish_diagnostics(uri, Vec::new(), None).await;
        }
    }

    // ── State management ──────────────────────────────────────────────────────

    /// Atomically transition to `State::Ready`.
    async fn set_state(&self, data: ReadyState) {
        self.phase.write().await.set_state(data);
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn current_root(&self) -> Option<PathBuf> {
        self.indexer
            .workspace_root
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn set_root(&self, root: PathBuf) {
        if let Ok(mut guard) = self.indexer.workspace_root.write() {
            *guard = Some(root);
        } else {
            log::warn!("Actor: failed to write workspace root");
        }
    }

    fn write_source_paths(&self, paths: Vec<String>) {
        match self.indexer.source_paths_raw.write() {
            Ok(mut guard) => *guard = paths,
            Err(err) => log::warn!("Actor: failed to write source_paths_raw: {err}"),
        }
    }

    fn apply_ignore_patterns(&self, patterns: &[String], root: &Path) {
        match self.indexer.ignore_matcher.write() {
            Ok(mut guard) => {
                // Always write — even when empty — to clear any stale matcher from
                // a previous Initialize or root switch.
                *guard = (!patterns.is_empty())
                    .then(|| Arc::new(IgnoreMatcher::new(patterns.to_vec(), root)));
            }
            Err(err) => log::warn!("Actor: failed to write ignore_matcher: {err}"),
        }
    }

    fn detect_workspace_root_switch(
        &self,
        workspace_pinned: bool,
        opened_file_path: Option<&Path>,
    ) -> Option<PathBuf> {
        if workspace_pinned {
            return None;
        }

        let opened_file_path = opened_file_path?;
        let candidate_workspace_root = Self::auto_detect_workspace_root(opened_file_path)?;
        self.should_switch_workspace_root(opened_file_path, &candidate_workspace_root)
            .then_some(candidate_workspace_root)
    }

    fn auto_detect_workspace_root(opened_file_path: &Path) -> Option<PathBuf> {
        let strong_markers = [
            "build.gradle",
            "settings.gradle",
            "build.gradle.kts",
            "Cargo.toml",
            "pom.xml",
            "settings.gradle.kts",
        ];
        let weak_markers = ["Package.swift"];
        let mut current_directory = opened_file_path.parent().map(Path::to_path_buf);
        let mut nearest_strong_marker_root: Option<PathBuf> = None;
        let mut git_root: Option<PathBuf> = None;
        let mut nearest_weak_marker_root: Option<PathBuf> = None;

        while let Some(directory) = current_directory {
            if nearest_strong_marker_root.is_none()
                && strong_markers
                    .iter()
                    .any(|marker| directory.join(marker).exists())
            {
                nearest_strong_marker_root = Some(directory.clone());
            }
            if directory.join(".git").exists() {
                git_root = Some(directory.clone());
                break;
            }
            if nearest_weak_marker_root.is_none()
                && weak_markers
                    .iter()
                    .any(|marker| directory.join(marker).exists())
            {
                nearest_weak_marker_root = Some(directory.clone());
            }
            current_directory = directory.parent().map(Path::to_path_buf);
        }

        nearest_strong_marker_root
            .or(git_root)
            .or(nearest_weak_marker_root)
            .or_else(|| opened_file_path.parent().map(Path::to_path_buf))
    }

    fn should_switch_workspace_root(
        &self,
        opened_file_path: &Path,
        candidate_workspace_root: &Path,
    ) -> bool {
        let candidate_workspace_root = Self::canonicalize_or_clone(candidate_workspace_root);
        match self.current_root() {
            None => true,
            Some(current_workspace_root) => {
                let current_workspace_root = Self::canonicalize_or_clone(&current_workspace_root);
                let opened_file_path = Self::canonicalize_or_clone(opened_file_path);
                !opened_file_path.starts_with(&current_workspace_root)
                    && candidate_workspace_root != current_workspace_root
            }
        }
    }

    fn canonicalize_or_clone(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    async fn switch_workspace_root_for_opened_document(
        &mut self,
        workspace_root: PathBuf,
        opened_file_path: Option<PathBuf>,
    ) {
        let config = Config {
            root: workspace_root.clone(),
            explicit_source_paths: Vec::new(),
            ignore_patterns: Vec::new(),
        };
        let data = ReadyState::from_config(&config);

        self.apply_ignore_patterns(&config.ignore_patterns, &workspace_root);
        let source_paths = data.source_paths.clone();
        self.write_source_paths(source_paths.clone());
        self.set_root(workspace_root.clone());
        self.set_state(data).await;
        self.indexer.workspace_pinned.store(true, Ordering::Relaxed);
        self.indexer.root_generation.fetch_add(1, Ordering::SeqCst);
        self.indexer.reset_index_state();
        log::info!(
            "Auto-detected workspace root (now pinned): {}",
            workspace_root.display()
        );
        let args = ScanArgs {
            root: workspace_root,
            kind: ScanKind::Prioritized {
                initial_paths: opened_file_path.into_iter().collect(),
            },
            source_paths,
            completion_tx: None,
        };
        self.enqueue_and_start_scan(args);
    }

    fn is_outside_pinned_workspace_root(
        &self,
        workspace_pinned: bool,
        opened_file_path: Option<&Path>,
    ) -> bool {
        if !workspace_pinned {
            return false;
        }

        match (opened_file_path, self.current_root()) {
            (Some(opened_file_path), Some(current_workspace_root)) => {
                let opened_file_path = Self::canonicalize_or_clone(opened_file_path);
                let current_workspace_root =
                    Self::canonicalize_or_clone(current_workspace_root.as_path());
                !opened_file_path.starts_with(&current_workspace_root)
            }
            _ => false,
        }
    }

    async fn store_live_document_state(&self, uri: &Url, content: &str) {
        self.indexer.set_live_lines(uri, content);

        let indexer = Arc::clone(&self.indexer);
        let uri = uri.clone();
        let content = content.to_owned();
        let _ = tokio::task::spawn_blocking(move || indexer.store_live_tree(&uri, &content)).await;
    }

    fn spawn_outside_root_document_indexing(&self, uri: Url, content: String) {
        let indexer = Arc::clone(&self.indexer);
        let semaphore = indexer.parse_sem();
        tokio::task::spawn(async move {
            if let Ok(permit) = semaphore.acquire_owned().await {
                let _ = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    indexer.index_content(&uri, &content);
                })
                .await;
            }
        });
    }

    fn spawn_open_document_indexing(&self, uri: Url, content: String) {
        let indexer = Arc::clone(&self.indexer);
        let semaphore = indexer.parse_sem();
        let cached_indexer = Arc::clone(&self.indexer);
        let client = self.client.clone();
        tokio::task::spawn(async move {
            let diagnostics_uri = uri.clone();
            let Ok(permit) = semaphore.acquire_owned().await else {
                return;
            };
            let result = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let data = indexer.index_content(&uri, &content);
                Arc::clone(&indexer).prewarm_completion_cache(&uri);
                data
            })
            .await;

            let diagnostics = match result {
                Ok(Some(indexed_file_data)) => syntax_diagnostics(&indexed_file_data.syntax_errors),
                Ok(None) => cached_indexer
                    .files
                    .get(diagnostics_uri.as_str())
                    .map(|file_data| syntax_diagnostics(&file_data.syntax_errors))
                    .unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            if let Some(client) = client {
                client
                    .publish_diagnostics(diagnostics_uri, diagnostics, None)
                    .await;
            }
        });
    }

    // ── Scan queue management ─────────────────────────────────────────────────

    /// Queue a scan request and start it immediately if no scan is in progress.
    ///
    /// If a scan is already running, bump `root_generation` to invalidate it
    /// (so stale results are discarded) and store the new request as pending.
    fn enqueue_and_start_scan(&mut self, args: ScanArgs) {
        if self.scan_queue.is_in_progress() {
            // Invalidate the in-flight scan so it discards its (now-stale) results.
            self.indexer.root_generation.fetch_add(1, Ordering::SeqCst);
        }
        self.scan_queue.request(args);
        if let Some(args) = self.scan_queue.try_start() {
            self.execute_scan(args);
        }
    }

    /// Spawn a workspace scan task.
    ///
    /// Sends to `scan_done_tx` when the task finishes — even on panic — so the
    /// actor's `run()` loop always clears `in_progress` and can start queued work.
    fn execute_scan(&self, args: ScanArgs) {
        let indexer = Arc::clone(&self.indexer);
        let reporter = Arc::clone(&self.reporter);
        let scan_done_tx = self.scan_done_tx.clone();

        let scan_handle = tokio::spawn(async move {
            // Claim this scan's source paths right before running.  By the time
            // this task executes, the actor may have processed later events that
            // overwrote source_paths_raw.  Writing here ensures finalize_workspace_scan
            // uses the paths that were configured when this scan was enqueued.
            if let Ok(mut guard) = indexer.source_paths_raw.write() {
                *guard = args.source_paths.clone();
            }
            match args.kind {
                ScanKind::Full => {
                    indexer.index_workspace_full(&args.root, reporter).await;
                }
                ScanKind::Prioritized { initial_paths } => {
                    indexer
                        .index_workspace_prioritized(&args.root, initial_paths, reporter)
                        .await;
                }
            }
            args.completion_tx
        });

        // Watcher task: forwards completion_tx signal and notifies actor.
        // Runs even if the scan task panics (JoinError path).
        tokio::spawn(async move {
            let completion_tx = scan_handle.await.ok().flatten();
            if let Some(tx) = completion_tx {
                let _ = tx.send(());
            }
            let _ = scan_done_tx.send(()).await;
        });
    }
}

#[cfg(test)]
#[path = "actor_tests.rs"]
mod tests;
