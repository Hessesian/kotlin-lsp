use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{oneshot, RwLock};

use crate::indexer::{Indexer, ProgressReporter};
use crate::rg::IgnoreMatcher;

use super::phase::{ReadyState, State};
use super::Config;

pub(crate) struct ScanHandler<R: ProgressReporter + 'static> {
    indexer: Arc<Indexer>,
    reporter: Arc<R>,
    state: Arc<RwLock<State>>,
}

impl<R: ProgressReporter + 'static> ScanHandler<R> {
    pub(crate) fn new(indexer: Arc<Indexer>, reporter: Arc<R>, state: Arc<RwLock<State>>) -> Self {
        Self {
            indexer,
            reporter,
            state,
        }
    }

    pub(crate) fn state_stream(&self) -> Arc<RwLock<State>> {
        Arc::clone(&self.state)
    }

    pub(crate) async fn handle_initialize(
        &self,
        config: Config,
        completion_tx: Option<oneshot::Sender<()>>,
    ) {
        let root = config.root.clone();
        self.apply_root_switch_config(&config).await;
        self.enqueue_and_start_scan(root, Vec::new(), false, completion_tx);
    }

    pub(crate) async fn handle_reindex(&self) {
        let Some(root) = self.current_root() else {
            log::warn!("Actor: Reindex received but no workspace root is set");
            return;
        };

        self.indexer.reset_index_state();
        self.enqueue_and_start_scan(root, Vec::new(), true, None);
    }

    pub(crate) async fn handle_change_root(&self, root: PathBuf) {
        let config = Config {
            root: root.clone(),
            explicit_source_paths: Vec::new(),
            ignore_patterns: Vec::new(),
            pin_workspace: true,
        };

        self.apply_root_switch_config(&config).await;
        self.indexer.reset_index_state();
        self.enqueue_and_start_scan(root, Vec::new(), true, None);
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

        self.apply_root_switch_config(&config).await;
        self.indexer
            .workspace_pinned
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.indexer
            .root_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.indexer.reset_index_state();
        log::info!(
            "Auto-detected workspace root (now pinned): {}",
            workspace_root.display()
        );
        self.enqueue_and_start_scan(
            workspace_root,
            opened_file_path.into_iter().collect(),
            false,
            None,
        );
    }

    fn enqueue_and_start_scan(
        &self,
        root: PathBuf,
        initial_paths: Vec<PathBuf>,
        full_scan: bool,
        completion_tx: Option<oneshot::Sender<()>>,
    ) {
        self.execute_scan(root, initial_paths, full_scan, completion_tx);
    }

    fn execute_scan(
        &self,
        root: PathBuf,
        initial_paths: Vec<PathBuf>,
        full_scan: bool,
        completion_tx: Option<oneshot::Sender<()>>,
    ) {
        let indexer = Arc::clone(&self.indexer);
        let reporter = Arc::clone(&self.reporter);
        tokio::spawn(async move {
            if full_scan {
                indexer.index_workspace_full(&root, reporter).await;
            } else {
                indexer
                    .index_workspace_prioritized(&root, initial_paths, reporter)
                    .await;
            }
            Self::on_scan_completed(completion_tx);
        });
    }

    fn on_scan_completed(completion_tx: Option<oneshot::Sender<()>>) {
        if let Some(completion_tx) = completion_tx {
            let _ = completion_tx.send(());
        }
    }

    async fn apply_root_switch_config(&self, config: &Config) {
        let data = ReadyState::from_config(config);
        let root = data.root.clone();

        self.set_root(root.clone());
        self.indexer
            .workspace_pinned
            .store(config.pin_workspace, std::sync::atomic::Ordering::Relaxed);
        self.apply_ignore_patterns(&config.ignore_patterns, &root);
        self.set_state(data).await;
        self.write_source_paths(self.current_source_paths().await);
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

    fn set_root(&self, root: PathBuf) {
        if let Ok(mut guard) = self.indexer.workspace_root.write() {
            *guard = Some(root);
        } else {
            log::warn!("Actor: failed to write workspace root");
        }
    }

    async fn set_state(&self, data: ReadyState) {
        self.state.write().await.set_state(data);
    }

    async fn current_source_paths(&self) -> Vec<String> {
        self.state
            .read()
            .await
            .ready()
            .map(|ready| ready.source_paths.clone())
            .unwrap_or_default()
    }

    fn current_root(&self) -> Option<PathBuf> {
        self.indexer
            .workspace_root
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }
}

#[cfg(test)]
#[path = "scan_handler_tests.rs"]
mod tests;
