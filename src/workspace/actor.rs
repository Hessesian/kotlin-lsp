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

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, RwLock};
use tower_lsp::lsp_types::Url;
use tower_lsp::Client;

use crate::indexer::{Indexer, ProgressReporter};

use super::document_handler::DocumentHandler;
use super::file_change_handler::FileChangeHandler;
use super::phase::State;
use super::scan_handler::ScanHandler;
use super::{Config, Event};

/// MVI-style actor that owns all workspace write operations.
///
/// Generic over `R` (the progress reporter) so that LSP mode uses
/// [`LspProgressReporter`](crate::indexer::ProgressReporter) and CLI / tests
/// use [`NoopReporter`](crate::indexer::NoopReporter) — no heap allocation or
/// vtable dispatch needed at the actor level.
///
/// Construct with [`Actor::new`] and drive with [`Actor::run`].
pub(crate) struct Actor<R: ProgressReporter + 'static> {
    rx: mpsc::Receiver<Event>,
    scan_done_rx: mpsc::UnboundedReceiver<()>,
    scan_handler: ScanHandler<R>,
    file_change_handler: FileChangeHandler,
    document_handler: DocumentHandler,
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
        let state = Arc::new(RwLock::new(State::Uninitialized));
        let (scan_done_tx, scan_done_rx) = mpsc::unbounded_channel();
        Self {
            rx,
            scan_done_rx,
            scan_handler: ScanHandler::new(
                Arc::clone(&indexer),
                reporter,
                Arc::clone(&state),
                scan_done_tx,
            ),
            file_change_handler: FileChangeHandler::new(Arc::clone(&indexer), client.clone()),
            document_handler: DocumentHandler::new(indexer, client),
        }
    }

    /// Expose the shared phase handle for read-path consumers introduced in Wave 3.
    #[allow(dead_code)]
    pub(crate) fn state_stream(&self) -> Arc<RwLock<State>> {
        self.scan_handler.state_stream()
    }

    /// Run the event loop until the sender side is dropped.
    ///
    /// The exhaustive `match` is the architectural guarantee: every new
    /// [`Event`] variant must be handled here or the code will not compile.
    ///
    /// After each event or scan completion, checks whether the workspace has
    /// transitioned into the quiescent "ready" state and fires [`on_became_ready`]
    /// exactly once per such transition.
    pub(crate) async fn run(mut self) {
        let mut was_ready = false;
        loop {
            tokio::select! {
                biased;
                maybe_event = self.rx.recv() => {
                    let Some(event) = maybe_event else { break };
                    self.handle_event(event).await;
                }
                Some(()) = self.scan_done_rx.recv() => {
                    // scan_done_rx fires when a background scan finishes;
                    // is_scanning was already cleared by the scan task before sending.
                }
            }
            let is_ready = self.is_ready().await;
            if !was_ready && is_ready {
                self.on_became_ready().await;
            }
            was_ready = is_ready;
        }
    }

    /// Returns `true` when the workspace has been initialised **and** no
    /// background scan is currently in flight.
    async fn is_ready(&self) -> bool {
        self.scan_handler
            .state_stream()
            .read()
            .await
            .ready()
            .is_some()
            && !self.scan_handler.is_scanning()
    }

    /// Called exactly once each time the workspace transitions from a
    /// non-quiescent state into the quiescent ready state.
    async fn on_became_ready(&self) {
        if let Some(state) = self.scan_handler.state_stream().read().await.ready() {
            log::info!(
                "Workspace ready: {} ({} source path(s))",
                state.root.display(),
                state.source_paths.len(),
            );
        }
    }

    async fn handle_event(&mut self, event: Event) {
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
                self.handle_file_changed(uri, changes).await;
            }
            Event::FileSaved { uri } => self.handle_file_saved(uri).await,
            Event::FileClosed { uri } => self.handle_file_closed(uri).await,
            Event::FileDeleted { uri } => self.handle_file_deleted(uri).await,
        }
    }

    async fn handle_initialize(&self, config: Config, completion_tx: Option<oneshot::Sender<()>>) {
        self.scan_handler
            .handle_initialize(config, completion_tx)
            .await;
    }

    async fn handle_reindex(&self) {
        self.scan_handler.handle_reindex().await;
    }

    async fn handle_change_root(&self, root: std::path::PathBuf) {
        self.scan_handler.handle_change_root(root).await;
    }

    async fn handle_file_opened(&mut self, uri: Url, language_id: String, content: String) {
        let document_handler = &self.document_handler;
        let scan_handler = &self.scan_handler;
        document_handler
            .handle_file_opened(scan_handler, uri, language_id, content)
            .await;
    }

    async fn handle_file_changed(
        &mut self,
        uri: Url,
        changes: Vec<tower_lsp::lsp_types::TextDocumentContentChangeEvent>,
    ) {
        self.file_change_handler
            .handle_file_changed(uri, changes)
            .await;
    }

    async fn handle_file_saved(&self, uri: Url) {
        self.document_handler.handle_file_saved(uri).await;
    }

    async fn handle_file_closed(&mut self, uri: Url) {
        let document_handler = &self.document_handler;
        let file_change_handler = &mut self.file_change_handler;
        document_handler
            .handle_file_closed(file_change_handler, uri)
            .await;
    }

    async fn handle_file_deleted(&self, uri: Url) {
        self.document_handler.handle_file_deleted(uri).await;
    }
}

#[cfg(test)]
#[path = "actor_tests.rs"]
mod tests;
