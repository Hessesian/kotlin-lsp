use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::task::JoinHandle;
use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, Url};
use tower_lsp::Client;

use crate::backend::helpers::syntax_diagnostics;
use crate::features::call_arg_diagnostics::call_arg_diagnostics;
use crate::features::fill_when::when_diagnostics;
use crate::indexer::live_tree::{lang_for_path, parse_live};
use crate::indexer::Indexer;

pub(crate) struct FileChangeHandler {
    indexer: Arc<Indexer>,
    client: Option<Client>,
    pending_reindex: HashMap<String, JoinHandle<()>>,
    /// Per-URI generation counter. Bumped on every edit so debounce tasks
    /// can detect whether a newer edit arrived while they were running.
    diagnostic_generation: HashMap<String, Arc<AtomicU64>>,
}

impl FileChangeHandler {
    pub(crate) fn new(indexer: Arc<Indexer>, client: Option<Client>) -> Self {
        Self {
            indexer,
            client,
            pending_reindex: HashMap::new(),
            diagnostic_generation: HashMap::new(),
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

        // Bump generation — any in-flight debounce task with an older
        // generation will skip publishing diagnostics.
        let key = uri.to_string();
        let generation = self
            .diagnostic_generation
            .entry(key)
            .or_insert_with(|| Arc::new(AtomicU64::new(0)));
        generation.fetch_add(1, Ordering::Release);

        // Clear stale diagnostics immediately so old positions don't linger
        // while the debounced reindex is pending.
        if let Some(ref client) = self.client {
            client
                .publish_diagnostics(uri.clone(), Vec::new(), None)
                .await;
        }

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
        let diag_indexer = Arc::clone(&self.indexer);
        let semaphore = indexer.parse_sem();
        let generation = Arc::clone(
            self.diagnostic_generation
                .entry(key.clone())
                .or_insert_with(|| Arc::new(AtomicU64::new(0))),
        );
        let my_generation = generation.load(Ordering::Acquire);
        let handle = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
            let Ok(permit) = semaphore.acquire_owned().await else {
                return;
            };
            let diagnostics_uri = uri.clone();
            let diagnostics_text = text.clone();
            let result = tokio::task::spawn_blocking(move || {
                let data = indexer.index_content(&uri, &text);
                drop(permit);
                data
            })
            .await;

            let Some(client) = client else { return };

            // If a newer edit arrived while we were working, skip publishing
            // — a newer debounce task will handle it.
            let current_gen = generation.load(Ordering::Acquire);
            log::warn!(
                "diag[gen={}]: generation check — current={current_gen} path={}",
                my_generation,
                diagnostics_uri.path(),
            );
            if current_gen != my_generation {
                log::warn!(
                    "diag[gen={}]: generation mismatch (current={current_gen}) — skipping publish",
                    my_generation
                );
                return;
            }

            // Parse tree from the exact same text that was just indexed —
            // this guarantees CST and indexed data are consistent.
            let live_doc = lang_for_path(diagnostics_uri.path())
                .and_then(|lang| parse_live(&diagnostics_text, lang));

            let index_hit_cache = matches!(result, Ok(None));
            log::warn!(
                "diag[gen={}]: index_content returned {} for {}",
                my_generation,
                if index_hit_cache {
                    "None (hash-cache hit)"
                } else {
                    "Some (reindexed)"
                },
                diagnostics_uri.path(),
            );
            let mut diagnostics = match result {
                Ok(Some(data)) => syntax_diagnostics(&data.syntax_errors),
                Ok(None) => diag_indexer
                    .files
                    .get(diagnostics_uri.as_str())
                    .map(|file_data| syntax_diagnostics(&file_data.syntax_errors))
                    .unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            diagnostics.extend(when_diagnostics(&diag_indexer, &diagnostics_uri));
            if let Some(ref doc) = live_doc {
                let arg_diags = call_arg_diagnostics(&diag_indexer, &diagnostics_uri, doc);
                log::warn!(
                    "diag[gen={}]: call_arg_diagnostics returned {} items",
                    my_generation,
                    arg_diags.len(),
                );
                diagnostics.extend(arg_diags);
            } else {
                log::warn!(
                    "diag[gen={}]: live_doc is None — no call-arg diagnostics",
                    my_generation,
                );
            }

            // Final generation check before publishing — prevents stale
            // diagnostics from overwriting a newer clear/publish.
            if generation.load(Ordering::Acquire) != my_generation {
                return;
            }

            client
                .publish_diagnostics(diagnostics_uri, diagnostics, None)
                .await;
        });
        self.pending_reindex.insert(key, handle);
    }

    pub(crate) fn cancel_pending_reindex(&mut self, uri: &Url) {
        let key = uri.to_string();
        if let Some(handle) = self.pending_reindex.remove(&key) {
            handle.abort();
        }
        self.diagnostic_generation.remove(&key);
    }
}

#[cfg(test)]
#[path = "file_change_handler_tests.rs"]
mod tests;
