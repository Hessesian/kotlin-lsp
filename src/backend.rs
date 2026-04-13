use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// True if the client advertised `snippetSupport: true` during initialize.
    /// Used to decide whether to send `InsertTextFormat::SNIPPET` in completions.
    snippet_support: Arc<AtomicBool>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            indexer: Arc::new(Indexer::new()),
            pending_reindex: DashMap::new(),
            snippet_support: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl LanguageServer for Backend {
    // ── lifecycle ────────────────────────────────────────────────────────────

    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Detect snippet support from client capabilities.
        let supports_snippets = params.capabilities
            .text_document.as_ref()
            .and_then(|td| td.completion.as_ref())
            .and_then(|c| c.completion_item.as_ref())
            .and_then(|ci| ci.snippet_support)
            .unwrap_or(false);
        self.snippet_support.store(supports_snippets, Ordering::Relaxed);
        log::info!("client snippet support: {supports_snippets}");

        // Accept either rootUri or the first workspaceFolder.
        let root_uri = params.root_uri.or_else(|| {
            params
                .workspace_folders
                .as_deref()
                .and_then(|f| f.first())
                .map(|f| f.uri.clone())
        });

        // KOTLIN_LSP_WORKSPACE_ROOT env var or ~/.config/kotlin-lsp/workspace file
        // overrides whatever root the LSP client sends.
        // Useful when the LSP client (e.g. Copilot CLI) is started from a different directory.
        let workspace_override = std::env::var("KOTLIN_LSP_WORKSPACE_ROOT")
            .ok()
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(|| {
                // Fall back to ~/.config/kotlin-lsp/workspace file
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                let config_file = std::path::Path::new(&home).join(".config/kotlin-lsp/workspace");
                std::fs::read_to_string(&config_file).ok()
                    .map(|s| std::path::PathBuf::from(s.trim()))
                    .filter(|p| p.is_dir())
            });

        let resolved_root = workspace_override.or_else(|| {
            root_uri.as_ref().and_then(|uri| uri.to_file_path().ok())
        });

        if let Some(path) = resolved_root {
            // Set workspace_root immediately so rg/fd calls work even before
            // indexing finishes (the background task can be slow on large projects).
            *self.indexer.workspace_root.write().unwrap() = Some(path.clone());
            let indexer = Arc::clone(&self.indexer);
            let client  = self.client.clone();
            // Background task — server is usable before indexing finishes.
            tokio::spawn(async move {
                // No specific open-file priorities at initialize.
                indexer.index_workspace_prioritized(&path, Vec::new(), Some(client)).await;
            });
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
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                references_provider:     Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                inlay_hint_provider: Some(OneOf::Left(true)),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: None,
                    file_operations: None,
                }),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec!["kotlin-lsp/reindex".into(), "kotlin-lsp/changeRoot".into()],
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
            },
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
        let _ = self.client.register_capability(vec![
            Registration {
                id:     "watched-source-files".into(),
                method: "workspace/didChangeWatchedFiles".into(),
                register_options: Some(
                    serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                        watchers,
                    })
                    .unwrap_or_default(),
                ),
            },
        ]).await;
    }

    async fn shutdown(&self) -> Result<()> {
        // Persist the index cache so the next startup can skip unchanged files.
        let idx = Arc::clone(&self.indexer);
        tokio::task::spawn_blocking(move || idx.save_cache_to_disk());
        Ok(())
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<serde_json::Value>> {
        if params.command == "kotlin-lsp/reindex" {
            let root = self.indexer.workspace_root.read().unwrap().clone();
            let Some(root) = root else {
                self.client.show_message(MessageType::WARNING, "kotlin-lsp: no workspace root set").await;
                return Ok(None);
            };
            let idx    = Arc::clone(&self.indexer);
            let client = self.client.clone();
            idx.reset_index_state();
            tokio::spawn(async move {
                idx.index_workspace(&root, Some(client)).await;
            });
            self.client.show_message(MessageType::INFO, "kotlin-lsp: reindexing workspace…").await;
        } else if params.command == "kotlin-lsp/changeRoot" {
            // Expected argument: a single JSON string with the new workspace root path.
            let new_root = params.arguments.first()
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from);
            let Some(new_root) = new_root else {
                self.client.show_message(MessageType::WARNING,
                    "kotlin-lsp/changeRoot: expected one string argument (path)").await;
                return Ok(None);
            };
            if !new_root.is_dir() {
                self.client.show_message(MessageType::WARNING,
                    format!("kotlin-lsp/changeRoot: not a directory: {}", new_root.display())).await;
                return Ok(None);
            }
            // Swap root and wipe stale index data.
            let prev_root = self.indexer.workspace_root.read().unwrap().clone();
            *self.indexer.workspace_root.write().unwrap() = Some(new_root.clone());
            // Increment generation so in-flight background tasks can detect staleness.
            self.indexer.root_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Clear all index maps for the new root.
            self.indexer.reset_index_state();
            log::info!("Root switched; preserving on-disk caches for other roots");
            let idx    = Arc::clone(&self.indexer);
            let client = self.client.clone();
            let new_root2 = new_root.clone();
            tokio::spawn(async move {
                // Preserve existing behavior but prefer prioritized startup (no initial files here).
                idx.index_workspace_prioritized(&new_root2, Vec::new(), Some(client)).await;
            });
            self.client.show_message(MessageType::INFO,
                format!("kotlin-lsp: switching root to {}…", new_root.display())).await;
        } else if params.command == "kotlin-lsp/clearCache" {
            // Optional arg: path to workspace root. If absent, clear current root's cache.
            let arg = params.arguments.first().and_then(|v| v.as_str()).map(|s| s.to_string());
            let target_root = if let Some(p) = arg {
                let pb = std::path::PathBuf::from(p);
                if !pb.is_dir() {
                    self.client.show_message(MessageType::WARNING,
                        format!("kotlin-lsp/clearCache: not a directory: {}", pb.display())).await;
                    return Ok(None);
                }
                pb
            } else {
                // Acquire current root upfront and drop the lock before any await.
                let current_root_opt = { self.indexer.workspace_root.read().unwrap().clone() };
                match current_root_opt {
                    Some(r) => r,
                    None => {
                        self.client.show_message(MessageType::WARNING,
                            "kotlin-lsp/clearCache: no workspace root set and no path provided").await;
                        return Ok(None);
                    }
                }
            };
            let cache_path = crate::indexer::workspace_cache_path(&target_root);
            if let Some(cache_dir) = cache_path.parent() {
                match std::fs::remove_dir_all(cache_dir) {
                    Ok(_) => {
                        log::info!("Cleared workspace cache directory: {}", cache_dir.display());
                        self.client.show_message(MessageType::INFO,
                            format!("kotlin-lsp: cleared cache for {}", target_root.display())).await;
                    }
                    Err(e) => {
                        log::warn!("Failed to remove cache dir {}: {}", cache_dir.display(), e);
                        self.client.show_message(MessageType::WARNING,
                            format!("kotlin-lsp: failed to clear cache: {}", e)).await;
                    }
                }
            } else {
                self.client.show_message(MessageType::WARNING,
                    "kotlin-lsp/clearCache: cache path parent missing").await;
            }
        }
        Ok(None)
    }

    // ── document sync ────────────────────────────────────────────────────────

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri  = params.text_document.uri;
        let text = params.text_document.text;

        // If workspace_root not set, infer from opened file: look for common project markers
        // (.git, build.gradle, settings.gradle, build.gradle.kts, Cargo.toml, Package.swift) walking up.
        let mut need_root_switch: Option<std::path::PathBuf> = None;
        // Keep the opened file path (if available) so prioritized indexing can seed it.
        let opened_path_opt = uri.to_file_path().ok();
        if let Some(ref path) = opened_path_opt {
            // Compute candidate root for the opened file
            let mut cur = path.parent().map(|p| p.to_path_buf());
            let mut found: Option<std::path::PathBuf> = None;
            while let Some(ref dir) = cur {
                let markers = [
                    ".git", "build.gradle", "settings.gradle", "build.gradle.kts",
                    "Cargo.toml", "Package.swift", "pom.xml", "settings.gradle.kts",
                ];
                if markers.iter().any(|m| dir.join(m).exists()) {
                    found = Some(dir.clone());
                    break;
                }
                cur = dir.parent().map(|p| p.to_path_buf());
            }
            let chosen = found.or_else(|| path.parent().map(|p| p.to_path_buf()));
            if let Some(candidate_root) = chosen {
                let current_root = self.indexer.workspace_root.read().unwrap().clone();
                // If no root set, or file is outside current root, schedule switch
                let should_switch = match current_root {
                    None => true,
                    Some(ref r) => !path.starts_with(r),
                };
                if should_switch {
                    need_root_switch = Some(candidate_root);
                }
            }
        }

        if let Some(root) = need_root_switch {
            // Perform root swap similar to changeRoot: bump generation, clear maps, spawn index.
            let prev_root = self.indexer.workspace_root.read().unwrap().clone();
            *self.indexer.workspace_root.write().unwrap() = Some(root.clone());
            self.indexer.root_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.indexer.reset_index_state();
            log::info!("Preserving on-disk caches after auto root detection");
            let idx = Arc::clone(&self.indexer);
            let client = self.client.clone();
            let root2 = root.clone();
            // Prefer prioritized indexing seeded with the just-opened file for fast responsiveness.
            let opened = opened_path_opt.clone();
            tokio::spawn(async move {
                if let Some(op) = opened {
                    idx.index_workspace_prioritized(&root2, vec![op], Some(client)).await;
                } else {
                    idx.index_workspace_prioritized(&root2, Vec::new(), Some(client)).await;
                }
            });
        }

        let idx  = Arc::clone(&self.indexer);
        let sem  = idx.parse_sem();
        let client = self.client.clone();
        let idx2 = Arc::clone(&self.indexer);
        tokio::task::spawn(async move {
            let uri2 = uri.clone();
            let result = tokio::task::spawn_blocking(move || {
                let _permit = sem.try_acquire_owned();
                let data = idx.index_content(&uri, &text);
                // Pre-warm completion cache for all types referenced in this file.
                Arc::clone(&idx).prewarm_completion_cache(&uri);
                data
            }).await;

            // Publish diagnostics from syntax errors (or clear if hash-skipped).
            let diags = match result {
                Ok(Some(data)) => syntax_diagnostics(&data.syntax_errors),
                Ok(None) => {
                    // Hash-skipped — read cached errors.
                    let uri_str = uri2.to_string();
                    idx2.files.get(&uri_str)
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
                }).await;

                if let Ok(Some(data)) = result {
                    let diags = syntax_diagnostics(&data.syntax_errors);
                    client.publish_diagnostics(uri2, diags, None).await;
                }
            });
            self.pending_reindex.insert(key, handle.abort_handle());
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // Clear diagnostics so stale errors don't linger after the file is closed.
        self.client.publish_diagnostics(params.text_document.uri, Vec::new(), None).await;
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
            tokio::task::spawn_blocking(move || {
                if let Ok(path) = uri.to_file_path() {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let _permit = sem.try_acquire_owned();
                        idx.index_content(&uri, &content);
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
        let pp       = params.text_document_position_params;
        let uri      = &pp.text_document.uri;
        let position = pp.position;

        let Some((word, qualifier)) = self.indexer.word_and_qualifier_at(uri, position) else {
            return Ok(None);
        };

        // Special case: `it` or a named lambda parameter — resolve to the
        // inferred element/receiver type class instead of trying a text search.
        if qualifier.is_none() && (word == "it" || word.chars().next().map(|c| c.is_lowercase()).unwrap_or(true)) {
            if let Some(type_name) = self.indexer.infer_lambda_param_type_at(&word, uri, position) {
                // For qualified names (e.g. `Outer.Inner`) try the full name first,
                // then fall back to the last segment which is what the index stores.
                let lookup = type_name.rsplit('.').next().unwrap_or(&type_name);
                let locs = self.indexer.find_definition_qualified(lookup, None, uri);
                if !locs.is_empty() {
                    return Ok(match locs.len() {
                        1 => Some(GotoDefinitionResponse::Scalar(locs.into_iter().next().unwrap())),
                        _ => Some(GotoDefinitionResponse::Array(locs)),
                    });
                }
            }
            // If the word is a lambda parameter (type resolution failed), jump to
            // the `{ name ->` declaration line in the current file.
            let lambda_params = self.indexer.lambda_params_at_col(uri, position.line as usize, position.character as usize);
            if lambda_params.contains(&word) {
                if let Some(loc) = self.indexer.find_lambda_param_decl(uri, &word, position.line as usize) {
                    return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
                }
                return Ok(None);
            }
        }

        let locs = self.indexer.find_definition_qualified(&word, qualifier.as_deref(), uri);
        if !locs.is_empty() {
            return Ok(match locs.len() {
                1 => Some(GotoDefinitionResponse::Scalar(locs.into_iter().next().unwrap())),
                _ => Some(GotoDefinitionResponse::Array(locs)),
            });
        }

        // Index miss (symbol not indexed or indexing in progress) → rg fallback.
        let root_opt = { self.indexer.workspace_root.read().unwrap().clone() };
        let name_clone = word.clone();
        let rg_locs = tokio::task::spawn_blocking(move || {
            crate::indexer::rg_find_definition(&name_clone, root_opt.as_deref())
        }).await.unwrap_or_default();
        Ok(match rg_locs.len() {
            0 => None,
            1 => Some(GotoDefinitionResponse::Scalar(rg_locs.into_iter().next().unwrap())),
            _ => Some(GotoDefinitionResponse::Array(rg_locs)),
        })
    }

    // ── textDocument/implementation ──────────────────────────────────────────

    async fn goto_implementation(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pp       = params.text_document_position_params;
        let uri      = &pp.text_document.uri;
        let position = pp.position;

        let Some((word, _qualifier)) = self.indexer.word_and_qualifier_at(uri, position) else {
            return Ok(None);
        };

        // Direct subtypes from the index.
        let mut locs: Vec<Location> = self.indexer.subtypes
            .get(&word)
            .map(|v| v.clone())
            .unwrap_or_default();

        // If index is empty for this symbol (cold start), try rg-based heuristic
        // to find implementors quickly to avoid client timeouts in large projects.
        if locs.is_empty() {
            let root_opt = { self.indexer.workspace_root.read().unwrap().clone() };
            let word_clone = word.clone();
            let rg_impls = tokio::task::spawn_blocking(move || {
                crate::indexer::rg_find_implementors(&word_clone, root_opt.as_deref())
            }).await.unwrap_or_default();
            if !rg_impls.is_empty() {
                // Return early with rg results.
                return Ok(match rg_impls.len() {
                    1 => Some(GotoDefinitionResponse::Scalar(rg_impls.into_iter().next().unwrap())),
                    _ => Some(GotoDefinitionResponse::Array(rg_impls)),
                });
            }
        }

        // Also collect transitive subtypes (BFS, depth-limited).
        let mut queue: Vec<String> = locs.iter()
            .filter_map(|loc| {
                let data = self.indexer.files.get(loc.uri.as_str())?;
                data.symbols.iter()
                    .find(|s| s.selection_range == loc.range)
                    .map(|s| s.name.clone())
            })
            .collect();
        let mut visited = vec![word.clone()];
        while let Some(name) = queue.pop() {
            if visited.contains(&name) { continue; }
            visited.push(name.clone());
            if let Some(sub_locs) = self.indexer.subtypes.get(&name) {
                for loc in sub_locs.iter() {
                    if !locs.iter().any(|l| l.uri == loc.uri && l.range == loc.range) {
                        locs.push(loc.clone());
                        if let Some(data) = self.indexer.files.get(loc.uri.as_str()) {
                            if let Some(sym) = data.symbols.iter().find(|s| s.selection_range == loc.range) {
                                queue.push(sym.name.clone());
                            }
                        }
                    }
                }
            }
        }

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
        let snippets = self.snippet_support.load(Ordering::Relaxed);

        let items = self.indexer.completions(uri, position, snippets);
        if items.is_empty() {
            return Ok(None);
        }
        // `is_incomplete: false` tells the client the list is complete for the
        // current context — it can filter by prefix client-side without re-requesting
        // on every subsequent keystroke. This dramatically reduces server CPU.
        Ok(Some(CompletionResponse::List(CompletionList {
            is_incomplete: false,
            items,
        })))
    }

    // ── textDocument/hover ───────────────────────────────────────────────────

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let pp       = params.text_document_position_params;
        let uri      = &pp.text_document.uri;
        let position = pp.position;

        let Some((word, qualifier)) = self.indexer.word_and_qualifier_at(uri, position) else {
            return Ok(None);
        };

        // For `it` or a named lambda param, generate hover showing the inferred type.
        if qualifier.is_none() && (word == "it" || word.chars().next().map(|c| c.is_lowercase()).unwrap_or(true)) {
            if let Some(type_name) = self.indexer.infer_lambda_param_type_at(&word, uri, position) {
                let lang = if uri.path().ends_with(".kt") { "kotlin" }
                           else if uri.path().ends_with(".swift") { "swift" }
                           else { "java" };
                // Show the inferred binding
                let kw = if uri.path().ends_with(".swift") { "let" } else { "val" };
                let sig_md = format!("```{lang}\n{kw} {word}: {type_name}\n```");
                // For symbol lookup use the last segment of a qualified name
                // (symbols are indexed by short name, e.g. `CardProduct` not
                // `CreditCardDashboardInteractor.CardProduct`).
                let lookup_name = type_name.rsplit('.').next().unwrap_or(&type_name);
                let type_hover = self.indexer.hover_info(lookup_name);
                let full = if let Some(th) = type_hover {
                    format!("{sig_md}\n\n---\n\n{th}")
                } else {
                    sig_md
                };
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind:  MarkupKind::Markdown,
                        value: full,
                    }),
                    range: None,
                }));
            }
            // If the word is a lambda parameter (type resolution failed), don't
            // fall through to rg-based definition lookup — it would find unrelated
            // symbols with the same name and show confusing hover text.
            let lambda_params = self.indexer.lambda_params_at_col(uri, position.line as usize, position.character as usize);
            if lambda_params.contains(&word) {
                return Ok(None);
            }
        }

        // Use the same resolution chain as go-to-definition so hover always
        // points at the same symbol (import-aware, not just first index match).
        let locs = self.indexer.find_definition_qualified(&word, qualifier.as_deref(), uri);
        let hover_md = if let Some(loc) = locs.first() {
            self.indexer.hover_info_at_location(loc, &word)
        } else {
            // Index lookup — works for already-indexed symbols + stdlib.
            let from_index = self.indexer.hover_info(&word);
            if from_index.is_some() {
                from_index
            } else {
                // rg fallback: find the declaration even when the index is empty.
                let root_guard = self.indexer.workspace_root.read().unwrap();
                let rg_locs = crate::indexer::rg_find_definition(&word, root_guard.as_deref());
                drop(root_guard);
                rg_locs.first().and_then(|loc| self.indexer.hover_info_at_location(loc, &word))
            }
        };

        Ok(hover_md.map(|md| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind:  MarkupKind::Markdown,
                value: md,
            }),
            range: None,
        }))
    }

    // ── textDocument/documentSymbol ──────────────────────────────────────────


    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri  = &params.text_document_position.text_document.uri;
        let pos  = params.text_document_position.position;
        let include_decl = params.context.include_declaration;

        let name = match self.indexer.word_at(uri, pos) {
            Some(w) => w,
            None    => return Ok(None),
        };

        // For uppercase symbols, determine parent_class and declared_pkg:
        // - If cursor is ON the declaration of this symbol → use enclosing_class_at(cursor)
        // - If cursor is on a REFERENCE → scan imports in current file to find which
        //   specific class is meant (handles multiple `Effect` classes across files)
        let (parent_class, declared_pkg) = if name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            let on_decl = self.indexer.is_declared_in(uri, &name)
                && self.indexer.definitions.get(&name)
                    .map(|locs| locs.iter().any(|l| l.uri == *uri && l.range.start.line == pos.line))
                    .unwrap_or(false);
            if on_decl {
                // Cursor is on the class declaration — use local enclosing class.
                let parent = self.indexer.enclosing_class_at(uri, pos.line);
                let pkg    = self.indexer.package_of(uri);
                (parent, pkg)
            } else {
                // Cursor is on a reference — resolve via import in current file.
                let (parent, pkg) = self.indexer.resolve_symbol_via_import(uri, &name);
                if parent.is_some() || pkg.is_some() {
                    (parent, pkg)
                } else {
                    // Not imported explicitly (same package). Use declaration site.
                    let parent = self.indexer.declared_parent_class_of(&name, uri);
                    let pkg    = self.indexer.declared_package_of(&name);
                    (parent, pkg)
                }
            }
        } else {
            (None, None)
        };
        let same_pkg = self.indexer.package_of(uri);

        eprintln!("[refs] name={name:?} parent={parent_class:?} same_pkg={same_pkg:?} declared_pkg={declared_pkg:?}");

        // Collect declaration file paths — but only those where the enclosing class
        // matches parent_class (if known).  Without this filter, every contract file
        // that has `sealed interface Event` would be included, causing false positives
        // for unrelated ViewModels in other packages.
        let decl_files: Vec<String> = self.indexer.definitions.get(&name)
            .map(|locs| locs.iter()
                .filter(|l| {
                    if let Some(ref parent) = parent_class {
                        self.indexer.enclosing_class_at(&l.uri, l.range.start.line)
                            .as_deref() == Some(parent.as_str())
                    } else {
                        true
                    }
                })
                .filter_map(|l| l.uri.to_file_path().ok())
                .filter_map(|p| p.to_str().map(|s| s.to_owned()))
                .collect())
            .unwrap_or_default();

        // Run rg off the async executor to avoid blocking the Tokio runtime.
        let root = self.indexer.workspace_root.read().unwrap().clone();
        let uri_clone = uri.clone();
        let name2 = name.clone();
        let parent2 = parent_class.clone();
        let same2 = same_pkg.clone();
        let decl2 = declared_pkg.clone();
        let mut locs = tokio::task::spawn_blocking(move || {
            crate::indexer::rg_find_references(
                &name2,
                parent2.as_deref(),
                same2.as_deref(),
                decl2.as_deref(),
                root.as_deref(),
                include_decl,
                &uri_clone,
                &decl_files,
            )
        })
        .await
        .unwrap_or_default();
        eprintln!("[refs] rg returned {} locs", locs.len());

        // Supplement with in-memory scan of the CURRENT file only.
        // This catches unsaved content in the active buffer that rg cannot see on disk.
        // We intentionally do NOT scan all files in memory because that would bypass the
        // scoping logic (package / parent-class filtering) applied by rg_find_references.
        let cur_uri_str = uri.as_str();
        if let Some(data) = self.indexer.files.get(cur_uri_str) {
            let name_len = name.chars().count() as u32;
            for (line_idx, line) in data.lines.iter().enumerate() {
                let dup_line = locs.iter().any(|l: &Location| {
                    l.uri == *uri && l.range.start.line == line_idx as u32
                });
                if dup_line { continue; }
                let mut search = line.as_str();
                let mut byte_off = 0usize;
                while let Some(pos) = search.find(name.as_str()) {
                    let abs = byte_off + pos;
                    let before_ok = abs == 0 || {
                        let ch = line[..abs].chars().next_back().unwrap_or(' ');
                        !ch.is_alphanumeric() && ch != '_'
                    };
                    let after_ok = {
                        let end = abs + name.len();
                        end >= line.len() || {
                            let ch = line[end..].chars().next().unwrap_or(' ');
                            !ch.is_alphanumeric() && ch != '_'
                        }
                    };
                    if before_ok && after_ok {
                        let col = line[..abs].chars().count() as u32;
                        let range = Range::new(
                            Position::new(line_idx as u32, col),
                            Position::new(line_idx as u32, col + name_len),
                        );
                        let already = locs.iter().any(|l: &Location| {
                            l.uri == *uri && l.range.start == range.start
                        });
                        if !already {
                            locs.push(Location { uri: uri.clone(), range });
                        }
                    }
                    byte_off += pos + name.len().max(1);
                    search = &line[byte_off.min(line.len())..];
                }
            }
        }

        Ok(if locs.is_empty() { None } else { Some(locs) })
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = &params.text_document.uri;
        let mut symbols = self.indexer.file_symbols(uri);
        // Disk fallback: if not indexed yet, parse on-demand and index.
        if symbols.is_empty() {
            if let Ok(path) = uri.to_file_path() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    self.indexer.index_content(uri, &content);
                    symbols = self.indexer.file_symbols(uri);
                }
            }
        }
        if symbols.is_empty() {
            return Ok(None);
        }

        #[allow(deprecated)] // `deprecated` field superseded by `tags` in LSP 3.16+
        let doc_symbols = symbols
            .into_iter()
            .map(|s| DocumentSymbol {
                name:             s.name,
                detail:           if s.detail.is_empty() { None } else { Some(s.detail) },
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

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri   = &params.text_document.uri;
        let range = params.range;
        let hints = crate::inlay_hints::compute_inlay_hints(&self.indexer, uri, range);
        Ok(if hints.is_empty() { None } else { Some(hints) })
    }

    // ── workspace/symbol ────────────────────────────────────────────────────

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let query = params.query.to_lowercase();
        let mut results: Vec<SymbolInformation> = Vec::new();

        // For dot-qualified queries like "StoreState.isReady", split into
        // receiver qualifier and function name to match extension functions.
        let (query_qualifier, query_name) = if let Some(dot) = query.rfind('.') {
            (Some(&query[..dot]), &query[dot + 1..])
        } else {
            (None, query.as_str())
        };

        for entry in self.indexer.files.iter() {
            let uri_str = entry.key();
            let file_data = entry.value();
            let uri = match Url::parse(uri_str) {
                Ok(u) => u,
                Err(_) => match Url::from_file_path(uri_str) {
                    Ok(u) => u,
                    Err(_) => continue,
                },
            };
            for sym in &file_data.symbols {
                let name_lower = sym.name.to_lowercase();
                let matches = if query.is_empty() {
                    true
                } else if let Some(qualifier) = query_qualifier {
                    // Dot-qualified: name must match AND detail must contain
                    // the receiver type (e.g. "fun StoreState.isReady()")
                    name_lower.contains(query_name)
                        && sym.detail.to_lowercase().contains(qualifier)
                } else {
                    name_lower.contains(&query)
                };
                if !matches {
                    continue;
                }
                #[allow(deprecated)]
                results.push(SymbolInformation {
                    name:           sym.name.clone(),
                    kind:           sym.kind,
                    tags:           None,
                    deprecated:     None,
                    location:       Location {
                        uri:   uri.clone(),
                        range: sym.selection_range,
                    },
                    container_name: if sym.detail.is_empty() { None } else { Some(sym.detail.clone()) },
                });
                if results.len() >= 512 {
                    break;
                }
            }
            if results.len() >= 512 {
                break;
            }
        }

        results.sort_by(|a, b| a.name.cmp(&b.name));

        // rg fallback when index is empty (indexing in progress or cold start).
        if results.is_empty() && !query.is_empty() && query_qualifier.is_none() {
            let root_opt = { self.indexer.workspace_root.read().unwrap().clone() };
            let q = query.to_string();
            let rg_locs = tokio::task::spawn_blocking(move || {
                crate::indexer::rg_find_definition(&q, root_opt.as_deref())
            }).await.unwrap_or_default();
            if !rg_locs.is_empty() {
                let rg_syms: Vec<SymbolInformation> = rg_locs.into_iter().map(|loc| {
                    #[allow(deprecated)]
                    SymbolInformation {
                        name: query_name.to_string(),
                        kind: tower_lsp::lsp_types::SymbolKind::FILE,
                        tags: None,
                        deprecated: None,
                        location: loc,
                        container_name: Some("rg fallback".to_string()),
                    }
                }).collect();
                return Ok(Some(rg_syms));
            }
        }

        Ok(if results.is_empty() { None } else { Some(results) })
    }

    // ── textDocument/signatureHelp ───────────────────────────────────────────

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        // Use live_lines for the current line (updated synchronously on every
        // keystroke) so signatureHelp fires immediately when `(` is typed,
        // without waiting for the 120ms debounce that updates `files`.
        let lines_owned: Arc<Vec<String>>;
        let lines: &[String] = if let Some(ll) = self.indexer.live_lines.get(uri.as_str()) {
            lines_owned = ll.clone();
            &lines_owned
        } else if let Some(data) = self.indexer.files.get(uri.as_str()) {
            lines_owned = data.lines.clone();
            &lines_owned
        } else {
            return Ok(None);
        };

        let line_idx = pos.line as usize;
        if line_idx >= lines.len() {
            return Ok(None);
        }
        let line_text = &lines[line_idx];
        let col = (pos.character as usize).min(line_text.len());
        let before = &line_text[..col];

        // Count commas at the current paren depth to find active param.
        let mut depth: i32 = 0;
        let mut active_param: u32 = 0;
        let mut call_name: Option<String> = None;
        let mut call_qualifier: Option<String> = None; // receiver before the dot
        let chars: Vec<char> = before.chars().collect();
        let mut i = chars.len();
        while i > 0 {
            i -= 1;
            match chars[i] {
                ')' | ']' => { depth += 1; }
                '{' | '}' => {
                    // Brace means we've exited the current lambda/block scope —
                    // stop scanning to avoid finding an outer function's paren.
                    break;
                }
                '(' => {
                    if depth == 0 {
                        let mut j = i;
                        while j > 0 && (chars[j - 1].is_alphanumeric() || chars[j - 1] == '_') {
                            j -= 1;
                        }
                        let candidate: String = chars[j..i].iter().collect();
                        if !candidate.is_empty() && !is_non_call_keyword(&candidate) {
                            call_name = Some(candidate);
                            // Capture qualifier: the identifier before a `.` if present.
                            if j > 0 && chars[j - 1] == '.' {
                                let mut k = j - 1;
                                while k > 0 && (chars[k - 1].is_alphanumeric() || chars[k - 1] == '_') {
                                    k -= 1;
                                }
                                let q: String = chars[k..j - 1].iter().collect();
                                if !q.is_empty() {
                                    call_qualifier = Some(q);
                                }
                            }
                        }
                        break;
                    }
                    depth -= 1;
                }
                ',' if depth == 0 => { active_param += 1; }
                _ => {}
            }
        }

        // If not found on this line, try multiline scan (up to 10 lines up).
        // Only cross into a previous line if the current line doesn't contain a
        // closing brace (which would mean we're inside a block body, not an arg list).
        let in_block_body = before.contains('{') || before.contains('}')
            || lines[line_idx].trim_start().starts_with('}');
        if call_name.is_none() && line_idx > 0 && !in_block_body {
            let scan_start = line_idx.saturating_sub(10);
            'outer: for scan_line in (scan_start..line_idx).rev() {
                let l = &lines[scan_line];
                // Stop if we cross a closing brace — that means we entered a block body.
                if l.contains('{') || l.contains('}') {
                    break;
                }
                // Find the last `(` on this line.
                for (p, _) in l.char_indices().filter(|&(_, c)| c == '(').collect::<Vec<_>>().into_iter().rev() {
                    let before_paren = &l[..p];
                    let name: String = before_paren.chars()
                        .rev()
                        .take_while(|&c| c.is_alphanumeric() || c == '_')
                        .collect::<String>()
                        .chars().rev().collect();
                    if !name.is_empty() && !is_non_call_keyword(&name) {
                        // Make sure this `(` is unmatched (not closed on the same line).
                        let after_paren = &l[p..];
                        let net: i32 = after_paren.chars().map(|c| match c {
                            '(' => 1, ')' => -1, _ => 0,
                        }).sum();
                        if net > 0 {
                            call_name = Some(name);
                            for mid in (scan_line + 1)..=line_idx {
                                let mid_text = if mid == line_idx { before } else { lines[mid].as_str() };
                                active_param += mid_text.chars().filter(|&c| c == ',').count() as u32;
                            }
                            break 'outer;
                        }
                    }
                }
            }
        }

        let name = match call_name {
            Some(n) if !n.is_empty() => n,
            _ => return Ok(None),
        };

        let params_text = self.indexer.find_fun_signature_with_receiver(uri, &name, call_qualifier.as_deref());
        if params_text.is_empty() {
            return Ok(None);
        }

        let raw = params_text.trim_matches(|c| c == '(' || c == ')');
        let param_parts: Vec<&str> = raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();

        let parameters: Vec<ParameterInformation> = param_parts.iter().map(|p| {
            ParameterInformation {
                label: ParameterLabel::Simple(p.to_string()),
                documentation: None,
            }
        }).collect();

        let label = format!("{}({})", name, param_parts.join(", "));
        let active_param = active_param.min(parameters.len().saturating_sub(1) as u32);

        Ok(Some(SignatureHelp {
            signatures: vec![SignatureInformation {
                label,
                documentation: None,
                parameters: Some(parameters),
                active_parameter: Some(active_param),
            }],
            active_signature: Some(0),
            active_parameter: Some(active_param),
        }))
    }

    // ── textDocument/rename ──────────────────────────────────────────────────

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = &params.text_document.uri;
        let pos = params.position;

        let (word, range) = match self.indexer.word_and_range_at(uri, pos) {
            Some(wr) => wr,
            None => return Ok(None),
        };

        // Don't allow renaming keywords or single-char identifiers that are likely noise.
        if word.len() <= 1 || is_non_call_keyword(&word) {
            return Ok(None);
        }

        Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
            range,
            placeholder: word,
        }))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let new_name = &params.new_name;

        let name = match self.indexer.word_at(uri, pos) {
            Some(w) => w,
            None    => return Ok(None),
        };

        // ── Resolve scoping (same logic as `references`) ────────────────────
        let (parent_class, declared_pkg) = if name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            let on_decl = self.indexer.is_declared_in(uri, &name)
                && self.indexer.definitions.get(&name)
                    .map(|locs| locs.iter().any(|l| l.uri == *uri && l.range.start.line == pos.line))
                    .unwrap_or(false);
            if on_decl {
                let parent = self.indexer.enclosing_class_at(uri, pos.line);
                let pkg    = self.indexer.package_of(uri);
                (parent, pkg)
            } else {
                let (parent, pkg) = self.indexer.resolve_symbol_via_import(uri, &name);
                if parent.is_some() || pkg.is_some() {
                    (parent, pkg)
                } else {
                    let parent = self.indexer.declared_parent_class_of(&name, uri);
                    let pkg    = self.indexer.declared_package_of(&name);
                    (parent, pkg)
                }
            }
        } else {
            // Lowercase symbol — limit to enclosing scope in current file only.
            let lines = match self.indexer.lines_for(uri) {
                Some(l) => l,
                None    => return Ok(None),
            };
            let scope = enclosing_scope(&lines, pos.line as usize);
            let mut file_edits = rename_in_scope(&lines, &name, new_name, scope);
            if file_edits.is_empty() { return Ok(None); }
            file_edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
            let mut changes = std::collections::HashMap::new();
            changes.insert(uri.clone(), file_edits);
            return Ok(Some(WorkspaceEdit { changes: Some(changes), document_changes: None, change_annotations: None }));
        };
        let same_pkg = self.indexer.package_of(uri);

        let decl_files: Vec<String> = self.indexer.definitions.get(&name)
            .map(|locs| locs.iter()
                .filter(|l| {
                    if let Some(ref parent) = parent_class {
                        self.indexer.enclosing_class_at(&l.uri, l.range.start.line)
                            .as_deref() == Some(parent.as_str())
                    } else {
                        true
                    }
                })
                .filter_map(|l| l.uri.to_file_path().ok())
                .filter_map(|p| p.to_str().map(|s| s.to_owned()))
                .collect())
            .unwrap_or_default();

        // ── Find all reference locations (off-thread, same as references handler) ──
        let root = self.indexer.workspace_root.read().unwrap().clone();
        let uri_clone = uri.clone();
        let name2 = name.clone();
        let parent2 = parent_class.clone();
        let same2 = same_pkg.clone();
        let decl2 = declared_pkg.clone();
        // include_declaration=true so we also rename the declaration site
        let ref_locs = tokio::task::spawn_blocking(move || {
            crate::indexer::rg_find_references(
                &name2,
                parent2.as_deref(),
                same2.as_deref(),
                decl2.as_deref(),
                root.as_deref(),
                true,
                &uri_clone,
                &decl_files,
            )
        })
        .await
        .unwrap_or_default();

        if ref_locs.is_empty() { return Ok(None); }

        // ── Collect unique files that have references ───────────────────────
        // Always include current file (may have unsaved content rg can't see).
        let mut files: Vec<Url> = vec![uri.clone()];
        for loc in &ref_locs {
            if !files.contains(&loc.uri) {
                files.push(loc.uri.clone());
            }
        }
        eprintln!("[rename] rg found {} locs across {} files", ref_locs.len(), files.len());

        // ── Build TextEdits per file using rename_in_scope ──────────────────
        // We do NOT use rg location columns directly because Pass A uses a
        // qualified pattern (ParentClass.Name) so the match column points to
        // ParentClass, not Name. Instead we use rg_find_references only to
        // identify which files need editing, then do precise word replacement.
        let mut changes: std::collections::HashMap<Url, Vec<TextEdit>> =
            std::collections::HashMap::new();

        for file_uri in &files {
            // Prefer in-memory lines (open buffer with unsaved edits), then fall
            // back to reading from disk so we can rename closed files too.
            let mem_lines = self.indexer.lines_for(file_uri);
            let disk_lines: Vec<String>;
            let lines: &[String] = match mem_lines {
                Some(ref arc) => arc.as_slice(),
                None    => {
                    let path = match file_uri.to_file_path() {
                        Ok(p)  => p,
                        Err(_) => continue,
                    };
                    match std::fs::read_to_string(&path) {
                        Ok(content) => {
                            disk_lines = content.lines().map(|l| l.to_owned()).collect();
                            &disk_lines
                        }
                        Err(_) => continue,
                    }
                }
            };
            let lines = lines.to_vec();

            let scope = (0, lines.len().saturating_sub(1));
            let edits = rename_in_scope(&lines, &name, new_name, scope);

            if !edits.is_empty() {
                let mut edits = edits;
                edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
                changes.insert(file_uri.clone(), edits);
            }
        }

        if changes.is_empty() { return Ok(None); }
        Ok(Some(WorkspaceEdit { changes: Some(changes), document_changes: None, change_annotations: None }))
    }

    // ── textDocument/foldingRange ────────────────────────────────────────────

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
        let uri = &params.text_document.uri;
        let data = match self.indexer.files.get(uri.as_str()) {
            Some(d) => d,
            None    => return Ok(None),
        };

        let mut ranges: Vec<FoldingRange> = Vec::new();
        let lines = &data.lines;
        let mut stack: Vec<u32> = Vec::new();

        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            let opens  = trimmed.chars().filter(|&c| c == '{').count() as i32;
            let closes = trimmed.chars().filter(|&c| c == '}').count() as i32;
            let net = opens - closes;

            if net > 0 {
                for _ in 0..net {
                    stack.push(i as u32);
                }
            } else if net < 0 {
                for _ in 0..(-net) {
                    if let Some(start_line) = stack.pop() {
                        if i as u32 > start_line + 1 {
                            ranges.push(FoldingRange {
                                start_line,
                                end_line: i as u32,
                                start_character: None,
                                end_character:   None,
                                kind:            Some(FoldingRangeKind::Region),
                                collapsed_text:  None,
                            });
                        }
                    }
                }
            }
        }

        // Fold consecutive comment blocks (// lines).
        let mut comment_start: Option<u32> = None;
        for (i, line) in lines.iter().enumerate() {
            if line.trim().starts_with("//") {
                if comment_start.is_none() {
                    comment_start = Some(i as u32);
                }
            } else if let Some(cs) = comment_start.take() {
                if i as u32 > cs + 1 {
                    ranges.push(FoldingRange {
                        start_line: cs,
                        end_line:   (i as u32) - 1,
                        start_character: None,
                        end_character:   None,
                        kind:        Some(FoldingRangeKind::Comment),
                        collapsed_text: None,
                    });
                }
            }
        }

        Ok(if ranges.is_empty() { None } else { Some(ranges) })
    }

    // ── textDocument/codeAction ──────────────────────────────────────────────

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<Vec<CodeActionOrCommand>>> {
        let uri      = &params.text_document.uri;
        let range    = params.range;

        // Read the current line from live_lines (most up-to-date).
        let line_text: String = {
            let ln = range.start.line as usize;
            if let Some(ll) = self.indexer.live_lines.get(uri.as_str()) {
                ll.get(ln).cloned().unwrap_or_default()
            } else if let Some(data) = self.indexer.files.get(uri.as_str()) {
                data.lines.get(ln).cloned().unwrap_or_default()
            } else {
                String::new()
            }
        };

        let mut actions: Vec<CodeActionOrCommand> = Vec::new();

        let trimmed = line_text.trim();
        let is_import_line = trimmed.starts_with("import ") || trimmed.starts_with("package ");

        // ── 1. Introduce local variable ──────────────────────────────────────
        // Available when there is a non-empty selection on a single line,
        // but NOT on import/package lines.
        let sel_start = range.start;
        let sel_end   = range.end;
        let has_selection = sel_start != sel_end && sel_start.line == sel_end.line;

        if has_selection && !is_import_line {
            let chars: Vec<char> = line_text.chars().collect();
            let raw_s = (sel_start.character as usize).min(chars.len());
            let raw_e = (sel_end.character as usize).min(chars.len());

            // Expand the selection to capture the full dotted-call expression.
            // Helix often sends only the word under the cursor (e.g. `isRefreshing`)
            // even when the user wants the whole `receiver.isRefreshing()`.
            let (s, e) = expand_call_expr(&chars, raw_s, raw_e);
            let expr: String = chars[s..e].iter().collect();

            if !expr.trim().is_empty() {
                let var_name = derive_var_name(&expr);
                let indent: String = line_text.chars().take_while(|c| c.is_whitespace()).collect();

                // Single edit: replace entire line with two lines:
                //   1) val <name> = <expr>
                //   2) original line with <expr> substituted by <name>
                let prefix: String = chars[..s].iter().collect();
                let suffix: String = chars[e..].iter().collect();
                let replaced_line = format!("{prefix}{var_name}{suffix}");
                let line_char_count = chars.len() as u32;
                let new_text = format!("{indent}val {var_name} = {expr}\n{replaced_line}");

                let mut changes = std::collections::HashMap::new();
                changes.insert(uri.clone(), vec![TextEdit {
                    range: Range {
                        start: Position { line: sel_start.line, character: 0 },
                        end:   Position { line: sel_start.line, character: line_char_count },
                    },
                    new_text,
                }]);

                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Introduce local variable `{var_name}`"),
                    kind:  Some(CodeActionKind::REFACTOR_EXTRACT),
                    edit:  Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                    ..Default::default()
                }));
            }
        }

        // ── 2. Add import alias / rename in file ────────────────────────────

        // Read all lines once.
        let all_lines: Vec<String> = {
            if let Some(ll) = self.indexer.live_lines.get(uri.as_str()) {
                ll.clone().to_vec()
            } else if let Some(data) = self.indexer.files.get(uri.as_str()) {
                data.lines.to_vec()
            } else {
                vec![]
            }
        };

        // Word under cursor.
        let cursor_word: String = {
            let chars: Vec<char> = line_text.chars().collect();
            let col = (range.start.character as usize).min(chars.len());
            let mut ws = col;
            while ws > 0 && (chars[ws-1].is_alphanumeric() || chars[ws-1] == '_') { ws -= 1; }
            let mut we = col;
            while we < chars.len() && (chars[we].is_alphanumeric() || chars[we] == '_') { we += 1; }
            chars[ws..we].iter().collect()
        };

        // Case A: cursor on import line — append ` as <last_segment>`.
        if trimmed.starts_with("import ") && !trimmed.contains(" as ") {
            let path  = trimmed.trim_start_matches("import ").trim().trim_end_matches(".*");
            let alias = path.rsplit('.').next().unwrap_or(path);
            if !alias.is_empty() {
                let ln  = range.start.line;
                let col = line_text.chars().count() as u32;
                let mut changes = std::collections::HashMap::new();
                changes.insert(uri.clone(), vec![TextEdit {
                    range: Range {
                        start: Position { line: ln, character: col },
                        end:   Position { line: ln, character: col },
                    },
                    new_text: format!(" as {alias}"),
                }]);
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Add import alias `as {alias}`"),
                    kind:  Some(CodeActionKind::QUICKFIX),
                    edit:  Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                    ..Default::default()
                }));
            }
        }

        // Case B: cursor on a type name in code — offer two actions:
        //   B1. Add ` as <word>` to matching import (import line only, safe).
        //   B2. Replace all whole-word occurrences in this file with `_<word>`
        //       as a placeholder (single whole-file TextEdit, no crash risk).
        //       User then does  %s_Word<ret>cNewName<esc>  in Helix.
        if !is_import_line && !cursor_word.is_empty()
            && cursor_word.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
        {
            // Combined: add `as _Word` to matching import + rename Word→_Word in body (single action).
            if !all_lines.is_empty() {
                let placeholder = format!("_{cursor_word}");
                // Rename in non-import lines only (whole-file TextEdit).
                let new_content = whole_word_replace_file(&all_lines, &cursor_word, &placeholder);
                let last_line   = (all_lines.len() - 1) as u32;
                let last_col    = all_lines.last().map(|l| l.chars().count() as u32).unwrap_or(0);

                // Check if there's a matching import to also alias.
                let import_edit = all_lines.iter().enumerate()
                    .find(|(_, l)| {
                        let t = l.trim();
                        t.starts_with("import ") && !t.contains(" as ")
                        && t.rsplit(['.', ' ']).next().map(|s| s == cursor_word).unwrap_or(false)
                    })
                    .map(|(import_ln, import_line_text)| {
                        let col = import_line_text.chars().count() as u32;
                        TextEdit {
                            range: Range {
                                start: Position { line: import_ln as u32, character: col },
                                end:   Position { line: import_ln as u32, character: col },
                            },
                            new_text: format!(" as {placeholder}"),
                        }
                    });

                // Body rename replaces the whole file (skipping import lines).
                let mut body_edit = TextEdit {
                    range: Range {
                        start: Position { line: 0,        character: 0 },
                        end:   Position { line: last_line, character: last_col },
                    },
                    new_text: new_content,
                };

                // If we also have an import alias edit, we must embed it inside the body
                // content (since both edits touch the same file and LSP applies them
                // sequentially — easiest is to patch the already-replaced body content
                // at the import line position directly).
                if let Some(ie) = import_edit {
                    // Splice the alias into the body content at the right line.
                    let mut body_lines: Vec<&str> = body_edit.new_text.split('\n').collect();
                    let iln = ie.range.start.line as usize;
                    if iln < body_lines.len() {
                        let orig = body_lines[iln].to_owned();
                        let patched = format!("{orig}{}", ie.new_text);
                        body_lines[iln] = Box::leak(patched.into_boxed_str());
                    }
                    body_edit.new_text = body_lines.join("\n");
                }

                let title = if body_edit.new_text.contains(&placeholder) {
                    format!("Alias `{cursor_word}` as `{placeholder}` in file (then :%s/{placeholder}/NewName)")
                } else {
                    format!("Rename `{cursor_word}` → `{placeholder}` in file")
                };

                let mut changes = std::collections::HashMap::new();
                changes.insert(uri.clone(), vec![body_edit]);
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title,
                    kind:  Some(CodeActionKind::REFACTOR),
                    edit:  Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                    ..Default::default()
                }));
            }
        }

        Ok(if actions.is_empty() { None } else { Some(actions) })
    }
}

