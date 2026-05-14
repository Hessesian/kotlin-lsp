use super::Backend;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

#[cfg(test)]
pub(crate) use crate::features::rename::{
    any_local_var_decl_in_scope, cst_cursor_is_method, enclosing_scope, rename_in_scope,
};
#[cfg(test)]
pub(crate) use crate::indexer::cst_cursor_is_local_var;

impl Backend {
    pub(super) async fn prepare_rename_impl(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = &params.text_document.uri;
        crate::features::rename::prepare_rename_impl(&self.indexer, uri, params.position).await
    }

    pub(super) async fn rename_impl(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let position = params.text_document_position.position;
        let uri = &params.text_document_position.text_document.uri;
        crate::features::rename::rename_impl(&self.indexer, uri, position, &params.new_name).await
    }
}

#[cfg(test)]
#[path = "rename_tests.rs"]
mod tests;
