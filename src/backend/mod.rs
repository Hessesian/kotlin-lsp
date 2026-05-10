use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{async_trait, Client, LanguageServer};

use crate::indexer::{workspace_cache_path, IgnoreMatcher, Indexer, ProgressReporter};
use crate::semantic_tokens;
use crate::workspace::{WorkspaceConfig, WorkspaceEvent};

pub(crate) mod actions;
pub(crate) mod cursor;
pub(crate) mod format;
pub(crate) mod handlers;
pub(crate) mod helpers;
pub(crate) mod nav;
pub(crate) mod rename;

// ─── LSP progress reporter (outbound adapter) ────────────────────────────────

mod progress {
    use tower_lsp::lsp_types::ProgressParams;

    /// `$/progress` notification — reports workspace indexing status to the editor.
    pub(super) enum KotlinProgress {}
    impl tower_lsp::lsp_types::notification::Notification for KotlinProgress {
        type Params = ProgressParams;
        const METHOD: &'static str = "$/progress";
    }
}

/// Sends LSP `$/progress` notifications via `tower_lsp::Client`.
struct LspProgressReporter(Client);

impl ProgressReporter for LspProgressReporter {
    async fn begin(&self, token: &NumberOrString, message: &str) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            self.0
                .send_request::<tower_lsp::lsp_types::request::WorkDoneProgressCreate>(
                    WorkDoneProgressCreateParams {
                        token: token.clone(),
                    },
                ),
        )
        .await;
        self.0
            .send_notification::<progress::KotlinProgress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: "kotlin-lsp".into(),
                        cancellable: Some(false),
                        message: Some(message.to_owned()),
                        percentage: Some(0),
                    },
                )),
            })
            .await;
    }

    async fn report(&self, token: &NumberOrString, done: usize, total: usize) {
        let pct = ((done * 100).checked_div(total).unwrap_or(0)) as u32;
        self.0
            .send_notification::<progress::KotlinProgress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(
                    WorkDoneProgressReport {
                        cancellable: Some(false),
                        message: Some(format!("{done}/{total} files…")),
                        percentage: Some(pct),
                    },
                )),
            })
            .await;
    }

    async fn end(&self, token: &NumberOrString, message: &str) {
        self.0
            .send_notification::<progress::KotlinProgress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: Some(message.to_owned()),
                })),
            })
            .await;
    }
}

pub(crate) struct Backend {
    pub(super) client: Client,
    pub(super) indexer: Arc<Indexer>,
    event_tx: mpsc::Sender<WorkspaceEvent>,
    /// True if the client advertised `snippetSupport: true` during initialize.
    /// Used to decide whether to send `InsertTextFormat::SNIPPET` in completions.
    pub(super) snippet_support: Arc<AtomicBool>,
}

impl Backend {
    pub(crate) fn new(
        client: Client,
        indexer: Arc<Indexer>,
        event_tx: mpsc::Sender<WorkspaceEvent>,
    ) -> Self {
        Self {
            client,
            indexer,
            event_tx,
            snippet_support: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) async fn rg_context(&self) -> (Option<PathBuf>, Option<Arc<IgnoreMatcher>>) {
        let root = self.indexer.workspace_root.read().unwrap().clone();
        let ignore = self.indexer.ignore_matcher.read().unwrap().clone();
        (root, ignore)
    }

    /// Try `find_definition_qualified` with `rt.qualified`, falling back to `rt.leaf`
    /// when the first lookup is empty and the two names differ.
    pub(super) fn resolve_with_receiver_fallback(
        &self,
        word: &str,
        rt: &crate::resolver::ReceiverType,
        uri: &Url,
    ) -> Vec<Location> {
        let locs = self
            .indexer
            .find_definition_qualified(word, Some(&rt.qualified), uri);
        if locs.is_empty() && rt.leaf != rt.qualified {
            self.indexer
                .find_definition_qualified(word, Some(&rt.leaf), uri)
        } else {
            locs
        }
    }

    fn detect_snippet_support(params: &InitializeParams) -> bool {
        params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|text_document| text_document.completion.as_ref())
            .and_then(|completion| completion.completion_item.as_ref())
            .and_then(|completion_item| completion_item.snippet_support)
            .unwrap_or(false)
    }

    fn resolve_workspace_root(params: &InitializeParams) -> Option<PathBuf> {
        if std::env::var("KOTLIN_LSP_PREFER_CONFIG_ROOT").is_ok() {
            // Copilot CLI mode: config file overrides client rootUri so
            // kotlin_lsp_set_workspace works correctly.
            Self::workspace_root_from_environment()
                .or_else(Self::workspace_root_from_config)
                .or_else(|| Self::workspace_root_from_client(params))
        } else {
            // Editor mode: always honour the client's rootUri.
            Self::workspace_root_from_environment()
                .or_else(|| Self::workspace_root_from_client(params))
                .or_else(Self::workspace_root_from_config)
        }
    }

