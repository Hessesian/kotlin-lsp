use super::cursor::CursorContext;
use super::Backend;
use crate::inlay_hints::compute_inlay_hints;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

/// Maximum number of workspace symbol results to return.
const WORKSPACE_SYMBOL_CAP: usize = 512;

impl Backend {
    pub(super) async fn hover_impl(&self, params: HoverParams) -> Result<Option<Hover>> {
        let pp = params.text_document_position_params;
        let uri = &pp.text_document.uri;
        let position = pp.position;
        let workspace = self.indexer.as_ref();

        let Some(ctx) = CursorContext::build(&self.indexer, uri, position) else {
            return Ok(None);
        };

        Ok(crate::features::hover::compute_hover(
            workspace, &ctx, uri, position,
        ))
    }

    pub(super) async fn references_impl(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let Some((name, _)) = self.indexer.word_and_qualifier_at(uri, position) else {
            return Ok(None);
        };

        let locations = crate::features::references::find_references(
            &name,
            uri,
            position.line,
            params.context.include_declaration,
            &*self.indexer,
        )
        .await;

        Ok((!locations.is_empty()).then_some(locations))
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
        self.indexer.for_each_indexed_file(|uri_str, data| {
            let Some(uri) = workspace_symbol_uri(uri_str) else {
                return true;
            };
            collect_matching_workspace_symbols(&uri, &data.symbols, query, &mut results);
            results.len() < WORKSPACE_SYMBOL_CAP
        });
        results.sort_by(|left, right| left.name.cmp(&right.name));
        results
    }

    async fn rg_workspace_symbols(&self, query: &WorkspaceSymbolQuery) -> Vec<SymbolInformation> {
        if !query.allows_rg_fallback() {
            return vec![];
        }
        let (workspace_root, ignore_matcher) = self.rg_context().await;
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
        Ok(crate::features::signature_help::compute_signature_help(
            uri,
            pos,
            self.indexer.as_ref(),
        ))
    }

    pub(super) async fn folding_range_impl(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
        let uri = &params.text_document.uri;
        Ok(crate::features::folding::compute_folding_ranges(
            uri,
            self.indexer.as_ref(),
        ))
    }

    // ── textDocument/documentHighlight ───────────────────────────────────────

    pub(super) async fn document_highlight_impl(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        Ok(crate::features::highlight::compute_document_highlight(
            uri,
            pos,
            self.indexer.as_ref(),
        ))
    }
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

#[cfg(test)]
#[path = "handlers_tests.rs"]
mod tests;
