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
    collect_all_fun_params_texts,
    extract_first_arg,
    // args.rs
    extract_named_arg_name,
    // sig.rs
    find_fun_signature_full,
    find_named_param_type_in_sig,
    has_named_params_not_it,
    lambda_type_first_input,
    lambda_type_nth_input,
    lambda_type_receiver,
    last_fun_param_type_str,
    nth_fun_param_type_str,
    strip_trailing_call_args,
    // deps.rs
    InferDeps,
    // lambda.rs
    RECEIVER_THIS_FNS,
};

use crate::indexer::NodeExt;
use crate::queries::{
    KIND_CALL_EXPR, KIND_CALL_SUFFIX, KIND_CLASS_DECL, KIND_LAMBDA_LIT, KIND_NAV_EXPR,
    KIND_NAV_SUFFIX, KIND_OBJECT_DECL, KIND_SIMPLE_IDENT, KIND_TYPE_IDENT, KIND_VALUE_ARG,
};
use crate::resolver::{extract_collection_element_type, infer_lines::first_type_arg};
use crate::StrExt;

// ── from indexer.rs (parent of infer; descendants can access private items) ──
use super::super::{find_enclosing_call_name, last_ident_in, Indexer};

use crate::types::CursorPos;

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
enum LambdaParamKind {
    /// Infer the type of `it` (the implicit element parameter).
    It,
    /// Infer the type of `this` (the receiver in a receiver lambda).
    This,
}

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

pub(crate) fn find_this_element_type_in_lines(
    lines: &[String],
    pos: CursorPos,
    idx: &Indexer,
    uri: &Url,
) -> Option<String> {
    find_it_element_type_in_lines_impl(lines, pos, idx, uri, LambdaParamKind::This)
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
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let trimmed = before_brace.trim_end();

    // If there's an unclosed `(` in before_brace, the lambda is an inline
    // argument (not a trailing lambda).  Prioritize positional param lookup.
    if has_unclosed_paren(trimmed) {
        if let result @ Some(_) = inline_lambda_param_type(trimmed, deps, uri) {
            return result;
        }
    }

    let callee = normalized_lambda_callee(trimmed);

    receiver_dot_lambda_type(&callee, deps, uri)
        .or_else(|| plain_trailing_lambda_type(trimmed, &callee, deps, uri))
        .or_else(|| inline_lambda_param_type(trimmed, deps, uri))
}

fn normalized_lambda_callee(before_brace: &str) -> String {
    strip_trailing_call_args(before_brace)
        .replace("?.", ".")
        .trim()
        .to_owned()
}

/// Returns true when `s` contains more `(` than `)` — indicating the lambda
/// sits inside a function's argument list (inline lambda, not trailing).
fn has_unclosed_paren(s: &str) -> bool {
    let mut depth: i32 = 0;
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
    }
    depth > 0
}

fn receiver_dot_lambda_type(callee: &str, deps: &impl InferDeps, uri: &Url) -> Option<String> {
    let dot_pos = find_last_dot_at_depth_zero(callee)?;
    let receiver_expr = callee[..dot_pos].trim_end();
    let receiver_var = last_ident_in(strip_trailing_call_args(receiver_expr));
    let method = callee[dot_pos + 1..].trim_start().ident_prefix();
    if receiver_var.is_empty() {
        return None;
    }

    receiver_var_lambda_type(receiver_var, receiver_expr, &method, deps, uri)
        .or_else(|| uppercase_name(receiver_var))
}

fn receiver_var_lambda_type(
    receiver_var: &str,
    receiver_expr: &str,
    method: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    direct_receiver_lambda_type(receiver_var, method, deps, uri)
        .or_else(|| nested_receiver_lambda_type(receiver_expr, method, deps, uri))
        .or_else(|| chain_with_type_subst(receiver_expr, method, deps, uri))
        .or_else(|| method_chain_lambda_type(receiver_var, method, deps, uri))
}

fn direct_receiver_lambda_type(
    receiver_var: &str,
    method: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let raw = deps.find_var_type(receiver_var, uri)?;
    inferred_receiver_lambda_type(&raw, method, deps, uri)
}

fn nested_receiver_lambda_type(
    receiver_expr: &str,
    method: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let (outer_var, field) = receiver_outer_field(receiver_expr)?;
    let outer_type = deps.find_var_type(outer_var, uri)?;
    let outer_dotted = outer_type.dotted_ident_prefix();
    let outer_base = outer_dotted.last_segment();
    if outer_base.is_empty() {
        return None;
    }
    let field_raw = deps.find_field_type(outer_base, field)?;
    inferred_receiver_lambda_type(&field_raw, method, deps, uri)
}

fn receiver_outer_field(receiver_expr: &str) -> Option<(&str, &str)> {
    let dot = receiver_expr.rfind('.')?;
    let outer_var = last_ident_in(&receiver_expr[..dot]);
    let field = &receiver_expr[dot + 1..];
    if outer_var.is_empty() || field.is_empty() {
        return None;
    }
    Some((outer_var, field))
}

fn method_chain_lambda_type(
    receiver_var: &str,
    method: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let ret_raw = deps.find_fun_return_type(receiver_var)?;
    inferred_receiver_lambda_type(&ret_raw, method, deps, uri)
}

