use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::FutureExt;
use tokio::sync::mpsc;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{async_trait, Client, LanguageServer};

use crate::indexer::{workspace_cache_path, Indexer, ProgressReporter};
use crate::semantic_tokens;
use crate::workspace::{Config, Event};

/// Wraps an async handler in `catch_unwind` so a panic in one request doesn't
/// kill the server process. Returns an internal error to the client on panic.
pub(crate) async fn panic_safe<F, T>(method: &str, future: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send,
    T: Send + 'static,
{
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    // Wrapper that sets PANIC_CAUGHT on each poll so the panic hook
    // always sees the flag regardless of which thread resumes the future.
    struct PanicGuarded<Fut>(Pin<Box<Fut>>);

    impl<Fut: Future> Future for PanicGuarded<Fut> {
        type Output = Fut::Output;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            crate::PANIC_CAUGHT.with(|c| c.set(true));
            let result = self.0.as_mut().poll(cx);
            if result.is_ready() {
                crate::PANIC_CAUGHT.with(|c| c.set(false));
            }
            result
        }
    }

    impl<Fut> Drop for PanicGuarded<Fut> {
        fn drop(&mut self) {
            crate::PANIC_CAUGHT.with(|c| c.set(false));
        }
    }

    let guarded = PanicGuarded(Box::pin(future));
    let result = std::panic::AssertUnwindSafe(guarded).catch_unwind().await;

    match result {
        Ok(result) => result,
        Err(payload) => {
            let message = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_owned()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_owned()
            };
            log::error!("PANIC in {method}: {message}");
            Err(tower_lsp::jsonrpc::Error {
                code: tower_lsp::jsonrpc::ErrorCode::InternalError,
                message: format!("internal error in {method}").into(),
                data: None,
            })
        }
    }
}

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
pub(crate) struct LspProgressReporter(pub(crate) Client);

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
    event_tx: mpsc::Sender<Event>,
    /// True if the client advertised `snippetSupport: true` during initialize.
    /// Used to decide whether to send `InsertTextFormat::SNIPPET` in completions.
    pub(super) snippet_support: Arc<AtomicBool>,
}

