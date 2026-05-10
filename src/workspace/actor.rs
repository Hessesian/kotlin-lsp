//! [`WorkspaceActor`] — the single serialised writer of workspace state.
//!
//! All workspace-level mutations (root, source paths, ignore patterns, scans)
//! are processed here, one at a time, in arrival order.  Request handlers that
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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;

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
    ) -> Self {
        Self {
            indexer,
            reporter,
            rx,
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
                WorkspaceEvent::Initialize { config } => self.handle_initialize(config).await,
                WorkspaceEvent::Reindex => self.handle_reindex().await,
                WorkspaceEvent::ChangeRoot { root } => self.handle_change_root(root).await,
            }
        }
    }

    // ── Event handlers ────────────────────────────────────────────────────────

    async fn handle_initialize(&self, config: WorkspaceConfig) {
        let root = config.root.clone();

        // Set the root immediately so read-path handlers can see it without
        // waiting for index_workspace_impl to run.  The scan will overwrite
        // this with the same value once it acquires the indexing guard.
        self.set_root(root.clone());
        self.apply_ignore_patterns(&config.ignore_patterns, &root);

        // Always write source paths — even when empty — to clear any prior state.
        self.write_source_paths(config.resolve_sources());

        self.spawn_scan(root, Vec::new()).await;
    }

    async fn handle_reindex(&self) {
        let Some(root) = self.current_root() else {
            log::warn!("WorkspaceActor: Reindex received but no workspace root is set");
            return;
        };
        self.indexer.reset_index_state();
        self.spawn_full_scan(root).await;
    }

    async fn handle_change_root(&self, root: PathBuf) {
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

    /// Spawn a prioritized bounded scan in the background.
    /// `initial_paths` are indexed first (empty = no prioritization).
    async fn spawn_scan(&self, root: PathBuf, initial_paths: Vec<PathBuf>) {
        let indexer = Arc::clone(&self.indexer);
        let reporter = Arc::clone(&self.reporter);
        tokio::spawn(async move {
            indexer
                .index_workspace_prioritized(&root, initial_paths, reporter)
                .await;
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
