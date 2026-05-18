//! `it`/`this` type inference helpers for Kotlin lambda contexts.
//!
//! All functions take explicit `(inputs) -> output` signatures — no hidden state,
//! no side effects beyond the on-demand file-indexing in `lambda_receiver_type_named_arg_ml`.
//!
//! Public surface (re-exported through `infer::mod`):
//! - `find_it_element_type`            — single-line `it.` completion
//! - `find_it_element_type_in_lines`   — multi-line hover `it.`
//! - `find_this_element_type_in_lines` — multi-line hover `this.`
//! - `find_named_lambda_param_type_in_lines` — hover on named lambda param
//! - `find_named_lambda_param_type`    — completion for named lambda param
//! - `is_lambda_param`                 — guard before named-param inference
//! - `lambda_receiver_type_from_context` — core: `before_brace` → element type

use tower_lsp::lsp_types::Url;

use crate::indexer::Indexer;
#[cfg(test)]
use crate::indexer::NodeExt;
use crate::types::CursorPos;
use crate::StrExt;

use super::args::has_named_params_not_it;
#[cfg(test)]
#[allow(unused_imports)]
pub(super) use super::chain::resolve_member_type_on;
pub(crate) use super::cst_lambda::ThisContext;
use super::cst_lambda::{
    classify_this_lambda_context, cst_it_or_this_type, cst_named_lambda_param_type,
    cst_this_context, cursor_node_at, ThisLambdaCtx,
};
#[cfg(test)]
#[allow(unused_imports)]
pub(super) use super::cst_lambda::{
    cst_lambda_param_type_via_call, is_inside_receiver_lambda, lambda_before_brace_context,
};
use super::receiver::{lambda_receiver_type_from_context, lambda_receiver_type_named_arg_ml};
#[cfg(test)]
#[allow(unused_imports)]
pub(super) use super::type_subst::build_ext_fn_type_subst;
#[cfg(test)]
pub(crate) use super::type_subst::find_last_dot_at_depth_zero;

/// Lines to scan backward for `{ param_name ->` in the multiline hover/goto path
/// (full-file scan from `find_named_lambda_param_type_in_lines`).
const LAMBDA_PARAM_SCAN_BACK_LINES: usize = 40;

/// Lines to scan backward for `{ param_name ->` in the single-line completion path
/// (from `find_named_lambda_param_type`).
const LAMBDA_PARAM_SCAN_BACK: usize = 20;

/// Selects which implicit lambda parameter is being inferred.
///
/// Replaces the `for_this: bool` flag in `find_it_element_type_in_lines_impl`
/// and `cst_it_or_this_type` with an explicit, self-documenting variant.
#[derive(Copy, Clone, Eq, PartialEq)]
pub(super) enum LambdaParamKind {
    /// Infer the type of `it` (the implicit element parameter).
    It,
    /// Infer the type of `this` (the receiver in a receiver lambda).
    This,
}

/// Lines to scan backward when searching for the enclosing lambda opener
/// in the text-fallback path of `find_it_element_type_in_lines_impl`.
pub(super) const IT_SCAN_BACK_LINES: usize = 15;

/// Resolve the element type of `it` when inside a lambda.
///
/// Scans `before_cursor` (text from line start to cursor, ending with `it.`)
/// backward to find the lambda opening `{`, then the callee before it
/// (e.g. `users.forEach`), then the receiver (`users`).
///
/// Delegates to `lambda_receiver_type_from_context` for the actual inference.
pub(crate) fn find_it_element_type(
    before_cursor: &str,
    idx: &Indexer,
    uri: &Url,
) -> Option<String> {
    let brace_byte = before_cursor.rfind('{')?;
    let before_brace = &before_cursor[..brace_byte];
    lambda_receiver_type_from_context(before_brace, idx, uri)
}

/// Multi-line version of `find_it_element_type` for hover/goto-def contexts.
///
/// When hovering over `it`, the cursor is ON `it` in the lambda body — which
/// may be on a DIFFERENT line than the opening `{`.  The simple `rfind('{')` on
/// `before_cursor` would miss it.
///
/// Algorithm: scan backward from `cursor_line` tracking `{}` depth to find
/// the opening `{` of the immediately enclosing lambda.  Then inspect that
/// line for a receiver expression before the brace.
pub(crate) fn find_it_element_type_in_lines(
    lines: &[String],
    pos: CursorPos,
    idx: &Indexer,
    uri: &Url,
) -> Option<String> {
    find_it_element_type_in_lines_impl(lines, pos, idx, uri, LambdaParamKind::It)
}

