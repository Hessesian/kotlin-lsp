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
                references_provider:     Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                inlay_hint_provider: Some(OneOf::Left(true)),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: None,
                    file_operations: None,
                }),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
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

        // Register a file-system watcher so we get notified when *.kt / *.java
        // files change on disk (e.g. after a workspace/rename edit is applied to
        // closed files that never send didChange).
        let _ = self.client.register_capability(vec![
            Registration {
                id:     "watched-kotlin-files".into(),
                method: "workspace/didChangeWatchedFiles".into(),
                register_options: Some(
                    serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                        watchers: vec![
                            FileSystemWatcher {
                                glob_pattern: GlobPattern::String("**/*.kt".into()),
                                kind: None,
                            },
                            FileSystemWatcher {
                                glob_pattern: GlobPattern::String("**/*.java".into()),
                                kind: None,
                            },
                        ],
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
        Ok(Some(CompletionResponse::Array(items)))
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
                let lang = if uri.path().ends_with(".kt") { "kotlin" } else { "java" };
                // Show the inferred binding: `val it: Product` or `val item: Product`
                let sig_md = format!("```{lang}\nval {word}: {type_name}\n```");
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
            self.indexer.hover_info(&word)
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

        // If the symbol is a class-like name (Uppercase), attempt to find its
        // enclosing class so we can scope the search and avoid matching
        // identically-named symbols in other sealed hierarchies.
        let parent_class: Option<String> = if name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            self.indexer.enclosing_class_at(uri, pos.line)
        } else {
            None
        };
        let same_pkg = self.indexer.package_of(uri);

        let root = self.indexer.workspace_root.get().map(std::path::PathBuf::as_path);
        let mut locs = crate::indexer::rg_find_references(
            &name,
            parent_class.as_deref(),
            same_pkg.as_deref(),
            root,
            include_decl,
            uri,
        );

        // Supplement with in-memory scan of all open/indexed files.
        // This catches unsaved buffers (e.g. right after a rename where the file
        // has the new name in the editor but hasn't been written to disk yet).
        let mem_locs = self.indexer.in_memory_references(&name);
        for loc in mem_locs {
            // Skip if rg already found a match at the same file+line.
            let dup = locs.iter().any(|l: &Location| {
                l.uri == loc.uri && l.range.start.line == loc.range.start.line
            });
            if dup { continue; }
            if !include_decl {
                // Exclude declaration lines (any line containing a def keyword + name).
                // Use a simple heuristic: skip if the source line looks like a declaration.
                if let Some(data) = self.indexer.files.get(loc.uri.as_str()) {
                    let line_idx = loc.range.start.line as usize;
                    if let Some(line) = data.lines.get(line_idx) {
                        let decl_kws = ["class ", "interface ", "object ", "fun ", "val ", "var ",
                                        "typealias ", "enum class "];
                        if decl_kws.iter().any(|kw| line.contains(kw)) {
                            continue;
                        }
                    }
                }
            }
            locs.push(loc);
        }

        Ok(if locs.is_empty() { None } else { Some(locs) })
    }

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
                let matches = query.is_empty()
                    || sym.name.to_lowercase().contains(&query);
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
                    container_name: None,
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
        let lines_owned: Vec<String>;
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

        let params_text = self.indexer.collect_fun_params_text(uri, &name);
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

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let new_name = &params.new_name;

        let name = match self.indexer.word_at(uri, pos) {
            Some(w) => w,
            None    => return Ok(None),
        };

        let root = self.indexer.workspace_root.get().map(std::path::PathBuf::as_path);
        let locs = crate::indexer::rg_find_references(
            &name,
            None,
            None,
            root,
            true, // include declaration
            uri,
        );

        if locs.is_empty() {
            return Ok(None);
        }

        let name_len = name.chars().count() as u32;
        let mut changes: std::collections::HashMap<Url, Vec<TextEdit>> = std::collections::HashMap::new();
        for loc in locs {
            // rg gives point ranges (start==end); expand to cover the full word.
            let start = loc.range.start;
            let end   = Position::new(start.line, start.character + name_len);
            let edit  = TextEdit {
                range:    Range::new(start, end),
                new_text: new_name.clone(),
            };
            changes.entry(loc.uri).or_default().push(edit);
        }

        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }))
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
}

/// Returns true if `name` is a Kotlin/Java keyword that uses `()` but is NOT
/// a function call — i.e. we should NOT show signature help for it.
fn is_non_call_keyword(name: &str) -> bool {
    matches!(name,
        "fun" | "if" | "while" | "for" | "when" | "catch" | "constructor"
        | "override" | "else" | "return" | "throw" | "try" | "finally"
        | "object" | "class" | "interface" | "enum" | "init"
    )
}
