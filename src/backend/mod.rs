use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tokio::task::AbortHandle;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{async_trait, Client, LanguageServer};

use self::helpers::syntax_diagnostics;
use crate::indexer::{workspace_cache_path, IgnoreMatcher, Indexer, ProgressReporter};

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
        let pct = if total > 0 {
            ((done * 100) / total) as u32
        } else {
            0
        };
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
    /// Per-URI abort handle for the pending debounced reindex task.
    /// When a new change arrives we abort the previous pending task so only
    /// the latest content is ever parsed.
    pub(super) pending_reindex: DashMap<String, AbortHandle>,
    /// True if the client advertised `snippetSupport: true` during initialize.
    /// Used to decide whether to send `InsertTextFormat::SNIPPET` in completions.
    pub(super) snippet_support: Arc<AtomicBool>,
}

#[derive(Clone)]
struct OpenedDocumentContext {
    uri: Url,
    text: String,
    opened_file_path: Option<PathBuf>,
}

impl OpenedDocumentContext {
    fn from_open_params(params: DidOpenTextDocumentParams) -> Self {
        let uri = params.text_document.uri;
        let opened_file_path = uri.to_file_path().ok();
        Self {
            uri,
            text: params.text_document.text,
            opened_file_path,
        }
    }
}

impl Backend {
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client,
            indexer: Arc::new(Indexer::new()),
            pending_reindex: DashMap::new(),
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

    fn configure_initialized_workspace(
        &self,
        params: &InitializeParams,
        workspace_root: &Path,
        workspace_pinned: bool,
    ) {
        self.set_workspace_root(workspace_root.to_path_buf());
        if workspace_pinned {
            self.indexer.workspace_pinned.store(true, Ordering::Relaxed);
        }
        self.apply_initialization_options(params.initialization_options.as_ref(), workspace_root);
        self.spawn_workspace_indexing(workspace_root.to_path_buf(), Vec::new());
    }

