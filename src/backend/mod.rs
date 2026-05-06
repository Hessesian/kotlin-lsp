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

impl Backend {
    pub(crate) fn new(client: Client) -> Self {
        Self {
            client,
            indexer: Arc::new(Indexer::new()),
            pending_reindex: DashMap::new(),
            snippet_support: Arc::new(AtomicBool::new(false)),
        }
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
        // Detect snippet support from client capabilities.
        let supports_snippets = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|td| td.completion.as_ref())
            .and_then(|c| c.completion_item.as_ref())
            .and_then(|ci| ci.snippet_support)
            .unwrap_or(false);
        self.snippet_support
            .store(supports_snippets, Ordering::Relaxed);
        log::info!("client snippet support: {supports_snippets}");

        // Accept either rootUri or the first workspaceFolder.
        let root_uri = params.root_uri.or_else(|| {
            params
                .workspace_folders
                .as_deref()
                .and_then(|f| f.first())
                .map(|f| f.uri.clone())
        });

        // Priority:
        //   1. KOTLIN_LSP_WORKSPACE_ROOT env var  — explicit override, always wins
        //   2. LSP client rootUri / workspaceFolders — editor knows best when present
        //   3. ~/.config/kotlin-lsp/workspace file — fallback for clients that send no root
        //      (e.g. Copilot CLI agentic use)
        let env_override = std::env::var("KOTLIN_LSP_WORKSPACE_ROOT")
            .ok()
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir());

        let client_root = root_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok())
            .filter(|p| p.is_dir())
            .map(|p| {
                // Walk up to the nearest .git root so that opening a sub-module
                // (e.g. ios/Modules/ScenesCommon) still indexes the whole repo.
                // This is critical for cross-module go-to-definition.
                let mut cur = p.as_path();
                loop {
                    if cur.join(".git").exists() {
                        return cur.to_path_buf();
                    }
                    match cur.parent() {
                        Some(p) => cur = p,
                        None => return p.clone(),
                    }
                }
            });

        let config_fallback = || -> Option<std::path::PathBuf> {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            let config_file = std::path::Path::new(&home).join(".config/kotlin-lsp/workspace");
            std::fs::read_to_string(&config_file)
                .ok()
                .map(|s| std::path::PathBuf::from(s.trim()))
                .filter(|p| p.is_dir())
        };

        // Any resolved workspace root — env var, client rootUri, or config file — pins the
        // workspace and disables did_open auto-detection.  Without pinning, opening a file from
        // a second project in the same editor session triggers a spurious root switch that aborts
        // the in-progress workspace index and discards half its results.
        // Pure auto-detection (no config at all) still works: workspace_pinned stays false until
        // did_open fires and a root is detected for the first time.
        let resolved_root = env_override.or(client_root).or_else(config_fallback);
        let workspace_pinned = resolved_root.is_some();

        if let Some(path) = resolved_root {
            // Set workspace_root immediately so rg/fd calls work even before
            // indexing finishes (the background task can be slow on large projects).
            *self.indexer.workspace_root.write().unwrap() = Some(path.clone());
            if workspace_pinned {
                // Explicitly configured — prevent did_open auto-detection from overriding.
                self.indexer
                    .workspace_pinned
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }

            // Parse ignore patterns from initializationOptions.indexingOptions.ignorePatterns.
            if let Some(opts) = params.initialization_options.as_ref() {
                if let Some(patterns) = opts
                    .get("indexingOptions")
                    .and_then(|o| o.get("ignorePatterns"))
                    .and_then(|v| v.as_array())
                {
                    let pats: Vec<String> = patterns
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect();
                    if !pats.is_empty() {
                        log::info!("ignorePatterns: {:?}", pats);
                        *self.indexer.ignore_matcher.write().unwrap() =
                            Some(std::sync::Arc::new(IgnoreMatcher::new(pats, &path)));
                    }
                }

                // Parse sourcePaths — extra directories to index for hover/definition/autocomplete.
                // Stored as raw strings; resolved against workspace root when indexing starts.
                if let Some(source_paths) = opts
                    .get("indexingOptions")
                    .and_then(|o| o.get("sourcePaths"))
                    .and_then(|v| v.as_array())
                {
                    let paths_raw: Vec<String> = source_paths
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect();
                    if !paths_raw.is_empty() {
                        log::info!("sourcePaths: {:?}", paths_raw);
                        *self.indexer.source_paths_raw.write().unwrap() = paths_raw;
                    }
                }
            }

            let indexer = Arc::clone(&self.indexer);
            let client = self.client.clone();
            // Background task — server is usable before indexing finishes.
            tokio::spawn(async move {
                // No specific open-file priorities at initialize.
                indexer
                    .index_workspace_prioritized(
                        &path,
                        Vec::new(),
                        Arc::new(LspProgressReporter(client)),
                    )
                    .await;
            });
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
        let uri = params.text_document.uri;
        let text = params.text_document.text;

        // Keep the opened file path (if available) so prioritized indexing can seed it.
        let opened_path_opt = uri.to_file_path().ok();

        // Auto-detect workspace root from the opened file when workspace is not yet pinned
        // (i.e. no explicit env var / config file override was set at initialize).
        // Marker tiers (highest → lowest priority):
        //   1. Strong project markers (build.gradle.kts, settings.gradle, pom.xml, Cargo.toml)
        //      — typically appear exactly once at the project root. Nearest wins over .git.
        //   2. .git — repo root; wins over weak markers like Package.swift.
        //   3. Weak markers (Package.swift) — present at every Swift module; last resort.
        //
        // This correctly handles mono-repos where .git is at the parent of a subproject
        // (e.g. Moneta/.git with Moneta/android/settings.gradle.kts) and Swift mono-repos
        // where ios/.git is the right root but ios/Modules/*/Package.swift should be ignored.
        let pinned = self
            .indexer
            .workspace_pinned
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut need_root_switch: Option<std::path::PathBuf> = None;

        if !pinned {
            if let Some(ref path) = opened_path_opt {
                let strong_markers = [
                    "build.gradle",
                    "settings.gradle",
                    "build.gradle.kts",
                    "Cargo.toml",
                    "pom.xml",
                    "settings.gradle.kts",
                ];
                let weak_markers = ["Package.swift"];
                let mut cur = path.parent().map(|p| p.to_path_buf());
                let mut nearest_strong: Option<std::path::PathBuf> = None;
                let mut git_root: Option<std::path::PathBuf> = None;
                let mut nearest_weak: Option<std::path::PathBuf> = None;
                while let Some(ref dir) = cur {
                    if nearest_strong.is_none()
                        && strong_markers.iter().any(|m| dir.join(m).exists())
                    {
                        nearest_strong = Some(dir.clone());
                    }
                    if dir.join(".git").exists() {
                        git_root = Some(dir.clone());
                        break;
                    }
                    if nearest_weak.is_none() && weak_markers.iter().any(|m| dir.join(m).exists()) {
                        nearest_weak = Some(dir.clone());
                    }
                    cur = dir.parent().map(|p| p.to_path_buf());
                }
                let found = nearest_strong.or(git_root).or(nearest_weak);
                let chosen = found.or_else(|| path.parent().map(|p| p.to_path_buf()));
                if let Some(candidate_root) = chosen {
                    let current_root = self.indexer.workspace_root.read().unwrap().clone();
                    let cand_canon = std::fs::canonicalize(&candidate_root)
                        .unwrap_or_else(|_| candidate_root.clone());
                    let should_switch = match current_root {
                        None => true,
                        Some(ref r) => {
                            let cur_canon = std::fs::canonicalize(r).unwrap_or_else(|_| r.clone());
                            let path_canon =
                                std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                            !path_canon.starts_with(&cur_canon) && cand_canon != cur_canon
                        }
                    };
                    if should_switch {
                        need_root_switch = Some(candidate_root);
                    }
                }
            }
        }

        if let Some(root) = need_root_switch {
            *self.indexer.workspace_root.write().unwrap() = Some(root.clone());
            // Pin the workspace after the first auto-detection so that opening a file
            // from a second project later in the same session doesn't switch again.
            self.indexer
                .workspace_pinned
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.indexer
                .root_generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.indexer.reset_index_state();
            log::info!(
                "Auto-detected workspace root (now pinned): {}",
                root.display()
            );
            let idx = Arc::clone(&self.indexer);
            let client = self.client.clone();
            let root2 = root.clone();
            let opened = opened_path_opt.clone();
            tokio::spawn(async move {
                let reporter = Arc::new(LspProgressReporter(client));
                if let Some(op) = opened {
                    idx.index_workspace_prioritized(&root2, vec![op], reporter)
                        .await;
                } else {
                    idx.index_workspace_prioritized(&root2, Vec::new(), reporter)
                        .await;
                }
            });
        }

        // For files outside the current workspace root (e.g. agent opened a file from
        // another project): still index the file itself so hover/go-to-def work on it,
        // but skip workspace-wide re-indexing to avoid polluting workspaceSymbol results.
        let outside_root = pinned && {
            matches!(
                (opened_path_opt.as_ref(), self.indexer.workspace_root.read().unwrap().clone()),
                (Some(path), Some(root)) if {
                    // Use canonical paths to avoid symlink/path-form mismatches
                    // (consistent with the root-switch guard above).
                    let canon_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                    let canon_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
                    !canon_path.starts_with(&canon_root)
                }
            )
        };
        if outside_root {
            log::info!(
                "Outside-root file — indexing content only: {}",
                opened_path_opt
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            );
            self.indexer.set_live_lines(&uri, &text);
            {
                let idx2 = Arc::clone(&self.indexer);
                let uri2 = uri.clone();
                let text2 = text.clone();
                let _ =
                    tokio::task::spawn_blocking(move || idx2.store_live_tree(&uri2, &text2)).await;
            }
            // Index just this file so hover/go-to-def work, then return.
            let idx = Arc::clone(&self.indexer);
            let sem = idx.parse_sem();
            tokio::task::spawn(async move {
                if let Ok(permit) = sem.acquire_owned().await {
                    tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        idx.index_content(&uri, &text);
                    })
                    .await
                    .ok();
                }
            });
            return;
        }

        // Set live_lines immediately so completion can read the current file content
        // even before the async index_content task finishes.
        self.indexer.set_live_lines(&uri, &text);
        {
            let idx2 = Arc::clone(&self.indexer);
            let uri2 = uri.clone();
            let text2 = text.clone();
            let _ = tokio::task::spawn_blocking(move || idx2.store_live_tree(&uri2, &text2)).await;
        }

        let idx = Arc::clone(&self.indexer);
        let sem = idx.parse_sem();
        let client = self.client.clone();
        let idx2 = Arc::clone(&self.indexer);
        tokio::task::spawn(async move {
            let uri2 = uri.clone();
            let Ok(permit) = sem.acquire_owned().await else {
                return;
            };
            let result = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let data = idx.index_content(&uri, &text);
                // Pre-warm completion cache for all types referenced in this file.
                Arc::clone(&idx).prewarm_completion_cache(&uri);
                data
            })
            .await;

            // Publish diagnostics from syntax errors (or clear if hash-skipped).
            let diags = match result {
                Ok(Some(data)) => syntax_diagnostics(&data.syntax_errors),
                Ok(None) => {
                    // Hash-skipped — read cached errors.
                    let uri_str = uri2.to_string();
                    idx2.files
                        .get(&uri_str)
                        .map(|fd| syntax_diagnostics(&fd.syntax_errors))
                        .unwrap_or_default()
                }
                Err(_) => Vec::new(),
            };
            client.publish_diagnostics(uri2, diags, None).await;
        });
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
        self.indexer.live_lines.remove(uri.as_str());
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
                self.indexer.files.remove(change.uri.as_str());
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
