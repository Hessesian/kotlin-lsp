use super::actions::is_non_call_keyword;
use super::cursor::CursorContext;
use super::format::{format_contextual_hover, format_symbol_hover};
use super::helpers::resolve_references_scope;
use super::Backend;
use crate::indexer::apply_type_subst;
use crate::indexer::find_fun_signature_with_receiver;
use crate::indexer::resolution::{
    enrich_at_location, resolve_symbol_info, ResolveOptions, SubstitutionContext,
};
use crate::indexer::NodeExt;
use crate::inlay_hints::compute_inlay_hints;
use crate::queries::{KIND_CALL_EXPR, KIND_LAMBDA_LIT, KIND_VALUE_ARG};
use crate::StrExt;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

/// Maximum number of workspace symbol results to return.
const WORKSPACE_SYMBOL_CAP: usize = 512;

impl Backend {
    pub(super) async fn hover_impl(&self, params: HoverParams) -> Result<Option<Hover>> {
        let pp = params.text_document_position_params;
        let uri = &pp.text_document.uri;
        let position = pp.position;

        let Some(ctx) = CursorContext::build(&self.indexer, uri, position) else {
            return Ok(None);
        };

        if let Some(hover) = self.contextual_lambda_hover(&ctx, uri, position) {
            return Ok(Some(hover));
        }
        if ctx.qualifier.is_none() && ctx.lambda_decl.is_some() {
            return Ok(None);
        }
        if let Some(hover) = self.contextual_receiver_hover(&ctx, uri, position) {
            return Ok(Some(hover));
        }

        Ok(self.regular_symbol_hover(&ctx, uri, position))
    }

    fn contextual_lambda_hover(
        &self,
        ctx: &CursorContext,
        uri: &Url,
        position: Position,
    ) -> Option<Hover> {
        if ctx.qualifier.is_some() {
            return None;
        }
        let receiver_type = ctx.contextual.as_ref()?;
        let type_name = self.contextual_hover_type_name(receiver_type, uri, position.line);
        let leaf = type_name.rsplit('.').next().unwrap_or(type_name.as_str());
        let signature = format!("{} {}: {type_name}", hover_binding_keyword(uri), ctx.word);
        let detail = self
            .resolve_hover_markdown(leaf, None, uri, position.line)
            .or_else(|| crate::stdlib::hover(leaf));
        Some(make_markdown_hover(format_contextual_hover(
            &signature,
            uri.path(),
            detail.as_deref(),
        )))
    }

    fn contextual_hover_type_name(
        &self,
        receiver_type: &crate::resolver::ReceiverType,
        uri: &Url,
        line: u32,
    ) -> String {
        let subst =
            crate::indexer::resolution::build_subst_map(self.indexer.as_ref(), uri.as_str(), line);
        if subst.is_empty() {
            return receiver_type.raw.clone();
        }
        apply_type_subst(&receiver_type.raw, &subst)
    }

    fn contextual_receiver_hover(
        &self,
        ctx: &CursorContext,
        uri: &Url,
        position: Position,
    ) -> Option<Hover> {
        let receiver_type = ctx.contextual.as_ref()?;
        ctx.qualifier.as_ref()?;
        let location = self
            .resolve_with_receiver_fallback(&ctx.word, receiver_type, uri)
            .first()?
            .clone();
        let info = enrich_at_location(
            self.indexer.as_ref(),
            &location,
            &ctx.word,
            hover_substitution_context(uri, position.line),
            &ResolveOptions::hover(),
        )?;
        Some(make_markdown_hover(format_symbol_hover(&info, uri.path())))
    }

    fn regular_symbol_hover(
        &self,
        ctx: &CursorContext,
        uri: &Url,
        position: Position,
    ) -> Option<Hover> {
        let markdown = self
            .resolve_hover_markdown(&ctx.word, ctx.qualifier.as_deref(), uri, position.line)
            .or_else(|| crate::stdlib::hover(&ctx.word))?;
        Some(make_markdown_hover(markdown))
    }

    fn resolve_hover_markdown(
        &self,
        word: &str,
        qualifier: Option<&str>,
        uri: &Url,
        line: u32,
    ) -> Option<String> {
        resolve_symbol_info(
            self.indexer.as_ref(),
            word,
            qualifier,
            uri,
            hover_substitution_context(uri, line),
            &ResolveOptions::hover(),
        )
        .map(|info| format_symbol_hover(&info, uri.path()))
    }

    pub(super) async fn references_impl(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let Some(search) = self.reference_search(&params) else {
            return Ok(None);
        };

        let mut locations = self.rg_reference_locations(&search).await;
        self.filter_library_reference_locations(&mut locations);
        self.add_current_file_reference_locations(&search.uri, &search.name, &mut locations);

        Ok((!locations.is_empty()).then_some(locations))
    }

