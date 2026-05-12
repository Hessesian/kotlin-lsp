use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot, RwLock};

use crate::indexer::{Indexer, ProgressReporter};
use crate::rg::IgnoreMatcher;

use super::phase::{ReadyState, State};
use super::scan_queue::{ScanArgs, ScanKind, ScanQueue};
use super::Config;

pub(crate) struct ScanHandler<R: ProgressReporter + 'static> {
    indexer: Arc<Indexer>,
    reporter: Arc<R>,
    state: Arc<RwLock<State>>,
    scan_queue: Mutex<ScanQueue>,
    scan_done_tx: mpsc::UnboundedSender<()>,
}

/// RAII guard that sends on `scan_done_tx` when dropped, guaranteeing the
/// actor's `scan_done_rx` is always unblocked even if the task panics.
struct ScanDoneGuard(Option<mpsc::UnboundedSender<()>>);

impl Drop for ScanDoneGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
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
            scan_queue: Mutex::new(ScanQueue::new()),
            scan_done_tx,
        }
    }

    /// Returns `true` while a background index scan is in flight.
    pub(crate) fn is_scanning(&self) -> bool {
        self.scan_queue.lock().is_in_progress()
    }

    /// Called by the actor when `scan_done_rx` fires.
    ///
    /// Marks the current scan complete and starts any pending follow-up.
    pub(crate) fn on_scan_completed(&self) {
        let maybe_next = {
            let mut queue = self.scan_queue.lock();
            queue.completed();
            queue.try_start()
        };
        if let Some(args) = maybe_next {
            self.execute_scan(args);
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
        let data = self.apply_config(config).await;
        self.enqueue_scan(ScanArgs {
            root: data.root,
            kind: ScanKind::Prioritized {
                initial_paths: Vec::new(),
            },
            completion_tx,
            expected_generation: 0,
            reset_before_scan: false,
        });
    }

    pub(crate) async fn handle_reindex(&self) {
        let Some(root) = self.current_root() else {
            log::warn!("Actor: Reindex received but no workspace root is set");
            return;
        };
        // `reset_index_state()` is deferred into the scan task so it never
        // races with a concurrently running scan.
        self.enqueue_scan(ScanArgs {
            root,
            kind: ScanKind::Full,
            completion_tx: None,
            expected_generation: 0,
            reset_before_scan: true,
        });
    }

    pub(crate) async fn handle_change_root(&self, root: PathBuf) {
        let config = Config {
            root,
            explicit_source_paths: Vec::new(),
            ignore_patterns: Vec::new(),
            pin_workspace: true,
        };
        let data = self.apply_config(config).await;
        // `reset_index_state()` is deferred into the scan task (see `execute_scan`)
        // so it cannot race with a concurrently running scan for the old root.
        self.enqueue_scan(ScanArgs {
            root: data.root,
            kind: ScanKind::Full,
            completion_tx: None,
            expected_generation: 0,
            reset_before_scan: true,
        });
    }

    pub(crate) async fn switch_workspace_root_for_opened_document(
        &self,
        workspace_root: PathBuf,
        opened_file_path: Option<PathBuf>,
    ) {
        let config = Config {
            root: workspace_root,
            explicit_source_paths: Vec::new(),
            ignore_patterns: Vec::new(),
            pin_workspace: true,
        };
        let data = self.apply_config(config).await;
        log::info!(
            "Auto-detected workspace root (now pinned): {}",
            data.root.display()
        );
        // `reset_index_state()` is deferred into the scan task so it cannot
        // race with any scan still completing for the previous root.
        self.enqueue_scan(ScanArgs {
            root: data.root,
            kind: ScanKind::Prioritized {
                initial_paths: opened_file_path.into_iter().collect(),
            },
            completion_tx: None,
            expected_generation: 0,
            reset_before_scan: true,
        });
    }

    /// Apply a [`Config`] to the indexer and transition the phase state.
    ///
    /// The single write path shared by Initialize, ChangeRoot, and
    /// switch_workspace_root_for_opened_document. Returns the resolved
    /// [`ReadyState`] so callers can extract the root for subsequent scans.
    async fn apply_config(&self, config: Config) -> ReadyState {
        let data = ReadyState::from_config(&config);
        self.set_root(data.root.clone());
        self.apply_ignore_patterns(&config.ignore_patterns, &data.root);
        self.indexer
            .workspace_pinned
            .store(config.pin_workspace, std::sync::atomic::Ordering::Relaxed);
        self.write_source_paths(data.source_paths.clone());
        self.state.write().await.set_state(data.clone());
        data
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

    /// Enqueue a scan request. If a scan is in progress the generation is
    /// bumped to invalidate it; the new request replaces any earlier pending
    /// one (last-write-wins). Starts the scan immediately when the queue is idle.
    fn enqueue_scan(&self, args: ScanArgs) {
        let maybe_args = {
            let mut queue = self.scan_queue.lock();
            if queue.is_in_progress() {
                self.indexer.workspace_root.bump_generation();
            }
            let gen = self.indexer.workspace_root.generation();
            let args = ScanArgs {
                expected_generation: gen,
                ..args
            };
            queue.request(args);
            queue.try_start()
        };
        if let Some(args) = maybe_args {
            self.execute_scan(args);
        }
    }

    /// Spawn the tokio task for a single scan. Bails out early if the scan
    /// has been superseded (generation mismatch) before or after indexing.
    ///
    /// The `ScanDoneGuard` RAII type guarantees `scan_done_tx` is always
    /// signalled on task completion or panic, keeping the queue unblocked.
    fn execute_scan(&self, args: ScanArgs) {
        let indexer = Arc::clone(&self.indexer);
        let reporter = Arc::clone(&self.reporter);
        let scan_done_tx = self.scan_done_tx.clone();
        tokio::spawn(async move {
            let ScanArgs {
                root,
                kind,
                completion_tx,
                expected_generation,
                reset_before_scan,
                ..
            } = args;

            // The RAII guard ensures scan_done_tx fires even if this task panics.
            let _done = ScanDoneGuard(Some(scan_done_tx));

            if indexer.workspace_root.generation() != expected_generation {
                return;
            }

            // Safe to reset here: the queue was idle when execute_scan was called
            // (either the queue had no in-progress scan, or on_scan_completed just
            // finished the previous one). No other scan task is running at this point.
            if reset_before_scan {
                indexer.reset_index_state();
            }

            match kind {
                ScanKind::Prioritized { initial_paths } => {
                    Arc::clone(&indexer)
                        .index_workspace_prioritized(&root, initial_paths, reporter)
                        .await;
                }
                ScanKind::Full => {
                    Arc::clone(&indexer)
                        .index_workspace_full(&root, reporter)
                        .await;
                }
            }

            // _done drops here → scan_done_tx fires.
            if indexer.workspace_root.generation() == expected_generation {
                if let Some(tx) = completion_tx {
                    let _ = tx.send(());
                }
            }
        });
    }
}

#[cfg(test)]
#[path = "scan_handler_tests.rs"]
mod tests;
