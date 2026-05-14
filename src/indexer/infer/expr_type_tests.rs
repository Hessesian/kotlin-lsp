use tree_sitter::Parser;

use super::infer_expr_type;
use crate::indexer::infer::deps::TestDeps;

fn fun_body_expr_node(src: &str) -> (tree_sitter::Tree, Vec<u8>) {
    let mut p = Parser::new();
    p.set_language(&tree_sitter_kotlin::language()).unwrap();
    let bytes = src.as_bytes().to_vec();
    let tree = p.parse(src, None).unwrap();
    (tree, bytes)
}

/// Parse `fun f() = <expr>` and run `infer_expr_type` on the expression.
fn infer(src: &str) -> Option<String> {
    let full = format!("fun f() = {src}");
    let (tree, bytes) = fun_body_expr_node(&full);
    let root = tree.root_node();
    let fun_decl = root.child(0)?;
    let body = (0..fun_decl.child_count())
        .map(|i| fun_decl.child(i).unwrap())
        .find(|n| n.kind() == "function_body")?;
    let expr = body.child(1)?;
    infer_expr_type(expr, &bytes, &TestDeps::new())
}

// ─── literals ─────────────────────────────────────────────────────────────────

#[test]
fn integer_literal() {
    assert_eq!(infer("42"), Some("Int".into()));
}

#[test]
fn long_literal() {
    assert_eq!(infer("42L"), Some("Long".into()));
}

#[test]
fn float_literal() {
    assert_eq!(infer("1.0f"), Some("Float".into()));
}

#[test]
fn double_literal() {
    assert_eq!(infer("3.14"), Some("Double".into()));
}

#[test]
fn string_literal() {
    assert_eq!(infer(r#""hello""#), Some("String".into()));
}

#[test]
fn boolean_true() {
    assert_eq!(infer("true"), Some("Boolean".into()));
}

#[test]
fn null_literal() {
    assert_eq!(infer("null"), Some("Nothing?".into()));
}

#[test]
fn char_literal() {
    assert_eq!(infer("'x'"), Some("Char".into()));
}

// ─── boolean-returning expressions ────────────────────────────────────────────

#[test]
fn check_expression() {
    assert_eq!(infer("a is String"), Some("Boolean".into()));
}

#[test]
fn check_not_expression() {
    assert_eq!(infer("a !is String"), Some("Boolean".into()));
}

#[test]
fn comparison_expression() {
    assert_eq!(infer("a > 0"), Some("Boolean".into()));
}

#[test]
fn disjunction_expression() {
    assert_eq!(infer("a || b"), Some("Boolean".into()));
}

#[test]
fn conjunction_expression() {
    assert_eq!(infer("a && b"), Some("Boolean".into()));
}

#[test]
fn prefix_not() {
    assert_eq!(infer("!flag"), Some("Boolean".into()));
}

#[test]
fn prefix_minus_no_hint() {
    assert_eq!(infer("-x"), None);
}

// ─── if expression ────────────────────────────────────────────────────────────

#[test]
fn if_else_literal() {
    assert_eq!(infer("if (ok) 1 else 2"), Some("Int".into()));
}

#[test]
fn if_else_string() {
    assert_eq!(infer(r#"if (ok) "yes" else "no""#), Some("String".into()));
}

#[test]
fn if_else_boolean_then() {
    // then-branch is a check expression → Boolean
    assert_eq!(
        infer("if (cond) a is String else false"),
        Some("Boolean".into())
    );
}

#[test]
fn if_without_else_no_hint() {
    // bare if is a statement, not an expression with a known type
    assert_eq!(infer("if (ok) 1"), None);
}

#[test]
fn if_else_unknown_call_no_hint() {
    // listOf is a stdlib function not in TestDeps → None
    assert_eq!(infer("if (ok) listOf(A()) else listOf()"), None);
}

// ─── range expression ─────────────────────────────────────────────────────────

#[test]
fn int_range() {
    assert_eq!(infer("1..10"), Some("IntRange".into()));
}

#[test]
fn long_range() {
    assert_eq!(infer("1L..10L"), Some("LongRange".into()));
}

#[test]
fn char_range() {
    assert_eq!(infer("'a'..'z'"), Some("CharRange".into()));
}

#[test]
fn mixed_range_no_hint() {
    // variable operands — can't infer without type-checking
    assert_eq!(infer("a..b"), None);
}

// ─── unresolvable forms (should remain None) ──────────────────────────────────

#[test]
fn navigation_expr_no_hint() {
    assert_eq!(infer("list.size"), None);
}

#[test]
fn additive_no_hint() {
    assert_eq!(infer("a + b"), None);
}

#[test]
fn elvis_no_hint() {
    assert_eq!(infer("a ?: 0"), None);
}

#[test]
fn when_expr_no_hint() {
    assert_eq!(infer(r#"when { x > 0 -> "pos"; else -> "neg" }"#), None);
}