/// Replace all whole-word occurrences of `word` in `lines` with `replacement`.
/// Returns the full new file content as a single string (lines joined with `\n`).
fn whole_word_replace_file(lines: &[String], word: &str, replacement: &str) -> String {
    let pattern = format!(r"\b{word}\b");
    // Use simple char-by-char replacement to avoid regex dependency.
    let wchars: Vec<char> = word.chars().collect();
    let wlen = wchars.len();
    let mut result = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 { result.push('\n'); }
        let trimmed = line.trim_start();
        if trimmed.starts_with("import ") || trimmed.starts_with("package ") {
            result.push_str(line);
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut j = 0usize;
        while j < chars.len() {
            // Check whole-word match at position j.
            if chars[j..].starts_with(&wchars) {
                let before_ok = j == 0 || !(chars[j-1].is_alphanumeric() || chars[j-1] == '_');
                let end = j + wlen;
                let after_ok  = end >= chars.len() || !(chars[end].is_alphanumeric() || chars[end] == '_');
                if before_ok && after_ok {
                    result.push_str(replacement);
                    j = end;
                    continue;
                }
            }
            result.push(chars[j]);
            j += 1;
        }
    }
    // Drop unused pattern variable.
    let _ = pattern;
    result
}


/// expression around it — e.g. `isRefreshing` → `refreshDashboardInteractor.isRefreshing()`.
///
/// - Expands LEFT:  eats `[a-zA-Z0-9_.]` (dotted receiver chain)
/// - Expands RIGHT: eats remaining identifier chars, then a balanced `(…)` if present
fn expand_call_expr(chars: &[char], s: usize, e: usize) -> (usize, usize) {
    // Expand left over [a-zA-Z0-9_.]
    let mut new_s = s;
    while new_s > 0 {
        let c = chars[new_s - 1];
        if c.is_alphanumeric() || c == '_' || c == '.' { new_s -= 1; } else { break; }
    }
    // Strip leading dots we may have swallowed.
    while new_s < e && chars[new_s] == '.' { new_s += 1; }

    // Expand right over remaining identifier chars.
    let mut new_e = e;
    while new_e < chars.len() {
        let c = chars[new_e];
        if c.is_alphanumeric() || c == '_' { new_e += 1; } else { break; }
    }
    // Eat balanced `(…)` if present.
    if new_e < chars.len() && chars[new_e] == '(' {
        let mut depth = 0usize;
        while new_e < chars.len() {
            match chars[new_e] {
                '(' => { depth += 1; new_e += 1; }
                ')' => { new_e += 1; depth -= 1; if depth == 0 { break; } }
                _   => { new_e += 1; }
            }
        }
    }
    (new_s, new_e)
}

