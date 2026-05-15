use std::collections::HashMap;
use std::sync::Arc;

use tokio::task::JoinHandle;
use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, Url};
use tower_lsp::Client;

use crate::backend::helpers::syntax_diagnostics;
use crate::features::fill_when::when_diagnostics;
use crate::indexer::Indexer;

pub(crate) struct FileChangeHandler {
    indexer: Arc<Indexer>,
    client: Option<Client>,
    pending_reindex: HashMap<String, JoinHandle<()>>,
}

impl FileChangeHandler {
    pub(crate) fn new(indexer: Arc<Indexer>, client: Option<Client>) -> Self {
        Self {
            indexer,
            client,
            pending_reindex: HashMap::new(),
        }
    }

    pub(crate) async fn handle_file_changed(
        &mut self,
        uri: Url,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) {
        self.drain_and_apply_file_changes(uri, changes).await;
    }

    async fn drain_and_apply_file_changes(
        &mut self,
        uri: Url,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) {
        let Some(text) = self.drain_file_changed_batch(changes) else {
            return;
        };

        self.indexer.set_live_lines(&uri, &text);
        self.spawn_live_tree_update(uri.clone(), text.clone());
        self.reschedule_debounced_reindex(uri, text);
    }

    fn drain_file_changed_batch(
        &mut self,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Option<String> {
        changes.into_iter().last().map(|change| change.text)
    }

    fn spawn_live_tree_update(&self, uri: Url, text: String) {
        let indexer = Arc::clone(&self.indexer);
        drop(tokio::task::spawn_blocking(move || {
            indexer.store_live_tree(&uri, &text);
        }));
    }

    fn reschedule_debounced_reindex(&mut self, uri: Url, text: String) {
        self.pending_reindex.retain(|_, h| !h.is_finished());

        let key = uri.to_string();
        if let Some(handle) = self.pending_reindex.remove(&key) {
            handle.abort();
        }

        let client = self.client.clone();
        let indexer = Arc::clone(&self.indexer);
        let semaphore = indexer.parse_sem();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
            let Ok(permit) = semaphore.acquire_owned().await else {
                return;
            };
            let diagnostics_uri = uri.clone();
            let diagnostics_indexer = Arc::clone(&indexer);
            let result = tokio::task::spawn_blocking(move || {
                let data = indexer.index_content(&uri, &text);
                drop(permit);
                data
            })
            .await;

            if let (Some(client), Ok(Some(data))) = (client, result) {
                let mut diagnostics = syntax_diagnostics(&data.syntax_errors);
                diagnostics.extend(when_diagnostics(&diagnostics_indexer, &diagnostics_uri));
                client
                    .publish_diagnostics(diagnostics_uri, diagnostics, None)
                    .await;
            }
        });
        self.pending_reindex.insert(key, handle);
    }

    pub(crate) fn cancel_pending_reindex(&mut self, uri: &Url) {
        let key = uri.to_string();
        if let Some(handle) = self.pending_reindex.remove(&key) {
            handle.abort();
        }
    }
}

#[cfg(test)]
#[path = "file_change_handler_tests.rs"]
mod tests;