/// Handles chains like `wrapper.getOrNull()?.also { p -> }` and
/// `state.value.getOrNull()?.also { p -> }` where the scope function's
/// lambda parameter type comes from a generic method return type that needs
/// to be resolved against the receiver's concrete type arguments.
///
/// Strategy:
/// 1. Strip the final method call and resolve the intermediate expression to
///    its raw type (with generics, substituting along field access paths).
/// 2. If the final method's return type is indexed, apply a type parameter
///    substitution map built from the intermediate type's concrete type args.
/// 3. Fall back to extracting the first concrete type argument when the method
///    is not indexed (e.g. stdlib not in `sourcePaths`).
fn chain_with_type_subst(
    receiver_expr: &str,
    method: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let without_call = strip_trailing_call_args(receiver_expr);
    let dot = find_last_dot_at_depth_zero(without_call)?;
    let intermediate = without_call[..dot].trim();
    let final_method = without_call[dot + 1..].trim().ident_prefix();
    if intermediate.is_empty() || final_method.is_empty() {
        return None;
    }
    let intermediate_type_raw = resolve_expr_type_raw(intermediate, deps, uri)?;
    let intermediate_dotted = intermediate_type_raw.dotted_ident_prefix();
    let intermediate_base = intermediate_dotted.last_segment().to_owned();
    if intermediate_base.is_empty() {
        return None;
    }
    let concrete_type = if let Some(method_return) =
        deps.find_method_return_type_for_type(&intermediate_base, &final_method)
    {
        let subst = build_type_arg_subst(deps, &intermediate_base, &intermediate_type_raw);
        let applied = crate::indexer::apply_type_subst(&method_return, &subst);
        let base = applied.trim_end_matches('?').ident_prefix();
        if base.is_empty() || is_generic_param(&base) {
            first_concrete_type_arg_str(&intermediate_type_raw)?
        } else {
            base
        }
    } else {
        first_concrete_type_arg_str(&intermediate_type_raw)?
    };
    // We already resolved a concrete type through substitution — return it
    // directly for the scope function's lambda param.  Calling
    // `inferred_receiver_lambda_type` here would re-lookup the method signature
    // and get a raw generic (T) from stdlib, shadowing our resolved type.
    if concrete_type.starts_with_uppercase() && !is_generic_param(&concrete_type) {
        return Some(concrete_type);
    }
    inferred_receiver_lambda_type(&concrete_type, method, deps, uri)
}

/// Resolve a simple variable or `var.field` expression to its raw type string,
/// preserving generic parameters and applying type parameter substitution along
/// field-access paths.
///
/// Examples:
/// - `"resultWrapped"` → `"Result<FamilyAccount>"`
/// - `"resultState.value"` where `resultState: ResultState<Account>`,
///   `value: Result<T>` → `"Result<Account>"` (T substituted via `ResultState`'s params)
fn resolve_expr_type_raw(expr: &str, deps: &impl InferDeps, uri: &Url) -> Option<String> {
    if !expr.contains('.') {
        return deps.find_var_type(expr, uri);
    }
    let dot = find_last_dot_at_depth_zero(expr)?;
    let outer_var = last_ident_in(expr[..dot].trim_end());
    let field = expr[dot + 1..].trim_start().ident_prefix();
    if outer_var.is_empty() || field.is_empty() {
        return None;
    }
    let outer_type = deps.find_var_type(outer_var, uri)?;
    let outer_dotted = outer_type.dotted_ident_prefix();
    let outer_base = outer_dotted.last_segment();
    let raw_field = deps.find_field_type(outer_base, &field)?;
    let subst = build_type_arg_subst(deps, outer_base, &outer_type);
    Some(crate::indexer::apply_type_subst(&raw_field, &subst))
}

/// Build a type-parameter substitution map from a concrete instantiation.
///
/// Looks up the declared type parameters of `class_name` (e.g. `["T"]` for
/// `Result<T>`), then zips them with the type arguments extracted from
/// `concrete_type` (e.g. `"Result<FamilyAccount>"`) to build the map
/// `{"T" → "FamilyAccount"}`.
///
/// Returns an empty map when the class params are unknown or the concrete type
/// carries no type arguments.
fn build_type_arg_subst(
    deps: &impl InferDeps,
    class_name: &str,
    concrete_type: &str,
) -> std::collections::HashMap<String, String> {
    let type_params = deps.find_class_type_params(class_name);
    if type_params.is_empty() {
        return std::collections::HashMap::new();
    }
    let Some(inner) = type_args_inner(concrete_type) else {
        return std::collections::HashMap::new();
    };
    let type_args: Vec<String> = split_top_level_commas(inner)
        .into_iter()
        .map(|s| s.trim().trim_end_matches('?').to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    type_params.into_iter().zip(type_args).collect()
}

/// Extract the content between the outermost `<` and `>` of a generic type.
fn type_args_inner(ty: &str) -> Option<&str> {
    let open = ty.find('<')?;
    let close = ty.rfind('>')?;
    if close <= open {
        return None;
    }
    Some(&ty[open + 1..close])
}

/// Split a generic parameter list at top-level commas, respecting nested `<>`.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                result.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    result.push(&s[start..]);
    result
}