/// Derive a local variable name from an expression.
///
/// `refreshDashboardInteractor.isRefreshing()` → `isRefreshing`
/// `user.getName()` → `name`  (strips "get" prefix)
/// `someValue` → `someValue`
fn derive_var_name(expr: &str) -> String {
    // Take the last `.`-separated segment, strip trailing `()` / `(…)`.
    let seg = expr.trim().rsplit('.').next().unwrap_or(expr.trim());
    let seg = if let Some(p) = seg.find('(') { &seg[..p] } else { seg };
    let seg = seg.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');

    // Strip common accessor prefixes: getXxx → xxx, isXxx → isXxx (keep),
    // hasXxx → hasXxx (keep), setXxx → skip (nothing useful).
    let result = if seg.starts_with("get") && seg.len() > 3 {
        let rest = &seg[3..];
        // Only strip if next char is uppercase (proper camelCase).
        if rest.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            let mut r = rest.to_string();
            if let Some(first) = r.get_mut(0..1) {
                first.make_ascii_lowercase();
            }
            r
        } else {
            seg.to_string()
        }
    } else {
        seg.to_string()
    };

    if result.is_empty() { "value".to_string() } else { result }
}


/// a function call — i.e. we should NOT show signature help for it.
fn is_non_call_keyword(name: &str) -> bool {
    matches!(name,
        "fun" | "if" | "while" | "for" | "when" | "catch" | "constructor"
        | "override" | "else" | "return" | "throw" | "try" | "finally"
        | "object" | "class" | "interface" | "enum" | "init"
    )
}

