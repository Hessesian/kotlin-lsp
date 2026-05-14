//! Completion feature — pipeline and helpers extracted from `Indexer`.
//!
//! # CompletionItem data keys
//!
//! The completion pipeline writes a small JSON blob into `CompletionItem.data`
//! so that `completionItem/resolve` can look up the full signature + doc comment.
//! The `DATA_*` constants are defined in `resolver::complete` (the write side)
//! and re-exported here for use by `resolve_completion_item` (the read side).

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, CompletionResponse, Documentation,
    MarkupContent, MarkupKind, Position, Url,
};

use crate::indexer::resolution::{enrich_at_line, IndexRead, ResolveOptions, SubstitutionContext};
use crate::indexer::Indexer;
use crate::indexer::{
    find_it_element_type, find_named_lambda_param_type, is_lambda_param, last_ident_in,
};
use crate::resolver::complete::{
    complete_symbol, complete_symbol_with_context, is_annotation_context,
};
use crate::types::CursorPos;
use crate::StrExt;

use super::text_utils::utf16_column;
use super::traits::CompletionIndex;

// Re-export so callers only need to import from one place.
pub(crate) use crate::resolver::complete::{DATA_CALLING_URI, DATA_COL, DATA_LINE, DATA_URI};

const IT: &str = "it";
const THIS: &str = "this";

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

// ─── pipeline ────────────────────────────────────────────────────────────────

/// Full completion pipeline. Called by `Indexer::completions` (inherent method).
pub(crate) fn run_completions(
    index: &Indexer,
    uri: &Url,
    position: Position,
    snippets: bool,
) -> (Vec<CompletionItem>, bool) {
    index.ensure_indexed(uri);

    let Some(line) = line_for_position(index, uri, position.line) else {
        return (vec![], false);
    };
    let before = before_cursor(&line, position.character);
    let (prefix, before_prefix) = split_prefix(before);
    let cache_key = completion_cache_key(uri, before_prefix, position.line, prefix);
    if let Some(cached) = cache_hit(index, &cache_key) {
        return (cached, false);
    }

    let dot_recv = dot_receiver(before_prefix);

    if let Some(ref recv) = dot_recv {
        if is_lambda_recv(recv, before, index, uri, position.line) {
            return (
                complete_lambda_dot(index, recv, before, position, uri, snippets, prefix),
                false,
            );
        }
    }

    let annotation_only = dot_recv.is_none() && is_annotation_context(before, prefix);
    let (mut items, hit_cap) = complete_symbol_with_context(
        index,
        prefix,
        dot_recv.as_deref(),
        uri,
        snippets,
        annotation_only,
        Some(position.line),
    );
    if dot_recv.is_none() {
        add_lambda_param_completions(index, &mut items, uri, position.line as usize, prefix);
    }

    store_in_cache(index, cache_key, prefix, &items);
    (items, hit_cap)
}

// ─── pipeline helpers ─────────────────────────────────────────────────────────

/// Build the cache key for a completion request.
///
/// Dot-completion: key omits `prefix` so repeated keystrokes after `.` are fast
/// (the client fuzzy-filters the full member list).
/// Bare-word completion: key includes `prefix` so each keystroke gets a fresh,
/// precisely-capped result rather than a stale earlier query.
fn completion_cache_key(uri: &Url, before_prefix: &str, line: u32, prefix: &str) -> String {
    if before_prefix.ends_with('.') {
        format!("{}|{}|{}", uri.as_str(), before_prefix, line)
    } else {
        format!("{}|{}|{}|{}", uri.as_str(), before_prefix, line, prefix)
    }
}

/// Return cached items if the last completion key matches, `None` otherwise.
fn cache_hit(index: &Indexer, key: &str) -> Option<Vec<CompletionItem>> {
    let guard = index.last_completion.lock().ok()?;
    let (ref k, _, ref cached) = (*guard).as_ref()?;
    (k.as_str() == key).then(|| cached.clone())
}

/// Persist the latest completion result for subsequent identical requests.
fn store_in_cache(index: &Indexer, key: String, prefix: &str, items: &[CompletionItem]) {
    if let Ok(mut guard) = index.last_completion.lock() {
        *guard = Some((key, prefix.to_owned(), items.to_vec()));
    }
}

/// True when `recv` is an `it`/`this` default name or an explicitly declared
/// lambda parameter at the cursor position.
fn is_lambda_recv(recv: &str, before: &str, index: &Indexer, uri: &Url, line: u32) -> bool {
    recv == IT || recv == THIS || is_lambda_param(recv, before, index, uri, line as usize)
}

