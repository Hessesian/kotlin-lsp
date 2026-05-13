use super::cursor::CursorContext;
use super::Backend;
use crate::inlay_hints::compute_inlay_hints;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

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
        if symbols.is_empty() {
            if let Ok(path) = uri.to_file_path() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    self.indexer.index_content(uri, &content);
                    symbols = self.indexer.file_symbols(uri);
                }
            }
        }
        Ok(crate::features::symbols::compute_document_symbols(symbols))
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
        let results = crate::features::workspace_symbols::compute_workspace_symbols(
            params.query,
            self.indexer.as_ref(),
        )
        .await;
        Ok((!results.is_empty()).then_some(results))
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

#[cfg(test)]
#[path = "handlers_tests.rs"]
mod tests;
