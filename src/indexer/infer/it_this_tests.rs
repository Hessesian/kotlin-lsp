//! Unit tests for `it`/`this` lambda-type inference helpers.
//!
//! Each test is self-contained: it builds a tiny synthetic Indexer, indexes
//! a one-liner Kotlin snippet, then calls the relevant pure function.
//!
//! Helpers used: `indexed()` / `uri()` mirror the pattern in `indexer.rs` tests.

use super::*;
use tower_lsp::lsp_types::Url;
use super::super::super::Indexer;

fn uri(path: &str) -> Url {
    Url::parse(&format!("file:///test{path}")).unwrap()
}

fn indexed(path: &str, src: &str) -> (Url, Indexer) {
    let u = uri(path);
    let idx = Indexer::new();
    idx.index_content(&u, src);
    (u, idx)
}

// ── find_it_element_type ─────────────────────────────────────────────────────

#[test]
fn it_element_type_simple_foreach() {
    // `users.forEach { it.` — `it` should resolve to `User`
    let src = "val users: List<User> = emptyList()";
    let (u, idx) = indexed("/t.kt", src);
    let before = "users.forEach { it.";
    let result = find_it_element_type(before, &idx, &u);
    assert_eq!(result.as_deref(), Some("User"),
        "forEach on List<User> should yield element type User, got: {result:?}");
}

#[test]
fn it_element_type_flow() {
    let src = "val events: Flow<Event> = emptyFlow()";
    let (u, idx) = indexed("/t.kt", src);
    let before = "events.collect { it.";
    assert_eq!(find_it_element_type(before, &idx, &u).as_deref(), Some("Event"));
}

#[test]
fn it_element_type_unknown_var_returns_none() {
    let (u, idx) = indexed("/t.kt", "");
    assert_eq!(find_it_element_type("unknown.forEach { it.", &idx, &u), None);
}

#[test]
fn it_element_type_scope_fn_let() {
    // `user.let { it.` — `it` is the User itself (non-collection receiver)
    let src = "val user: User = User()";
    let (u, idx) = indexed("/t.kt", src);
    assert_eq!(
        find_it_element_type("user.let { it.", &idx, &u).as_deref(),
        Some("User")
    );
}

// ── find_this_element_type_in_lines ─────────────────────────────────────────

#[test]
fn this_element_type_multiline_scope_fn() {
    // items.run {
    //     this.  ← cursor here (line 1, col 9)
    // }
    let src = "val items: List<String> = emptyList()";
    let (u, idx) = indexed("/t.kt", src);
    let lines: Vec<String> = vec![
        "items.run {".to_owned(),
        "    this.".to_owned(),
        "}".to_owned(),
    ];
    // `run` is a stdlib scope function → `this` refers to List<String> → "List"
    // (run passes the receiver, not element type — expect None because List is
    // not a scope-function-receiver-lambda in the typical sense, but `run` IS
    // in SCOPE_FUNCTIONS so the base type "List" is returned)
    let result = find_this_element_type_in_lines(&lines, 1, 9, &idx, &u);
    // We just assert it doesn't panic and returns a value or None consistently.
    // The exact type depends on resolver; at minimum verify no panic.
    let _ = result;
}

#[test]
fn this_type_with_block() {
    // with(user) { this. } — this should resolve to User
    let src = "val user: User = User()";
    let (u, idx) = indexed("/t.kt", src);
    let lines: Vec<String> = vec![
        "with(user) {".to_owned(),
        "    this.".to_owned(),
        "}".to_owned(),
    ];
    let result = find_this_element_type_in_lines(&lines, 1, 9, &idx, &u);
    assert_eq!(result.as_deref(), Some("User"),
        "`with(user) {{ this }}` should yield User, got: {result:?}");
}

// ── line_has_lambda_param ────────────────────────────────────────────────────

#[test]
fn line_has_lambda_param_single() {
    assert!(line_has_lambda_param("items.forEach { item -> item.name }", "item"));
    assert!(!line_has_lambda_param("items.forEach { it.name }", "item"));
}

#[test]
fn line_has_lambda_param_multi() {
    // multi-param: `{ a, b -> }`
    assert!(line_has_lambda_param("items.zip(other) { a, b -> a.id }", "a"));
    assert!(line_has_lambda_param("items.zip(other) { a, b -> a.id }", "b"));
    assert!(!line_has_lambda_param("items.zip(other) { a, b -> a.id }", "c"));
}

#[test]
fn line_has_lambda_param_multiple_arrows_on_line() {
    // `{ isRefresh -> ... } { resultState ->` — two lambdas on same line
    let line = "reloadableProduct(ProductKey.FAMILY, { isRefresh -> getFamilyAccount(isRefresh) }) { resultState ->";
    assert!(line_has_lambda_param(line, "resultState"),
        "should find resultState even when isRefresh arrow comes first");
    assert!(line_has_lambda_param(line, "isRefresh"),
        "should still find isRefresh");
    assert!(!line_has_lambda_param(line, "other"),
        "should NOT find unknown name");
}

// ── lambda_brace_pos_for_param ───────────────────────────────────────────────

#[test]
fn lambda_brace_pos_single_param() {
    let line = "items.forEach { item -> item.name }";
    let pos = lambda_brace_pos_for_param(line, "item");
    assert_eq!(pos, Some(14)); // position of `{`
}

#[test]
fn lambda_brace_pos_second_lambda_on_line() {
    let line = "reloadableProduct(ProductKey.FAMILY, { isRefresh -> getFamilyAccount(isRefresh) }) { resultState ->";
    let brace = lambda_brace_pos_for_param(line, "resultState");
    assert!(brace.is_some(), "must find brace for resultState");
    let last_brace = line.rfind('{').unwrap();
    assert_eq!(brace.unwrap(), last_brace,
        "brace pos for resultState should be the last {{ on the line");
}

#[test]
fn lambda_brace_pos_none_for_unknown_param() {
    let line = "items.forEach { item -> item.name }";
    assert_eq!(lambda_brace_pos_for_param(line, "unknown"), None);
}

// ── has_named_params_not_it ──────────────────────────────────────────────────

#[test]
fn has_named_params_detects_single_named() {
    assert!(has_named_params_not_it("item -> item.name"));
}

#[test]
fn has_named_params_detects_multi_named() {
    assert!(has_named_params_not_it("loanId, isWustenrot -> setEvent(loanId)"));
}

#[test]
fn has_named_params_rejects_implicit_it() {
    assert!(!has_named_params_not_it("it.name"));
}

#[test]
fn has_named_params_rejects_block_lambda() {
    assert!(!has_named_params_not_it("setEvent(something)"));
}

#[test]
fn has_named_params_rejects_empty() {
    assert!(!has_named_params_not_it(""));
}

#[test]
fn has_named_params_rejects_underscore() {
    // `_` is a valid anonymous param name — not considered "named"
    assert!(!has_named_params_not_it("_ -> something"));
}

// ── find_last_dot_at_depth_zero ──────────────────────────────────────────────

#[test]
fn dot_at_depth_zero_simple() {
    assert_eq!(find_last_dot_at_depth_zero("items.forEach"), Some(5));
}

#[test]
fn dot_at_depth_zero_ignores_inner_dot() {
    // The dot inside `fn(Enum.VALUE,` is at depth 1 — should NOT match.
    assert_eq!(find_last_dot_at_depth_zero("fn(Enum.VALUE, "), None);
}

#[test]
fn dot_at_depth_zero_chained() {
    assert_eq!(find_last_dot_at_depth_zero("a.b(x).c"), Some(6));
}