    fn reference_search(&self, params: &ReferenceParams) -> Option<ReferenceSearch> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let include_decl = params.context.include_declaration;
        let (name, _) = self.indexer.word_and_qualifier_at(uri, position)?;
        let (parent_class, declared_pkg) =
            resolve_references_scope(&self.indexer, uri, position.line, &name);
        let decl_files = self.declaration_files_for_reference(&name, parent_class.as_deref());
        Some(ReferenceSearch {
            uri: uri.clone(),
            name,
            include_decl,
            parent_class,
            declared_pkg,
            decl_files,
        })
    }

    fn declaration_files_for_reference(
        &self,
        name: &str,
        parent_class: Option<&str>,
    ) -> Vec<String> {
        self.indexer
            .definitions
            .get(name)
            .map(|locations| {
                locations
                    .iter()
                    .filter(|location| {
                        reference_matches_parent_class(&self.indexer, location, parent_class)
                    })
                    .filter_map(|location| location.uri.to_file_path().ok())
                    .filter_map(|path| path.to_str().map(|path| path.to_owned()))
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn rg_reference_locations(&self, search: &ReferenceSearch) -> Vec<Location> {
        let workspace_root = self
            .indexer
            .workspace_root
            .read()
            .ok()
            .and_then(|root| root.clone());
        let ignore_matcher = self
            .indexer
            .ignore_matcher
            .read()
            .ok()
            .and_then(|matcher| matcher.clone());
        let request = search.clone();
        tokio::task::spawn_blocking(move || {
            let rg_request = crate::rg::RgSearchRequest::new(
                &request.name,
                request.parent_class.as_deref(),
                request.declared_pkg.as_deref(),
                workspace_root.as_deref(),
                request.include_decl,
                &request.uri,
                &request.decl_files,
            );
            crate::rg::rg_find_references(&rg_request, ignore_matcher.as_deref())
        })
        .await
        .unwrap_or_default()
    }

    fn filter_library_reference_locations(&self, locations: &mut Vec<Location>) {
        if self.indexer.library_uris.is_empty() {
            return;
        }
        locations.retain(|location| !self.indexer.library_uris.contains(location.uri.as_str()));
    }

    fn add_current_file_reference_locations(
        &self,
        uri: &Url,
        name: &str,
        locations: &mut Vec<Location>,
    ) {
        let Some(data) = self.indexer.files.get(uri.as_str()) else {
            return;
        };
        for (line_idx, line) in data.lines.iter().enumerate() {
            let line_number = line_idx as u32;
            if has_reference_line(locations, uri, line_number) {
                continue;
            }
            append_line_reference_locations(uri, name, line_number, line, locations);
        }
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
                name: s.name,
                detail: if s.detail.is_empty() {
                    None
                } else {
                    Some(s.detail)
                },
                kind: s.kind,
                tags: None,
                deprecated: None,
                range: s.range,
                selection_range: s.selection_range,
                children: None,
            })
            .collect();

        Ok(Some(DocumentSymbolResponse::Nested(doc_symbols)))
    }

    pub(super) async fn inlay_hint_impl(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        let uri = &params.text_document.uri;
        let range = params.range;
        let hints = compute_inlay_hints(&self.indexer, uri, range);
        Ok(if hints.is_empty() { None } else { Some(hints) })
    }

    pub(super) async fn symbol_impl(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let query = WorkspaceSymbolQuery::new(params.query);
        let mut results = self.collect_workspace_symbols(&query);
        if results.is_empty() {
            results = self.rg_workspace_symbols(&query).await;
        }
        Ok((!results.is_empty()).then_some(results))
    }

    fn collect_workspace_symbols(&self, query: &WorkspaceSymbolQuery) -> Vec<SymbolInformation> {
        let mut results = Vec::new();
        for entry in self.indexer.files.iter() {
            let Some(uri) = workspace_symbol_uri(entry.key()) else {
                continue;
            };
            collect_matching_workspace_symbols(&uri, &entry.value().symbols, query, &mut results);
            if results.len() >= WORKSPACE_SYMBOL_CAP {
                break;
            }
        }
        results.sort_by(|left, right| left.name.cmp(&right.name));
        results
    }

    async fn rg_workspace_symbols(&self, query: &WorkspaceSymbolQuery) -> Vec<SymbolInformation> {
        if !query.allows_rg_fallback() {
            return vec![];
        }
        let workspace_root = self
            .indexer
            .workspace_root
            .read()
            .ok()
            .and_then(|root| root.clone());
        let ignore_matcher = self
            .indexer
            .ignore_matcher
            .read()
            .ok()
            .and_then(|matcher| matcher.clone());
        let query_text = query.raw.clone();
        let rg_locations = tokio::task::spawn_blocking(move || {
            crate::rg::rg_find_definition(
                &query_text,
                workspace_root.as_deref(),
                ignore_matcher.as_deref(),
            )
        })
        .await
        .unwrap_or_default();
        rg_locations
            .into_iter()
            .map(|location| rg_workspace_symbol(query.name.clone(), location))
            .collect()
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

        // Extract CallInfo — CST first, text fallback.
        let Some(ci) = extract_call_info(pos, &self.indexer, uri, lines, before, line_idx) else {
            return Ok(None);
        };

        let params_text = find_fun_signature_with_receiver(
            &self.indexer,
            uri,
            &ci.fn_name,
            ci.qualifier.as_deref(),
        );
        if params_text.is_empty() {
            return Ok(None);
        }

        Ok(build_signature_help(
            &ci.fn_name,
            &params_text,
            ci.active_param,
        ))
    }

    pub(super) async fn folding_range_impl(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
        let uri = &params.text_document.uri;
        let data = match self.indexer.files.get(uri.as_str()) {
            Some(d) => d,
            None => return Ok(None),
        };

        let mut ranges: Vec<FoldingRange> = Vec::new();
        let lines = &data.lines;
        let mut stack: Vec<u32> = Vec::new();

        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            let opens = trimmed.chars().filter(|&c| c == '{').count() as i32;
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
                                end_character: None,
                                kind: Some(FoldingRangeKind::Region),
                                collapsed_text: None,
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
                        end_line: (i as u32) - 1,
                        start_character: None,
                        end_character: None,
                        kind: Some(FoldingRangeKind::Comment),
                        collapsed_text: None,
                    });
                }
            }
        }

        Ok(if ranges.is_empty() {
            None
        } else {
            Some(ranges)
        })
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
            None => return Ok(None),
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
                highlights.push(DocumentHighlight {
                    range,
                    kind: Some(kind),
                });
            }
        }

        Ok(if highlights.is_empty() {
            None
        } else {
            Some(highlights)
        })
    }
}

