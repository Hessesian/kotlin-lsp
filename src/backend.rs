use std::sync::Arc;

use dashmap::DashMap;
use tokio::task::AbortHandle;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{async_trait, Client, LanguageServer};

use crate::indexer::Indexer;

pub struct Backend {
    client:  Client,
    indexer: Arc<Indexer>,
    /// Per-URI abort handle for the pending debounced reindex task.
    /// When a new change arrives we abort the previous pending task so only
    /// the latest content is ever parsed.
    pending_reindex: DashMap<String, AbortHandle>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self { client, indexer: Arc::new(Indexer::new()), pending_reindex: DashMap::new() }
    }
}

#[async_trait]
impl LanguageServer for Backend {
    // ── lifecycle ────────────────────────────────────────────────────────────

    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Accept either rootUri or the first workspaceFolder.
        let root_uri = params.root_uri.or_else(|| {
            params
                .workspace_folders
                .as_deref()
                .and_then(|f| f.first())
                .map(|f| f.uri.clone())
        });

        if let Some(uri) = root_uri {
            if let Ok(path) = uri.to_file_path() {
                // Set workspace_root immediately so rg/fd calls work even before
                // indexing finishes (the background task can be slow on large projects).
                let _ = self.indexer.workspace_root.set(path.clone());
                let indexer = Arc::clone(&self.indexer);
                let client  = self.client.clone();
                // Background task — server is usable before indexing finishes.
                tokio::spawn(async move {
                    indexer.index_workspace(&path, client).await;
                });
            }
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name:    "kotlin-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                // FULL sync: each change event carries the whole document.
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".into(), ":".into()]),
                    resolve_provider:   Some(false),
                    ..Default::default()
                }),
                hover_provider:          Some(HoverProviderCapability::Simple(true)),
                definition_provider:     Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "kotlin-lsp ready")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    // ── document sync ────────────────────────────────────────────────────────

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri  = params.text_document.uri;
        let text = params.text_document.text;
        let idx  = Arc::clone(&self.indexer);
        let sem  = idx.parse_sem();
        tokio::task::spawn_blocking(move || {
            let _permit = sem.try_acquire_owned();
            idx.index_content(&uri, &text);
            // Pre-warm completion cache for all types referenced in this file.
            Arc::clone(&idx).prewarm_completion_cache(&uri);
        });
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            let uri  = params.text_document.uri;
            let text = change.text;
            let idx  = Arc::clone(&self.indexer);

            // Update live_lines immediately (no debounce) so completions()
            // always sees the current line text even before re-indexing.
            self.indexer.set_live_lines(&uri, &text);

            // True debounce: cancel any pending reindex for this file.
            let key = uri.to_string();
            if let Some((_, handle)) = self.pending_reindex.remove(&key) {
                handle.abort();
            }

            let pending = Arc::clone(&self.indexer);
            let _ = pending;
            let sem = idx.parse_sem();
            let handle = tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(120)).await;
                let _permit = sem.acquire_owned().await;
                tokio::task::spawn_blocking(move || {
                    idx.index_content(&uri, &text);
                    // Re-warm after change so new/renamed types are cached.
                    Arc::clone(&idx).prewarm_completion_cache(&uri);
                });
            });
            self.pending_reindex.insert(key, handle.abort_handle());
        }
    }

    async fn did_close(&self, _: DidCloseTextDocumentParams) {
        // Nothing to do — we keep the index entry so cross-file lookup still works.
    }

    // ── textDocument/definition ──────────────────────────────────────────────

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pp       = params.text_document_position_params;
        let uri      = &pp.text_document.uri;
        let position = pp.position;

        let Some((word, qualifier)) = self.indexer.word_and_qualifier_at(uri, position) else {
            return Ok(None);
        };

        let locs = self.indexer.find_definition_qualified(&word, qualifier.as_deref(), uri);
        Ok(match locs.len() {
            0 => None,
            1 => Some(GotoDefinitionResponse::Scalar(locs.into_iter().next().unwrap())),
            _ => Some(GotoDefinitionResponse::Array(locs)),
        })
    }

    // ── textDocument/completion ──────────────────────────────────────────────

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let pp       = params.text_document_position;
        let uri      = &pp.text_document.uri;
        let position = pp.position;

        let items = self.indexer.completions(uri, position);
        if items.is_empty() {
            return Ok(None);
        }
        Ok(Some(CompletionResponse::Array(items)))
    }

    // ── textDocument/hover ───────────────────────────────────────────────────

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let pp       = params.text_document_position_params;
        let uri      = &pp.text_document.uri;
        let position = pp.position;

        let Some(word) = self.indexer.word_at(uri, position) else {
            return Ok(None);
        };

        Ok(self.indexer.hover_info(&word).map(|md| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind:  MarkupKind::Markdown,
                value: md,
            }),
            range: None,
        }))
    }

    // ── textDocument/documentSymbol ──────────────────────────────────────────

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let symbols = self.indexer.file_symbols(&params.text_document.uri);
        if symbols.is_empty() {
            return Ok(None);
        }

        #[allow(deprecated)] // `deprecated` field superseded by `tags` in LSP 3.16+
        let doc_symbols = symbols
            .into_iter()
            .map(|s| DocumentSymbol {
                name:             s.name,
                detail:           None,
                kind:             s.kind,
                tags:             None,
                deprecated:       None,
                range:            s.range,
                selection_range:  s.selection_range,
                children:         None,
            })
            .collect();

        Ok(Some(DocumentSymbolResponse::Nested(doc_symbols)))
    }
}