/// Run dot-completion for a lambda receiver (`it.`, `this.`, or named param).
///
/// Returns a type-hint placeholder item when the type is known but no members
/// matched yet (gives the user a visible signal of what type was inferred).
fn complete_lambda_dot(
    index: &Indexer,
    recv: &str,
    before: &str,
    position: Position,
    uri: &Url,
    snippets: bool,
    prefix: &str,
) -> Vec<CompletionItem> {
    let cursor_line = position.line as usize;
    let cursor_col = utf16_column(before) as usize;
    let Some(elem_type) =
        resolve_lambda_recv_type(index, recv, before, cursor_line, cursor_col, uri)
    else {
        return vec![];
    };
    let (items, _) = complete_symbol(
        index,
        prefix,
        Some(&elem_type),
        uri,
        snippets,
        Some(position.line),
    );
    if items.is_empty() {
        vec![type_hint_item(recv, &elem_type)]
    } else {
        items
    }
}

/// A placeholder `CompletionItem` showing the inferred type when no members matched.
fn type_hint_item(recv: &str, elem_type: &str) -> CompletionItem {
    CompletionItem {
        label: format!("{recv}: {elem_type}"),
        kind: Some(CompletionItemKind::TYPE_PARAMETER),
        detail: Some(format!("Inferred type: {elem_type}")),
        sort_text: Some("~hint".into()),
        ..Default::default()
    }
}

// ─── private helpers ─────────────────────────────────────────────────────────

/// Returns the text of line `line_idx` for `uri`, preferring live lines.
fn line_for_position(index: &Indexer, uri: &Url, line_idx: u32) -> Option<String> {
    let idx = line_idx as usize;
    if let Some(ll) = index.live_lines.get(uri.as_str()) {
        return ll.get(idx).cloned();
    }
    index.files.get(uri.as_str())?.lines.get(idx).cloned()
}

/// Resolves the element type for an `it`/`this`/named-param dot-receiver.
fn resolve_lambda_recv_type(
    index: &Indexer,
    recv: &str,
    before: &str,
    cursor_line: usize,
    cursor_col: usize,
    uri: &Url,
) -> Option<String> {
    if recv != IT && recv != THIS {
        return find_named_lambda_param_type(
            before,
            recv,
            index,
            uri,
            CursorPos {
                line: cursor_line,
                utf16_col: cursor_col,
            },
        );
    }
    // Unified inference path (same as hover/inlay-hints) — handles multi-line
    // scan, enclosing_class_at for this, and call-arg type fallback.
    let position = Position::new(cursor_line as u32, cursor_col as u32);
    if let Some(ty) = index.infer_lambda_param_type_at(recv, uri, position) {
        return Some(ty);
    }
    // Single-line fallback: when mem_lines is unavailable (e.g. file not yet
    // opened), use the raw before-cursor text to find the lambda receiver.
    if recv == IT {
        return find_it_element_type(before, index, uri);
    }
    None
}

/// Appends lambda-parameter completions for bare-word (non-dot) completion.
fn add_lambda_param_completions(
    index: &Indexer,
    items: &mut Vec<CompletionItem>,
    uri: &Url,
    line_idx: usize,
    prefix: &str,
) {
    let prefix_lower = prefix.to_lowercase();
    for param in index.lambda_params_at(uri, line_idx) {
        if param.to_lowercase().starts_with(prefix_lower.as_str())
            && !items.iter().any(|i| i.label == param)
        {
            items.push(CompletionItem {
                label: param.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                sort_text: Some(format!("005:{param}")),
                ..Default::default()
            });
        }
    }
}

// ─── pure string helpers ──────────────────────────────────────────────────────

/// Returns a slice of `line` up to the UTF-16 column `utf16_col`.
fn before_cursor(line: &str, utf16_col: u32) -> &str {
    let target = utf16_col as usize;
    let mut utf16 = 0usize;
    let mut byte_end = line.len();
    for (bi, ch) in line.char_indices() {
        if utf16 >= target {
            byte_end = bi;
            break;
        }
        utf16 += ch.len_utf16();
    }
    &line[..byte_end]
}

/// Splits `before` into the trailing identifier fragment (`prefix`) and
/// everything that precedes it (`before_prefix`).
fn split_prefix(before: &str) -> (&str, &str) {
    let prefix = last_ident_in(before);
    let before_prefix = &before[..before.len() - prefix.len()];
    (prefix, before_prefix)
}

/// Returns the expression immediately before a trailing dot in `before_prefix`,
/// or `None` if `before_prefix` does not end with a dot.
///
/// Handles one level of qualification: `Outer.Inner.` → `"Outer.Inner"`.
fn dot_receiver(before_prefix: &str) -> Option<String> {
    let before_dot = before_prefix.strip_suffix('.')?;
    let inner = last_ident_in(before_dot);
    if inner.is_empty() {
        return None;
    }
    let remaining = &before_dot[..before_dot.len() - inner.len()];
    if remaining.ends_with('.') && inner.starts_with_uppercase() {
        let outer = last_ident_in(&remaining[..remaining.len() - 1]);
        if !outer.is_empty() && outer.starts_with_uppercase() {
            return Some(format!("{outer}.{inner}"));
        }
    }
    Some(inner.to_owned())
}

#[cfg(test)]
#[path = "completion_tests.rs"]
mod tests;