/// Resolve the `this` context at `pos` in `lines`.
///
/// Returns a [`ThisContext`] that lets callers distinguish between a resolved
/// receiver type, an unresolvable receiver lambda (must not fall back to
/// `enclosing_class_at`), and "not inside any receiver lambda" (fallback valid).
pub(crate) fn find_this_context_in_lines(
    lines: &[String],
    pos: CursorPos,
    idx: &Indexer,
    uri: &Url,
) -> ThisContext {
    if let Some(doc) = idx.live_doc(uri) {
        if let Some(node) = cursor_node_at(&doc, pos) {
            return cst_this_context(node, &doc, idx, uri);
        }
    }
    find_this_context_text(lines, pos, idx, uri)
}

pub(crate) fn find_this_element_type_in_lines(
    lines: &[String],
    pos: CursorPos,
    idx: &Indexer,
    uri: &Url,
) -> Option<String> {
    match find_this_context_in_lines(lines, pos, idx, uri) {
        ThisContext::Resolved(ty) => Some(ty),
        ThisContext::InsideReceiver | ThisContext::NotFound => None,
    }
}

/// Text-scan fallback for [`find_this_context_in_lines`] when no live CST is
/// available.
fn find_this_context_text(
    lines: &[String],
    pos: CursorPos,
    idx: &Indexer,
    uri: &Url,
) -> ThisContext {
    let mut depth: i32 = 0;
    let scan_start = pos.line.saturating_sub(IT_SCAN_BACK_LINES);

    for ln in (scan_start..=pos.line).rev() {
        let line = match lines.get(ln) {
            Some(l) => l,
            None => continue,
        };
        let scan_slice: &str = if ln == pos.line {
            let byte_end = crate::indexer::live_tree::utf16_col_to_byte(line, pos.utf16_col);
            &line[..byte_end]
        } else {
            line.as_str()
        };

        for (bi, ch) in scan_slice.char_indices().rev() {
            match ch {
                '}' => depth += 1,
                '{' => {
                    depth -= 1;
                    if depth < 0 {
                        let before_brace = &scan_slice[..bi];
                        if before_brace.ends_with('$') {
                            depth = 0;
                            continue;
                        }
                        if has_named_params_not_it(scan_slice[bi + 1..].trim_start()) {
                            depth = 0;
                            continue;
                        }
                        return match classify_this_lambda_context(before_brace, idx, uri) {
                            ThisLambdaCtx::Resolved(ty) => ThisContext::Resolved(ty),
                            ThisLambdaCtx::Receiver => ThisContext::InsideReceiver,
                            ThisLambdaCtx::NotReceiver => {
                                depth = 0;
                                continue;
                            }
                        };
                    }
                }
                _ => {}
            }
        }
    }
    ThisContext::NotFound
}

/// Multi-line version of `find_named_lambda_param_type` for hover/inlay-hint paths.
///
/// Scans the whole file (not just `before_cursor`) for `{ param_name ->`,
/// including the CURRENT line.  Also handles multi-param lambdas `{ id, scan -> }`.
///
/// `cursor_utf16_col` is the UTF-16 column of the parameter name at `cursor_line`.
///
/// `live_doc` must be the **same** snapshot that produced `cursor_line`/
/// `cursor_utf16_col`.  Passing a different snapshot (or re-calling `idx.live_doc`
/// internally) risks position mismatches when a `did_change` races with
/// inlay-hint computation.  Pass `None` to skip the CST path; the text
/// fallback will still run.
pub(crate) fn find_named_lambda_param_type_in_lines(
    lines: &[String],
    param_name: &str,
    cursor_line: usize,
    cursor_utf16_col: usize,
    live_doc: Option<&crate::indexer::live_tree::LiveDoc>,
    idx: &Indexer,
    uri: &Url,
) -> Option<String> {
    // CST fast-path: use the caller-provided position (may be col=0 when unknown,
    // but is the real column when called from infer_lambda_param_type_at).
    let pos = CursorPos {
        line: cursor_line,
        utf16_col: cursor_utf16_col,
    };
    if let Some(doc) = live_doc {
        if let Some(result) = cst_named_lambda_param_type(pos, param_name, doc, idx, uri) {
            return Some(result);
        }
    }

    // Text fallback: needed when live_doc is absent (e.g. did_open not yet
    // processed by the actor, or first inlay-hint request races with indexing).
    let scan_start = cursor_line.saturating_sub(LAMBDA_PARAM_SCAN_BACK_LINES);
    // Include cursor_line itself (different from completion path which is exclusive).
    for ln in (scan_start..=cursor_line).rev() {
        let line = match lines.get(ln) {
            Some(l) => l,
            None => continue,
        };
        let Some((brace_pos, pos)) = find_lambda_brace_for_param(line, param_name) else {
            continue;
        };
        let before_brace = &line[..brace_pos];
        let result = lambda_receiver_type_from_context(before_brace, idx, uri)
            .or_else(|| lambda_receiver_type_named_arg_ml(before_brace, pos, lines, ln, idx, uri));
        if result.is_some() {
            return result;
        }
    }
    None
}

