//! Pure generic type-parameter substitution helpers.

use tower_lsp::lsp_types::Url;

use crate::resolver::infer_lines::first_type_arg;
use crate::StrExt;

use super::super::last_ident_in;
use super::deps::InferDeps;
use super::sig::strip_trailing_call_args;

/// Build a type-parameter substitution map from a concrete instantiation.
///
/// Looks up the declared type parameters of `class_name` (e.g. `["T"]` for
/// `Result<T>`), then zips them with the type arguments extracted from
/// `concrete_type` (e.g. `"Result<FamilyAccount>"`) to build the map
/// `{"T" → "FamilyAccount"}`.
///
/// Returns an empty map when the class params are unknown or the concrete type
/// carries no type arguments.
pub(super) fn build_type_arg_subst(
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
pub(super) fn type_args_inner(ty: &str) -> Option<&str> {
    let open = ty.find('<')?;
    let close = ty.rfind('>')?;
    if close <= open {
        return None;
    }
    Some(&ty[open + 1..close])
}

/// Split a generic parameter list at top-level commas, respecting nested `<>`.
pub(super) fn split_top_level_commas(s: &str) -> Vec<&str> {
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
pub(super) fn build_ext_fn_type_subst(
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
pub(super) fn is_declared_type_param(name: &str, fun_type_params: &[String]) -> bool {
    fun_type_params.iter().any(|p| p == name)
}

/// Returns `true` if `name` looks like a generic type parameter: a short
/// all-uppercase identifier like `T`, `R`, `IN`, `OUT`, `KEY`, `VAL`.
pub(crate) fn is_generic_param(name: &str) -> bool {
    !name.is_empty() && name.len() <= 3 && name.chars().all(|c| c.is_uppercase())
}

/// Extract the first type argument from `ty` and return it only if it is a
/// concrete class name (i.e. not a generic type parameter like `T`, `R`, `IN`).
pub(super) fn first_concrete_type_arg_str(ty: &str) -> Option<String> {
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
pub(super) fn first_type_arg_raw(ty: &str) -> Option<String> {
    let inner = type_args_inner(ty)?;
    let arg = first_type_arg(inner).trim().trim_matches('?');
    let base = arg.ident_prefix();
    if base.is_empty() || is_generic_param(&base) {
        return None;
    }
    Some(arg.to_owned())
}

/// Try to substitute a generic type parameter with its concrete type by resolving
/// the extension function's receiver type from the call-site context.
///
/// Given a raw lambda input like `"EffectType"` from `collectState`'s params,
/// checks if it's a declared type param and, if so, resolves the concrete receiver
/// expression to build a substitution map.
///
/// Works for both text-based and CST-based paths.
pub(super) fn try_substitute_ext_fn_type_param(
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
pub(super) fn resolve_chain_receiver_type(
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
pub(super) fn capitalize_first_char(s: &str) -> String {
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
