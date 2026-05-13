//! Document highlight feature — marks same-symbol occurrences in a file.

use tower_lsp::lsp_types::{DocumentHighlight, DocumentHighlightKind, Position, Range, Url};

use super::text_utils::{utf16_column, word_byte_offsets};
use super::traits::{DocumentAccess, SymbolIndex};

/// Compute all highlight ranges for the symbol under `pos` in `uri`.
///
/// Definition sites are marked as `Write`; all other occurrences as `Read`.
/// Returns `None` when the cursor is not on a word or the file has no lines.
pub(crate) fn compute_document_highlight(
    uri: &Url,
    pos: tower_lsp::lsp_types::Position,
    index: &(impl SymbolIndex + DocumentAccess),
) -> Option<Vec<DocumentHighlight>> {
    let (name, _) = index.word_and_qualifier_at(uri, pos)?;

    let decl_lines: std::collections::HashSet<u32> = index
        .definition_locations(&name)
        .into_iter()
        .filter(|loc| loc.uri == *uri)
        .map(|loc| loc.range.start.line)
        .collect();

    let lines = index.mem_lines_for(uri.as_str())?;
    let mut highlights = Vec::new();

    for (line_idx, line) in lines.iter().enumerate() {
        for abs in word_byte_offsets(line, &name) {
            let col = utf16_column(&line[..abs]);
            let col_end = col + utf16_column(&name);
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

    if highlights.is_empty() {
        None
    } else {
        Some(highlights)
    }
}
