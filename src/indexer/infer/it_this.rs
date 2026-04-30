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

// ── from infer submodules (re-exported through infer/mod.rs) ─────────────────
use super::{
    // lambda.rs
    RECEIVER_THIS_FNS,
    lambda_type_first_input,
    lambda_type_nth_input,
    lambda_type_receiver,
    // sig.rs
    find_fun_signature_full,
    collect_all_fun_params_texts,
    nth_fun_param_type_str,
    last_fun_param_type_str,
    strip_trailing_call_args,
    // args.rs
    extract_named_arg_name,
    find_named_param_type_in_sig,
    has_named_params_not_it,
    extract_first_arg,
    // deps.rs
    InferDeps,
};

use crate::indexer::NodeExt;
use crate::queries::{KIND_LAMBDA_LIT, KIND_CALL_EXPR};
use crate::StrExt;
use crate::resolver::extract_collection_element_type;

// ── from indexer.rs (parent of infer; descendants can access private items) ──
use super::super::{
    Indexer,
    last_ident_in,
    find_enclosing_call_name,
};

use crate::types::CursorPos;

/// Lines to scan backward for `{ param_name ->` in the multiline hover/goto path
/// (full-file scan from `find_named_lambda_param_type_in_lines`).
const LAMBDA_PARAM_SCAN_BACK_LINES: usize = 40;

/// Lines to scan backward for `{ param_name ->` in the single-line completion path
/// (from `find_named_lambda_param_type`).
const LAMBDA_PARAM_SCAN_BACK: usize = 20;

/// Lines to scan backward when searching for the enclosing lambda opener
/// in the text-fallback path of `find_it_element_type_in_lines_impl`.
const IT_SCAN_BACK_LINES: usize = 15;

// ─── public API ──────────────────────────────────────────────────────────────

/// Resolve the element type of `it` when inside a lambda.
///
/// Scans `before_cursor` (text from line start to cursor, ending with `it.`)
/// backward to find the lambda opening `{`, then the callee before it
/// (e.g. `users.forEach`), then the receiver (`users`).
///
/// Delegates to `lambda_receiver_type_from_context` for the actual inference.
pub(crate) fn find_it_element_type(before_cursor: &str, idx: &Indexer, uri: &Url) -> Option<String> {
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
    pos:   CursorPos,
    idx:   &Indexer,
    uri:   &Url,
) -> Option<String> {
    find_it_element_type_in_lines_impl(lines, pos, idx, uri, false)
}

pub(crate) fn find_this_element_type_in_lines(
    lines: &[String],
    pos:   CursorPos,
    idx:   &Indexer,
    uri:   &Url,
) -> Option<String> {
    find_it_element_type_in_lines_impl(lines, pos, idx, uri, true)
}

