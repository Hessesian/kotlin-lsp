//! Completion feature — delegates the full pipeline via `CompletionIndex`.

use tower_lsp::lsp_types::{CompletionList, CompletionResponse, Position, Url};

use super::traits::CompletionIndex;

/// Compute completions at `position` in `uri`.
///
/// Returns the LSP `CompletionResponse` (possibly incomplete), or `None` when
/// there are no items and the workspace is fully indexed.
pub(crate) fn compute_completions(
    uri: &Url,
    position: Position,
    snippets: bool,
    index: &impl CompletionIndex,
) -> Option<CompletionResponse> {
    let (mut items, hit_cap) = index.completions(uri, position, snippets);
    let still_indexing = index.is_indexing_in_progress();
    if items.is_empty() && !still_indexing {
        return None;
    }
    // Pre-select the best match so the editor highlights it without an extra keystroke.
    if let Some(first) = items.first_mut() {
        first.preselect = Some(true);
    }
    // When hit_cap is true the list was truncated — tell the client to re-request
    // on every keystroke. Also mark incomplete while indexing is in progress.
    Some(CompletionResponse::List(CompletionList {
        is_incomplete: hit_cap || still_indexing,
        items,
    }))
}
