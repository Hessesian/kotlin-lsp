//! Document symbols feature — maps indexed `SymbolEntry` to LSP `DocumentSymbol`.

use tower_lsp::lsp_types::{DocumentSymbol, DocumentSymbolResponse};

use crate::types::SymbolEntry;

/// Build the LSP document symbols response from a pre-fetched symbol list.
///
/// Returns `None` when `symbols` is empty.
/// On-demand indexing (disk fallback) and symbol fetching are handled by the
/// backend adapter before this function is called.
pub(crate) fn compute_document_symbols(
    symbols: Vec<SymbolEntry>,
) -> Option<DocumentSymbolResponse> {
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