    fn workspace_root_from_environment() -> Option<PathBuf> {
        std::env::var("KOTLIN_LSP_WORKSPACE_ROOT")
            .ok()
            .map(PathBuf::from)
            .filter(|workspace_root| workspace_root.is_dir())
    }

    fn workspace_root_from_client(params: &InitializeParams) -> Option<PathBuf> {
        Self::initialize_root_uri(params)
            .and_then(|root_uri| root_uri.to_file_path().ok())
            .filter(|workspace_root| workspace_root.is_dir())
            .map(|workspace_root| Self::walk_up_to_git_root(&workspace_root))
    }

    fn initialize_root_uri(params: &InitializeParams) -> Option<Url> {
        params.root_uri.clone().or_else(|| {
            params
                .workspace_folders
                .as_deref()
                .and_then(|workspace_folders| workspace_folders.first())
                .map(|workspace_folder| workspace_folder.uri.clone())
        })
    }

    fn walk_up_to_git_root(workspace_root: &Path) -> PathBuf {
        let mut current_directory = workspace_root;
        loop {
            if current_directory.join(".git").exists() {
                return current_directory.to_path_buf();
            }
            match current_directory.parent() {
                Some(parent_directory) => current_directory = parent_directory,
                None => return workspace_root.to_path_buf(),
            }
        }
    }

    fn workspace_root_from_config() -> Option<PathBuf> {
        let home_directory = std::env::var("HOME")
            .ok()
            .unwrap_or_else(|| "/tmp".to_string());
        let config_file = Path::new(&home_directory).join(".config/kotlin-lsp/workspace");
        std::fs::read_to_string(config_file)
            .ok()
            .map(|workspace_root| PathBuf::from(workspace_root.trim()))
            .filter(|workspace_root| workspace_root.is_dir())
    }

    async fn configure_initialized_workspace(
        &self,
        params: &InitializeParams,
        workspace_root: &Path,
        workspace_pinned: bool,
    ) {
        if workspace_pinned {
            self.indexer.workspace_pinned.store(true, Ordering::Relaxed);
        }
        let (explicit_source_paths, ignore_patterns) =
            self.apply_initialization_options(params.initialization_options.as_ref());
        let _ = self
            .event_tx
            .send(WorkspaceEvent::Initialize {
                config: WorkspaceConfig {
                    root: workspace_root.to_path_buf(),
                    explicit_source_paths,
                    ignore_patterns,
                },
                completion_tx: None,
            })
            .await;
    }

    fn apply_initialization_options(
        &self,
        initialization_options: Option<&serde_json::Value>,
    ) -> (Vec<String>, Vec<String>) {
        let ignore_patterns =
            Self::collect_indexing_option_strings(initialization_options, "ignorePatterns")
                .unwrap_or_default();
        if !ignore_patterns.is_empty() {
            log::info!("ignorePatterns: {:?}", ignore_patterns);
        }

        let explicit_source_paths =
            Self::collect_indexing_option_strings(initialization_options, "sourcePaths")
                .unwrap_or_default();
        if !explicit_source_paths.is_empty() {
            log::info!("sourcePaths: {:?}", explicit_source_paths);
        }

        (explicit_source_paths, ignore_patterns)
    }

    fn collect_indexing_option_strings(
        initialization_options: Option<&serde_json::Value>,
        option_name: &str,
    ) -> Option<Vec<String>> {
        let option_values = initialization_options?
            .get("indexingOptions")?
            .get(option_name)?
            .as_array()?;
        let collected_values: Vec<String> = option_values
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        (!collected_values.is_empty()).then_some(collected_values)
    }
}

fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
                ..Default::default()
            },
        )),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".into(), ":".into()]),
            resolve_provider: Some(true),
            ..Default::default()
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        declaration_provider: Some(DeclarationCapability::Simple(true)),
        implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        inlay_hint_provider: Some(OneOf::Left(true)),
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: None,
            file_operations: None,
        }),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec!["kotlin-lsp/reindex".into(), "kotlin-lsp/clearCache".into()],
            ..Default::default()
        }),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".into(), ",".into()]),
            retrigger_characters: None,
            work_done_progress_options: Default::default(),
        }),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: semantic_tokens::legend(),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: Some(true),
                work_done_progress_options: Default::default(),
            },
        )),
        ..Default::default()
    }
}

#[async_trait]
impl LanguageServer for Backend {
    // ── lifecycle ────────────────────────────────────────────────────────────

    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let supports_snippets = Self::detect_snippet_support(&params);
        self.snippet_support
            .store(supports_snippets, Ordering::Relaxed);
        log::info!("client snippet support: {supports_snippets}");

