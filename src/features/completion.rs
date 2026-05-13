//! Completion feature — delegates the full pipeline via `CompletionIndex`.
//!
//! # CompletionItem data keys
//!
//! The completion pipeline writes a small JSON blob into `CompletionItem.data`
//! so that `completionItem/resolve` can look up the full signature + doc comment.
//! The `DATA_*` constants are defined in `resolver::complete` (the write side)
//! and re-exported here for use by `resolve_completion_item` (the read side).

use tower_lsp::lsp_types::{
    CompletionItem, CompletionList, CompletionResponse, Documentation, MarkupContent, MarkupKind,
    Position, Url,
};

use crate::indexer::resolution::{enrich_at_line, IndexRead, ResolveOptions, SubstitutionContext};

use super::traits::CompletionIndex;

// Re-export so callers only need to import from one place.
pub(crate) use crate::resolver::complete::{DATA_CALLING_URI, DATA_COL, DATA_LINE, DATA_URI};

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

/// Enrich a completion item with signature + doc comment on `completionItem/resolve`.
///
/// Reads `uri`, `line`, `col`, and optionally `calling_uri` from the item's
/// custom `data` blob written by the completion pipeline.
pub(crate) fn resolve_completion_item<I: IndexRead>(
    item: CompletionItem,
    index: &I,
) -> CompletionItem {
    let mut item = item;
    if let Some(ref data) = item.data {
        if let (Some(uri), Some(line)) = (
            data.get(DATA_URI).and_then(|v| v.as_str()),
            data.get(DATA_LINE).and_then(|v| v.as_u64()),
        ) {
            let col = data.get(DATA_COL).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let calling_uri = data.get(DATA_CALLING_URI).and_then(|v| v.as_str());

            let subst_ctx = match calling_uri {
                Some(cu) if cu != uri => SubstitutionContext::CrossFile {
                    calling_uri: cu,
                    cursor_line: None,
                },
                _ => SubstitutionContext::None,
            };

            if let Some(info) = enrich_at_line(
                index,
                uri,
                line as u32,
                col,
                subst_ctx,
                &ResolveOptions::completion(),
            ) {
                if !info.signature.is_empty() {
                    item.detail = Some(info.signature);
                }
                if !info.doc.is_empty() {
                    item.documentation = Some(Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: info.doc,
                    }));
                }
            }
        }
    }
    item
}
