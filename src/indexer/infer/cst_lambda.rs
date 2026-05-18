//! CST-backed lambda context helpers.

use tower_lsp::lsp_types::Url;

use crate::indexer::{Indexer, NodeExt};
use crate::queries::{KIND_CALL_SUFFIX, KIND_LAMBDA_LIT, KIND_VALUE_ARG};
use crate::types::CursorPos;
use crate::StrExt;

use super::super::last_ident_in;
#[cfg(test)]
use super::args::has_named_params_not_it;
use super::args::{extract_first_arg, find_named_param_type_in_sig};
use super::chain::{
    cst_forward_resolve_receiver_type, resolve_callee_chain, resolve_callee_receiver_type,
};
use super::deps::InferDeps;
use super::it_this::LambdaParamKind;
#[cfg(test)]
use super::it_this::IT_SCAN_BACK_LINES;
use super::lambda::{lambda_type_nth_input, RECEIVER_THIS_FNS};
use super::receiver::{
    fun_trailing_lambda_this_type, lambda_receiver_type_from_context,
    lambda_receiver_type_named_arg_ml, resolve_call_params,
};
use super::sig::{last_fun_param_type_str, nth_fun_param_type_str, strip_trailing_call_args};
use super::type_subst::{
    build_ext_fn_type_subst, find_last_dot_at_depth_zero, is_declared_type_param, is_generic_param,
    try_substitute_ext_fn_type_param,
};

/// Tri-state result of classifying a lambda's `this`-receiver context.
///
/// Distinguishes between "receiver lambda with type resolved", "receiver lambda
/// but type not found", and "not a receiver-`this` lambda at all".  The last
/// case is important: in a non-receiver lambda (e.g. `forEach`) `this` refers
/// to the enclosing class, so the walk-up to outer lambdas and the
/// `enclosing_class_at` fallback should still be allowed.
#[derive(Debug)]
pub(crate) enum ThisLambdaCtx {
    /// Receiver-`this` type resolved to the given name.
    Resolved(String),
    /// Lambda is a known receiver context (`apply`/`run`/`with`/indexed
    /// receiver-lambda fn) but the receiver object's type could not be found.
    /// Callers must NOT walk outward or fall back to the enclosing class.
    Receiver,
    /// Not a receiver-`this` lambda (e.g. `forEach`, `map`).
    /// `this` refers to the enclosing class; fallback is valid.
    NotReceiver,
}

/// The result of resolving `this` at the cursor position.
///
/// Returned by [`cst_this_context`] so callers can distinguish the three
/// semantically distinct cases without a second scan.
#[derive(Debug, PartialEq)]
pub(crate) enum ThisContext {
    /// Type resolved — use this string directly.
    Resolved(String),
    /// Cursor is inside a receiver-`this` lambda (`apply`, `run`, `with`, …)
    /// but the receiver object's type could not be determined.
    /// Callers **must not** fall back to `enclosing_class_at`.
    InsideReceiver,
    /// Cursor is not inside any receiver-`this` lambda.
    /// Callers may fall back to `enclosing_class_at`.
    NotFound,
}