/// Resolve the element/receiver type for an EXPLICITLY NAMED lambda parameter.
///
/// Handles both same-line and multi-line lambda declarations:
///
/// Same-line:  `items.forEach { item -> item.`
/// Multi-line: `items.forEach { item ->\n    item.`  ← cursor on second line
///
/// Scans backward (up to 20 lines) for `{ param_name ->` to find where the lambda
/// was opened, then infers the element type from what's before the `{`.
pub(crate) fn find_named_lambda_param_type(
    before_cursor: &str,
    param_name: &str,
    idx: &Indexer,
    uri: &Url,
    pos: CursorPos,
) -> Option<String> {
    if let Some(doc) = idx.live_doc(uri) {
        if let Some(result) = cst_named_lambda_param_type(pos, param_name, &doc, idx, uri) {
            return Some(result);
        }
    }

    let lines = idx.mem_lines_for(uri.as_str());

    // 1. Check same line first — covers `items.forEach { item -> item.`
    //    Also handles multi-param: `items.map { a, b -> a.`
    if let Some((brace_pos, param_pos)) = find_lambda_brace_for_param(before_cursor, param_name) {
        let before_brace = &before_cursor[..brace_pos];
        let result = lambda_receiver_type_from_context(before_brace, idx, uri).or_else(|| {
            lines.as_deref().and_then(|ls| {
                lambda_receiver_type_named_arg_ml(before_brace, param_pos, ls, pos.line, idx, uri)
            })
        });
        if result.is_some() {
            return result;
        }
    }

    // 2. Scan backward through previous lines.
    let lines = lines?;
    let scan_start = pos.line.saturating_sub(LAMBDA_PARAM_SCAN_BACK);
    for ln in (scan_start..pos.line).rev() {
        let line = match lines.get(ln) {
            Some(l) => l,
            None => continue,
        };
        let Some((brace_pos, param_pos)) = find_lambda_brace_for_param(line, param_name) else {
            continue;
        };
        let before_brace = &line[..brace_pos];
        let result = lambda_receiver_type_from_context(before_brace, idx, uri).or_else(|| {
            lambda_receiver_type_named_arg_ml(before_brace, param_pos, &lines, ln, idx, uri)
        });
        if result.is_some() {
            return result;
        }
    }
    None
}

/// Check whether `recv` looks like an explicitly-named lambda parameter
/// in the current editing context (same line or recent lines).
///
/// Used to avoid triggering lambda inference for ordinary local variables
/// that just happen to be lowercase.  Handles single and multi-param lambdas.
pub(crate) fn is_lambda_param(
    recv: &str,
    before_cur: &str,
    idx: &Indexer,
    uri: &Url,
    cursor_line: usize,
) -> bool {
    // Fast reject: if `recv` starts with uppercase or contains `.` it's a type/qualified
    // name, never a lambda parameter name.
    if recv.starts_with_uppercase() {
        return false;
    }
    if recv.contains('.') {
        return false;
    }

    // Same-line fast check: the lambda declaration may be on the cursor line
    // itself (e.g. `items.forEach { item -> item.`).
    if line_has_lambda_param(before_cur, recv) {
        return true;
    }

    // Delegate to lambda_params_at_col for multi-line detection.  That function
    // uses the CST live-tree when available (O(depth) walk) and falls back to a
    // brace-depth text scan covering up to 50 prior lines — both more thorough
    // than the old 10-line ad-hoc scan here.
    let cursor_col = before_cur.encode_utf16().count();
    idx.lambda_params_at_col(uri, cursor_line, cursor_col)
        .iter()
        .any(|p| p == recv)
}

