//! Infer the Kotlin type of a CST expression node.
//!
//! Handles the cases that are knowable without a compiler:
//!
//! | Node kind                 | Inferred type          |
//! |---------------------------|------------------------|
//! | `integer_literal`         | `Int`                  |
//! | `long_literal`            | `Long`                 |
//! | `real_literal`            | `Float` or `Double`    |
//! | `string_literal`          | `String`               |
//! | `boolean_literal`         | `Boolean`              |
//! | `null`                    | `Nothing?`             |
//! | `character_literal`       | `Char`                 |
//! | `call_expression`         | return type from index |
//! | `check_expression`        | `Boolean`              |
//! | `comparison_expression`   | `Boolean`              |
//! | `disjunction_expression`  | `Boolean`              |
//! | `conjunction_expression`  | `Boolean`              |
//! | `prefix_expression` (`!`) | `Boolean`              |
//! | `if_expression`           | type of then-branch    |
//! | `range_expression` (int)  | `IntRange`             |
//!
//! Navigation expressions (e.g. `list.size`), `when` expressions, and other
//! compound forms are not resolved — callers receive `None` and can omit the
//! type annotation.

use tree_sitter::Node;

use crate::indexer::NodeExt;
use crate::queries::{
    KIND_CALL_EXPR, KIND_CHECK_EXPR, KIND_COMPARISON_EXPR, KIND_CONJUNCTION_EXPR,
    KIND_CONTROL_STRUCTURE_BODY, KIND_DISJUNCTION_EXPR, KIND_IF_EXPR, KIND_PREFIX_EXPR,
    KIND_RANGE_EXPR,
};

use super::deps::InferDeps;

// ─── public API ───────────────────────────────────────────────────────────────

/// Infer the Kotlin type of `node` as a human-readable string (e.g. `"Int"`).
///
/// Returns `None` when the type cannot be determined without compiler
/// type-resolution (e.g. navigation expressions, generic calls).
pub(crate) fn infer_expr_type(
    node: Node<'_>,
    bytes: &[u8],
    deps: &impl InferDeps,
) -> Option<String> {
    match node.kind() {
        "integer_literal" => Some("Int".to_owned()),
        "long_literal" => Some("Long".to_owned()),
        "real_literal" => infer_real_literal(node, bytes),
        "string_literal" | "multiline_string_literal" => Some("String".to_owned()),
        "boolean_literal" => Some("Boolean".to_owned()),
        "null" => Some("Nothing?".to_owned()),
        "character_literal" => Some("Char".to_owned()),
        k if k == KIND_CALL_EXPR => infer_call_expr_type(node, bytes, deps),
        k if k == KIND_CHECK_EXPR
            || k == KIND_COMPARISON_EXPR
            || k == KIND_DISJUNCTION_EXPR
            || k == KIND_CONJUNCTION_EXPR =>
        {
            Some("Boolean".to_owned())
        }
        k if k == KIND_PREFIX_EXPR => infer_prefix_expr_type(node, bytes),
        k if k == KIND_IF_EXPR => infer_if_expr_type(node, bytes, deps),
        k if k == KIND_RANGE_EXPR => infer_range_expr_type(node, bytes, deps),
        _ => None,
    }
}

// ─── private helpers ──────────────────────────────────────────────────────────

/// `3.14f` / `3.14F` → `Float`; `3.14` → `Double`.
fn infer_real_literal(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let text = node.utf8_text(bytes).ok()?;
    if text.ends_with('f') || text.ends_with('F') {
        Some("Float".to_owned())
    } else {
        Some("Double".to_owned())
    }
}

/// For a `call_expression` whose callee is a `simple_identifier`:
/// - If the callee starts with uppercase it's a constructor call → return the class name.
/// - Otherwise look up the function's return type from the index via
///   [`InferDeps::find_fun_return_type`].
fn infer_call_expr_type(node: Node<'_>, bytes: &[u8], deps: &impl InferDeps) -> Option<String> {
    let fn_name = node.call_fn_name(bytes)?;
    if fn_name.starts_with(|c: char| c.is_uppercase()) {
        return Some(fn_name);
    }
    let raw = deps.find_fun_return_type(&fn_name)?;
    Some(raw.trim_start_matches(':').trim().to_owned())
}

/// `!expr` → `Boolean`; other prefix operators (`-`, `+`) are arithmetic and
/// not inferable without knowing the operand type.
fn infer_prefix_expr_type(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    let op = node.child(0)?;
    let text = op.utf8_text(bytes).ok()?;
    if text == "!" {
        Some("Boolean".to_owned())
    } else {
        None
    }
}

/// For `if (cond) <then> else <else>`: return the type of the then-branch as a
/// best-effort hint. We don't verify that both branches agree (that would
/// require full type-checking), so we only emit a hint when the then-branch
/// type is unambiguous. No hint is emitted for bare `if` without `else`.
fn infer_if_expr_type<D: InferDeps>(node: Node<'_>, bytes: &[u8], deps: &D) -> Option<String> {
    // Must have an else branch to be a valid expression (not a statement).
    let has_else =
        (0..node.child_count()).any(|i| node.child(i).map(|c| c.kind() == "else").unwrap_or(false));
    if !has_else {
        return None;
    }

    // then-branch is the first control_structure_body child.
    let then_body = (0..node.child_count())
        .map(|i| node.child(i).unwrap())
        .find(|c| c.kind() == KIND_CONTROL_STRUCTURE_BODY)?;

    // control_structure_body wraps exactly one expression.
    let expr = then_body.child(0)?;
    infer_expr_type(expr, bytes, deps)
}

/// `a..b` or `a..<b`: infer `IntRange` only when both operands are integer
/// literals. Any other operand type requires the compiler.
fn infer_range_expr_type<D: InferDeps>(node: Node<'_>, bytes: &[u8], deps: &D) -> Option<String> {
    let lhs = node.child(0)?;
    let rhs_idx = node.child_count().checked_sub(1)?;
    let rhs = node.child(rhs_idx)?;
    let lhs_ty = infer_expr_type(lhs, bytes, deps)?;
    let rhs_ty = infer_expr_type(rhs, bytes, deps)?;
    match (lhs_ty.as_str(), rhs_ty.as_str()) {
        ("Int", "Int") => Some("IntRange".to_owned()),
        ("Long", "Long") | ("Int", "Long") | ("Long", "Int") => Some("LongRange".to_owned()),
        ("Char", "Char") => Some("CharRange".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
#[path = "expr_type_tests.rs"]
mod tests;
