//! Document symbols feature — maps indexed `SymbolEntry` to LSP `DocumentSymbol`.

use tower_lsp::lsp_types::{DocumentSymbol, DocumentSymbolResponse, Url};

use crate::types::SymbolEntry;

use super::traits::SymbolIndex;

/// Build the LSP document symbols response for `uri`.
///
/// Returns `None` when no symbols are indexed for the file.
/// On-demand indexing (disk fallback) is handled by the backend adapter before
/// this function is called.
pub(crate) fn compute_document_symbols(
    uri: &Url,
    index: &impl SymbolIndex,
) -> Option<DocumentSymbolResponse> {
    let symbols = index.file_symbols(uri);
    if symbols.is_empty() {
        return None;
    }
    let doc_symbols = symbols.into_iter().map(symbol_to_document_symbol).collect();
    Some(DocumentSymbolResponse::Nested(doc_symbols))
}

#[allow(deprecated)] // `deprecated` field superseded by `tags` in LSP 3.16+
fn symbol_to_document_symbol(s: SymbolEntry) -> DocumentSymbol {
    DocumentSymbol {
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
    }
}
