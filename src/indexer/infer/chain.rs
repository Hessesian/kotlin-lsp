//! CST navigation-chain resolution helpers.

use tower_lsp::lsp_types::Url;

use crate::indexer::NodeExt;
use crate::queries::{
    KIND_CALL_EXPR, KIND_CLASS_DECL, KIND_NAV_EXPR, KIND_NAV_SUFFIX, KIND_OBJECT_DECL,
    KIND_SIMPLE_IDENT, KIND_TYPE_IDENT,
};
use crate::StrExt;

use super::deps::InferDeps;
use super::lambda::SCOPE_FUNCTIONS;
use super::receiver::uppercase_ident_prefix;
use super::type_subst::{
    build_type_arg_subst, capitalize_first_char, first_type_arg_raw, is_generic_param,
    split_top_level_commas, type_args_inner,
};

/// A segment in a navigation chain: either a root identifier or a suffix member.
#[derive(Debug)]
pub(super) enum NavSegment<'a> {
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
pub(super) fn collect_nav_segments<'a>(
    node: tree_sitter::Node<'a>,
    bytes: &[u8],
) -> Vec<NavSegment<'a>> {
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
pub(super) fn forward_resolve_segments(
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
    // Track whether the last Suffix actually changed current_type.
    // Used by the CallExpr dedup check: only skip re-resolution when the Suffix
    // already resolved the type (avoids false dedup when the Suffix found nothing).
    let mut last_suffix_resolved = false;

    for segment in segments {
        match segment {
            NavSegment::Root(node) => {
                current_type = resolve_root_node_type(*node, bytes, deps, uri);
                // If the root is a call_expression, record its fn name so that
                // a subsequent CallExpr for the same call (trailing-lambda wrapper)
                // is recognized as redundant by the dedup check below.
                if node.kind() == KIND_CALL_EXPR {
                    last_suffix = node.call_fn_name(bytes);
                    last_suffix_resolved = current_type.is_some();
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
                last_suffix_resolved = false;
                if let Some(ref cur) = current_type {
                    if let Some(resolved) = resolve_member_type_on(cur, name, deps) {
                        current_type = Some(resolved);
                        last_suffix_resolved = true;
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
                    if last_suffix.as_deref() == Some(name.as_str()) && last_suffix_resolved {
                        continue;
                    }
                    if let Some(ref cur) = current_type {
                        if let Some(resolved) = resolve_member_type_on(cur, name, deps) {
                            current_type = Some(resolved);
                            continue;
                        }
                    }
                    if let Some(ret_ty) = deps.find_fun_return_type(name) {
                        let ret_base = ret_ty.trim_end_matches('?').ident_prefix();
                        if !is_generic_param(&ret_base) {
                            current_type = Some(ret_ty);
                            continue;
                        }
                        // Generic return type — fall through to first_type_arg_raw fallback.
                    } else if let Some(class_name) = enclosing_class_name(*call_node, bytes) {
                        if let Some(ret_ty) =
                            deps.find_method_return_type_for_type(&class_name, name)
                        {
                            current_type = Some(ret_ty);
                            continue;
                        }
                    }
                    // Method not indexed (or returns generic): use first concrete type arg
                    // of the receiver as a best-effort return type.  Only applies when the
                    // receiver has exactly one type parameter (e.g. `Optional<T>`) — for
                    // multi-param types like `Map<String, Order>` the first arg would be
                    // wrong (it would infer `String` instead of `Order`).
                    let is_single_param = current_type
                        .as_deref()
                        .and_then(type_args_inner)
                        .is_some_and(|inner| split_top_level_commas(inner).len() == 1);
                    if is_single_param {
                        if let Some(first_arg) =
                            current_type.as_deref().and_then(first_type_arg_raw)
                        {
                            if first_arg.starts_with_uppercase() && !is_generic_param(&first_arg) {
                                current_type = Some(first_arg);
                            }
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

/// Resolve the callee navigation chain left-to-right, returning the type
/// of the expression before the final method call, and the final method name.
///
/// For `settings.familyCreationDate?.let`:
///   - root = "settings" → type "IFamilySettings"
///   - ".familyCreationDate" → type "Long" (field on IFamilySettings)
///   - returns ("Long", "let")
pub(super) fn resolve_callee_chain(
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

/// Forward-walk chain resolution for the receiver type of a lambda's enclosing
/// call expression. Given `a.b.method { lambda }`, resolves left-to-right:
///   1. Find root identifier (`a`) → resolve its type
///   2. Walk through each navigation suffix (`.b`, `.method`) tracking the type
///   3. Return the type of the expression just before the final method call
///
/// This handles arbitrary chains like `settings.familyCreationDate?.let { }`
/// without backward heuristics.
pub(super) fn cst_forward_resolve_receiver_type(
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

/// Resolve the receiver type that flows into a call expression's lambda.
/// For scope functions, this is the type of the expression before `.let`/`.also`/etc.
pub(super) fn resolve_callee_receiver_type(
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
pub(super) fn resolve_member_type_on(
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
pub(super) fn enclosing_class_name(node: tree_sitter::Node<'_>, bytes: &[u8]) -> Option<String> {
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
pub(super) fn resolve_root_node_type(
    node: tree_sitter::Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
    match node.kind() {
        k if k == KIND_SIMPLE_IDENT || k == KIND_TYPE_IDENT => {
            let name = node.utf8_text_owned(bytes)?;
            if let Some(raw) = deps.find_var_type(&name, uri) {
                // Validate it is a type name (starts with uppercase, not a generic
                // placeholder like `T`), then return the FULL raw string including
                // generics so that downstream `build_type_arg_subst` can extract
                // type arguments (e.g. `ResultState.Success<Optional<FamilyAccount>>`).
                let base = raw.ident_prefix();
                if !base.is_empty() && !is_generic_param(&base) && base.starts_with_uppercase() {
                    return Some(raw);
                }
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
pub(super) fn resolve_call_expr_type(
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
pub(super) fn resolve_segments_type(
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
pub(super) fn resolve_dotted_text_type(
    text: &str,
    deps: &impl InferDeps,
    uri: &Url,
) -> Option<String> {
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
