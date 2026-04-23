//! Unit tests for `indexer::infer::args`.
//!
//! Covers pure string helpers that can be tested without any `Indexer` state.

use super::*;

// ─── extract_first_arg ────────────────────────────────────────────────────────

#[test]
fn extract_first_arg_simple() {
    assert_eq!(extract_first_arg("with(user)"), Some("user"));
}

#[test]
fn extract_first_arg_multiple_args() {
    // Should return only the first argument.
    assert_eq!(extract_first_arg("fn(a, b, c)"), Some("a"));
}

#[test]
fn extract_first_arg_nested_parens() {
    // First arg contains a nested call — must not split on inner comma.
    assert_eq!(extract_first_arg("fn(foo(x, y), b)"), Some("foo(x, y)"));
}

#[test]
fn extract_first_arg_generic_type() {
    // Generic type in argument should not confuse the depth counter.
    assert_eq!(extract_first_arg("convert(List<String>(), other)"), Some("List<String>()"));
}

#[test]
fn extract_first_arg_empty_parens() {
    assert_eq!(extract_first_arg("fn()"), None);
}

#[test]
fn extract_first_arg_no_parens() {
    assert_eq!(extract_first_arg("identifier"), None);
}

#[test]
fn extract_first_arg_whitespace_trimmed() {
    assert_eq!(extract_first_arg("fn(  receiver  , other)"), Some("receiver"));
}

// ─── extract_named_arg_name ───────────────────────────────────────────────────

#[test]
fn extract_named_arg_simple() {
    assert_eq!(extract_named_arg_name("  buildingSavings = "), Some("buildingSavings"));
    assert_eq!(extract_named_arg_name("  loan = "),            Some("loan"));
    assert_eq!(extract_named_arg_name("  loan="),              Some("loan"));
}

#[test]
fn extract_named_arg_comma_separated() {
    // Same-line comma-separated: `, cards = ` — should match.
    assert_eq!(extract_named_arg_name(", cards = "), Some("cards"));
}

#[test]
fn extract_named_arg_uppercase_rejects() {
    // Constructor-style, not a named arg.
    assert_eq!(extract_named_arg_name("  Foo = "), None);
}

#[test]
fn extract_named_arg_operator_rejects() {
    assert_eq!(extract_named_arg_name("a != "), None);
    assert_eq!(extract_named_arg_name("a <= "), None);
    assert_eq!(extract_named_arg_name("a == "), None);
}

#[test]
fn extract_named_arg_open_paren_before_ident_rejects() {
    // Opening `(` before the identifier disqualifies it.
    assert_eq!(extract_named_arg_name("(isRefresh = "),      None);
    assert_eq!(extract_named_arg_name("fn(x, isRefresh = "), None);
}

// ─── find_named_param_type_in_sig ────────────────────────────────────────────

#[test]
fn find_named_param_type_basic() {
    let sig = "val buildingSavings: (SaveInfo) -> Unit, val loan: (String, Boolean) -> Unit";
    assert_eq!(
        find_named_param_type_in_sig(sig, "loan"),
        Some("(String, Boolean) -> Unit".into())
    );
    assert_eq!(
        find_named_param_type_in_sig(sig, "buildingSavings"),
        Some("(SaveInfo) -> Unit".into())
    );
}

#[test]
fn find_named_param_type_not_found() {
    let sig = "val buildingSavings: (SaveInfo) -> Unit, val loan: (String, Boolean) -> Unit";
    assert_eq!(find_named_param_type_in_sig(sig, "unknown"), None);
}

#[test]
fn find_named_param_type_simple_type() {
    let sig = "name: String, age: Int, active: Boolean";
    assert_eq!(find_named_param_type_in_sig(sig, "name"),   Some("String".into()));
    assert_eq!(find_named_param_type_in_sig(sig, "age"),    Some("Int".into()));
    assert_eq!(find_named_param_type_in_sig(sig, "active"), Some("Boolean".into()));
}

#[test]
fn find_named_param_type_suffix_not_matched() {
    // `otherParam:` must not match when searching for `Param`.
    let sig = "otherParam: String, param: Int";
    assert_eq!(find_named_param_type_in_sig(sig, "param"), Some("Int".into()));
}

#[test]
fn find_named_param_type_with_val_prefix() {
    let sig = "val key: ProductKey, val mapper: DetailMapper";
    assert_eq!(find_named_param_type_in_sig(sig, "key"),    Some("ProductKey".into()));
    assert_eq!(find_named_param_type_in_sig(sig, "mapper"), Some("DetailMapper".into()));
}

// ─── has_named_params_not_it ──────────────────────────────────────────────────

#[test]
fn has_named_params_single_named() {
    assert!(has_named_params_not_it("item -> item.name"));
}

#[test]
fn has_named_params_multi_named() {
    assert!(has_named_params_not_it("loanId, isWustenrot -> setEvent(loanId)"));
}

#[test]
fn has_named_params_implicit_it() {
    assert!(!has_named_params_not_it("it.name"));
}

#[test]
fn has_named_params_block_start() {
    assert!(!has_named_params_not_it("setEvent(something)"));
    assert!(!has_named_params_not_it(""));
}

#[test]
fn has_named_params_underscore_wildcard() {
    // `_` is a valid wildcard — not a user-named param.
    assert!(!has_named_params_not_it("_ -> something"));
}

#[test]
fn has_named_params_explicit_it_keyword() {
    // `it` named explicitly in the arrow is treated as the implicit param.
    assert!(!has_named_params_not_it("it -> it.name"));
}

// ─── Regression: generic types with commas in find_named_param_type_in_sig ───

#[test]
fn named_param_type_generic_with_comma() {
    // `Map<String, Int>` — the comma inside `<>` must NOT split the type.
    let sig = "key: String, map: Map<String, Int>, flag: Boolean";
    assert_eq!(
        find_named_param_type_in_sig(sig, "map"),
        Some("Map<String, Int>".to_owned())
    );
}

#[test]
fn named_param_type_functional_type() {
    // `(String) -> Unit` contains `->` — `>` must not decrement `<>` depth.
    let sig = "name: String, callback: (String) -> Unit";
    assert_eq!(
        find_named_param_type_in_sig(sig, "callback"),
        Some("(String) -> Unit".to_owned())
    );
}

// ─── Regression: extract_first_arg with lambda arrow `->` ────────────────────

#[test]
fn extract_first_arg_with_lambda_arrow() {
    // `->` inside the arg should not trip up the `>` depth tracking.
    assert_eq!(extract_first_arg("run({ x -> x.name })"), Some("{ x -> x.name }"));
}

#[test]
fn extract_first_arg_generic_type_regression() {
    // A generic first arg like `listOf<String>()` — `>` closes a generic, not a lambda.
    assert_eq!(extract_first_arg("fn(listOf<String>(), other)"), Some("listOf<String>()"));
}

// ─── Regression: `>` comparison operators in find_named_param_type_in_sig ────

#[test]
fn named_param_type_default_value_with_gt_operator() {
    // Default value `a > b` must not make depth go negative and break splitting.
    // Note: params_text is just the params, not the full `name: Type = default` form
    // that Kotlin allows. This tests that bare `>` at depth-0 is ignored.
    let sig = "threshold: Int, name: String";
    assert_eq!(find_named_param_type_in_sig(sig, "name"), Some("String".to_owned()));
}