fn find_it_element_type_in_lines_impl(
    lines: &[String],
    pos: CursorPos,
    idx: &Indexer,
    uri: &Url,
    kind: LambdaParamKind,
) -> Option<String> {
    if let Some(doc) = idx.live_doc(uri) {
        if let Some(node) = cursor_node_at(&doc, pos) {
            return cst_it_or_this_type(node, &doc, lines, kind, idx, uri);
        }
    }

    // Keep the text fallback for callers that provide indexed lines without a
    // live CST document (tests, disk-backed hover/inlay-hint paths).
    let mut depth: i32 = 0;
    let scan_start = pos.line.saturating_sub(IT_SCAN_BACK_LINES);

    for ln in (scan_start..=pos.line).rev() {
        let line = match lines.get(ln) {
            Some(l) => l,
            None => continue,
        };
        let scan_slice: &str = if ln == pos.line {
            let byte_end = crate::indexer::live_tree::utf16_col_to_byte(line, pos.utf16_col);
            &line[..byte_end]
        } else {
            line.as_str()
        };

        for (bi, ch) in scan_slice.char_indices().rev() {
            match ch {
                '}' => depth += 1,
                '{' => {
                    depth -= 1;
                    if depth < 0 {
                        let before_brace = &scan_slice[..bi];
                        if before_brace.ends_with('$') {
                            depth = 0;
                            continue;
                        }
                        let after_brace = scan_slice[bi + 1..].trim_start();
                        if has_named_params_not_it(after_brace) {
                            depth = 0;
                            continue;
                        }
                        if kind == LambdaParamKind::This {
                            return match classify_this_lambda_context(before_brace, idx, uri) {
                                ThisLambdaCtx::Resolved(ty) => Some(ty),
                                _ => None,
                            };
                        }
                        return lambda_receiver_type_from_context(before_brace, idx, uri).or_else(
                            || {
                                lambda_receiver_type_named_arg_ml(
                                    before_brace,
                                    0,
                                    lines,
                                    ln,
                                    idx,
                                    uri,
                                )
                            },
                        );
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Iterator over `(brace_pos, names_str)` for each `->` in `line` that has a
/// preceding `{`. `names_str` is the text between `{` and `->` (not trimmed).
/// This is the shared scanning kernel used by all lambda-param helpers.
fn lambda_brace_arrows(line: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut search_from = 0usize;
    std::iter::from_fn(move || loop {
        let rel = line[search_from..].find("->")?;
        let arrow_pos = search_from + rel;
        search_from = arrow_pos + 2;
        if let Some(brace_pos) = line[..arrow_pos].rfind('{') {
            let names_str = &line[brace_pos + 1..arrow_pos];
            return Some((brace_pos, names_str));
        }
    })
}

fn names_has_param(names_str: &str, param_name: &str) -> bool {
    names_str.split(',').any(|tok| {
        let n = tok.trim().ident_prefix();
        n == param_name
    })
}

fn param_index_in(names_str: &str, param_name: &str) -> Option<usize> {
    names_str.split(',').enumerate().find_map(|(i, tok)| {
        let n = tok.trim().ident_prefix();
        if n == param_name {
            Some(i)
        } else {
            None
        }
    })
}

/// Returns true if `line` contains a lambda declaration that names `param_name`
/// as one of its parameters (handles single and multi-param patterns):
///   `{ param -> ... }`, `{ a, param, b -> ... }`
pub(crate) fn line_has_lambda_param(line: &str, param_name: &str) -> bool {
    lambda_brace_arrows(line).any(|(_, names)| names_has_param(names, param_name))
}

/// Find the `{` byte position in `line` for the lambda that declares `param_name`.
/// Scans all `->` occurrences (a line may have multiple lambdas).
pub(crate) fn lambda_brace_pos_for_param(line: &str, param_name: &str) -> Option<usize> {
    lambda_brace_arrows(line)
        .find(|(_, names)| names_has_param(names, param_name))
        .map(|(pos, _)| pos)
}

/// Returns `(brace_pos, param_index)` for the lambda on `line` that declares
/// `param_name`, combining `lambda_brace_pos_for_param` + `lambda_param_position_on_line`
/// into a single scan.
pub(crate) fn find_lambda_brace_for_param(line: &str, param_name: &str) -> Option<(usize, usize)> {
    lambda_brace_arrows(line).find_map(|(brace_pos, names)| {
        param_index_in(names, param_name).map(|idx| (brace_pos, idx))
    })
}

/// 0-based index of `param_name` in a multi-param lambda opening `{ a, b, c ->`.
/// Returns 0 for single-param lambdas.
#[allow(dead_code)]
pub(crate) fn lambda_param_position_on_line(line: &str, param_name: &str) -> usize {
    lambda_brace_arrows(line)
        .find_map(|(_, names)| param_index_in(names, param_name))
        .unwrap_or(0)
}

// ─── test helpers ─────────────────────────────────────────────────────────────

/// Returns `true` if `lambda_node` (a `lambda_literal` CST node) has a
/// `lambda_parameters` child with at least one named parameter that is
/// neither `it` nor `_`.
///
/// Thin wrapper around [`NodeExt::has_lambda_named_params`] for `super::` access
/// in the companion test module.
#[cfg(test)]
pub(super) fn has_lambda_named_params(lambda_node: tree_sitter::Node<'_>, bytes: &[u8]) -> bool {
    lambda_node.has_lambda_named_params(bytes)
}

#[cfg(test)]
#[path = "it_this_tests.rs"]
mod tests;