/// Classify the `this` receiver context from the text before a lambda `{`.
///
/// Rules:
///  - Case A `receiver.method { this }`: if `method` has an indexed receiver-lambda
///    type → `Resolved`.  If `method` ∈ `RECEIVER_THIS_FNS` (`run`, `apply`):
///    resolve receiver → `Resolved`; if unresolvable → `Receiver`.
///    Other dot-call methods → `NotReceiver`.
///  - Case B `with(receiver) { this }` → `Resolved` or `Receiver`.
///  - Everything else → `NotReceiver`.
pub(crate) fn classify_this_lambda_context(
    before_brace: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> ThisLambdaCtx {
    let trimmed = before_brace.trim_end();
    let callee_raw = strip_trailing_call_args(trimmed).replace("?.", ".");
    let callee = callee_raw.trim();

    // ── Case A: `receiver.method` ────────────────────────────────────────────
    if let Some(dot_pos) = find_last_dot_at_depth_zero(callee) {
        let receiver_expr = callee[..dot_pos].trim_end();
        let receiver_var = last_ident_in(receiver_expr);
        let method = callee[dot_pos + 1..].trim_start().ident_prefix();

        if !receiver_var.is_empty() && !method.is_empty() {
            // Indexed function with a receiver-lambda last param → always Resolved.
            if let Some(ty) = fun_trailing_lambda_this_type(&method, deps, uri) {
                return ThisLambdaCtx::Resolved(ty);
            }
            // Known stdlib scope functions (`run`, `apply`).
            if RECEIVER_THIS_FNS.contains(&method.as_str()) {
                if let Some(raw) = deps.find_var_type(receiver_var, uri) {
                    let base = raw.ident_prefix();
                    if !base.is_empty() {
                        return ThisLambdaCtx::Resolved(base);
                    }
                }
                if receiver_var.starts_with_uppercase() {
                    return ThisLambdaCtx::Resolved(receiver_var.to_owned());
                }
                // In a known scope-fn lambda but type not found.
                return ThisLambdaCtx::Receiver;
            }
        }
        // Other dot-call (forEach, map, …): `this` = enclosing class.
        return ThisLambdaCtx::NotReceiver;
    }

    // ── Case B: `with(receiver) { this }` ───────────────────────────────────
    let trailing_fn = last_ident_in(callee);
    if trailing_fn == "with" {
        if let Some(recv_name) = extract_first_arg(trimmed) {
            if let Some(raw) = deps.find_var_type(recv_name, uri) {
                let base = raw.ident_prefix();
                if !base.is_empty() {
                    return ThisLambdaCtx::Resolved(base);
                }
            }
            let base = recv_name.ident_prefix();
            if base.starts_with_uppercase() {
                return ThisLambdaCtx::Resolved(base);
            }
        }
        return ThisLambdaCtx::Receiver;
    }

    ThisLambdaCtx::NotReceiver
}

/// Return `true` when the text-scan of `lines` around `pos` determines that
/// the cursor is inside a **receiver-`this` lambda** whose receiver type is
/// either resolved or simply not found — either way `this` refers to the
/// lambda receiver, NOT the enclosing class.
///
/// Used in `infer_lambda_param_type_at` to suppress the `enclosing_class_at`
/// fallback when `find_this_element_type_in_lines` returned `None` only
/// because the receiver variable's type couldn't be resolved.
/// Used by tests only — production code uses [`cst_this_context`] via
/// [`crate::indexer::find_this_context_in_lines`] which returns the richer
/// [`ThisContext`] enum and avoids a redundant second scan.
#[cfg(test)]
pub(crate) fn is_inside_receiver_lambda(
    lines: &[String],
    pos: CursorPos,
    idx: &crate::indexer::Indexer,
    uri: &Url,
) -> bool {
    if let Some(doc) = idx.live_doc(uri) {
        if let Some(mut cur) = cursor_node_at(&doc, pos) {
            while let Some(lambda) = cur.enclosing_lambda_literal() {
                if !lambda.has_lambda_named_params(&doc.bytes) {
                    if let Some((before_brace, _)) = lambda_before_brace_context(lambda, &doc) {
                        if !matches!(
                            classify_this_lambda_context(&before_brace, idx, uri),
                            ThisLambdaCtx::NotReceiver
                        ) {
                            return true;
                        }
                    }
                }
                let Some(parent) = lambda.parent() else {
                    break;
                };
                cur = parent;
            }
        }
    }

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
                        return !matches!(
                            classify_this_lambda_context(before_brace, idx, uri),
                            ThisLambdaCtx::NotReceiver
                        );
                    }
                }
                _ => {}
            }
        }
    }
    false
}

pub(super) fn cursor_node_at(
    doc: &crate::indexer::live_tree::LiveDoc,
    pos: CursorPos,
) -> Option<tree_sitter::Node<'_>> {
    use tree_sitter::Point;

    let source = std::str::from_utf8(&doc.bytes).ok()?;
    let line_text = source.lines().nth(pos.line).unwrap_or("");
    let byte_col =
        crate::indexer::live_tree::utf16_col_to_byte(line_text, pos.utf16_col).min(line_text.len());
    let point = Point {
        row: pos.line,
        column: byte_col,
    };
    doc.tree
        .root_node()
        .descendant_for_point_range(point, point)
}