        let resolved_workspace_root = Self::resolve_workspace_root(&params);
        let workspace_pinned = resolved_workspace_root.is_some();
        if let Some(workspace_root) = resolved_workspace_root {
            self.configure_initialized_workspace(&params, &workspace_root, workspace_pinned)
                .await;
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "kotlin-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: server_capabilities(),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "kotlin-lsp ready")
            .await;

        // Register a file-system watcher so we get notified when source
        // files change on disk (e.g. after a workspace/rename edit is applied to
        // closed files that never send didChange).
        let watchers: Vec<FileSystemWatcher> = crate::indexer::SOURCE_EXTENSIONS
            .iter()
            .map(|ext| FileSystemWatcher {
                glob_pattern: GlobPattern::String(format!("**/*.{ext}")),
                kind: None,
            })
            .collect();
        let _ = self
            .client
            .register_capability(vec![Registration {
                id: "watched-source-files".into(),
                method: "workspace/didChangeWatchedFiles".into(),
                register_options: Some(
                    serde_json::to_value(DidChangeWatchedFilesRegistrationOptions { watchers })
                        .unwrap_or_default(),
                ),
            }])
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        // Spawn cache write in background so the LSP shutdown response is sent
        // immediately. The process stays alive until the `exit` notification
        // arrives, giving the write enough time to complete for typical caches.
        let idx = Arc::clone(&self.indexer);
        tokio::task::spawn_blocking(move || idx.save_cache_to_disk());
        Ok(())
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        if params.command == "kotlin-lsp/reindex" {
            let root = self.indexer.workspace_root.read().unwrap().clone();
            let Some(root) = root else {
                self.client
                    .show_message(MessageType::WARNING, "kotlin-lsp: no workspace root set")
                    .await;
                return Ok(None);
            };
            let idx = Arc::clone(&self.indexer);
            let client = self.client.clone();
            idx.reset_index_state();
            tokio::spawn(async move {
                idx.index_workspace(&root, Arc::new(LspProgressReporter(client)))
                    .await;
            });
            self.client
                .show_message(MessageType::INFO, "kotlin-lsp: reindexing workspace…")
                .await;
        } else if params.command == "kotlin-lsp/clearCache" {
            // Optional arg: path to workspace root. If absent, clear current root's cache.
            let arg = params
                .arguments
                .first()
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let target_root = if let Some(p) = arg {
                let pb = std::path::PathBuf::from(p);
                if !pb.is_dir() {
                    self.client
                        .show_message(
                            MessageType::WARNING,
                            format!("kotlin-lsp/clearCache: not a directory: {}", pb.display()),
                        )
                        .await;
                    return Ok(None);
                }
                pb
            } else {
                // Acquire current root upfront and drop the lock before any await.
                let current_root_opt = { self.indexer.workspace_root.read().unwrap().clone() };
                match current_root_opt {
                    Some(r) => r,
                    None => {
                        self.client
                            .show_message(
                                MessageType::WARNING,
                                "kotlin-lsp/clearCache: no workspace root set and no path provided",
                            )
                            .await;
                        return Ok(None);
                    }
                }
            };
            let cache_path = workspace_cache_path(&target_root);
            if let Some(cache_dir) = cache_path.parent() {
                match std::fs::remove_dir_all(cache_dir) {
                    Ok(_) => {
                        log::info!("Cleared workspace cache directory: {}", cache_dir.display());
                        self.client
                            .show_message(
                                MessageType::INFO,
                                format!("kotlin-lsp: cleared cache for {}", target_root.display()),
                            )
                            .await;
                    }
                    Err(e) => {
                        log::warn!("Failed to remove cache dir {}: {}", cache_dir.display(), e);
                        self.client
                            .show_message(
                                MessageType::WARNING,
                                format!("kotlin-lsp: failed to clear cache: {}", e),
                            )
                            .await;
                    }
                }
            } else {
                self.client
                    .show_message(
                        MessageType::WARNING,
                        "kotlin-lsp/clearCache: cache path parent missing",
                    )
                    .await;
            }
        }
        Ok(None)
    }

