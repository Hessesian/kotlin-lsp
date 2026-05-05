use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use crate::indexer::find_fun_signature_with_receiver;
use crate::indexer::NodeExt;
use crate::StrExt;
use crate::queries::KIND_VALUE_ARG;
use crate::inlay_hints::compute_inlay_hints;
use super::Backend;
use super::cursor::CursorContext;
use super::helpers::resolve_references_scope;
use super::actions::is_non_call_keyword;

/// Maximum number of workspace symbol results to return.
const WORKSPACE_SYMBOL_CAP: usize = 512;

impl Backend {
    pub(super) async fn hover_impl(&self, params: HoverParams) -> Result<Option<Hover>> {
        let pp       = params.text_document_position_params;
        let uri      = &pp.text_document.uri;
        let position = pp.position;

        let Some(ctx) = CursorContext::build(&self.indexer, uri, position) else {
            return Ok(None);
        };

        // For `it` or a named lambda param, generate hover showing the inferred type.
        if ctx.qualifier.is_none() {
            if let Some(ref rt) = ctx.contextual {
                // Apply type parameter substitution (same as inlay hints)
                let subst = self.indexer.type_subst_for_enclosing_class(uri.as_str(), position.line);
                let type_name = if subst.is_empty() {
                    rt.raw.clone()
                } else {
                    crate::indexer::apply_type_subst(&rt.raw, &subst)
                };
                let lang = match crate::Language::from_path(uri.path()) {
                    crate::Language::Kotlin => "kotlin",
                    crate::Language::Swift  => "swift",
                    crate::Language::Java   => "java",
                };
                let kw = if crate::Language::from_path(uri.path()).is_swift() { "let" } else { "val" };
                let sig_md = format!("```{lang}\n{kw} {}: {type_name}\n```", ctx.word);
                // Resolve the type using the same path as go-to-definition (import-aware)
                let leaf = type_name.rsplit('.').next().unwrap_or(type_name.as_str());
                let locs = self.indexer.find_definition_qualified(leaf, None, uri);
                let type_hover = if let Some(loc) = locs.first() {
                    self.indexer.hover_info_at_location(loc, leaf, Some(uri.as_str()), Some(position.line))
                } else {
                    self.indexer.hover_info(leaf, Some(uri.as_str()))
                };
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
            // Lambda parameter with failed type inference — don't fall through to rg
            // lookup (would show confusing hover for unrelated symbols).
            if ctx.lambda_decl.is_some() {
                return Ok(None);
            }
        }

        // Use the same resolution chain as go-to-definition so hover always
        // points at the same symbol (import-aware, not just first index match).

        // `this.field` / `it.field` — use the already-resolved contextual receiver
        // so hover shows the member from the correct class.
        if ctx.qualifier.is_some() {
            if let Some(ref rt) = ctx.contextual {
                let locs = self.resolve_with_receiver_fallback(&ctx.word, rt, uri);
                if let Some(loc) = locs.first() {
                    if let Some(md) = self.indexer.hover_info_at_location(loc, &ctx.word, Some(uri.as_str()), Some(position.line)) {
                        return Ok(Some(Hover {
                            contents: HoverContents::Markup(MarkupContent {
                                kind:  MarkupKind::Markdown,
                                value: md,
                            }),
                            range: None,
                        }));
                    }
                }
            }
        }

        let locs = self.indexer.find_definition_qualified(&ctx.word, ctx.qualifier.as_deref(), uri);
        let hover_md = if let Some(loc) = locs.first() {
            self.indexer.hover_info_at_location(loc, &ctx.word, Some(uri.as_str()), Some(position.line))
        } else {
            // Index lookup — works for already-indexed symbols + stdlib.
            let from_index = self.indexer.hover_info(&ctx.word, Some(uri.as_str()));
            if from_index.is_some() {
                from_index
            } else {
                // rg fallback: find the declaration even when the index is empty.
                let file_path = uri.to_file_path().ok();
                let (rg_root, matcher) = {
                    let wr = self.indexer.workspace_root.read().unwrap().clone();
                    let m = self.indexer.ignore_matcher.read().unwrap().clone();
                    (crate::rg::effective_rg_root(wr.as_deref(), file_path.as_deref()), m)
                };
                let rg_locs = crate::rg::rg_find_definition(&ctx.word, rg_root.as_deref(), matcher.as_deref());
                rg_locs.first().and_then(|loc| self.indexer.hover_info_at_location(loc, &ctx.word, Some(uri.as_str()), Some(position.line)))
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

    pub(super) async fn references_impl(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let uri  = &params.text_document_position.text_document.uri;
        let pos  = params.text_document_position.position;
        let include_decl = params.context.include_declaration;

        let (name, _qualifier) = match self.indexer.word_and_qualifier_at(uri, pos) {
            Some(pair) => pair,
            None       => return Ok(None),
        };

        // For uppercase symbols, determine parent_class and declared_pkg:
        // - If cursor is ON the declaration of this symbol → use enclosing_class_at(cursor)
        // - If cursor is on a REFERENCE → scan imports in current file to find which
        //   specific class is meant (handles multiple `Effect` classes across files)
        let (parent_class, declared_pkg) = resolve_references_scope(
            &self.indexer, uri, pos.line, &name,
        );
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
        let matcher = self.indexer.ignore_matcher.read().unwrap().clone();
        let uri_clone = uri.clone();
        let name2 = name.clone();
        let parent2 = parent_class.clone();
        let decl2 = declared_pkg.clone();
        let mut locs = tokio::task::spawn_blocking(move || {
            crate::rg::rg_find_references(
                &name2,
                parent2.as_deref(),
                decl2.as_deref(),
                root.as_deref(),
                include_decl,
                &uri_clone,
                &decl_files,
                matcher.as_deref(),
            )
        })
        .await
        .unwrap_or_default();
        eprintln!("[refs] rg returned {} locs", locs.len());

        // Filter out library-source locations (from sourcePaths outside workspace root).
        let lib = &self.indexer.library_uris;
        if !lib.is_empty() {
            locs.retain(|loc| !lib.contains(loc.uri.as_str()));
        }

        // Supplement with in-memory scan of the CURRENT file only.
        // This catches unsaved content in the active buffer that rg cannot see on disk.
        // We intentionally do NOT scan all files in memory because that would bypass the
        // scoping logic (package / parent-class filtering) applied by rg_find_references.
        let cur_uri_str = uri.as_str();
        if let Some(data) = self.indexer.files.get(cur_uri_str) {
            for (line_idx, line) in data.lines.iter().enumerate() {
                let dup_line = locs.iter().any(|l: &Location| {
                    l.uri == *uri && l.range.start.line == line_idx as u32
                });
                if dup_line { continue; }
                for abs in word_byte_offsets(line, name.as_str()) {
                    // Compute UTF-16 column (LSP units) for the match start.
                    let col: u32 = line[..abs].chars().map(|c| c.len_utf16() as u32).sum();
                    let col_end: u32 = col + name.chars().map(|c| c.len_utf16() as u32).sum::<u32>();
                    let range = Range::new(
                        Position::new(line_idx as u32, col),
                        Position::new(line_idx as u32, col_end),
                    );
                    if !locs.iter().any(|l: &Location| l.uri == *uri && l.range.start == range.start) {
                        locs.push(Location { uri: uri.clone(), range });
                    }
                }
            }
        }

        Ok(if locs.is_empty() { None } else { Some(locs) })
    }

    pub(super) async fn document_symbol_impl(
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

    pub(super) async fn inlay_hint_impl(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        let uri   = &params.text_document.uri;
        let range = params.range;
        let hints = compute_inlay_hints(&self.indexer, uri, range);
        Ok(if hints.is_empty() { None } else { Some(hints) })
    }

    pub(super) async fn symbol_impl(
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
                if results.len() >= WORKSPACE_SYMBOL_CAP {
                    break;
                }
            }
            if results.len() >= WORKSPACE_SYMBOL_CAP {
                break;
            }
        }

        results.sort_by(|a, b| a.name.cmp(&b.name));

        // rg fallback when index is empty (indexing in progress or cold start).
        if results.is_empty() && !query.is_empty() && query_qualifier.is_none() {
            let root_opt = self.indexer.workspace_root.read().unwrap().clone();
            let matcher = self.indexer.ignore_matcher.read().unwrap().clone();
            let q = query.to_string();
            let rg_locs = tokio::task::spawn_blocking(move || {
                crate::rg::rg_find_definition(&q, root_opt.as_deref(), matcher.as_deref())
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

    pub(super) async fn signature_help_impl(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        // Use live_lines for the current line (updated synchronously on every
        // keystroke) so signatureHelp fires immediately when `(` is typed,
        // without waiting for the 120ms debounce that updates `files`.
        let Some(lines_owned) = self.indexer.mem_lines_for(uri.as_str()) else {
            return Ok(None);
        };
        let lines: &[String] = &lines_owned;

        let line_idx = pos.line as usize;
        if line_idx >= lines.len() {
            return Ok(None);
        }
        let line_text = &lines[line_idx];
        // pos.character is UTF-16 units — convert to a byte offset.
        let col = crate::indexer::live_tree::utf16_col_to_byte(line_text, pos.character as usize);
        let before = &line_text[..col];

        // Extract (fn_name, qualifier, active_param) — CST first, text fallback.
        let Some((name, qualifier, active_param)) =
            extract_call_info(pos, &self.indexer, uri, lines, before, line_idx)
        else {
            return Ok(None);
        };

        let params_text = find_fun_signature_with_receiver(
            &self.indexer, uri, &name, qualifier.as_deref(),
        );
        if params_text.is_empty() {
            return Ok(None);
        }

        Ok(build_signature_help(&name, &params_text, active_param))
    }

    pub(super) async fn folding_range_impl(
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

    // ── textDocument/documentHighlight ───────────────────────────────────────

    pub(super) async fn document_highlight_impl(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        use tower_lsp::lsp_types::{DocumentHighlight, DocumentHighlightKind};

        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let Some((name, _)) = self.indexer.word_and_qualifier_at(uri, pos) else {
            return Ok(None);
        };

        // Collect definition line numbers in this file so we can mark them
        // as Write highlights; all other occurrences are Read.
        let decl_lines: std::collections::HashSet<u32> = self
            .indexer
            .definitions
            .get(&name)
            .map(|locs| {
                locs.iter()
                    .filter(|l| l.uri == *uri)
                    .map(|l| l.range.start.line)
                    .collect()
            })
            .unwrap_or_default();

        let data = match self.indexer.files.get(uri.as_str()) {
            Some(d) => d,
            None    => return Ok(None),
        };

        let mut highlights = Vec::new();
        for (line_idx, line) in data.lines.iter().enumerate() {
            for abs in word_byte_offsets(line, &name) {
                let col: u32 = line[..abs].chars().map(|c| c.len_utf16() as u32).sum();
                let col_end: u32 = col + name.chars().map(|c| c.len_utf16() as u32).sum::<u32>();
                let range = Range::new(
                    Position::new(line_idx as u32, col),
                    Position::new(line_idx as u32, col_end),
                );
                let kind = if decl_lines.contains(&(line_idx as u32)) {
                    DocumentHighlightKind::WRITE
                } else {
                    DocumentHighlightKind::READ
                };
                highlights.push(DocumentHighlight { range, kind: Some(kind) });
            }
        }

        Ok(if highlights.is_empty() { None } else { Some(highlights) })
    }
}

// ─── Private helpers for signature_help_impl ─────────────────────────────────

/// Build a `SignatureHelp` response from pre-computed parts.
fn build_signature_help(fn_name: &str, params_text: &str, active_param: u32) -> Option<SignatureHelp> {
    let raw = params_text.trim_matches(|c| c == '(' || c == ')');
    let param_parts: Vec<&str> = raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    let parameters: Vec<ParameterInformation> = param_parts.iter().map(|p| {
        ParameterInformation {
            label: ParameterLabel::Simple(p.to_string()),
            documentation: None,
        }
    }).collect();
    let label = format!("{}({})", fn_name, param_parts.join(", "));
    let active_param = active_param.min(parameters.len().saturating_sub(1) as u32);
    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation: None,
            parameters: Some(parameters),
            active_parameter: Some(active_param),
        }],
        active_signature: Some(0),
        active_parameter: Some(active_param),
    })
}

/// Extract `(fn_name, qualifier, active_param)` for the call under the cursor.
///
/// Tries the CST (live tree) first — O(depth), accurate qualifier extraction.
/// Falls back to a text scan when no live tree is available, when the cursor
/// is inside a lambda literal, or when the callee shape is not recognised.
fn extract_call_info(
    pos:      Position,
    indexer:  &crate::indexer::Indexer,
    uri:      &Url,
    lines:    &[String],
    before:   &str,
    line_idx: usize,
) -> Option<(String, Option<String>, u32)> {
    // ── CST path ─────────────────────────────────────────────────────────────
    if let Some(result) = cst_call_info(pos, indexer, uri) {
        return Some(result);
    }

    // ── Text path ────────────────────────────────────────────────────────────
    text_call_info(lines, before, line_idx)
}

/// CST path: walk from cursor up to `call_expression`, extract name/qualifier/param.
///
/// Returns `None` when:
/// - no live tree available
/// - cursor is inside a `lambda_literal`
/// - callee shape not recognised (`simple_identifier` / `navigation_expression`)
fn cst_call_info(
    pos:     Position,
    indexer: &crate::indexer::Indexer,
    uri:     &Url,
) -> Option<(String, Option<String>, u32)> {
    use tree_sitter::Point;
    use crate::indexer::live_tree::utf16_col_to_byte;

    let doc = indexer.live_doc(uri)?;
    let bytes = &doc.bytes;
    let full_text = std::str::from_utf8(bytes).ok()?;

    let line_idx = pos.line as usize;
    let line_text = full_text.lines().nth(line_idx)?;
    let byte_col = utf16_col_to_byte(line_text, pos.character as usize);
    let point = Point { row: line_idx, column: byte_col };
    let start_node = doc.tree.root_node().descendant_for_point_range(point, point)?;

    // Walk up: find call_expression; bail out if we cross into a lambda literal.
    let mut cur = start_node;
    let call_expr = loop {
        match cur.kind() {
            "call_expression" => break Some(cur),
            "lambda_literal" => break None,
            _ => match cur.parent() {
                Some(p) => cur = p,
                None => break None,
            }
        }
    }?;

    // Extract function name and optional qualifier from the callee.
    let (fn_name, qualifier) = call_expr.call_fn_and_qualifier(bytes)?;

    // Find the value_arguments node (may be inside call_suffix).
    let value_arguments = call_expr.find_value_arguments()?;

    // Count active param: how many value_argument children end before the cursor.
    let cursor_byte = full_text.lines()
        .take(line_idx)
        .map(|l| l.len() + 1) // +1 for the newline
        .sum::<usize>() + byte_col;
    let active_param = {
        let mut count = 0u32;
        let mut walker = value_arguments.walk();
        for child in value_arguments.children(&mut walker) {
            if child.kind() == KIND_VALUE_ARG {
                if child.end_byte() <= cursor_byte { count += 1; } else { break; }
            }
        }
        count
    };

    Some((fn_name, qualifier, active_param))
}

/// Scans a single source line for an unclosed call-site opening.
/// Returns `(call_name, qualifier)` if an unbalanced `name(` is found,
/// where net > 0 means more opens than closes on this line.
fn find_call_open_on_line(line: &str) -> Option<(String, Option<String>)> {
    for (p, _) in line.char_indices().filter(|&(_, c)| c == '(')
        .collect::<Vec<_>>().into_iter().rev()
    {
        let before_paren = &line[..p];
        let name = before_paren.last_ident_in();
        if !name.is_empty() && !is_non_call_keyword(name) {
            let net: i32 = line[p..].chars()
                .map(|c| match c { '(' => 1, ')' => -1, _ => 0 }).sum();
            if net > 0 {
                // Qualifier before the dot on the same line.
                let before_name = &before_paren[..before_paren.len() - name.len()];
                let qualifier = if before_name.ends_with('.') {
                    let q = before_name.strip_suffix('.').unwrap_or(before_name).last_ident_in();
                    if q.is_empty() { None } else { Some(q.to_owned()) }
                } else { None };
                return Some((name.to_owned(), qualifier));
            }
        }
    }
    None
}

/// Scans up to `MAX_SCAN_BACK_LINES` lines before `line_idx` for an unclosed `fn(` call site.
/// Returns `(call_name, qualifier, extra_commas)` where `extra_commas` counts commas on the
/// intermediate lines only (between the opening line and `line_idx`, exclusive). Commas on
/// `line_idx` itself (in `before`) are already counted by the caller.
/// Maximum number of lines to scan backward when looking for a multi-line call opener.
const MAX_SCAN_BACK_LINES: usize = 10;

fn scan_multiline_call_open(
    lines: &[String],
    line_idx: usize,
) -> Option<(String, Option<String>, u32)> {
    let scan_start = line_idx.saturating_sub(MAX_SCAN_BACK_LINES);
    for scan_line in (scan_start..line_idx).rev() {
        let l = &lines[scan_line];
        if l.contains('{') || l.contains('}') { break; }
        if let Some((name, qualifier)) = find_call_open_on_line(l) {
            let mut extra: u32 = 0;
            if scan_line + 1 < line_idx {
                for mid in &lines[(scan_line + 1)..line_idx] {
                    extra += mid.chars().filter(|&c| c == ',').count() as u32;
                }
            }
            return Some((name, qualifier, extra));
        }
    }
    None
}

/// Given `chars` and position `j` (start of the identifier), extract
/// the qualifier immediately before a `.` if present.
fn extract_dot_qualifier(chars: &[char], j: usize) -> Option<String> {
    if j > 0 && chars[j - 1] == '.' {
        let mut k = j - 1;
        while k > 0 && (chars[k - 1].is_alphanumeric() || chars[k - 1] == '_') {
            k -= 1;
        }
        let q: String = chars[k..j - 1].iter().collect();
        if !q.is_empty() { Some(q) } else { None }
    } else {
        None
    }
}

/// Text-scan fallback: extract `(fn_name, qualifier, active_param)` by walking
/// backwards through `before` (and up to 10 previous lines for multiline calls).
fn text_call_info(
    lines:    &[String],
    before:   &str,
    line_idx: usize,
) -> Option<(String, Option<String>, u32)> {
    let mut depth: i32 = 0;
    let mut active_param: u32 = 0;
    let mut call_name: Option<String> = None;
    let mut call_qualifier: Option<String> = None;

    let chars: Vec<char> = before.chars().collect();
    let mut i = chars.len();
    while i > 0 {
        i -= 1;
        match chars[i] {
            ')' | ']' => { depth += 1; }
            '{' | '}' => { break; }
            '(' => {
                if depth == 0 {
                    let mut j = i;
                    while j > 0 && (chars[j - 1].is_alphanumeric() || chars[j - 1] == '_') {
                        j -= 1;
                    }
                    let candidate: String = chars[j..i].iter().collect();
                    if !candidate.is_empty() && !is_non_call_keyword(&candidate) {
                        call_name = Some(candidate);
                        call_qualifier = extract_dot_qualifier(&chars, j);
                    }
                    break;
                }
                depth -= 1;
            }
            ',' if depth == 0 => { active_param += 1; }
            _ => {}
        }
    }

    // Multiline: scan up to 10 lines back when the call opens on a previous line.
    let in_block_body = before.contains('{') || before.contains('}')
        || lines[line_idx].trim_start().starts_with('}');
    if call_name.is_none() && line_idx > 0 && !in_block_body {
        if let Some((name, qual, extra)) = scan_multiline_call_open(lines, line_idx) {
            call_name = Some(name);
            call_qualifier = qual;
            active_param += extra;
        }
    }

    let name = call_name.filter(|n| !n.is_empty())?;
    Some((name, call_qualifier, active_param))
}

/// Iterator over the byte offsets in `line` where `word` occurs as a whole
/// word (not as a substring of a longer identifier).
fn word_byte_offsets<'a>(line: &'a str, word: &'a str) -> impl Iterator<Item = usize> + 'a {
    let word_len = word.len();
    let is_id = |c: char| c.is_alphanumeric() || c == '_';
    let mut search_from = 0;
    std::iter::from_fn(move || {
        while let Some(rel) = line[search_from..].find(word) {
            let pos = search_from + rel;
            search_from = pos + word_len;
            let before_ok = pos == 0 || !is_id(line[..pos].chars().next_back()?);
            let after_ok = pos + word_len >= line.len()
                || !is_id(line[pos + word_len..].chars().next()?);
            if before_ok && after_ok { return Some(pos); }
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::word_byte_offsets;

    #[test]
    fn finds_single_word() {
        let offsets: Vec<_> = word_byte_offsets("hello world", "world").collect();
        assert_eq!(offsets, vec![6]);
    }

    #[test]
    fn skips_partial_match() {
        // "name" should not match inside "rename"
        let offsets: Vec<_> = word_byte_offsets("rename name", "name").collect();
        assert_eq!(offsets, vec![7]);
    }

    #[test]
    fn multiple_occurrences() {
        let offsets: Vec<_> = word_byte_offsets("a b a c a", "a").collect();
        assert_eq!(offsets, vec![0, 4, 8]);
    }

    #[test]
    fn unicode_line() {
        // "ñ" is 2 bytes in UTF-8; "name" after it still at correct byte offset
        let line = "ñ name ñ";
        let offsets: Vec<_> = word_byte_offsets(line, "name").collect();
        assert_eq!(offsets.len(), 1);
        assert_eq!(&line[offsets[0]..offsets[0] + 4], "name");
    }

    #[test]
    fn no_match() {
        let offsets: Vec<_> = word_byte_offsets("foo bar", "baz").collect();
        assert!(offsets.is_empty());
    }
}