/// Find the line range of the innermost function/lambda scope enclosing `cursor_line`.
/// Returns `(start_line, end_line)` inclusive, or the whole file if not found.
fn enclosing_scope(lines: &[String], cursor_line: usize) -> (usize, usize) {
    // Walk backwards to find the opening `{` of the enclosing fun/lambda.
    let mut depth = 0i32;
    let mut scope_start = 0usize;
    'outer: for i in (0..=cursor_line.min(lines.len().saturating_sub(1))).rev() {
        for ch in lines[i].chars().rev() {
            match ch {
                '}' => depth += 1,
                '{' => {
                    if depth == 0 {
                        scope_start = i;
                        break 'outer;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
    }
    // Walk forward from scope_start to find matching `}`.
    let mut depth = 0i32;
    let mut scope_end = lines.len().saturating_sub(1);
    for i in scope_start..lines.len() {
        for ch in lines[i].chars() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        scope_end = i;
                        // break both loops
                        return (scope_start, scope_end);
                    }
                }
                _ => {}
            }
        }
    }
    (scope_start, scope_end)
}

/// Return TextEdits replacing all whole-word occurrences of `word` with `new_name`
/// within `lines[scope.0..=scope.1]`, in reverse order (safe for sequential apply).
fn rename_in_scope(
    lines: &[String],
    word: &str,
    new_name: &str,
    scope: (usize, usize),
) -> Vec<TextEdit> {
    let wchars: Vec<char> = word.chars().collect();
    let wlen = wchars.len();
    if wlen == 0 { return vec![]; }
    let mut edits: Vec<TextEdit> = Vec::new();

    for ln in scope.0..=scope.1.min(lines.len().saturating_sub(1)) {
        // Skip package declaration — never rename the package statement.
        let trimmed = lines[ln].trim_start();
        if trimmed.starts_with("package ") {
            continue;
        }
        let chars: Vec<char> = lines[ln].chars().collect();
        let mut j = 0usize;
        let char_to_utf16: Vec<u32> = {
            let mut v = Vec::with_capacity(chars.len() + 1);
            let mut acc = 0u32;
            for &c in &chars {
                v.push(acc);
                acc += c.len_utf16() as u32;
            }
            v.push(acc); // sentinel
            v
        };

        while j < chars.len() {
            if chars[j..].starts_with(&wchars) {
                let before_ok = j == 0 || !(chars[j-1].is_alphanumeric() || chars[j-1] == '_');
                let end_idx = j + wlen;
                let after_ok = end_idx >= chars.len()
                    || !(chars[end_idx].is_alphanumeric() || chars[end_idx] == '_');
                if before_ok && after_ok {
                    let start_utf16 = char_to_utf16[j];
                    let end_utf16   = char_to_utf16[end_idx];
                    edits.push(TextEdit {
                        range: Range {
                            start: Position::new(ln as u32, start_utf16),
                            end:   Position::new(ln as u32, end_utf16),
                        },
                        new_text: new_name.to_owned(),
                    });
                    j = end_idx;
                    continue;
                }
            }
            j += 1;
        }
    }

    // Reverse so callers applying sequentially won't shift earlier positions.
    edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
    edits
}

// ─── Diagnostics helper ──────────────────────────────────────────────────────

use crate::types::SyntaxError;

fn syntax_diagnostics(errors: &[SyntaxError]) -> Vec<Diagnostic> {
    errors.iter().map(|e| Diagnostic {
        range:    e.range,
        severity: Some(DiagnosticSeverity::ERROR),
        source:   Some("kotlin-lsp".into()),
        message:  e.message.clone(),
        ..Default::default()
    }).collect()
}
