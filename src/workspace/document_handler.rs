use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tower_lsp::lsp_types::Url;
use tower_lsp::Client;

use crate::backend::helpers::syntax_diagnostics;
use crate::features::call_arg_diagnostics::call_arg_diagnostics;
use crate::features::code_actions::missing_package_diagnostic;
use crate::features::fill_when::when_diagnostics;
use crate::indexer::live_tree::{lang_for_path, parse_live};
use crate::indexer::{Indexer, ProgressReporter};

use super::file_change_handler::FileChangeHandler;
use super::scan_handler::ScanHandler;

const STRONG_BUILD_MARKERS: [&str; 6] = [
    "build.gradle",
    "settings.gradle",
    "build.gradle.kts",
    "Cargo.toml",
    "pom.xml",
    "settings.gradle.kts",
];
const WEAK_BUILD_MARKERS: [&str; 1] = ["Package.swift"];

pub(crate) struct DocumentHandler {
    indexer: Arc<Indexer>,
    client: Option<Client>,
}

impl DocumentHandler {
    pub(crate) fn new(indexer: Arc<Indexer>, client: Option<Client>) -> Self {
        Self { indexer, client }
    }

    pub(crate) async fn handle_file_opened<R: ProgressReporter + 'static>(
        &self,
        scan_handler: &ScanHandler<R>,
        uri: Url,
        _language_id: String,
        content: String,
    ) {
        let opened_file_path = uri.to_file_path().ok();
        let workspace_pinned = self.indexer.workspace_pinned.load(Ordering::Relaxed);

        if let Some(workspace_root) =
            self.detect_workspace_root_switch(workspace_pinned, opened_file_path.as_deref())
        {
            scan_handler
                .switch_workspace_root_for_opened_document(workspace_root, opened_file_path.clone())
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

    pub(crate) async fn handle_file_saved(&self, uri: Url) {
        let indexer = Arc::clone(&self.indexer);
        let semaphore = indexer.parse_sem();
        tokio::task::spawn(async move {
            let Ok(path) = uri.to_file_path() else {
                return;
            };
            let Ok(content) = tokio::fs::read_to_string(&path).await else {
                return;
            };
            let Ok(permit) = semaphore.acquire_owned().await else {
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

    pub(crate) async fn handle_file_closed(
        &self,
        file_change_handler: &mut FileChangeHandler,
        uri: Url,
    ) {
        file_change_handler.cancel_pending_reindex(&uri);
        self.indexer.remove_live_tree(&uri);
        self.indexer.remove_live_lines(&uri);
        if let Some(client) = &self.client {
            client.publish_diagnostics(uri, Vec::new(), None).await;
        }
    }

    pub(crate) async fn handle_file_deleted(&self, uri: Url) {
        self.indexer.remove_indexed_file(&uri);
        self.indexer.remove_live_tree(&uri);
        self.indexer.remove_live_lines(&uri);
        if let Some(client) = &self.client {
            client.publish_diagnostics(uri, Vec::new(), None).await;
        }
    }

    async fn store_live_document_state(&self, uri: &Url, content: &str) {
        self.indexer.set_live_lines(uri, content);

        let indexer = Arc::clone(&self.indexer);
        let uri = uri.clone();
        let content = content.to_owned();
        let _ = tokio::task::spawn_blocking(move || indexer.store_live_tree(&uri, &content)).await;
    }

    fn spawn_open_document_indexing(&self, uri: Url, content: String) {
        let indexer = Arc::clone(&self.indexer);
        let diag_indexer = Arc::clone(&self.indexer);
        let client = self.client.clone();
        let semaphore = indexer.parse_sem();
        tokio::task::spawn(async move {
            let diagnostics_uri = uri.clone();
            let diagnostics_text = content.clone();
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

            // Parse tree from the exact same text that was just indexed
            let live_doc = lang_for_path(diagnostics_uri.path())
                .and_then(|lang| parse_live(&diagnostics_text, lang));

            let mut diagnostics = match result {
                Ok(Some(indexed_file_data)) => syntax_diagnostics(&indexed_file_data.syntax_errors),
                Ok(None) => diag_indexer
                    .files
                    .get(diagnostics_uri.as_str())
                    .map(|file_data| syntax_diagnostics(&file_data.syntax_errors))
                    .unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            // Skip semantic diagnostics while the workspace scan is still in
            // progress — the index is partial and would produce false positives
            // (e.g. sealed subtypes not yet indexed).  `on_became_ready` in the
            // actor will call `republish_open_file_diagnostics` once the scan
            // completes to fill in the diagnostics for all open files.
            if !diag_indexer.indexing_in_progress.load(Ordering::Acquire) {
                diagnostics.extend(when_diagnostics(&diag_indexer, &diagnostics_uri));
                if let Some(ref doc) = live_doc {
                    diagnostics.extend(call_arg_diagnostics(&diag_indexer, &diagnostics_uri, doc));
                }
            }
            let diag_lines = diag_indexer.mem_lines_for(diagnostics_uri.as_str());
            let diag_lines: Vec<String> = diag_lines
                .as_ref()
                .map(|l| l.as_ref().clone())
                .unwrap_or_default();
            if let Some(pkg_diag) = missing_package_diagnostic(&diag_lines, &diagnostics_uri) {
                diagnostics.push(pkg_diag);
            }
            if let Some(client) = client {
                client
                    .publish_diagnostics(diagnostics_uri, diagnostics, None)
                    .await;
            }
        });
    }

    /// Re-publish diagnostics for every currently-open file.
    ///
    /// Called by the actor's `on_became_ready` after the workspace scan
    /// completes so that files opened during the scan get their semantic
    /// diagnostics (which were suppressed while the index was partial).
    pub(crate) fn republish_open_file_diagnostics(&self) {
        for entry in self.indexer.live_trees.iter() {
            let Ok(uri) = Url::parse(entry.key()) else {
                continue;
            };
            let indexer = Arc::clone(&self.indexer);
            let client = self.client.clone();
            tokio::task::spawn(async move {
                let mut diagnostics = indexer
                    .files
                    .get(uri.as_str())
                    .map(|f| syntax_diagnostics(&f.syntax_errors))
                    .unwrap_or_default();
                diagnostics.extend(when_diagnostics(&indexer, &uri));
                if let Some(doc) = indexer.live_doc(&uri) {
                    diagnostics.extend(call_arg_diagnostics(&indexer, &uri, &doc));
                }
                let lines = indexer.mem_lines_for(uri.as_str());
                let lines: Vec<String> = lines
                    .as_ref()
                    .map(|l| l.as_ref().clone())
                    .unwrap_or_default();
                if let Some(pkg_diag) = missing_package_diagnostic(&lines, &uri) {
                    diagnostics.push(pkg_diag);
                }
                if let Some(client) = client {
                    client.publish_diagnostics(uri, diagnostics, None).await;
                }
            });
        }
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
        let mut current_directory = opened_file_path.parent().map(Path::to_path_buf);
        let mut nearest_strong_marker_root: Option<PathBuf> = None;
        let mut git_root: Option<PathBuf> = None;
        let mut nearest_weak_marker_root: Option<PathBuf> = None;

        while let Some(directory) = current_directory {
            if nearest_strong_marker_root.is_none()
                && has_any_marker(&directory, &STRONG_BUILD_MARKERS)
            {
                nearest_strong_marker_root = Some(directory.clone());
            }
            if directory.join(".git").exists() {
                git_root = Some(directory.clone());
                break;
            }
            if nearest_weak_marker_root.is_none() && has_any_marker(&directory, &WEAK_BUILD_MARKERS)
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

    fn current_root(&self) -> Option<PathBuf> {
        self.indexer.workspace_root.get()
    }

    fn canonicalize_or_clone(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }
}

fn has_any_marker(directory: &Path, markers: &[&str]) -> bool {
    markers.iter().any(|marker| directory.join(marker).exists())
}

#[cfg(test)]
#[path = "document_handler_tests.rs"]
mod tests;