/// Multi-line version of `find_named_lambda_param_type` for hover/goto-def.
///
/// Scans the whole file (not just `before_cursor`) for `{ param_name ->`,
/// including the CURRENT line (needed when cursor is on the param name before
/// the `->` is written, or when scanning the declaration line itself).
///
/// Also handles multi-param lambdas `{ id, scan -> }`.
pub(crate) fn find_named_lambda_param_type_in_lines(
    lines:       &[String],
    param_name:  &str,
    cursor_line: usize,
    idx:         &Indexer,
    uri:         &Url,
) -> Option<String> {
    let scan_start = cursor_line.saturating_sub(LAMBDA_PARAM_SCAN_BACK_LINES);
    // Include cursor_line itself (different from completion path which is exclusive).
    for ln in (scan_start..=cursor_line).rev() {
        let line = match lines.get(ln) { Some(l) => l, None => continue };
        let Some((brace_pos, pos)) = find_lambda_brace_for_param(line, param_name) else { continue; };
        let before_brace = &line[..brace_pos];
        let result = lambda_receiver_type_from_context(before_brace, idx, uri)
            .or_else(|| lambda_receiver_type_named_arg_ml(before_brace, pos, lines, ln, idx, uri));
        if result.is_some() { return result; }
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
    param_name:   &str,
    idx:          &Indexer,
    uri:          &Url,
    cursor_line:  usize,
) -> Option<String> {
    let lines = idx.mem_lines_for(uri.as_str());

    // 1. Check same line first — covers `items.forEach { item -> item.`
    //    Also handles multi-param: `items.map { a, b -> a.`
    if let Some((brace_pos, pos)) = find_lambda_brace_for_param(before_cursor, param_name) {
        let before_brace = &before_cursor[..brace_pos];
        let result = lambda_receiver_type_from_context(before_brace, idx, uri)
            .or_else(|| lines.as_deref().and_then(|ls|
                lambda_receiver_type_named_arg_ml(before_brace, pos, ls, cursor_line, idx, uri)
            ));
        if result.is_some() { return result; }
    }

    // 2. Scan backward through previous lines.
    let lines = lines?;
    let scan_start = cursor_line.saturating_sub(LAMBDA_PARAM_SCAN_BACK);
    for ln in (scan_start..cursor_line).rev() {
        let line = match lines.get(ln) { Some(l) => l, None => continue };
        let Some((brace_pos, pos)) = find_lambda_brace_for_param(line, param_name) else { continue; };
        let before_brace = &line[..brace_pos];
        let result = lambda_receiver_type_from_context(before_brace, idx, uri)
            .or_else(|| lambda_receiver_type_named_arg_ml(before_brace, pos, &lines, ln, idx, uri));
        if result.is_some() { return result; }
    }
    None
}

/// Check whether `recv` looks like an explicitly-named lambda parameter
/// in the current editing context (same line or recent lines).
///
/// Used to avoid triggering lambda inference for ordinary local variables
/// that just happen to be lowercase.  Handles single and multi-param lambdas.
pub(crate) fn is_lambda_param(
    recv:        &str,
    before_cur:  &str,
    idx:         &Indexer,
    uri:         &Url,
    cursor_line: usize,
) -> bool {
    // Fast reject: if `recv` starts with uppercase or contains `.` it's a type/qualified
    // name, never a lambda parameter name.
    if recv.starts_with_uppercase() { return false; }
    if recv.contains('.') { return false; }

    // Same-line fast check: the lambda declaration may be on the cursor line
    // itself (e.g. `items.forEach { item -> item.`).
    if line_has_lambda_param(before_cur, recv) { return true; }

    // Delegate to lambda_params_at_col for multi-line detection.  That function
    // uses the CST live-tree when available (O(depth) walk) and falls back to a
    // brace-depth text scan covering up to 50 prior lines — both more thorough
    // than the old 10-line ad-hoc scan here.
    let cursor_col = before_cur.encode_utf16().count();
    idx.lambda_params_at_col(uri, cursor_line, cursor_col)
        .iter()
        .any(|p| p == recv)
}

/// Shared core: given the text BEFORE the `{` that opens a lambda, infer
/// the element type that `it` / the named param will have.
///
/// Three cases:
///   A) `receiver.method { it }`          — infer element type from receiver
///   B) `plainFun(args) { it }`           — look up fun's last param type
///   C) `fn(arg1, { namedParam -> ... })` — look up fun's N-th param type
///   D) multi-line named-arg `name = {\n  it }` — resolved by callers via `_ml` variant
pub(crate) fn lambda_receiver_type_from_context(
    before_brace: &str,
    deps:         &impl InferDeps,
    uri:          &Url,
) -> Option<String> {
    let trimmed = before_brace.trim_end();

    // Strip a trailing balanced `(args)` to expose the callee expression.
    let callee_raw = strip_trailing_call_args(trimmed).replace("?.", ".");
    let callee = callee_raw.trim(); // trim both ends — leading spaces from indentation matter

    // ── Case A: `receiver.method` ────────────────────────────────────────────
    // Use a depth-aware dot search so dots INSIDE argument lists are ignored
    // (e.g., `fn(Enum.VALUE, {` must not match the dot inside `Enum.VALUE`).
    if let Some(dot_pos) = find_last_dot_at_depth_zero(callee) {
        let receiver_expr = callee[..dot_pos].trim_end();
        let receiver_var = last_ident_in(receiver_expr);
        // Extract method name (everything after the dot up to the first non-id char).
        let method = callee[dot_pos + 1..].trim_start().ident_prefix();

        if !receiver_var.is_empty() {
            if let Some(raw) = deps.find_var_type(receiver_var, uri) {
                if let Some(elem) = extract_collection_element_type(&raw) {
                    return Some(elem);
                }
                // Non-collection receiver: prefer the method's own lambda param type when
                // the method is indexed (e.g. `flow.collectIn { it }` → T from `collectIn`'s
                // `block: suspend (T) -> Unit`).  Fall back to receiver type when the method
                // is not found (e.g. stdlib `run`, `apply`, `let` → receiver type is correct).
                if !method.is_empty() {
                    if let Some(ty) = fun_trailing_lambda_it_type(&method, deps, uri) {
                        return Some(ty);
                    }
                }
                let base = raw.ident_prefix();
                if !base.is_empty() && base.starts_with_uppercase() {
                    return Some(base);
                }
            }
            if receiver_var.starts_with_uppercase() {
                return Some(receiver_var.to_owned());
            }
        }
    }

    // ── Case B: plain trailing lambda — `fnName(args) { it/this }` ─────────
    // Extract the trailing identifier from callee — handles cases where callee
    // is prefixed by outer-lambda context like `{ setState` (the `{` belongs
    // to an enclosing lambda, not this call).
    let trailing_fn = last_ident_in(callee);
    if !trailing_fn.is_empty() {
        // Known stdlib scope function `with(receiver) { this }` — extract the
        // first argument as the receiver and infer its type directly.
        if trailing_fn == "with" {
            if let Some(recv_name) = extract_first_arg(trimmed) {
                if let Some(raw) = deps.find_var_type(recv_name, uri) {
                    let base = raw.ident_prefix();
                    if !base.is_empty() { return Some(base); }
                }
                // If recv_name starts uppercase it IS the type (companion / object ref).
                let base = recv_name.ident_prefix();
                if base.starts_with_uppercase() {
                    return Some(base);
                }
            }
        }
        if let Some(ty) = fun_trailing_lambda_it_type(trailing_fn, deps, uri) {
            return Some(ty);
        }
    }

    // ── Case C: inline lambda arg — `fn(arg, { param -> ... }, ...)` ─────────
    // `before_brace` ends inside an unclosed `(`, so scan backward to find
    // the function name and the positional index of this lambda argument.
    inline_lambda_param_type(trimmed, deps, uri)
}

// ─── private helpers ─────────────────────────────────────────────────────────

/// Walk ancestors from `start_node` looking for a `lambda_literal` without
/// named params, then infer the `it`/`this` type for that lambda.
///
/// This is the extracted body of the CST fast-path in
/// `find_it_element_type_in_lines_impl`.
fn cst_it_or_this_type(
    start_node: tree_sitter::Node<'_>,
    doc:        &crate::indexer::live_tree::LiveDoc,
    lines:      &[String],
    for_this:   bool,
    idx:        &Indexer,
    uri:        &Url,
) -> Option<String> {
    let mut cur = start_node;
    loop {
        if cur.kind() == KIND_LAMBDA_LIT && !cur.has_lambda_named_params(&doc.bytes) {
            // Extract text of the lambda-opening line up to the `{`.
            let brace_byte = cur.start_byte();
            let line_start = doc.bytes[..brace_byte]
                .iter()
                .rposition(|&b| b == b'\n')
                .map(|i| i + 1)
                .unwrap_or(0);
            let before_brace = std::str::from_utf8(&doc.bytes[line_start..brace_byte])
                .unwrap_or("")
                .trim_end();
            let ln = cur.start_position().row;

            if for_this {
                if let Some(t) = lambda_receiver_this_type_from_context(before_brace, idx, uri) {
                    return Some(t);
                }
            } else {
                // CST structural fallback: walk up the call tree to find
                // the enclosing function call and the lambda's argument
                // position. Handles multi-line cases where the call site
                // is on a different line than the lambda `{`.
                let result = lambda_receiver_type_from_context(before_brace, idx, uri)
                    .or_else(|| lambda_receiver_type_named_arg_ml(
                        before_brace, 0, lines, ln, idx, uri,
                    ))
                    .or_else(|| cst_lambda_param_type_via_call(doc, &cur, idx, uri));
                if result.is_some() { return result; }
            }
        }
        let p = cur.parent()?;
        cur = p;
    }
}

fn find_it_element_type_in_lines_impl(
    lines:    &[String],
    pos:      CursorPos,
    idx:      &Indexer,
    uri:      &Url,
    for_this: bool,
) -> Option<String> {
    // ── CST fast path ────────────────────────────────────────────────────────
    if let Some(doc) = idx.live_doc(uri) {
        use tree_sitter::Point;
        let line_text = lines.get(pos.line).map(|s| s.as_str()).unwrap_or("");
        let byte_col  = crate::indexer::live_tree::utf16_col_to_byte(line_text, pos.utf16_col);
        let point     = Point { row: pos.line, column: byte_col };
        if let Some(node) = doc.tree.root_node().descendant_for_point_range(point, point) {
            return cst_it_or_this_type(node, &doc, lines, for_this, idx, uri);
        }
        // Node not found for this position — fall through to text scan.
    }

    // ── Text fallback ────────────────────────────────────────────────────────
    // Scan right-to-left tracking brace depth.
    // Convention: depth starts at 0. `}` increments, `{` decrements.
    // When depth goes < 0, we've found the `{` that opens our enclosing lambda.
    //
    // IMPORTANT: On cursor_line, only scan characters *before* cursor_col.
    // Characters to the right of the cursor (e.g., closing `}`) must not affect
    // the depth; otherwise a balanced `{ it.name }` would never trigger depth < 0.
    let mut depth: i32 = 0;
    let scan_start = pos.line.saturating_sub(IT_SCAN_BACK_LINES);

    for ln in (scan_start..=pos.line).rev() {
        let line = match lines.get(ln) { Some(l) => l, None => continue };
        // On cursor_line restrict to chars at byte positions < cursor_col.
        let scan_slice: &str = if ln == pos.line {
            // pos.utf16_col is a UTF-16 character offset (from LSP); convert to a byte boundary.
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
                        // Skip string interpolation `${`.
                        if before_brace.ends_with('$') { depth = 0; continue; }
                        // Skip named-param lambdas `{ name -> }` or `{ a, b -> }` — that's not `it`.
                        // Use depth-aware `->` detection to handle multi-param lambdas where
                        // `rest` starts with `,` not `->` (e.g. `{ loanId, isWustenrot ->`).
                        let after_brace = scan_slice[bi + 1..].trim_start();
                        if has_named_params_not_it(after_brace) {
                            depth = 0; continue;
                        }
                        if for_this {
                            // `this` only gets a hint from receiver lambdas (`T.() -> R`).
                            return lambda_receiver_this_type_from_context(before_brace, idx, uri);
                        }
                        let result = lambda_receiver_type_from_context(before_brace, idx, uri)
                            .or_else(|| lambda_receiver_type_named_arg_ml(
                                before_brace, 0, lines, ln, idx, uri,
                            ));
                        return result;
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
    std::iter::from_fn(move || {
        loop {
            let rel = line[search_from..].find("->")?;
            let arrow_pos = search_from + rel;
            search_from = arrow_pos + 2;
            if let Some(brace_pos) = line[..arrow_pos].rfind('{') {
                let names_str = &line[brace_pos + 1..arrow_pos];
                return Some((brace_pos, names_str));
            }
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
        if n == param_name { Some(i) } else { None }
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
    lambda_brace_arrows(line)
        .find_map(|(brace_pos, names)| {
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

///
/// `this` in Kotlin refers to the implicit receiver only inside a **receiver lambda**
/// (`T.() -> R`).  In a regular lambda (`(T) -> R`) `this` is the enclosing class —
/// we must NOT emit a hint from the lambda in that case.
///
/// Rules:
///  - Case A `receiver.method { this }`: check if `method` has a receiver-lambda last
///    param (`T.() -> R`) — if so return `T`.  If method not indexed but is a known
///    stdlib scope function listed in `RECEIVER_THIS_FNS` (`run`, `apply`), return the
///    receiver type.  `also` and `let` are intentionally excluded — they expose the
///    receiver as `it`, not `this`.
///  - Case B `with(receiver) { this }`: return the receiver's type (special-cased).
///  - Everything else: return `None` (don't hint `this` from the lambda).
fn lambda_receiver_this_type_from_context(
    before_brace: &str,
    deps:         &impl InferDeps,
    uri:          &Url,
) -> Option<String> {
    let trimmed = before_brace.trim_end();
    let callee_raw = strip_trailing_call_args(trimmed).replace("?.", ".");
    let callee = callee_raw.trim();

    // ── Case A: `receiver.method` ────────────────────────────────────────────
    if let Some(dot_pos) = find_last_dot_at_depth_zero(callee) {
        let receiver_expr = callee[..dot_pos].trim_end();
        let receiver_var = last_ident_in(receiver_expr);
        let method = callee[dot_pos + 1..].trim_start().ident_prefix();

        if !receiver_var.is_empty() && !method.is_empty() {
            // Prefer the method's own receiver-lambda type (only for indexed fns).
            if let Some(ty) = fun_trailing_lambda_this_type(&method, deps, uri) {
                return Some(ty);
            }
            // Only functions listed in `RECEIVER_THIS_FNS` (`run`, `apply`) are treated as
            // receiver-`this` lambdas; `also`/`let` are intentionally excluded.
            if RECEIVER_THIS_FNS.contains(&method.as_str()) {
                if let Some(raw) = deps.find_var_type(receiver_var, uri) {
                    let base = raw.ident_prefix();
                    if !base.is_empty() { return Some(base); }
                }
                if receiver_var.starts_with_uppercase() {
                    return Some(receiver_var.to_owned());
                }
            }
        }
        return None; // Non-scope, non-receiver-lambda: `this` is enclosing class.
    }

    // ── Case B: `with(receiver) { this }` ───────────────────────────────────
    let trailing_fn = last_ident_in(callee);
    if trailing_fn == "with" {
        if let Some(recv_name) = extract_first_arg(trimmed) {
            if let Some(raw) = deps.find_var_type(recv_name, uri) {
                let base = raw.ident_prefix();
                if !base.is_empty() { return Some(base); }
            }
            let base = recv_name.ident_prefix();
            if base.starts_with_uppercase() {
                return Some(base);
            }
        }
    }

    None
}

/// Handles named-arg lambdas spread across multiple lines:
/// opener like `  buildingSavings = ` or `  loan = ` spread across multiple
/// lines (the enclosing `(` is on a previous line).
///
/// Returns `Some(type_name)` for the Nth input type of the parameter's functional
/// type, where N = `lambda_param_pos` (0-based position of the named param in the
/// multi-param lambda, e.g. `{ loanId, isWustenrot -> }` → loanId=0, isWustenrot=1).
fn lambda_receiver_type_named_arg_ml(
    before_brace:      &str,
    lambda_param_pos:  usize,
    lines:             &[String],
    line_no:           usize,
    idx:               &Indexer,
    uri:               &Url,
) -> Option<String> {
    let named_arg = extract_named_arg_name(before_brace)?;

    // Find the enclosing function/constructor call by scanning backward.
    let callee_full = find_enclosing_call_name(lines, line_no, before_brace.chars().count())?;

    // Use the LAST segment of a dotted callee as the function name to look up.
    // `DashboardProductsReducer.SheetReloadActions` → `SheetReloadActions`
    let fn_name = callee_full.split('.').next_back()?;

    // If callee is qualified (e.g. `DashboardProductsReducer.SheetReloadActions`),
    // resolve the outer class to its file and search only there.  This prevents
    // picking a same-named class from a different file when multiple classes share
    // the same short name (e.g. two `SheetReloadActions` in the same project).
    let sig = if let Some(dot) = callee_full.rfind('.') {
        let outer = &callee_full[..dot];
        // Find outer class file; try indexed files first (no rg), then rg fallback.
        let outer_file: Option<String> = {
            let locs = idx.resolve_symbol_no_rg(outer, uri);
            locs.first().map(|l| l.uri.to_string())
                .or_else(|| {
                    // On-demand: use rg to find and index the outer class.
                    let root = idx.workspace_root.read().unwrap().clone();
                    let matcher = idx.ignore_matcher.read().unwrap().clone();
                    let rg_locs = crate::rg::rg_find_definition(
                        outer, root.as_deref(), matcher.as_deref()
                    );
                    for loc in &rg_locs {
                        if !idx.files.contains_key(loc.uri.as_str()) {
                            if let Ok(path) = loc.uri.to_file_path() {
                                if let Ok(content) = std::fs::read_to_string(&path) {
                                    idx.index_content(&loc.uri, &content);
                                }
                            }
                        }
                    }
                    rg_locs.first().map(|l| l.uri.to_string())
                })
        };
        if let Some(file_uri) = outer_file {
            // Try ALL symbols named `fn_name` in the outer-class file — the file
            // may have multiple same-named nested classes (e.g. two `SheetReloadActions`
            // in different reducers).  Pick the first one whose params contain `named_arg`.
            let sigs = collect_all_fun_params_texts(fn_name, &file_uri, idx);
            let found = sigs.into_iter()
                .find_map(|s| find_named_param_type_in_sig(&s, named_arg).map(|ty| (s, ty)));
            if let Some((_sig, param_type)) = found {
                return lambda_type_nth_input(&param_type, lambda_param_pos);
            }
            find_fun_signature_full(fn_name, idx, uri)
        } else {
            find_fun_signature_full(fn_name, idx, uri)
        }
    } else {
        find_fun_signature_full(fn_name, idx, uri)
    }?;

    let param_type = find_named_param_type_in_sig(&sig, named_arg)?;
    lambda_type_nth_input(&param_type, lambda_param_pos)
}

/// CST structural fallback for `it` type: given a `lambda_literal` node, walk
/// up the call tree to find the enclosing function call and the lambda's
/// positional or named argument index, then return the first input type of
/// that parameter.
///
/// This handles multi-line cases where the function call is on a different line
/// than the lambda opening `{`, so the text-based `before_brace` approach cannot
/// reach the function name.
fn cst_lambda_param_type_via_call(
    doc:    &crate::indexer::live_tree::LiveDoc,
    lambda: &tree_sitter::Node<'_>,
    deps:   &impl InferDeps,
    uri:    &Url,
) -> Option<String> {
    let bytes = &doc.bytes;
    let mut cur = *lambda;
    loop {
        let parent = cur.parent()?;
        match parent.kind() {
            "value_argument" => {
                // Lambda is a parenthesised argument.
                let mut node = parent;
                let call_expr = loop {
                    let p = node.parent()?;
                    if p.kind() == KIND_CALL_EXPR { break p; }
                    node = p;
                };
                let fn_name = call_expr.call_fn_name(bytes)?;
                let sig = deps.find_fun_params_text(&fn_name, uri)?;
                let param_type = if let Some(label) = parent.named_arg_label(bytes) {
                    find_named_param_type_in_sig(&sig, &label)
                } else {
                    nth_fun_param_type_str(&sig, parent.value_arg_position())
                }?;
                return lambda_type_first_input(&param_type);
            }
            "call_suffix" => {
                // Trailing lambda: `fn(...) { it }` or `fn { it }`.
                let call_expr = parent.parent()?;
                if call_expr.kind() != KIND_CALL_EXPR { cur = parent; continue; }
                let fn_name = call_expr.call_fn_name(bytes)?;
                let sig = deps.find_fun_params_text(&fn_name, uri)?;
                let last_type = last_fun_param_type_str(&sig)?;
                return lambda_type_first_input(&last_type);
            }
            // Stop at an enclosing lambda — its iteration in the outer loop will handle it.
            "lambda_literal" => return None,
            _ => { cur = parent; }
        }
    }
}

/// For an INLINE lambda argument `fn(a, b, { param -> ... })`:
/// find the enclosing function name and the 0-based position of this lambda,
/// then look up that function parameter's type.
fn inline_lambda_param_type(before_brace: &str, deps: &impl InferDeps, uri: &Url) -> Option<String> {
    // Scan right-to-left to find the nearest unclosed `(`.
    // Convention: `)` increments depth, `(` decrements.  depth < 0 → found it.
    let mut depth: i32 = 0;
    let mut open_paren_byte = None;
    let mut comma_count: usize = 0;

    for (bi, ch) in before_brace.char_indices().rev() {
        match ch {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth < 0 { open_paren_byte = Some(bi); break; }
            }
            ',' if depth == 0 => comma_count += 1,
            _ => {}
        }
    }

    let open_pos = open_paren_byte?;
    let fn_name = last_ident_in(before_brace[..open_pos].trim_end());

    if fn_name.is_empty() { return None; }

    let sig = deps.find_fun_params_text(fn_name, uri)?;
    let param_type = nth_fun_param_type_str(&sig, comma_count)?;
    lambda_type_first_input(&param_type)
}

/// Look up a function by name, find its last parameter's type, and return the
/// first input type if that parameter is a lambda/function type.
///
/// Example: `fun loadProduct(key: K, flow: Flow<T>, map: (ResultState<T>) -> Model)`
/// returns `Some("ResultState")` so that `it` in `loadProduct(...) { it }` resolves.
fn fun_trailing_lambda_it_type(fn_name: &str, deps: &impl InferDeps, uri: &Url) -> Option<String> {
    let sig = deps.find_fun_params_text(fn_name, uri)?;
    let last_type = last_fun_param_type_str(&sig)?;
    lambda_type_first_input(&last_type)
}

/// Like `fun_trailing_lambda_it_type` but for `this`: only returns a type when
/// the trailing lambda parameter is a **receiver lambda** `T.() -> R`.
fn fun_trailing_lambda_this_type(fn_name: &str, deps: &impl InferDeps, uri: &Url) -> Option<String> {
    let sig = deps.find_fun_params_text(fn_name, uri)?;
    let last_type = last_fun_param_type_str(&sig)?;
    lambda_type_receiver(&last_type)
}

// ─── cluster-exclusive pure utilities ────────────────────────────────────────

/// Find the position of the last `.` that is at parenthesis/bracket depth 0
/// (scanning left-to-right so that `fn(Enum.VALUE,` returns None — the dot
/// is at depth 1 inside the argument list).
pub(crate) fn find_last_dot_at_depth_zero(s: &str) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut last_dot: Option<usize> = None;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '.' if depth == 0 => last_dot = Some(i),
            _ => {}
        }
    }
    last_dot
}

#[cfg(test)]
#[path = "it_this_tests.rs"]
mod tests;