pub(super) fn lambda_before_brace_context(
    lambda: tree_sitter::Node<'_>,
    doc: &crate::indexer::live_tree::LiveDoc,
) -> Option<(String, usize)> {
    let brace_byte = lambda.start_byte();
    let line_start = doc.bytes[..brace_byte]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let before_brace = std::str::from_utf8(&doc.bytes[line_start..brace_byte])
        .ok()?
        .trim_end()
        .to_owned();
    Some((before_brace, lambda.start_position().row))
}

pub(super) fn cst_named_lambda_param_type(
    pos: CursorPos,
    param_name: &str,
    doc: &crate::indexer::live_tree::LiveDoc,
    idx: &Indexer,
    uri: &Url,
) -> Option<String> {
    let mut cur = cursor_node_at(doc, pos)?;
    while let Some(lambda) = cur.enclosing_lambda_literal() {
        if let Some(param_pos) = lambda.lambda_param_position(param_name, &doc.bytes) {
            return cst_lambda_param_type_via_call(doc, &lambda, idx, uri, param_pos);
        }
        let Some(parent) = lambda.parent() else {
            break;
        };
        cur = parent;
    }
    None
}

fn cst_with_receiver_ctx(
    call_expr: tree_sitter::Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<ThisLambdaCtx> {
    let recv_name = call_expr.first_value_argument_text(bytes)?;
    if let Some(raw) = deps.find_var_type(&recv_name, uri) {
        let base = raw.ident_prefix();
        if !base.is_empty() {
            return Some(ThisLambdaCtx::Resolved(base));
        }
    }
    let base = recv_name.ident_prefix();
    if base.starts_with_uppercase() {
        Some(ThisLambdaCtx::Resolved(base))
    } else {
        Some(ThisLambdaCtx::Receiver)
    }
}

/// Walk ancestors from `start_node` and return a [`ThisContext`] that
/// distinguishes resolved types, unresolvable receiver lambdas, and
/// "not inside any receiver lambda" — without requiring a second scan.
///
/// This is the CST fast-path for [`find_this_context_in_lines`] in `it_this`.
pub(super) fn cst_this_context(
    start_node: tree_sitter::Node<'_>,
    doc: &crate::indexer::live_tree::LiveDoc,
    idx: &impl InferDeps,
    uri: &Url,
) -> ThisContext {
    let mut cur = start_node;
    loop {
        if cur.kind() == KIND_LAMBDA_LIT && !cur.has_lambda_named_params(&doc.bytes) {
            let Some((before_brace, _)) = lambda_before_brace_context(cur, doc) else {
                let Some(p) = cur.parent() else { break };
                cur = p;
                continue;
            };

            let ctx = cur
                .enclosing_call_expression()
                .and_then(|call_expr| {
                    (call_expr.call_fn_name(&doc.bytes).as_deref() == Some("with"))
                        .then(|| cst_with_receiver_ctx(call_expr, &doc.bytes, idx, uri))
                        .flatten()
                })
                .unwrap_or_else(|| classify_this_lambda_context(&before_brace, idx, uri));

            match ctx {
                ThisLambdaCtx::Resolved(t) => return ThisContext::Resolved(t),
                ThisLambdaCtx::Receiver => return ThisContext::InsideReceiver,
                ThisLambdaCtx::NotReceiver => {}
            }
        }
        let Some(p) = cur.parent() else { break };
        cur = p;
    }
    ThisContext::NotFound
}

/// Walk ancestors from `start_node` looking for a `lambda_literal` without
/// named params, then infer the `it`/`this` type for that lambda.
///
/// This is the extracted body of the CST fast-path in
/// `find_it_element_type_in_lines_impl`.
pub(super) fn cst_it_or_this_type(
    start_node: tree_sitter::Node<'_>,
    doc: &crate::indexer::live_tree::LiveDoc,
    lines: &[String],
    kind: LambdaParamKind,
    idx: &Indexer,
    uri: &Url,
) -> Option<String> {
    let mut cur = start_node;
    log::debug!(
        "cst_it_or_this_type: start_node kind={}, text={:?}",
        start_node.kind(),
        start_node
            .utf8_text(&doc.bytes)
            .ok()
            .map(|s| s.chars().take(40).collect::<String>())
    );
    loop {
        log::debug!(
            "cst_it_or_this_type: cur kind={} at {:?}",
            cur.kind(),
            cur.start_position()
        );
        if cur.kind() == KIND_LAMBDA_LIT && !cur.has_lambda_named_params(&doc.bytes) {
            let Some((before_brace, ln)) = lambda_before_brace_context(cur, doc) else {
                log::debug!(
                    "cst_it_or_this_type: no before_brace context for lambda at {:?}",
                    cur.start_position()
                );
                let p = cur.parent()?;
                cur = p;
                continue;
            };

            if kind == LambdaParamKind::This {
                let ctx = cur
                    .enclosing_call_expression()
                    .and_then(|call_expr| {
                        (call_expr.call_fn_name(&doc.bytes).as_deref() == Some("with"))
                            .then(|| cst_with_receiver_ctx(call_expr, &doc.bytes, idx, uri))
                            .flatten()
                    })
                    .unwrap_or_else(|| classify_this_lambda_context(&before_brace, idx, uri));
                match ctx {
                    ThisLambdaCtx::Resolved(t) => return Some(t),
                    // Receiver-lambda context but type not found: stop walking up.
                    // `this` here is the receiver, not an outer lambda's receiver.
                    ThisLambdaCtx::Receiver => return None,
                    // Non-receiver lambda (forEach, map…): keep walking outward.
                    // `this` inside these lambdas is the enclosing class / outer receiver.
                    ThisLambdaCtx::NotReceiver => {}
                }
            } else {
                // CST-first: use the unified resolver (no rg spawns, HashMap only).
                log::trace!("cst_it_or_this_type: trying resolve_lambda_param_type_cst");
                if let Some(resolved) = resolve_lambda_param_type_cst(doc, &cur, idx, uri, 0) {
                    log::trace!("cst_it_or_this_type: CST resolved to {resolved}");
                    return Some(resolved);
                }
                log::trace!("cst_it_or_this_type: CST resolver returned None, trying text fallback with before_brace={before_brace:?}");
                // Text fallback for cases the CST resolver can't handle yet
                // (function not indexed, no call_expression parent).
                let result = lambda_receiver_type_from_context(&before_brace, idx, uri)
                    .or_else(|| {
                        lambda_receiver_type_named_arg_ml(&before_brace, 0, lines, ln, idx, uri)
                    })
                    .or_else(|| cst_lambda_param_type_via_call(doc, &cur, idx, uri, 0));
                match result.as_deref() {
                    Some(t) if is_generic_param(t) => {
                        if let Some(concrete) =
                            cst_forward_resolve_receiver_type(&cur, &doc.bytes, idx, uri)
                        {
                            return Some(concrete);
                        }
                    }
                    Some(_) => return result,
                    None => {}
                }
            }
        }
        let p = cur.parent()?;
        cur = p;
    }
}

/// Unified CST-based lambda parameter type resolver.
///
/// Given a `lambda_literal` node and a parameter position (0 for `it`),
/// resolves the concrete type by:
///   1. Walking parents to find enclosing call_expression + lambda position
///   2. Looking up function params (receiver-aware)
///   3. Extracting the param type at the lambda's position
///   4. If the extracted type is a declared generic type param, resolving the
///      concrete receiver via `resolve_callee_chain` + `forward_resolve_segments`
///      and applying extension function type substitution
///
/// This replaces the fragmented text+CST hybrid in `cst_lambda_param_type_via_call`.
fn resolve_lambda_param_type_cst(
    doc: &crate::indexer::live_tree::LiveDoc,
    lambda: &tree_sitter::Node<'_>,
    deps: &impl InferDeps,
    uri: &Url,
    param_pos: usize,
) -> Option<String> {
    let bytes = &doc.bytes;

    // Step 1: Walk parents to find (call_expression, lambda_position).
    let (call_expr, raw_param_type) = find_enclosing_call_and_param(lambda, bytes, deps, uri)?;

    // Step 2: Extract the lambda input type at the requested position.
    let extracted = lambda_type_nth_input(&raw_param_type, param_pos)?;

    // Step 3: Check if it's a declared generic type param.
    let fn_name = call_expr.call_fn_name(bytes)?;
    log::debug!("resolve_lambda_param_type_cst: fn_name={fn_name}, raw_param_type={raw_param_type}, extracted={extracted}");
    let info = deps.find_fun_callable_info(&fn_name, uri);
    let is_generic = match &info {
        Some(ci) => is_declared_type_param(&extracted, &ci.type_params),
        None => is_generic_param(&extracted),
    };
    log::debug!(
        "resolve_lambda_param_type_cst: is_generic={is_generic}, info={:?}",
        info.as_ref()
            .map(|i| (&i.type_params, &i.extension_receiver_type))
    );
    if !is_generic {
        return Some(extracted);
    }

    // Step 4: Resolve the concrete receiver type via CST chain resolution.
    let info = info?;
    if info.extension_receiver_type.is_empty() {
        // Not an extension function — try scope-function fallback.
        log::debug!("resolve_lambda_param_type_cst: not ext fn, trying scope-function fallback");
        return resolve_callee_receiver_type(&call_expr, bytes, deps, uri);
    }

    let callee = call_expr.child(0)?;
    let (receiver_type, _final_method) = resolve_callee_chain(callee, bytes, deps, uri)?;
    log::debug!("resolve_lambda_param_type_cst: receiver_type={receiver_type}");

    // Step 5: Build substitution map and apply.
    let subst = build_ext_fn_type_subst(
        &info.extension_receiver_type,
        &receiver_type,
        &info.type_params,
    );
    log::debug!("resolve_lambda_param_type_cst: subst={subst:?}");
    subst.get(&extracted).cloned().or(Some(extracted))
}

/// Walk parent nodes from a lambda to find the enclosing call_expression and
/// the raw parameter type string for the lambda's position.
///
/// Reuses the parent-walking logic from `cst_lambda_call_param_type` (handles
/// VALUE_ARG, CALL_SUFFIX, nested LAMBDA_LIT boundaries).
fn find_enclosing_call_and_param<'a>(
    lambda: &tree_sitter::Node<'a>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<(tree_sitter::Node<'a>, String)> {
    let mut cur = *lambda;
    loop {
        let parent = cur.parent()?;
        let kind = parent.kind();
        log::debug!(
            "find_enclosing_call_and_param: parent kind={kind} at {:?}",
            parent.start_position()
        );
        if kind == KIND_VALUE_ARG {
            let call_expr = parent.enclosing_call_expression()?;
            let sig = receiver_aware_params(call_expr, bytes, deps, uri).or_else(|| {
                let fn_name = call_expr.call_fn_name(bytes)?;
                deps.find_fun_params_text(&fn_name, uri)
            })?;
            let param_type = if let Some(label) = parent.named_arg_label(bytes) {
                find_named_param_type_in_sig(&sig, &label)?
            } else {
                nth_fun_param_type_str(&sig, parent.value_arg_position())?.to_owned()
            };
            return Some((call_expr, param_type));
        }
        if kind == KIND_CALL_SUFFIX {
            let call_expr = lambda.enclosing_call_expression()?;
            log::debug!(
                "find_enclosing_call_and_param: call_suffix, call_expr fn={:?}",
                call_expr.call_fn_name(bytes)
            );
            let sig = receiver_aware_params(call_expr, bytes, deps, uri).or_else(|| {
                let fn_name = call_expr.call_fn_name(bytes)?;
                log::debug!(
                    "find_enclosing_call_and_param: looking up params for fn_name={fn_name}"
                );
                deps.find_fun_params_text(&fn_name, uri)
            })?;
            log::debug!("find_enclosing_call_and_param: sig={sig}");
            let last_type = last_fun_param_type_str(&sig)?;
            return Some((call_expr, last_type.to_owned()));
        }
        if kind == KIND_LAMBDA_LIT {
            return None;
        }
        cur = parent;
    }
}

