//! [`WorkspaceActor`] — the single serialised writer of workspace state.
//!
//! All workspace-level mutations (root, source paths, ignore patterns, scans)
//! are processed here, one at a time, in arrival order. Request handlers that
//! only read index data continue to run concurrently via `Arc<Indexer>`.
//!
//! # Invariants
//!
//! * Only `WorkspaceActor` event handlers may call `resolve_sources()` and write
//!   `Indexer::source_paths_raw` or `Indexer::ignore_matcher`.
//! * The `Indexer` is long-lived; it is never replaced, so live-document state
//!   accumulated in `live_lines`, `live_trees`, etc. survives reindex/root-switch.
// Items unused until Wave 2 wires this into backend/CLI (ws-backend, ws-cli, ws-main).
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tower_lsp::lsp_types::Url;
use tower_lsp::Client;

use crate::backend::helpers::syntax_diagnostics;
use crate::indexer::{Indexer, ProgressReporter};
use crate::rg::IgnoreMatcher;

use super::{WorkspaceConfig, WorkspaceEvent};

// ─── WorkspaceActor ──────────────────────────────────────────────────────────

/// MVI-style actor that owns all workspace write operations.
///
/// Generic over `R` (the progress reporter) so that LSP mode uses
/// [`LspProgressReporter`](crate::indexer::ProgressReporter) and CLI / tests
/// use [`NoopReporter`](crate::indexer::NoopReporter) — no heap allocation or
/// vtable dispatch needed at the actor level.
///
/// Construct with [`WorkspaceActor::new`] and drive with [`WorkspaceActor::run`].
pub(crate) struct WorkspaceActor<R: ProgressReporter + 'static> {
    indexer: Arc<Indexer>,
    reporter: Arc<R>,
    rx: mpsc::Receiver<WorkspaceEvent>,
    client: Option<Client>,
    pending_reindex: HashMap<String, AbortHandle>,
}

impl<R: ProgressReporter + 'static> WorkspaceActor<R> {
    /// Create a new actor.
    ///
    /// `reporter` is used for every workspace scan triggered by this actor.
    /// For LSP mode, pass `Arc::new(LspProgressReporter(client.clone()))`.
    /// For CLI mode or tests, pass `Arc::new(NoopReporter)`.
    pub(crate) fn new(
        indexer: Arc<Indexer>,
        reporter: Arc<R>,
        rx: mpsc::Receiver<WorkspaceEvent>,
        client: Option<Client>,
    ) -> Self {
        Self {
            indexer,
            reporter,
            rx,
            client,
            pending_reindex: HashMap::new(),
        }
    }

    /// Run the event loop until the sender side is dropped.
    ///
    /// The exhaustive `match` is the architectural guarantee: every new
    /// [`WorkspaceEvent`] variant must be handled here or the code will not
    /// compile.
    pub(crate) async fn run(mut self) {
        while let Some(event) = self.rx.recv().await {
            match event {
                WorkspaceEvent::Initialize {
                    config,
                    completion_tx,
                } => self.handle_initialize(config, completion_tx).await,
                WorkspaceEvent::Reindex => self.handle_reindex().await,
                WorkspaceEvent::ChangeRoot { root } => self.handle_change_root(root).await,
                WorkspaceEvent::FileOpened {
                    uri,
                    language_id,
                    content,
                } => self.handle_file_opened(uri, language_id, content).await,
                WorkspaceEvent::FileChanged { uri, changes } => {
                    self.handle_file_changed(uri, changes).await;
                }
                WorkspaceEvent::FileSaved { uri } => self.handle_file_saved(uri).await,
                WorkspaceEvent::FileClosed { uri } => self.handle_file_closed(uri).await,
            }
        }
    }

    // ── Event handlers ────────────────────────────────────────────────────────

    async fn handle_initialize(
        &mut self,
        config: WorkspaceConfig,
        completion_tx: Option<oneshot::Sender<()>>,
    ) {
        let root = config.root.clone();

        // Set the root immediately so read-path handlers can see it without
        // waiting for index_workspace_impl to run. The scan will overwrite
        // this with the same value once it acquires the indexing guard.
        self.set_root(root.clone());
        self.apply_ignore_patterns(&config.ignore_patterns, &root);

        // Always write source paths — even when empty — to clear any prior state.
        self.write_source_paths(config.resolve_sources());

        self.spawn_scan(root, Vec::new(), completion_tx).await;
    }

    async fn handle_reindex(&mut self) {
        let Some(root) = self.current_root() else {
            log::warn!("WorkspaceActor: Reindex received but no workspace root is set");
            return;
        };
        self.indexer.reset_index_state();
        self.spawn_full_scan(root).await;
    }

    async fn handle_change_root(&mut self, root: PathBuf) {
        self.set_root(root.clone());

        // Clear stale ignore patterns from the previous root, then re-resolve
        // source paths for the new root (workspace.json, build layout, etc.).
        // Explicit source paths from initialization are intentionally dropped
        // because they were relative to the old root and are editor-session-scoped.
        let config = WorkspaceConfig {
            root: root.clone(),
            explicit_source_paths: Vec::new(),
            ignore_patterns: Vec::new(),
        };
        self.apply_ignore_patterns(&config.ignore_patterns, &root);
        self.write_source_paths(config.resolve_sources());

        self.indexer.reset_index_state();
        self.spawn_full_scan(root).await;
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
            log::warn!("WorkspaceActor: failed to write workspace root");
        }
    }

    fn write_source_paths(&self, paths: Vec<String>) {
        match self.indexer.source_paths_raw.write() {
            Ok(mut guard) => *guard = paths,
            Err(err) => log::warn!("WorkspaceActor: failed to write source_paths_raw: {err}"),
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
            Err(err) => log::warn!("WorkspaceActor: failed to write ignore_matcher: {err}"),
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
        self.set_root(workspace_root.clone());
        self.indexer.workspace_pinned.store(true, Ordering::Relaxed);
        self.indexer.root_generation.fetch_add(1, Ordering::SeqCst);
        self.indexer.reset_index_state();
        log::info!(
            "Auto-detected workspace root (now pinned): {}",
            workspace_root.display()
        );
        self.spawn_scan(workspace_root, opened_file_path.into_iter().collect(), None)
            .await;
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

    /// Spawn a prioritized bounded scan in the background.
    /// `initial_paths` are indexed first (empty = no prioritization).
    async fn spawn_scan(
        &self,
        root: PathBuf,
        initial_paths: Vec<PathBuf>,
        completion_tx: Option<oneshot::Sender<()>>,
    ) {
        let indexer = Arc::clone(&self.indexer);
        let reporter = Arc::clone(&self.reporter);
        tokio::spawn(async move {
            indexer
                .index_workspace_prioritized(&root, initial_paths, reporter)
                .await;
            if let Some(completion_tx) = completion_tx {
                let _ = completion_tx.send(());
            }
        });
    }

    /// Spawn an unbounded full scan (used by Reindex and ChangeRoot).
    async fn spawn_full_scan(&self, root: PathBuf) {
        let indexer = Arc::clone(&self.indexer);
        let reporter = Arc::clone(&self.reporter);
        tokio::spawn(async move {
            indexer.index_workspace_full(&root, reporter).await;
        });
    }
}

#[cfg(test)]
#[path = "actor_tests.rs"]
mod tests;