impl Backend {
    pub(crate) fn new(
        client: Client,
        indexer: Arc<Indexer>,
        event_tx: mpsc::Sender<Event>,
    ) -> Self {
        Self {
            client,
            indexer,
            event_tx,
            snippet_support: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn execute_command_impl(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        if params.command == "kotlin-lsp/reindex" {
            let root = self.indexer.workspace_root.get();
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
                let current_root_opt = self.indexer.workspace_root.get();
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
        let config_file = crate::util::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join(".config/kotlin-lsp/workspace");
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
        let (explicit_source_paths, ignore_patterns) =
            self.apply_initialization_options(params.initialization_options.as_ref());
        if self
            .event_tx
            .send(Event::Initialize {
                config: Config {
                    root: workspace_root.to_path_buf(),
                    explicit_source_paths,
                    ignore_patterns,
                    pin_workspace: workspace_pinned,
                },
                completion_tx: None,
            })
            .await
            .is_err()
        {
            log::error!(
                "configure_initialized_workspace: workspace actor channel closed; \
                 indexing will not start"
            );
        }
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
            trigger_characters: Some(vec![".".into(), ":".into(), "@".into()]),
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
        // NOTE: dynamic capability registration via client.register_capability() is intentionally
        // omitted here. tower-lsp 0.20 panics when the oneshot receiver created by pending.wait()
        // is dropped before the client's response arrives — a race that occurs because tower-lsp
        // fires `initialized` as a fire-and-forget notification (no coroutine keepalive). When
        // the client (e.g. Zed) responds quickly, pending.rs:35 finds a dropped receiver and
        // calls tx.send(r).expect("receiver already dropped"), killing the server process.
        //
        // Clients that natively watch files (Zed, Helix) send workspace/didChangeWatchedFiles
        // without dynamic registration; our did_change_watched_files handler processes those.

        // Watch .git/HEAD (resolved to actual commit SHA) and trigger a full reindex
        // when it changes — branch switches swap many files at once without sending
        // per-file workspace/didChangeWatchedFiles notifications.
        if let Some(root) = self.indexer.workspace_root.get() {
            spawn_git_head_watcher(root, Arc::clone(&self.indexer), self.client.clone());
        }
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
        panic_safe("execute_command", self.execute_command_impl(params)).await
    }

    // ── document sync ────────────────────────────────────────────────────────

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let _ = self
            .event_tx
            .send(Event::FileOpened {
                uri: params.text_document.uri,
                language_id: params.text_document.language_id,
                content: params.text_document.text,
            })
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Update live_lines synchronously so that any subsequent request
        // (e.g. completion) on the same transport sees the latest content,
        // even before the actor processes the event.
        if let Some(change) = params.content_changes.last() {
            self.indexer
                .set_live_lines(&params.text_document.uri, &change.text);
        }
        let _ = self
            .event_tx
            .send(Event::FileChanged {
                uri: params.text_document.uri,
                changes: params.content_changes,
            })
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let _ = self
            .event_tx
            .send(Event::FileClosed {
                uri: params.text_document.uri,
            })
            .await;
    }

    // ── textDocument/didSave ─────────────────────────────────────────────────

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let _ = self
            .event_tx
            .send(Event::FileSaved {
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
                if self
                    .event_tx
                    .send(Event::FileDeleted {
                        uri: change.uri.clone(),
                    })
                    .await
                    .is_err()
                {
                    log::warn!("FileDeleted event dropped: workspace actor channel closed");
                }
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
        panic_safe("goto_definition", self.goto_definition_impl(params)).await
    }

    async fn goto_declaration(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        panic_safe("goto_declaration", self.goto_definition_impl(params)).await
    }

    async fn goto_implementation(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        panic_safe("goto_implementation", self.goto_implementation_impl(params)).await
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        panic_safe("completion", self.completion_impl(params)).await
    }

    async fn completion_resolve(&self, item: CompletionItem) -> Result<CompletionItem> {
        panic_safe("completion_resolve", async {
            Ok(crate::features::completion::resolve_completion_item(
                item,
                self.indexer.as_ref(),
            ))
        })
        .await
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        panic_safe("hover", self.hover_impl(params)).await
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        panic_safe("references", self.references_impl(params)).await
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        panic_safe("document_highlight", self.document_highlight_impl(params)).await
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        panic_safe("document_symbol", self.document_symbol_impl(params)).await
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        panic_safe("inlay_hint", self.inlay_hint_impl(params)).await
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        panic_safe("workspace_symbol", self.symbol_impl(params)).await
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        panic_safe("signature_help", self.signature_help_impl(params)).await
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        panic_safe("prepare_rename", self.prepare_rename_impl(params)).await
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        panic_safe("rename", self.rename_impl(params)).await
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        panic_safe("folding_range", self.folding_range_impl(params)).await
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<Vec<CodeActionOrCommand>>> {
        panic_safe("code_action", self.code_action_impl(params)).await
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        panic_safe("semantic_tokens_full", async {
            let uri = params.text_document.uri.to_string();
            let language = crate::Language::from_path(&uri);
            let Some(doc) = self.indexer.live_doc(&params.text_document.uri) else {
                return Ok(None);
            };
            let parsed_uri = params.text_document.uri;
            Ok(Some(SemanticTokensResult::Tokens(
                semantic_tokens::full_tokens(&self.indexer, &parsed_uri, &doc, language),
            )))
        })
        .await
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        panic_safe("semantic_tokens_range", async {
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
        })
        .await
    }
}

// ─── Git HEAD watcher ────────────────────────────────────────────────────────

/// Resolves the current git commit SHA from `.git/HEAD`.
///
/// For a symbolic ref (`ref: refs/heads/main`), reads the pointed-to ref file.
/// For a detached HEAD, returns the raw SHA from HEAD itself.
/// Returns `None` if the git directory doesn't exist or files can't be read.
fn read_git_commit(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(ref_path) = head.strip_prefix("ref: ") {
        // Symbolic ref — resolve to actual commit SHA.
        std::fs::read_to_string(git_dir.join(ref_path))
            .ok()
            .map(|s| s.trim().to_string())
            // If the ref file doesn't exist yet (empty branch), fall back to
            // the symbolic ref itself so we still detect the branch name change.
            .or_else(|| Some(head.to_string()))
    } else {
        // Detached HEAD.
        Some(head.to_string())
    }
}

/// Spawns a background task that polls `.git/HEAD` every 2 seconds.
/// When the resolved commit SHA changes (branch switch or new commit), clears
/// the in-memory index and triggers a full workspace reindex.
fn spawn_git_head_watcher(root: PathBuf, indexer: Arc<Indexer>, client: Client) {
    let git_dir = root.join(".git");
    if !git_dir.is_dir() {
        return;
    }
    let mut last_commit = read_git_commit(&git_dir).unwrap_or_default();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let current = read_git_commit(&git_dir).unwrap_or_default();
            if current.is_empty() || current == last_commit {
                continue;
            }
            last_commit = current;
            log::info!("git HEAD changed — triggering workspace reindex");
            client
                .show_message(
                    MessageType::INFO,
                    "kotlin-lsp: branch changed, reindexing workspace…",
                )
                .await;
            let idx = Arc::clone(&indexer);
            let root_clone = root.clone();
            let client_clone = client.clone();
            idx.reset_index_state();
            tokio::spawn(async move {
                idx.index_workspace(&root_clone, Arc::new(LspProgressReporter(client_clone)))
                    .await;
            });
        }
    });
}