/// CST structural fallback for lambda params: given a `lambda_literal` node,
/// walk up the call tree to find the enclosing function call and the lambda's
/// parameter type, then pick the requested lambda input position.
pub(super) fn cst_lambda_param_type_via_call(
    doc: &crate::indexer::live_tree::LiveDoc,
    lambda: &tree_sitter::Node<'_>,
    deps: &impl InferDeps,
    uri: &Url,
    param_pos: usize,
) -> Option<String> {
    let result = cst_lambda_call_param_type(doc, lambda, deps, uri);
    match result {
        Some(param_type) => {
            let extracted = lambda_type_nth_input(&param_type, param_pos)?;
            if is_generic_param(&extracted) {
                // Generic param (T/R/E) — resolve via forward chain walk.
                return cst_forward_resolve_receiver_type(lambda, &doc.bytes, deps, uri);
            }
            // Check longer generic param names (e.g. EffectType, StateType) against
            // the function's declared type params.
            if let Some(call_expr) = lambda.enclosing_call_expression() {
                if let Some(fn_name) = call_expr.call_fn_name(&doc.bytes) {
                    let before = cst_before_open_text(call_expr, doc);
                    if let Some(concrete) =
                        try_substitute_ext_fn_type_param(&extracted, &fn_name, &before, deps, uri)
                    {
                        return Some(concrete);
                    }
                }
            }
            Some(extracted)
        }
        None => {
            // Function not indexed — resolve via forward chain walk
            // (handles scope functions and dotted receivers).
            cst_forward_resolve_receiver_type(lambda, &doc.bytes, deps, uri)
        }
    }
}