/// Build a type-parameter substitution map for a generic extension function by
/// recursively matching the declared receiver type against the concrete receiver type.
///
/// Example: for `collectState` with
///   declared receiver `Flow<ReducedResult<EffectType, StateType>>`
///   concrete receiver `Flow<ReducedResult<BuildingSavingsEffect, SheetState>>`
///   fun type params `[EffectType, StateType, VMState, VMEffect]`
/// → returns `{EffectType → BuildingSavingsEffect, StateType → SheetState}`
fn build_ext_fn_type_subst(
    declared_receiver: &str,
    concrete_receiver: &str,
    fun_type_params: &[String],
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    match_type_args_recursive(
        declared_receiver,
        concrete_receiver,
        fun_type_params,
        &mut map,
    );
    map
}

/// Recursively match declared and concrete type structures to extract
/// generic parameter → concrete type mappings.
fn match_type_args_recursive(
    declared: &str,
    concrete: &str,
    params: &[String],
    map: &mut std::collections::HashMap<String, String>,
) {
    let decl_base = declared.trim().trim_end_matches('?');
    let conc_base = concrete.trim().trim_end_matches('?');
    if decl_base.is_empty() || conc_base.is_empty() {
        return;
    }
    // If the declared type is one of the function's type params, map it directly.
    if params.iter().any(|p| p == decl_base) {
        map.insert(decl_base.to_owned(), conc_base.to_owned());
        return;
    }
    // Otherwise recurse into type arguments.
    if let (Some(d_inner), Some(c_inner)) = (type_args_inner(decl_base), type_args_inner(conc_base))
    {
        let d_args = split_top_level_commas(d_inner);
        let c_args = split_top_level_commas(c_inner);
        for (d, c) in d_args.iter().zip(c_args.iter()) {
            match_type_args_recursive(d, c, params, map);
        }
    }
}

/// Check whether `name` is a declared type parameter of the given function.
fn is_declared_type_param(name: &str, fun_type_params: &[String]) -> bool {
    fun_type_params.iter().any(|p| p == name)
}

/// Returns `true` if `name` looks like a generic type parameter: a short
/// all-uppercase identifier like `T`, `R`, `IN`, `OUT`, `KEY`, `VAL`.
fn is_generic_param(name: &str) -> bool {
    !name.is_empty() && name.len() <= 3 && name.chars().all(|c| c.is_uppercase())
}

/// Extract the first type argument from `ty` and return it only if it is a
/// concrete class name (i.e. not a generic type parameter like `T`, `R`, `IN`).
fn first_concrete_type_arg_str(ty: &str) -> Option<String> {
    let inner = type_args_inner(ty)?;
    let arg = first_type_arg(inner).trim().trim_matches('?');
    let base = arg.ident_prefix();
    if base.is_empty() || is_generic_param(&base) {
        return None;
    }
    Some(base.to_owned())
}

/// Like `first_concrete_type_arg_str` but **preserves** the full generic type args on the
/// result (e.g. `"Optional<FamilyAccount>"` instead of `"Optional"`).
///
/// Used as the fallback in `resolve_member_type_on` when type-param substitution fails
/// because the class params are not in the index — in that case we must return the full
/// type argument so that downstream resolution can continue walking the chain.
fn first_type_arg_raw(ty: &str) -> Option<String> {
    let inner = type_args_inner(ty)?;
    let arg = first_type_arg(inner).trim().trim_matches('?');
    let base = arg.ident_prefix();
    if base.is_empty() || is_generic_param(&base) {
        return None;
    }
    Some(arg.to_owned())
}

/// Kotlin stdlib scope functions whose `T` parameter IS the receiver type.
const SCOPE_FUNCTIONS: &[&str] = &["let", "also", "run", "apply", "takeIf", "takeUnless"];

fn inferred_receiver_lambda_type(
    raw_type: &str,
    method: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    extract_collection_element_type(raw_type)
        .or_else(|| {
            let from_sig = method_lambda_input_type_aware(raw_type, method, deps, uri);
            match from_sig.as_deref() {
                Some(t) if is_generic_param(t) && SCOPE_FUNCTIONS.contains(&method) => {
                    uppercase_ident_prefix(raw_type).or(from_sig)
                }
                _ => from_sig,
            }
        })
        .or_else(|| uppercase_ident_prefix(raw_type))
}

/// Receiver-aware trailing lambda type: look up `method` on receiver's type,
/// find its last param, extract lambda input type.
fn method_lambda_input_type_aware(
    raw_type: &str,
    method: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    if method.is_empty() {
        return None;
    }
    let sig = resolve_call_params(method, Some(raw_type), deps, uri)?;
    let last_type = last_fun_param_type_str(&sig)?;
    lambda_type_first_input(&last_type)
}

fn uppercase_ident_prefix(raw: &str) -> Option<String> {
    let base = raw.ident_prefix();
    if base.is_empty() || is_generic_param(&base) {
        return None;
    }
    uppercase_name(&base)
}

fn uppercase_name(name: &str) -> Option<String> {
    name.starts_with_uppercase().then(|| name.to_owned())
}

