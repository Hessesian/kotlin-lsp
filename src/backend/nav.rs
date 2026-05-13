use super::cursor::CursorContext;
use super::Backend;
use crate::features::definition as def;
use crate::features::implementation as imp;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
impl Backend {
    pub(super) async fn goto_definition_impl(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pp = params.text_document_position_params;
        let uri = &pp.text_document.uri;
        let position = pp.position;

        let Some(ctx) = CursorContext::build(&self.indexer, uri, position) else {
            return Ok(None);
        };

        Ok(def::find_definition(&ctx, &*self.indexer, uri, position).await)
    }

    pub(super) async fn goto_implementation_impl(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pp = params.text_document_position_params;
        let uri = &pp.text_document.uri;
        let position = pp.position;

        let Some((word, _qualifier)) = self.indexer.word_and_qualifier_at(uri, position) else {
            return Ok(None);
        };

        Ok(imp::find_implementation(&word, &*self.indexer, uri).await)
    }
}