    // ── document sync ────────────────────────────────────────────────────────

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let _ = self
            .event_tx
            .send(WorkspaceEvent::FileOpened {
                uri: params.text_document.uri,
                language_id: params.text_document.language_id,
                content: params.text_document.text,
            })
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let _ = self
            .event_tx
            .send(WorkspaceEvent::FileChanged {
                uri: params.text_document.uri,
                changes: params.content_changes,
            })
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let _ = self
            .event_tx
            .send(WorkspaceEvent::FileClosed {
                uri: params.text_document.uri,
            })
            .await;
    }

    // ── textDocument/didSave ─────────────────────────────────────────────────

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let _ = self
            .event_tx
            .send(WorkspaceEvent::FileSaved {
                uri: params.text_document.uri,
            })
            .await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // Re-index any *.kt / *.java file that changed on disk.
        // This fires after workspace/rename edits are applied to closed files,
        // keeping the in-memory symbol index consistent.
        for change in params.changes {
            if change.typ == FileChangeType::DELETED {
                // Remove from index; definition map cleanup is handled lazily.
                self.indexer.remove_indexed_file(&change.uri);
                continue;
            }
            let uri = change.uri;
            let idx = Arc::clone(&self.indexer);
            let sem = idx.parse_sem();
            tokio::task::spawn(async move {
                if let Ok(path) = uri.to_file_path() {
                    if let Ok(content) = tokio::fs::read_to_string(&path).await {
                        if let Ok(permit) = sem.acquire_owned().await {
                            tokio::task::spawn_blocking(move || {
                                let _permit = permit;
                                idx.index_content(&uri, &content);
                            })
                            .await
                            .ok();
                        }
                    }
                }
            });
        }
    }

    // ── textDocument/definition ──────────────────────────────────────────────

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.goto_definition_impl(params).await
    }

    // ── textDocument/declaration ─────────────────────────────────────────────
    // In Kotlin/Java there is no separate declaration/definition concept,
    // so we delegate to the same implementation.

    async fn goto_declaration(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.goto_definition_impl(params).await
    }

    // ── textDocument/implementation ──────────────────────────────────────────

    async fn goto_implementation(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.goto_implementation_impl(params).await
    }

    // ── textDocument/completion ──────────────────────────────────────────────

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.completion_impl(params).await
    }

    // ── completionItem/resolve ────────────────────────────────────────────────

    async fn completion_resolve(&self, mut item: CompletionItem) -> Result<CompletionItem> {
        use crate::indexer::resolution::{enrich_at_line, ResolveOptions, SubstitutionContext};

        if let Some(ref data) = item.data {
            if let (Some(uri), Some(line)) = (
                data.get("u").and_then(|v| v.as_str()),
                data.get("l").and_then(|v| v.as_u64()),
            ) {
                let col = data.get("c").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let calling_uri = data.get("cu").and_then(|v| v.as_str());

                let subst_ctx = match calling_uri {
                    Some(cu) if cu != uri => SubstitutionContext::CrossFile {
                        calling_uri: cu,
                        cursor_line: None,
                    },
                    _ => SubstitutionContext::None,
                };

                if let Some(info) = enrich_at_line(
                    self.indexer.as_ref(),
                    uri,
                    line as u32,
                    col,
                    subst_ctx,
                    &ResolveOptions::completion(),
                ) {
                    if !info.signature.is_empty() {
                        item.detail = Some(info.signature);
                    }
                    if !info.doc.is_empty() {
                        item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: info.doc,
                        }));
                    }
                }
            }
        }
        Ok(item)
    }

    // ── textDocument/hover ───────────────────────────────────────────────────

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        self.hover_impl(params).await
    }

    // ── textDocument/references ──────────────────────────────────────────────

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        self.references_impl(params).await
    }

    // ── textDocument/documentHighlight ───────────────────────────────────────

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        self.document_highlight_impl(params).await
    }

    // ── textDocument/documentSymbol ──────────────────────────────────────────

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        self.document_symbol_impl(params).await
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        self.inlay_hint_impl(params).await
    }

    // ── workspace/symbol ────────────────────────────────────────────────────

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        self.symbol_impl(params).await
    }

    // ── textDocument/signatureHelp ───────────────────────────────────────────

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        self.signature_help_impl(params).await
    }

    // ── textDocument/rename ──────────────────────────────────────────────────

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        self.prepare_rename_impl(params).await
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        self.rename_impl(params).await
    }

    // ── textDocument/foldingRange ────────────────────────────────────────────

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        self.folding_range_impl(params).await
    }

    // ── textDocument/codeAction ──────────────────────────────────────────────

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<Vec<CodeActionOrCommand>>> {
        self.code_action_impl(params).await
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri.to_string();
        let language = crate::Language::from_path(&uri);
        let Some(doc) = self.indexer.live_doc(&params.text_document.uri) else {
            return Ok(None);
        };
        let parsed_uri = params.text_document.uri;
        Ok(Some(SemanticTokensResult::Tokens(
            semantic_tokens::full_tokens(&self.indexer, &parsed_uri, &doc, language),
        )))
    }

    // ── textDocument/semanticTokens/range ────────────────────────────────────

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let uri = params.text_document.uri.to_string();
        let language = crate::Language::from_path(&uri);
        let Some(doc) = self.indexer.live_doc(&params.text_document.uri) else {
            return Ok(None);
        };
        let parsed_uri = params.text_document.uri;
        Ok(Some(SemanticTokensRangeResult::Tokens(
            semantic_tokens::range_tokens(
                &self.indexer,
                &parsed_uri,
                &doc,
                language,
                &params.range,
            ),
        )))
    }
}