// ─── Private helpers for signature_help_impl ─────────────────────────────────

/// Build a `SignatureHelp` response from pre-computed parts.
fn build_signature_help(
    fn_name: &str,
    params_text: &str,
    active_param: u32,
) -> Option<SignatureHelp> {
    let raw = params_text.trim_matches(|c| c == '(' || c == ')');
    let param_parts: Vec<&str> = raw
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let parameters: Vec<ParameterInformation> = param_parts
        .iter()
        .map(|p| ParameterInformation {
            label: ParameterLabel::Simple(p.to_string()),
            documentation: None,
        })
        .collect();
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

/// Resolved call-site information needed for `textDocument/signatureHelp`.
struct CallInfo {
    /// Name of the function being called.
    fn_name: String,
    /// Optional receiver before the dot (e.g. `"builder"` in `builder.build()`).
    qualifier: Option<String>,
    /// Zero-based index of the parameter the cursor is currently inside.
    active_param: u32,
}

/// Extract [`CallInfo`] for the call under the cursor.
///
/// Tries the CST (live tree) first — O(depth), accurate qualifier extraction.
/// Falls back to a text scan when no live tree is available, when the cursor
/// is inside a lambda literal, or when the callee shape is not recognised.
fn extract_call_info(
    pos: Position,
    indexer: &crate::indexer::Indexer,
    uri: &Url,
    lines: &[String],
    before: &str,
    line_idx: usize,
) -> Option<CallInfo> {
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
fn cst_call_info(pos: Position, indexer: &crate::indexer::Indexer, uri: &Url) -> Option<CallInfo> {
    use crate::indexer::live_tree::utf16_col_to_byte;
    use tree_sitter::Point;

    let doc = indexer.live_doc(uri)?;
    let bytes = &doc.bytes;
    let full_text = std::str::from_utf8(bytes).ok()?;

    let line_idx = pos.line as usize;
    let line_text = full_text.lines().nth(line_idx)?;
    let byte_col = utf16_col_to_byte(line_text, pos.character as usize);
    let point = Point {
        row: line_idx,
        column: byte_col,
    };
    let start_node = doc
        .tree
        .root_node()
        .descendant_for_point_range(point, point)?;

    // Walk up: find call_expression; bail out if we cross into a lambda literal.
    let mut cur = start_node;
    let call_expr = loop {
        match cur.kind() {
            KIND_CALL_EXPR => break Some(cur),
            KIND_LAMBDA_LIT => break None,
            _ => match cur.parent() {
                Some(p) => cur = p,
                None => break None,
            },
        }
    }?;

    // Extract function name and optional qualifier from the callee.
    let (fn_name, qualifier) = call_expr.call_fn_and_qualifier(bytes)?;

    // Find the value_arguments node (may be inside call_suffix).
    let value_arguments = call_expr.find_value_arguments()?;

    // Count active param: how many value_argument children end before the cursor.
    let cursor_byte = full_text
        .lines()
        .take(line_idx)
        .map(|l| l.len() + 1) // +1 for the newline
        .sum::<usize>()
        + byte_col;
    let active_param = {
        let mut count = 0u32;
        let mut walker = value_arguments.walk();
        for child in value_arguments.children(&mut walker) {
            if child.kind() == KIND_VALUE_ARG {
                if child.end_byte() <= cursor_byte {
                    count += 1;
                } else {
                    break;
                }
            }
        }
        count
    };

    Some(CallInfo {
        fn_name,
        qualifier,
        active_param,
    })
}

/// Scans a single source line for an unclosed call-site opening.
/// Returns `(call_name, qualifier)` if an unbalanced `name(` is found,
/// where net > 0 means more opens than closes on this line.
fn find_call_open_on_line(line: &str) -> Option<(String, Option<String>)> {
    for (p, _) in line
        .char_indices()
        .filter(|&(_, c)| c == '(')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let before_paren = &line[..p];
        let name = before_paren.last_ident_in();
        if !name.is_empty() && !is_non_call_keyword(name) {
            let net: i32 = line[p..]
                .chars()
                .map(|c| match c {
                    '(' => 1,
                    ')' => -1,
                    _ => 0,
                })
                .sum();
            if net > 0 {
                // Qualifier before the dot on the same line.
                let before_name = &before_paren[..before_paren.len() - name.len()];
                let qualifier = if before_name.ends_with('.') {
                    let q = before_name
                        .strip_suffix('.')
                        .unwrap_or(before_name)
                        .last_ident_in();
                    if q.is_empty() {
                        None
                    } else {
                        Some(q.to_owned())
                    }
                } else {
                    None
                };
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
        if l.contains('{') || l.contains('}') {
            break;
        }
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
        if !q.is_empty() {
            Some(q)
        } else {
            None
        }
    } else {
        None
    }
}

/// Text-scan fallback: extract `(fn_name, qualifier, active_param)` by walking
/// backwards through `before` (and up to 10 previous lines for multiline calls).
fn text_call_info(lines: &[String], before: &str, line_idx: usize) -> Option<CallInfo> {
    let mut depth: i32 = 0;
    let mut active_param: u32 = 0;
    let mut call_name: Option<String> = None;
    let mut call_qualifier: Option<String> = None;

    let chars: Vec<char> = before.chars().collect();
    let mut i = chars.len();
    while i > 0 {
        i -= 1;
        match chars[i] {
            ')' | ']' => {
                depth += 1;
            }
            '{' | '}' => {
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
                        call_qualifier = extract_dot_qualifier(&chars, j);
                    }
                    break;
                }
                depth -= 1;
            }
            ',' if depth == 0 => {
                active_param += 1;
            }
            _ => {}
        }
    }

    // Multiline: scan up to 10 lines back when the call opens on a previous line.
    let in_block_body = before.contains('{')
        || before.contains('}')
        || lines[line_idx].trim_start().starts_with('}');
    if call_name.is_none() && line_idx > 0 && !in_block_body {
        if let Some((name, qual, extra)) = scan_multiline_call_open(lines, line_idx) {
            call_name = Some(name);
            call_qualifier = qual;
            active_param += extra;
        }
    }

    let fn_name = call_name.filter(|n| !n.is_empty())?;
    Some(CallInfo {
        fn_name,
        qualifier: call_qualifier,
        active_param,
    })
}

#[derive(Clone)]
struct WorkspaceSymbolQuery {
    raw: String,
    qualifier: Option<String>,
    name: String,
}

impl WorkspaceSymbolQuery {
    fn new(query: String) -> Self {
        let raw = query.to_lowercase();
        if let Some(dot) = raw.rfind('.') {
            return Self {
                qualifier: Some(raw[..dot].to_owned()),
                name: raw[dot + 1..].to_owned(),
                raw,
            };
        }
        Self {
            name: raw.clone(),
            raw,
            qualifier: None,
        }
    }

    fn matches(&self, symbol: &crate::types::SymbolEntry) -> bool {
        if self.raw.is_empty() {
            return true;
        }
        let name = symbol.name.to_lowercase();
        if let Some(qualifier) = self.qualifier.as_deref() {
            return name.contains(&self.name) && symbol.detail.to_lowercase().contains(qualifier);
        }
        name.contains(&self.raw)
    }

    fn allows_rg_fallback(&self) -> bool {
        !self.raw.is_empty() && self.qualifier.is_none()
    }
}

fn workspace_symbol_uri(uri_str: &str) -> Option<Url> {
    Url::parse(uri_str)
        .ok()
        .or_else(|| Url::from_file_path(uri_str).ok())
}

fn collect_matching_workspace_symbols(
    uri: &Url,
    symbols: &[crate::types::SymbolEntry],
    query: &WorkspaceSymbolQuery,
    results: &mut Vec<SymbolInformation>,
) {
    for symbol in symbols {
        if !query.matches(symbol) {
            continue;
        }
        results.push(workspace_symbol_information(uri, symbol));
        if results.len() >= WORKSPACE_SYMBOL_CAP {
            break;
        }
    }
}

fn workspace_symbol_information(
    uri: &Url,
    symbol: &crate::types::SymbolEntry,
) -> SymbolInformation {
    #[allow(deprecated)]
    SymbolInformation {
        name: symbol.name.clone(),
        kind: symbol.kind,
        tags: None,
        deprecated: None,
        location: Location {
            uri: uri.clone(),
            range: symbol.selection_range,
        },
        container_name: (!symbol.detail.is_empty()).then(|| symbol.detail.clone()),
    }
}

fn rg_workspace_symbol(name: String, location: Location) -> SymbolInformation {
    #[allow(deprecated)]
    SymbolInformation {
        name,
        kind: tower_lsp::lsp_types::SymbolKind::FILE,
        tags: None,
        deprecated: None,
        location,
        container_name: Some("rg fallback".to_string()),
    }
}

#[derive(Clone)]
struct ReferenceSearch {
    uri: Url,
    name: String,
    include_decl: bool,
    parent_class: Option<String>,
    declared_pkg: Option<String>,
    decl_files: Vec<String>,
}

fn reference_matches_parent_class(
    indexer: &crate::indexer::Indexer,
    location: &Location,
    parent_class: Option<&str>,
) -> bool {
    let Some(parent_class) = parent_class else {
        return true;
    };
    indexer
        .enclosing_class_at(&location.uri, location.range.start.line)
        .as_deref()
        == Some(parent_class)
}

fn has_reference_line(locations: &[Location], uri: &Url, line_number: u32) -> bool {
    locations
        .iter()
        .any(|location| location.uri == *uri && location.range.start.line == line_number)
}

fn append_line_reference_locations(
    uri: &Url,
    name: &str,
    line_number: u32,
    line: &str,
    locations: &mut Vec<Location>,
) {
    for location in line_reference_locations(uri, name, line_number, line) {
        if has_reference_start(locations, &location) {
            continue;
        }
        locations.push(location);
    }
}

fn line_reference_locations(uri: &Url, name: &str, line_number: u32, line: &str) -> Vec<Location> {
    word_byte_offsets(line, name)
        .map(|offset| reference_location(uri, name, line_number, line, offset))
        .collect()
}

fn reference_location(
    uri: &Url,
    name: &str,
    line_number: u32,
    line: &str,
    offset: usize,
) -> Location {
    let start = utf16_column(&line[..offset]);
    let end = start + utf16_column(name);
    Location {
        uri: uri.clone(),
        range: Range::new(
            Position::new(line_number, start),
            Position::new(line_number, end),
        ),
    }
}

fn utf16_column(text: &str) -> u32 {
    text.chars().map(|ch| ch.len_utf16() as u32).sum()
}

fn has_reference_start(locations: &[Location], candidate: &Location) -> bool {
    locations.iter().any(|location| {
        location.uri == candidate.uri && location.range.start == candidate.range.start
    })
}

fn hover_binding_keyword(uri: &Url) -> &'static str {
    crate::Language::from_path(uri.path()).val_keyword()
}

fn hover_substitution_context(uri: &Url, line: u32) -> SubstitutionContext<'_> {
    SubstitutionContext::CrossFile {
        calling_uri: uri.as_str(),
        cursor_line: Some(line),
    }
}

fn make_markdown_hover(markdown: String) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: None,
    }
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
            let after_ok =
                pos + word_len >= line.len() || !is_id(line[pos + word_len..].chars().next()?);
            if before_ok && after_ok {
                return Some(pos);
            }
        }
        None
    })
}

#[cfg(test)]
#[path = "handlers_tests.rs"]
mod tests;
