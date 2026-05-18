//! Lambda receiver type inference from text before `{`.

use tower_lsp::lsp_types::Url;

use crate::indexer::Indexer;
use crate::resolver::extract_collection_element_type;
use crate::StrExt;

use super::super::{find_enclosing_call_name, last_ident_in};
use super::args::{extract_first_arg, extract_named_arg_name, find_named_param_type_in_sig};
use super::deps::InferDeps;
use super::lambda::{
    lambda_type_first_input, lambda_type_nth_input, lambda_type_receiver, SCOPE_FUNCTIONS,
};
use super::sig::{
    collect_all_fun_params_texts, find_fun_signature_full, last_fun_param_type_str,
    nth_fun_param_type_str, strip_trailing_call_args,
};
use super::type_subst::{
    build_type_arg_subst, find_last_dot_at_depth_zero, first_concrete_type_arg_str,
    is_generic_param, try_substitute_ext_fn_type_param,
};

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

pub(super) fn uppercase_ident_prefix(raw: &str) -> Option<String> {
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

/// Look up a function by name, find its last parameter's type, and return the
/// first input type if that parameter is a lambda/function type.
///
/// Example: `fun loadProduct(key: K, flow: Flow<T>, map: (ResultState<T>) -> Model)`
/// returns `Some("ResultState")` so that `it` in `loadProduct(...) { it }` resolves.
pub(super) fn fun_trailing_lambda_it_type(
    fn_name: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let sig = deps.find_fun_params_text(fn_name, uri)?;
    let last_type = last_fun_param_type_str(&sig)?;
    lambda_type_first_input(&last_type)
}

/// Like `fun_trailing_lambda_it_type` but for `this`: only returns a type when
/// the trailing lambda parameter is a **receiver lambda** `T.() -> R`.
pub(super) fn fun_trailing_lambda_this_type(
    fn_name: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    let sig = deps.find_fun_params_text(fn_name, uri)?;
    let last_type = last_fun_param_type_str(&sig)?;
    lambda_type_receiver(&last_type)
}

/// For an INLINE lambda argument `fn(a, b, { param -> ... })`:
/// find the enclosing function name and the 0-based position of this lambda,
/// then look up that function parameter's type.
pub(super) fn inline_lambda_param_type(
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

/// Text-based receiver-aware params lookup for `inline_lambda_param_type`.
/// Given `before_open = "    depositAccountReducerFactory.create"` and `fn_name = "create"`,
/// extracts the receiver variable, resolves its type, and looks up `create` on that type.
pub(super) fn receiver_aware_params_from_text(
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

/// Resolve function params with receiver awareness: if the call has a dot-receiver
/// (e.g. `factory.create(...)`), resolve the receiver's type and look up the
/// method on that type.  Falls back to global name-based lookup.
/// Unified params lookup: try qualified on receiver type, fall back to global.
pub(super) fn resolve_call_params(
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

/// Handles named-arg lambdas spread across multiple lines:
/// opener like `  buildingSavings = ` or `  loan = ` spread across multiple
/// lines (the enclosing `(` is on a previous line).
///
/// Returns `Some(type_name)` for the Nth input type of the parameter's functional
/// type, where N = `lambda_param_pos` (0-based position of the named param in the
/// multi-param lambda, e.g. `{ loanId, isWustenrot -> }` → loanId=0, isWustenrot=1).
pub(super) fn lambda_receiver_type_named_arg_ml(
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
