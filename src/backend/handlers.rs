use std::sync::Arc;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use crate::indexer::find_fun_signature_with_receiver;
use super::Backend;
use super::helpers::resolve_references_scope;
use super::actions::is_non_call_keyword;

impl Backend {
    pub(super) async fn hover_impl(&self, params: HoverParams) -> Result<Option<Hover>> {
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

        // `this.field` / `it.field` — resolve to the actual receiver/lambda type
        // so hover shows the member from the correct class (mirrors goto_definition fix).
        if let Some(qual) = qualifier.as_deref() {
            if qual == "this" || qual == "it" {
                if let Some(type_name) = self.indexer.infer_lambda_param_type_at(qual, uri, position) {
                    // Try full qualified name first (e.g. `Outer.Inner`), then short segment.
                    let locs = self.indexer.find_definition_qualified(&word, Some(&type_name), uri);
                    let locs = if locs.is_empty() {
                        let short = type_name.rsplit('.').next().unwrap_or(&type_name);
                        if short != type_name {
                            self.indexer.find_definition_qualified(&word, Some(short), uri)
                        } else { locs }
                    } else { locs };
                    if let Some(loc) = locs.first() {
                        if let Some(md) = self.indexer.hover_info_at_location(loc, &word) {
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
        }

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
                let file_path = uri.to_file_path().ok();
                let (rg_root, matcher) = {
                    let wr = self.indexer.workspace_root.read().unwrap().clone();
                    let m = self.indexer.ignore_matcher.read().unwrap().clone();
                    (crate::rg::effective_rg_root(wr.as_deref(), file_path.as_deref()), m)
                };
                let rg_locs = crate::rg::rg_find_definition(&word, rg_root.as_deref(), matcher.as_deref());
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
        let hints = crate::inlay_hints::compute_inlay_hints(&self.indexer, uri, range);
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

        let params_text = find_fun_signature_with_receiver(&self.indexer, uri, &name, call_qualifier.as_deref());
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
}