fn plain_trailing_lambda_type(
    before_brace: &str,
    callee: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let trailing_fn = last_ident_in(callee);
    if trailing_fn.is_empty() {
        return None;
    }

    with_receiver_lambda_type(trailing_fn, before_brace, deps, uri)
        .or_else(|| fun_trailing_lambda_it_type(trailing_fn, deps, uri))
}

fn with_receiver_lambda_type(
    trailing_fn: &str,
    before_brace: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    if trailing_fn != "with" {
        return None;
    }
    let recv_name = extract_first_arg(before_brace)?;
    deps.find_var_type(recv_name, uri)
        .and_then(|raw| uppercase_ident_prefix(&raw))
        .or_else(|| uppercase_ident_prefix(recv_name))
}

// ─── private helpers ─────────────────────────────────────────────────────────

fn cursor_node_at(
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

fn lambda_before_brace_context(
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

fn cst_named_lambda_param_type(
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

/// Walk ancestors from `start_node` looking for a `lambda_literal` without
/// named params, then infer the `it`/`this` type for that lambda.
///
/// This is the extracted body of the CST fast-path in
/// `find_it_element_type_in_lines_impl`.
fn cst_it_or_this_type(
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
                            return lambda_receiver_this_type_from_context(before_brace, idx, uri);
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

/// Thin wrapper kept for callers that only need `Option<String>`.
fn lambda_receiver_this_type_from_context(
    before_brace: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    match classify_this_lambda_context(before_brace, deps, uri) {
        ThisLambdaCtx::Resolved(ty) => Some(ty),
        _ => None,
    }
}

/// Return `true` when the text-scan of `lines` around `pos` determines that
/// the cursor is inside a **receiver-`this` lambda** whose receiver type is
/// either resolved or simply not found — either way `this` refers to the
/// lambda receiver, NOT the enclosing class.
///
/// Used in `infer_lambda_param_type_at` to suppress the `enclosing_class_at`
/// fallback when `find_this_element_type_in_lines` returned `None` only
/// because the receiver variable's type couldn't be resolved.
pub(crate) fn is_inside_receiver_lambda(
    lines: &[String],
    pos: CursorPos,
    deps: &impl InferDeps,
    uri: &Url,
) -> bool {
    if let Some(doc) = deps.live_doc(uri) {
        if let Some(mut cur) = cursor_node_at(&doc, pos) {
            while let Some(lambda) = cur.enclosing_lambda_literal() {
                if !lambda.has_lambda_named_params(&doc.bytes) {
                    if let Some((before_brace, _)) = lambda_before_brace_context(lambda, &doc) {
                        if !matches!(
                            classify_this_lambda_context(&before_brace, deps, uri),
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
                            classify_this_lambda_context(before_brace, deps, uri),
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

/// Handles named-arg lambdas spread across multiple lines:
/// opener like `  buildingSavings = ` or `  loan = ` spread across multiple
/// lines (the enclosing `(` is on a previous line).
///
/// Returns `Some(type_name)` for the Nth input type of the parameter's functional
/// type, where N = `lambda_param_pos` (0-based position of the named param in the
/// multi-param lambda, e.g. `{ loanId, isWustenrot -> }` → loanId=0, isWustenrot=1).
fn lambda_receiver_type_named_arg_ml(
    before_brace: &str,
    lambda_param_pos: usize,
    lines: &[String],
    line_no: usize,
    idx: &Indexer,
    uri: &Url,
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
        // Find outer class file; index-only lookup (no rg — this runs on the
        // inlay-hints hot path where spawning rg would cause timeouts).
        let outer_file: Option<String> = {
            let locs = idx.resolve_symbol_no_rg(outer, uri);
            if locs.is_empty() {
                // Not indexed yet — submit for background enrichment.
                idx.submit_enrichment(outer);
            }
            locs.first().map(|l| l.uri.to_string())
        };
        if let Some(file_uri) = outer_file {
            // Try ALL symbols named `fn_name` in the outer-class file — the file
            // may have multiple same-named nested classes (e.g. two `SheetReloadActions`
            // in different reducers).  Pick the first one whose params contain `named_arg`.
            let sigs = collect_all_fun_params_texts(fn_name, &file_uri, idx);
            let found = sigs
                .into_iter()
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

/// Resolve the receiver type that flows into a call expression's lambda.
/// For scope functions, this is the type of the expression before `.let`/`.also`/etc.
fn resolve_callee_receiver_type(
    call_expr: &tree_sitter::Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let callee = call_expr.child(0)?;
    let (receiver_type, final_method) = resolve_callee_chain(callee, bytes, deps, uri)?;
    if SCOPE_FUNCTIONS.contains(&final_method.as_str()) {
        return Some(receiver_type);
    }
    None
}

/// CST structural fallback for lambda params: given a `lambda_literal` node,
/// walk up the call tree to find the enclosing function call and the lambda's
/// parameter type, then pick the requested lambda input position.
fn cst_lambda_param_type_via_call(
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

/// Forward-walk chain resolution for the receiver type of a lambda's enclosing
/// call expression. Given `a.b.method { lambda }`, resolves left-to-right:
///   1. Find root identifier (`a`) → resolve its type
///   2. Walk through each navigation suffix (`.b`, `.method`) tracking the type
///   3. Return the type of the expression just before the final method call
///
/// This handles arbitrary chains like `settings.familyCreationDate?.let { }`
/// without backward heuristics.
fn cst_forward_resolve_receiver_type(
    lambda: &tree_sitter::Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let call_expr = lambda.enclosing_call_expression()?;
    let callee = call_expr.child(0)?;

    // Collect the chain segments: (root_node, [suffix_member_names])
    // For `settings.familyCreationDate?.let`, we get:
    //   root = "settings", segments = ["familyCreationDate", "let"]
    let (root_type, final_method) = resolve_callee_chain(callee, bytes, deps, uri)?;

    // For scope functions, `it` type is the receiver type.
    // For collection methods (map/filter/etc), we'd need more complex logic.
    if SCOPE_FUNCTIONS.contains(&final_method.as_str()) {
        return Some(root_type);
    }
    None
}

/// Resolve the callee navigation chain left-to-right, returning the type
/// of the expression before the final method call, and the final method name.
///
/// For `settings.familyCreationDate?.let`:
///   - root = "settings" → type "IFamilySettings"
///   - ".familyCreationDate" → type "Long" (field on IFamilySettings)
///   - returns ("Long", "let")
fn resolve_callee_chain(
    callee: tree_sitter::Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<(String, String)> {
    match callee.kind() {
        k if k == KIND_NAV_EXPR => {
            let segments = collect_nav_segments(callee, bytes);
            if segments.is_empty() {
                return None;
            }
            forward_resolve_segments(&segments, bytes, deps, uri)
        }
        k if k == KIND_SIMPLE_IDENT || k == KIND_TYPE_IDENT => {
            let name = callee.utf8_text_owned(bytes)?;
            None.or_else(|| {
                let _ = name;
                None
            })
        }
        _ => None,
    }
}

/// A segment in a navigation chain: either a root identifier or a suffix member.
#[derive(Debug)]
enum NavSegment<'a> {
    /// Root identifier node (leftmost expression in the chain)
    Root(tree_sitter::Node<'a>),
    /// A navigation suffix member name (the identifier after `.` or `?.`).
    /// `safe_call` is true for `?.` navigation (strips nullability).
    Suffix { name: String, safe_call: bool },
    /// A call_expression intermediate (e.g. previous `.let { }` in a chain)
    CallExpr(tree_sitter::Node<'a>),
}

/// Collect navigation segments from a navigation_expression tree, left to right.
/// The structure is nested: `(a.b).c` is nav_expr(nav_expr(a, .b), .c)
fn collect_nav_segments<'a>(node: tree_sitter::Node<'a>, bytes: &[u8]) -> Vec<NavSegment<'a>> {
    let mut segments = Vec::new();
    collect_nav_segments_recursive(node, bytes, &mut segments);
    segments
}

fn collect_nav_segments_recursive<'a>(
    node: tree_sitter::Node<'a>,
    bytes: &[u8],
    segments: &mut Vec<NavSegment<'a>>,
) {
    if node.kind() != KIND_NAV_EXPR {
        // Base case: not a navigation expression
        segments.push(NavSegment::Root(node));
        return;
    }

    // Left child: either another nav_expr, call_expr, or identifier
    if let Some(left) = node.named_child(0) {
        match left.kind() {
            k if k == KIND_NAV_EXPR => {
                collect_nav_segments_recursive(left, bytes, segments);
            }
            k if k == KIND_CALL_EXPR => {
                // Intermediate call expression (e.g. `a.let { }.let { }`)
                // Recurse into its callee to get the chain up to that point
                if let Some(inner_callee) = left.child(0) {
                    collect_nav_segments_recursive(inner_callee, bytes, segments);
                }
                segments.push(NavSegment::CallExpr(left));
            }
            _ => {
                segments.push(NavSegment::Root(left));
            }
        }
    }

    // Right child: navigation_suffix → extract member name
    if let Some(suffix) = node.first_child_of_kind(KIND_NAV_SUFFIX) {
        // Detect safe-call `?.` by checking the raw text of the suffix node.
        let suffix_text = suffix.utf8_text(bytes).unwrap_or("");
        let is_safe = suffix_text.starts_with("?.");
        let mut suffix_cursor = suffix.walk();
        let member = suffix
            .children(&mut suffix_cursor)
            .find(|child| {
                let kind = child.kind();
                kind == KIND_SIMPLE_IDENT || kind == KIND_TYPE_IDENT
            })
            .and_then(|child| child.utf8_text_owned(bytes));
        if let Some(name) = member {
            segments.push(NavSegment::Suffix {
                name,
                safe_call: is_safe,
            });
        }
    }
}

/// Forward-resolve a chain of segments to get (receiver_type_before_last, last_method_name).
fn forward_resolve_segments(
    segments: &[NavSegment<'_>],
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<(String, String)> {
    if segments.is_empty() {
        return None;
    }

    let mut current_type: Option<String> = None;
    let mut last_suffix: Option<String> = None;

    for segment in segments {
        match segment {
            NavSegment::Root(node) => {
                current_type = resolve_root_node_type(*node, bytes, deps, uri);
                // If the root is a call_expression, record its fn name so that
                // a subsequent CallExpr for the same call (trailing-lambda wrapper)
                // is recognized as redundant by the dedup check below.
                if node.kind() == KIND_CALL_EXPR {
                    last_suffix = node.call_fn_name(bytes);
                }
            }
            NavSegment::Suffix {
                ref name,
                safe_call,
            } => {
                if *safe_call {
                    if let Some(ref mut t) = current_type {
                        if t.ends_with('?') {
                            t.pop();
                        }
                    }
                }
                if let Some(ref cur) = current_type {
                    if let Some(resolved) = resolve_member_type_on(cur, name, deps) {
                        current_type = Some(resolved);
                    } else if SCOPE_FUNCTIONS.contains(&name.as_str()) {
                        // Scope function: receiver type flows through.
                    }
                }
                last_suffix = Some(name.clone());
            }
            NavSegment::CallExpr(call_node) => {
                let fn_name = call_node.call_fn_name(bytes);
                if let Some(ref name) = fn_name {
                    if SCOPE_FUNCTIONS.contains(&name.as_str()) {
                        continue;
                    }
                    // If the preceding Suffix already resolved this method's return type,
                    // the CallExpr is redundant — skip re-resolution.
                    if last_suffix.as_deref() == Some(name.as_str()) {
                        continue;
                    }
                    if let Some(ref cur) = current_type {
                        if let Some(resolved) = resolve_member_type_on(cur, name, deps) {
                            current_type = Some(resolved);
                            continue;
                        }
                    }
                    if let Some(ret_ty) = deps.find_fun_return_type(name) {
                        current_type = Some(ret_ty);
                    } else if let Some(class_name) = enclosing_class_name(*call_node, bytes) {
                        if let Some(ret_ty) =
                            deps.find_method_return_type_for_type(&class_name, name)
                        {
                            current_type = Some(ret_ty);
                        }
                    }
                }
            }
        }
    }

    // Return (type_before_last_suffix, last_suffix_name)
    let method = last_suffix?;
    let receiver_type = current_type?;
    Some((receiver_type, method))
}

/// Given a current receiver type string, resolve a member access (field or method) and
/// return the resulting type with type substitution applied.
///
/// When `build_type_arg_subst` returns an empty map because the class type params are
/// not in the index, `apply_type_subst` leaves the generic placeholder (e.g. `T`) intact.
/// In that case we fall back to `first_concrete_type_arg_str` — the same strategy used
/// by the text path in `chain_with_type_subst` — to extract the first concrete type
/// argument from `current_type`.  This prevents `:T` from leaking through as a hover
/// result for chains like `resultState.value.getOrNull()?.also { param -> }` when
/// `ResultState.Success` type params are not indexed.
fn resolve_member_type_on(
    current_type: &str,
    member: &str,
    deps: &impl InferDeps,
) -> Option<String> {
    let type_name = current_type.dotted_ident_prefix();
    let type_base = type_name.last_segment();
    let effective_type = if !type_base.is_empty() && type_base.starts_with_uppercase() {
        type_base.to_owned()
    } else if !type_base.is_empty() {
        capitalize_first_char(type_base)
    } else {
        return None;
    };
    if let Some(field_ty) = deps.find_field_type(&effective_type, member) {
        let subst = build_type_arg_subst(deps, &effective_type, current_type);
        let applied = crate::indexer::apply_type_subst(&field_ty, &subst);
        if is_generic_param(applied.trim_end_matches('?')) {
            return first_type_arg_raw(current_type);
        }
        return Some(applied);
    }
    if let Some(ret_ty) = deps.find_method_return_type_for_type(&effective_type, member) {
        let subst = build_type_arg_subst(deps, &effective_type, current_type);
        let applied = crate::indexer::apply_type_subst(&ret_ty, &subst);
        if is_generic_param(applied.trim_end_matches('?')) {
            return first_type_arg_raw(current_type);
        }
        return Some(applied);
    }
    None
}

/// Walk up from a node to find the enclosing class/object declaration name.
fn enclosing_class_name(node: tree_sitter::Node<'_>, bytes: &[u8]) -> Option<String> {
    let mut cur = node;
    loop {
        match cur.kind() {
            KIND_CLASS_DECL | KIND_OBJECT_DECL => {
                return cur.extract_type_name(bytes);
            }
            _ => {
                cur = cur.parent()?;
            }
        }
    }
}

/// Resolve the type of a root node (identifier, navigation_expression for dotted access).
fn resolve_root_node_type(
    node: tree_sitter::Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    match node.kind() {
        k if k == KIND_SIMPLE_IDENT || k == KIND_TYPE_IDENT => {
            let name = node.utf8_text_owned(bytes)?;
            if let Some(raw) = deps.find_var_type(&name, uri) {
                return uppercase_ident_prefix(&raw);
            }
            // Return raw lowercase name — `forward_resolve_segments` will try
            // capitalize fallback when the next member lookup needs a type name.
            Some(name)
        }
        k if k == KIND_NAV_EXPR => {
            let text = node.utf8_text_owned(bytes)?;
            resolve_dotted_text_type(&text, deps, uri)
        }
        k if k == KIND_CALL_EXPR => resolve_call_expr_type(node, bytes, deps, uri),
        _ => None,
    }
}

/// Resolve the return type of a call_expression node used as a root in a nav chain.
///
/// Handles both simple calls (`foo()`) and method calls (`receiver.method()`).
fn resolve_call_expr_type(
    node: tree_sitter::Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let fn_name = node.call_fn_name(bytes)?;
    if SCOPE_FUNCTIONS.contains(&fn_name.as_str()) {
        let callee = node.child(0)?;
        return resolve_root_node_type(callee, bytes, deps, uri);
    }
    let callee = node.child(0)?;
    let receiver_type = if callee.kind() == KIND_NAV_EXPR {
        let segments = collect_nav_segments(callee, bytes);
        if segments.len() >= 2 {
            resolve_segments_type(&segments[..segments.len() - 1], bytes, deps, uri)
        } else {
            None
        }
    } else {
        resolve_root_node_type(callee, bytes, deps, uri)
    };
    if let Some(ref recv_ty) = receiver_type {
        let type_base = recv_ty.dotted_ident_prefix().last_segment().to_owned();
        let effective_type = if type_base.starts_with_uppercase() {
            type_base
        } else {
            capitalize_first_char(&type_base)
        };
        if !effective_type.is_empty() {
            if let Some(ret_ty) = deps.find_method_return_type_for_type(&effective_type, &fn_name) {
                let subst = build_type_arg_subst(deps, &effective_type, recv_ty);
                return Some(crate::indexer::apply_type_subst(&ret_ty, &subst));
            }
        }
    }
    deps.find_fun_return_type(&fn_name)
}

/// Resolve a chain of segments to a type (without returning method name).
/// Used when we need just the final type after processing all segments.
fn resolve_segments_type(
    segments: &[NavSegment<'_>],
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    if segments.is_empty() {
        return None;
    }
    // If there's just a root, resolve it directly.
    if segments.len() == 1 {
        if let NavSegment::Root(node) = &segments[0] {
            return resolve_root_node_type(*node, bytes, deps, uri);
        }
    }
    // Otherwise use forward_resolve_segments which returns (final_type, last_suffix).
    // The final type after all segments is what we want.
    forward_resolve_segments(segments, bytes, deps, uri).map(|(ty, _)| ty)
}

/// Resolve the type of a dotted text expression like `settings.familyCreationDate`.
fn resolve_dotted_text_type(text: &str, deps: &impl InferDeps, uri: &Url) -> Option<String> {
    // Try as single variable first
    if let Some(raw) = deps.find_var_type(text, uri) {
        return uppercase_ident_prefix(&raw);
    }
    // Split on dots and resolve segment by segment
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let mut current_type = deps.find_var_type(parts[0], uri)?;
    for &field in &parts[1..] {
        let type_name = current_type.dotted_ident_prefix();
        if type_name.is_empty() {
            return None;
        }
        current_type = deps.find_field_type(&type_name, field)?;
    }
    uppercase_ident_prefix(&current_type)
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

/// Resolve function params with receiver awareness: if the call has a dot-receiver
/// (e.g. `factory.create(...)`), resolve the receiver's type and look up the
/// method on that type.  Falls back to global name-based lookup.
/// Unified params lookup: try qualified on receiver type, fall back to global.
fn resolve_call_params(
    fn_name: &str,
    receiver_type: Option<&str>,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    if let Some(raw_type) = receiver_type {
        let dotted = raw_type.dotted_ident_prefix();
        if !dotted.is_empty() {
            if let Some(params) = deps.find_method_params_text(&dotted, fn_name) {
                return Some(params);
            }
        }
    }
    deps.find_fun_params_text(fn_name, uri)
}

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

/// Text-based receiver-aware params lookup for `inline_lambda_param_type`.
/// Given `before_open = "    depositAccountReducerFactory.create"` and `fn_name = "create"`,
/// extracts the receiver variable, resolves its type, and looks up `create` on that type.
fn receiver_aware_params_from_text(
    before_open: &str,
    fn_name: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let dot_pos = before_open.rfind('.')?;
    let receiver_text = before_open[..dot_pos].trim_end();
    let recv_var = last_ident_in(receiver_text);
    if recv_var.is_empty() {
        return None;
    }
    let recv_type = deps.find_var_type(recv_var, uri);
    resolve_call_params(fn_name, recv_type.as_deref(), deps, uri)
}

/// For an INLINE lambda argument `fn(a, b, { param -> ... })`:
/// find the enclosing function name and the 0-based position of this lambda,
/// then look up that function parameter's type.
fn inline_lambda_param_type(
    before_brace: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
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
                if depth < 0 {
                    open_paren_byte = Some(bi);
                    break;
                }
            }
            ',' if depth == 0 => comma_count += 1,
            _ => {}
        }
    }

    let open_pos = open_paren_byte?;
    let before_open = before_brace[..open_pos].trim_end();
    let fn_name = last_ident_in(before_open);

    if fn_name.is_empty() {
        return None;
    }

    // Try receiver-aware lookup when call is qualified (e.g. `factory.create(`)
    let sig = receiver_aware_params_from_text(before_open, fn_name, deps, uri)
        .or_else(|| deps.find_fun_params_text(fn_name, uri))?;
    let param_type = nth_fun_param_type_str(&sig, comma_count)?;
    let raw_input = lambda_type_first_input(&param_type)?;

    // If the extracted type is a declared type param of a generic extension function,
    // substitute it with the concrete type from the call-site receiver.
    if let Some(concrete) =
        try_substitute_ext_fn_type_param(&raw_input, fn_name, before_open, deps, uri)
    {
        return Some(concrete);
    }
    Some(raw_input)
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
fn fun_trailing_lambda_this_type(
    fn_name: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let sig = deps.find_fun_params_text(fn_name, uri)?;
    let last_type = last_fun_param_type_str(&sig)?;
    lambda_type_receiver(&last_type)
}

/// Extract the text before the opening `(` of a call expression from the CST.
/// Used to provide the `before_open` argument for `try_substitute_ext_fn_type_param`.
fn cst_before_open_text(
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

/// Try to substitute a generic type parameter with its concrete type by resolving
/// the extension function's receiver type from the call-site context.
///
/// Given a raw lambda input like `"EffectType"` from `collectState`'s params,
/// checks if it's a declared type param and, if so, resolves the concrete receiver
/// expression to build a substitution map.
///
/// Works for both text-based and CST-based paths.
fn try_substitute_ext_fn_type_param(
    raw_input: &str,
    fn_name: &str,
    before_open: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let info = deps.find_fun_callable_info(fn_name, uri)?;
    if !is_declared_type_param(raw_input, &info.type_params) {
        return None;
    }
    if info.extension_receiver_type.is_empty() {
        return None;
    }

    // Resolve the concrete receiver type from the method chain before `.fnName(`.
    let concrete_receiver = resolve_chain_receiver_type(before_open, fn_name, deps, uri)?;
    let subst = build_ext_fn_type_subst(
        &info.extension_receiver_type,
        &concrete_receiver,
        &info.type_params,
    );
    subst.get(raw_input).cloned()
}

/// Resolve the concrete type of the receiver expression in a method chain.
///
/// Given `before_open` = `"  buildingSavingsReducer.reduce(event.events) { ... }\n    .collectState"`,
/// strips the final `.fnName`, trailing lambdas and call args, then resolves the
/// remaining chain `base.method` to its return type with type substitution.
fn resolve_chain_receiver_type(
    before_open: &str,
    fn_name: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    // Strip the final `.fnName` to get the receiver expression.
    let trimmed = before_open.trim_end();
    let receiver_expr = trimmed.strip_suffix(fn_name)?.trim_end_matches('.');
    let receiver_expr = receiver_expr.trim_end();
    if receiver_expr.is_empty() {
        return None;
    }

    // Strip trailing lambda `{ ... }` and call args `(...)`.
    let stripped = strip_trailing_lambda_and_args(receiver_expr);
    if stripped.is_empty() {
        return None;
    }

    // Try to resolve as `base.method(...)` chain.
    if let Some(dot) = find_last_dot_at_depth_zero(stripped) {
        let base_expr = stripped[..dot].trim();
        let method = stripped[dot + 1..].trim().ident_prefix();
        if !base_expr.is_empty() && !method.is_empty() {
            let base_var = last_ident_in(base_expr);
            let base_type_resolved = deps.find_var_type(base_var, uri);
            let base_name = match &base_type_resolved {
                Some(t) => t.dotted_ident_prefix().last_segment().to_owned(),
                None => capitalize_first_char(base_var),
            };
            if !base_name.is_empty() {
                if let Some(return_type) =
                    deps.find_method_return_type_for_type(&base_name, &method)
                {
                    let subst = match &base_type_resolved {
                        Some(t) => build_type_arg_subst(deps, &base_name, t),
                        None => std::collections::HashMap::new(),
                    };
                    let applied = crate::indexer::apply_type_subst(&return_type, &subst);
                    return Some(applied);
                }
            }
        }
    }

    // Fallback: try as a simple variable.
    let var = last_ident_in(stripped);
    if !var.is_empty() {
        return deps.find_var_type(var, uri);
    }
    None
}

/// Strip trailing lambda `{ ... }` and call args `(...)` from a receiver expression.
///
/// Handles: `expr(args) { lambda }` → `expr`
fn strip_trailing_lambda_and_args(s: &str) -> &str {
    let mut result = s.trim_end();
    // Strip trailing `{ ... }` (trailing lambda)
    if result.ends_with('}') {
        if let Some(open) = rfind_balanced(result, '{', '}') {
            result = result[..open].trim_end();
        }
    }
    // Strip trailing `(...)` (call args)
    result = strip_trailing_call_args(result);
    result.trim_end()
}

/// Find the matching opening delimiter scanning right-to-left.
fn rfind_balanced(s: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0i32;
    for (i, ch) in s.char_indices().rev() {
        if ch == close {
            depth += 1;
        } else if ch == open {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

// ─── cluster-exclusive pure utilities ────────────────────────────────────────

/// Capitalize the first character of a string (Kotlin naming convention:
/// `buildingSavingsReducer` → `BuildingSavingsReducer`).
fn capitalize_first_char(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

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