fn cst_lambda_call_param_type(
    doc: &crate::indexer::live_tree::LiveDoc,
    lambda: &tree_sitter::Node<'_>,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let bytes = &doc.bytes;
    let mut cur = *lambda;

    loop {
        let parent = cur.parent()?;
        let kind = parent.kind();
        if kind == KIND_VALUE_ARG {
            let call_expr = parent.enclosing_call_expression()?;
            let sig = receiver_aware_params(call_expr, bytes, deps, uri)?;
            return if let Some(label) = parent.named_arg_label(bytes) {
                find_named_param_type_in_sig(&sig, &label)
            } else {
                nth_fun_param_type_str(&sig, parent.value_arg_position())
            };
        }
        if kind == KIND_CALL_SUFFIX {
            let call_expr = lambda.enclosing_call_expression()?;
            let sig = receiver_aware_params(call_expr, bytes, deps, uri)?;
            let last_type = last_fun_param_type_str(&sig)?;
            return Some(last_type.to_owned());
        }
        if kind == KIND_LAMBDA_LIT {
            return None;
        }
        cur = parent;
    }
}

/// Extract the text before the opening `(` of a call expression from the CST.
/// Used to provide the `before_open` argument for `try_substitute_ext_fn_type_param`.
pub(super) fn cst_before_open_text(
    call_expr: tree_sitter::Node<'_>,
    doc: &crate::indexer::live_tree::LiveDoc,
) -> String {
    // The callee is the first child of the call expression (before call_suffix).
    let Some(callee) = call_expr.child(0) else {
        return String::new();
    };
    let start = callee.start_byte();
    let end = callee.end_byte();
    std::str::from_utf8(&doc.bytes[start..end])
        .unwrap_or("")
        .to_owned()
}

// ─── Generic extension function type substitution ────────────────────────────

fn receiver_aware_params(
    call_expr: tree_sitter::Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let (fn_name, qualifier) = call_expr.call_fn_and_qualifier(bytes)?;
    let recv_type = qualifier
        .as_deref()
        .and_then(|v| deps.find_var_type(v, uri));
    resolve_call_params(&fn_name, recv_type.as_deref(), deps, uri)
}
