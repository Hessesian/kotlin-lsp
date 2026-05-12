use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, RwLock};

use crate::indexer::{Indexer, ProgressReporter};
use crate::rg::IgnoreMatcher;

use super::phase::{ReadyState, State};
use super::Config;

pub(crate) struct ScanHandler<R: ProgressReporter + 'static> {
    indexer: Arc<Indexer>,
    reporter: Arc<R>,
    state: Arc<RwLock<State>>,
    is_scanning: Arc<AtomicBool>,
    scan_done_tx: mpsc::UnboundedSender<()>,
}

impl<R: ProgressReporter + 'static> ScanHandler<R> {
    pub(crate) fn new(
        indexer: Arc<Indexer>,
        reporter: Arc<R>,
        state: Arc<RwLock<State>>,
        scan_done_tx: mpsc::UnboundedSender<()>,
    ) -> Self {
        Self {
            indexer,
            reporter,
            state,
            is_scanning: Arc::new(AtomicBool::new(false)),
            scan_done_tx,
        }
    }

    /// Returns `true` while a background index scan is in flight.
    pub(crate) fn is_scanning(&self) -> bool {
        self.is_scanning.load(Ordering::Acquire)
    }

    pub(crate) fn state_stream(&self) -> Arc<RwLock<State>> {
        Arc::clone(&self.state)
    }

    pub(crate) async fn handle_initialize(
        &self,
        config: Config,
        completion_tx: Option<oneshot::Sender<()>>,
    ) {
        let data = ReadyState::from_config(&config);
        let root = data.root.clone();

        self.set_root(root.clone());
        self.apply_ignore_patterns(&config.ignore_patterns, &root);
        self.indexer
            .workspace_pinned
            .store(config.pin_workspace, std::sync::atomic::Ordering::Relaxed);
        self.write_source_paths(data.source_paths.clone());
        self.set_phase(data).await;

        self.spawn_scan(root, Vec::new(), completion_tx).await;
    }

    pub(crate) async fn handle_reindex(&self) {
        let Some(root) = self.current_root() else {
            log::warn!("Actor: Reindex received but no workspace root is set");
            return;
        };
        self.indexer.reset_index_state();
        self.spawn_full_scan(root).await;
    }

    pub(crate) async fn handle_change_root(&self, root: PathBuf) {
        let config = Config {
            root: root.clone(),
            explicit_source_paths: Vec::new(),
            ignore_patterns: Vec::new(),
            pin_workspace: true,
        };
        let data = ReadyState::from_config(&config);

        self.apply_ignore_patterns(&config.ignore_patterns, &root);
        self.write_source_paths(data.source_paths.clone());
        self.set_root(root.clone());
        self.indexer
            .workspace_pinned
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.set_phase(data).await;

        self.indexer.reset_index_state();
        self.spawn_full_scan(root).await;
    }

    pub(crate) async fn switch_workspace_root_for_opened_document(
        &self,
        workspace_root: PathBuf,
        opened_file_path: Option<PathBuf>,
    ) {
        let config = Config {
            root: workspace_root.clone(),
            explicit_source_paths: Vec::new(),
            ignore_patterns: Vec::new(),
            pin_workspace: true,
        };
        let data = ReadyState::from_config(&config);

        self.apply_ignore_patterns(&config.ignore_patterns, &workspace_root);
        self.write_source_paths(data.source_paths.clone());
        self.set_root(workspace_root.clone());
        self.set_phase(data).await;
        self.indexer
            .workspace_pinned
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.indexer.reset_index_state();
        log::info!(
            "Auto-detected workspace root (now pinned): {}",
            workspace_root.display()
        );
        self.spawn_scan(workspace_root, opened_file_path.into_iter().collect(), None)
            .await;
    }

    pub(crate) async fn set_phase(&self, data: ReadyState) {
        self.state.write().await.set_state(data);
    }

    pub(crate) fn current_root(&self) -> Option<PathBuf> {
        self.indexer.workspace_root.get()
    }

    fn set_root(&self, root: PathBuf) {
        self.indexer.workspace_root.set(root);
    }

    fn write_source_paths(&self, paths: Vec<String>) {
        match self.indexer.source_paths_raw.write() {
            Ok(mut guard) => *guard = paths,
            Err(error) => log::warn!("Actor: failed to write source_paths_raw: {error}"),
        }
    }

    fn apply_ignore_patterns(&self, patterns: &[String], root: &Path) {
        match self.indexer.ignore_matcher.write() {
            Ok(mut guard) => {
                *guard = (!patterns.is_empty())
                    .then(|| Arc::new(IgnoreMatcher::new(patterns.to_vec(), root)));
            }
            Err(error) => log::warn!("Actor: failed to write ignore_matcher: {error}"),
        }
    }

    async fn spawn_scan(
        &self,
        root: PathBuf,
        initial_paths: Vec<PathBuf>,
        completion_tx: Option<oneshot::Sender<()>>,
    ) {
        let indexer = Arc::clone(&self.indexer);
        let reporter = Arc::clone(&self.reporter);
        let is_scanning = Arc::clone(&self.is_scanning);
        let scan_done_tx = self.scan_done_tx.clone();
        is_scanning.store(true, Ordering::Release);
        tokio::spawn(async move {
            indexer
                .index_workspace_prioritized(&root, initial_paths, reporter)
                .await;
            is_scanning.store(false, Ordering::Release);
            let _ = scan_done_tx.send(());
            if let Some(completion_tx) = completion_tx {
                let _ = completion_tx.send(());
            }
        });
    }

    async fn spawn_full_scan(&self, root: PathBuf) {
        let indexer = Arc::clone(&self.indexer);
        let reporter = Arc::clone(&self.reporter);
        let is_scanning = Arc::clone(&self.is_scanning);
        let scan_done_tx = self.scan_done_tx.clone();
        is_scanning.store(true, Ordering::Release);
        tokio::spawn(async move {
            indexer.index_workspace_full(&root, reporter).await;
            is_scanning.store(false, Ordering::Release);
            let _ = scan_done_tx.send(());
        });
    }
}

#[cfg(test)]
#[path = "scan_handler_tests.rs"]
mod tests;