    fn apply_initialization_options(
        &self,
        initialization_options: Option<&serde_json::Value>,
        workspace_root: &Path,
    ) {
        if let Some(ignore_patterns) =
            Self::collect_indexing_option_strings(initialization_options, "ignorePatterns")
        {
            log::info!("ignorePatterns: {:?}", ignore_patterns);
            match self.indexer.ignore_matcher.write() {
                Ok(mut ignore_matcher) => {
                    *ignore_matcher = Some(Arc::new(IgnoreMatcher::new(
                        ignore_patterns,
                        workspace_root,
                    )));
                }
                Err(error) => {
                    log::warn!("Failed to update ignore matcher: {error}");
                }
            }
        }

        if let Some(source_paths) =
            Self::collect_indexing_option_strings(initialization_options, "sourcePaths")
        {
            log::info!("sourcePaths: {:?}", source_paths);
            match self.indexer.source_paths_raw.write() {
                Ok(mut source_paths_raw) => {
                    *source_paths_raw = source_paths;
                }
                Err(error) => {
                    log::warn!("Failed to update source paths: {error}");
                }
            }
        }
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

    fn set_workspace_root(&self, workspace_root: PathBuf) {
        match self.indexer.workspace_root.write() {
            Ok(mut current_workspace_root) => {
                *current_workspace_root = Some(workspace_root);
            }
            Err(error) => {
                log::warn!("Failed to update workspace root: {error}");
            }
        }
    }

    fn current_workspace_root(&self) -> Option<PathBuf> {
        match self.indexer.workspace_root.read() {
            Ok(current_workspace_root) => current_workspace_root.clone(),
            Err(error) => {
                log::warn!("Failed to read workspace root: {error}");
                None
            }
        }
    }

    fn spawn_workspace_indexing(&self, workspace_root: PathBuf, prioritized_paths: Vec<PathBuf>) {
        let indexer = Arc::clone(&self.indexer);
        let client = self.client.clone();
        tokio::spawn(async move {
            indexer
                .index_workspace_prioritized(
                    &workspace_root,
                    prioritized_paths,
                    Arc::new(LspProgressReporter(client)),
                )
                .await;
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
        let strong_markers = [
            "build.gradle",
            "settings.gradle",
            "build.gradle.kts",
            "Cargo.toml",
            "pom.xml",
            "settings.gradle.kts",
        ];
        let weak_markers = ["Package.swift"];
        let mut current_directory = opened_file_path.parent().map(Path::to_path_buf);
        let mut nearest_strong_marker_root: Option<PathBuf> = None;
        let mut git_root: Option<PathBuf> = None;
        let mut nearest_weak_marker_root: Option<PathBuf> = None;

        while let Some(directory) = current_directory {
            if nearest_strong_marker_root.is_none()
                && strong_markers
                    .iter()
                    .any(|marker| directory.join(marker).exists())
            {
                nearest_strong_marker_root = Some(directory.clone());
            }
            if directory.join(".git").exists() {
                git_root = Some(directory.clone());
                break;
            }
            if nearest_weak_marker_root.is_none()
                && weak_markers
                    .iter()
                    .any(|marker| directory.join(marker).exists())
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
        match self.current_workspace_root() {
            None => true,
            Some(current_workspace_root) => {
                let current_workspace_root = Self::canonicalize_or_clone(&current_workspace_root);
                let opened_file_path = Self::canonicalize_or_clone(opened_file_path);
                !opened_file_path.starts_with(&current_workspace_root)
                    && candidate_workspace_root != current_workspace_root
            }
        }
    }

    fn canonicalize_or_clone(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    fn switch_workspace_root_for_opened_document(
        &self,
        workspace_root: PathBuf,
        opened_file_path: Option<PathBuf>,
    ) {
        self.set_workspace_root(workspace_root.clone());
        self.indexer.workspace_pinned.store(true, Ordering::Relaxed);
        self.indexer.root_generation.fetch_add(1, Ordering::SeqCst);
        self.indexer.reset_index_state();
        log::info!(
            "Auto-detected workspace root (now pinned): {}",
            workspace_root.display()
        );
        self.spawn_workspace_indexing(workspace_root, opened_file_path.into_iter().collect());
    }

    fn is_outside_pinned_workspace_root(
        &self,
        workspace_pinned: bool,
        opened_file_path: Option<&Path>,
    ) -> bool {
        if !workspace_pinned {
            return false;
        }

        match (opened_file_path, self.current_workspace_root()) {
            (Some(opened_file_path), Some(current_workspace_root)) => {
                let opened_file_path = Self::canonicalize_or_clone(opened_file_path);
                let current_workspace_root =
                    Self::canonicalize_or_clone(current_workspace_root.as_path());
                !opened_file_path.starts_with(&current_workspace_root)
            }
            _ => false,
        }
    }

    async fn store_live_document_state(&self, opened_document: &OpenedDocumentContext) {
        self.indexer
            .set_live_lines(&opened_document.uri, &opened_document.text);

        let indexer = Arc::clone(&self.indexer);
        let uri = opened_document.uri.clone();
        let text = opened_document.text.clone();
        let _ = tokio::task::spawn_blocking(move || indexer.store_live_tree(&uri, &text)).await;
    }

    fn spawn_outside_root_document_indexing(&self, opened_document: OpenedDocumentContext) {
        let indexer = Arc::clone(&self.indexer);
        let semaphore = indexer.parse_sem();
        tokio::task::spawn(async move {
            if let Ok(permit) = semaphore.acquire_owned().await {
                let uri = opened_document.uri;
                let text = opened_document.text;
                let _ = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    indexer.index_content(&uri, &text);
                })
                .await;
            }
        });
    }

    fn spawn_open_document_indexing(&self, opened_document: OpenedDocumentContext) {
        let indexer = Arc::clone(&self.indexer);
        let semaphore = indexer.parse_sem();
        let client = self.client.clone();
        let cached_indexer = Arc::clone(&self.indexer);
        tokio::task::spawn(async move {
            let uri = opened_document.uri;
            let text = opened_document.text;
            let uri_for_diagnostics = uri.clone();
            let Ok(permit) = semaphore.acquire_owned().await else {
                return;
            };
            let result = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let data = indexer.index_content(&uri, &text);
                Arc::clone(&indexer).prewarm_completion_cache(&uri);
                data
            })
            .await;

            let diagnostics = match result {
                Ok(Some(indexed_file_data)) => syntax_diagnostics(&indexed_file_data.syntax_errors),
                Ok(None) => {
                    let uri_string = uri_for_diagnostics.to_string();
                    cached_indexer
                        .files
                        .get(&uri_string)
                        .map(|file_data| syntax_diagnostics(&file_data.syntax_errors))
                        .unwrap_or_default()
                }
                Err(_) => Vec::new(),
            };
            client
                .publish_diagnostics(uri_for_diagnostics, diagnostics, None)
                .await;
        });
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
            self.configure_initialized_workspace(&params, &workspace_root, workspace_pinned);
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
        // Persist the index cache so the next startup can skip unchanged files.
        // Awaited to ensure the write completes before the process exits.
        let idx = Arc::clone(&self.indexer);
        let _ = tokio::task::spawn_blocking(move || idx.save_cache_to_disk()).await;
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
        let opened_document = OpenedDocumentContext::from_open_params(params);
        let workspace_pinned = self.indexer.workspace_pinned.load(Ordering::Relaxed);

        if let Some(workspace_root) = self.detect_workspace_root_switch(
            workspace_pinned,
            opened_document.opened_file_path.as_deref(),
        ) {
            self.switch_workspace_root_for_opened_document(
                workspace_root,
                opened_document.opened_file_path.clone(),
            );
        }

        if self.is_outside_pinned_workspace_root(
            workspace_pinned,
            opened_document.opened_file_path.as_deref(),
        ) {
            log::info!(
                "Outside-root file — indexing content only: {}",
                opened_document
                    .opened_file_path
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            );
            self.store_live_document_state(&opened_document).await;
            self.spawn_outside_root_document_indexing(opened_document);
            return;
        }

        self.store_live_document_state(&opened_document).await;
        self.spawn_open_document_indexing(opened_document);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            let uri = params.text_document.uri;
            let text = change.text;
            let idx = Arc::clone(&self.indexer);

            // Update live_lines immediately (no debounce) so completions()
            // always sees the current line text even before re-indexing.
            self.indexer.set_live_lines(&uri, &text);
            // Parsing is CPU-bound; run on the blocking pool to avoid
            // stalling the Tokio worker thread on large files.
            {
                let idx2 = Arc::clone(&self.indexer);
                let uri2 = uri.clone();
                let text2 = text.clone();
                let _ =
                    tokio::task::spawn_blocking(move || idx2.store_live_tree(&uri2, &text2)).await;
            }

            // True debounce: cancel any pending reindex for this file.
            let key = uri.to_string();
            if let Some((_, handle)) = self.pending_reindex.remove(&key) {
                handle.abort();
            }

            let client = self.client.clone();
            let sem = idx.parse_sem();
            let handle = tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
                let permit = sem.acquire_owned().await;
                let uri2 = uri.clone();
                // Move the permit INTO spawn_blocking so it's held for the
                // entire index_content call.  If this async task is aborted
                // (debounce cancelled), spawn_blocking still runs to
                // completion holding the permit — preventing a concurrent
                // reindex for the same file from corrupting the shared maps.
                let result = tokio::task::spawn_blocking(move || {
                    let data = idx.index_content(&uri, &text);
                    drop(permit);
                    data
                })
                .await;

                if let Ok(Some(data)) = result {
                    let diags = syntax_diagnostics(&data.syntax_errors);
                    client.publish_diagnostics(uri2, diags, None).await;
                }
            });
            self.pending_reindex.insert(key, handle.abort_handle());
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = &params.text_document.uri;

        // Cancel any pending debounced reindex so it cannot re-publish
        // diagnostics after the file has been closed.
        let key = uri.to_string();
        if let Some((_, handle)) = self.pending_reindex.remove(&key) {
            handle.abort();
        }

        self.indexer.remove_live_tree(uri);
        self.indexer.remove_live_lines(uri);
        // Clear diagnostics so stale errors don't linger after the file is closed.
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }

    // ── textDocument/didSave ─────────────────────────────────────────────────

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        // Re-index the saved file so the symbol index stays consistent with
        // what is on disk (e.g. after an external format or code-gen step).
        let uri = params.text_document.uri;
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
}
