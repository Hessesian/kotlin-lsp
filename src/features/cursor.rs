//! `RawCursor` — cheap cursor data struct, built once per LSP request.
//!
//! Holds the word + qualifier at the cursor position.  Heavy enrichment
//! (receiver type inference, lambda scope) stays inside individual feature
//! functions that need it — it does not belong here.

use tower_lsp::lsp_types::{Position, Url};

use super::traits::DocumentAccess;

/// Cursor position and identifier data for identifier-based LSP features.
#[allow(dead_code)]
pub(crate) struct RawCursor {
    /// The identifier token under the cursor.
    pub word: String,
    /// The dot-qualifier to the left of `word`, if any (e.g. `"viewModel"`).
    pub qualifier: Option<String>,
    /// The LSP cursor position.
    pub position: Position,
    /// The document URI.
    pub uri: Url,
}

#[allow(dead_code)]
impl RawCursor {
    /// Build a cursor for `uri` at `pos` using `DocumentAccess`.
    ///
    /// Returns `None` when there is no identifier under the cursor
    /// (e.g. cursor is in whitespace or on a non-identifier token).
    pub(crate) fn build(doc: &impl DocumentAccess, uri: &Url, pos: Position) -> Option<Self> {
        let (word, qualifier) = doc.word_and_qualifier_at(uri, pos)?;
        Some(Self {
            word,
            qualifier,
            position: pos,
            uri: uri.clone(),
        })
    }
}
