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

/// Index `sig_src` for signature lookup, plus store a live tree for `code_src`
/// at the same URI (for CST fast-path tests).
fn indexed_with_live(path: &str, sig_src: &str, code_src: &str) -> (Url, Indexer, Vec<String>) {
    let u = uri(path);
    let idx = Indexer::new();
    idx.index_content(&u, sig_src);
    idx.store_live_tree(&u, code_src);
    idx.set_live_lines(&u, code_src);
    let lines: Vec<String> = code_src.lines().map(String::from).collect();
    (u, idx, lines)
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

// ── two lambdas same line ─────────────────────────────────────────────────────

#[test]
fn it_type_second_of_two_lambdas_same_line() {
    // { setState { it } }, { setEffect { it } }
    // First `it` (inside setState lambda): should resolve to State
    // Second `it` (inside setEffect lambda): should resolve to Effect
    let src = "fun setState(block: (State) -> Unit) {}\nfun setEffect(block: (Effect) -> Unit) {}";
    let (u, idx) = indexed("/t.kt", src);
    let before1 = "{ setState { ";
    let before2 = "{ setState { it } }, { setEffect { ";
    assert_eq!(find_it_element_type(before1, &idx, &u).as_deref(), Some("State"),
        "first it (inside setState) should resolve to State");
    assert_eq!(find_it_element_type(before2, &idx, &u).as_deref(), Some("Effect"),
        "second it (inside setEffect) should resolve to Effect");
}

// ── two lambdas, multi-line, outer function not indexed ─────────────────────

/// Bug regression: when both lambdas are on separate lines inside an `observe()`
/// call and the inner function (`setEffect`) is NOT indexed, the second `it`
/// must still resolve via the CST structural walk-up to `observe`'s 2nd param.
#[test]
fn it_type_second_lambda_multiline_unindexed_inner() {
    // observe is indexed; setState/setEffect are NOT (only `observe` matters here).
    let sig_src = "fun observe(onState: (State) -> Unit, onEffect: (Effect) -> Unit) {}";
    // The code snippet has observe on line 0, lambdas on lines 1 and 2.
    let code_src = "observe(\n    { setState { it } },\n    { setEffect { it } }\n)";
    // Line 2: "    { setEffect { it } }"
    //          0123456789012345678901234
    // second `it` is at col 18 on line 2
    let (u, idx, lines) = indexed_with_live("/t.kt", sig_src, code_src);
    let pos = crate::types::CursorPos { line: 2, utf16_col: 18 };
    let result = find_it_element_type_in_lines(&lines, pos, &idx, &u);
    assert_eq!(result.as_deref(), Some("Effect"),
        "second it inside unindexed setEffect should resolve via observe's 2nd param");
}

/// Same scenario but for the FIRST lambda — must resolve to observe's 1st param.
#[test]
fn it_type_first_lambda_multiline_unindexed_inner() {
    let sig_src = "fun observe(onState: (State) -> Unit, onEffect: (Effect) -> Unit) {}";
    let code_src = "observe(\n    { setState { it } },\n    { setEffect { it } }\n)";
    // Line 1: "    { setState { it } },"
    //          012345678901234567890123
    // first `it` is at col 17 on line 1
    let (u, idx, lines) = indexed_with_live("/t.kt", sig_src, code_src);
    let pos = crate::types::CursorPos { line: 1, utf16_col: 17 };
    let result = find_it_element_type_in_lines(&lines, pos, &idx, &u);
    assert_eq!(result.as_deref(), Some("State"),
        "first it inside unindexed setState should resolve via observe's 1st param");
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
    // `run` is a stdlib scope function (RECEIVER_THIS_FNS) → `this` refers to List<String> → "List"
    let result = find_this_element_type_in_lines(&lines, CursorPos { line: 1, utf16_col: 9 }, &idx, &u);
    // `run` is in RECEIVER_THIS_FNS: passes receiver as `this`;
    // `items` is `List<String>`, so base type should be "List".
    assert_eq!(result.as_deref(), Some("List"),
        "`items.run {{ this }}` should yield List, got: {result:?}");
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
    let result = find_this_element_type_in_lines(&lines, CursorPos { line: 1, utf16_col: 9 }, &idx, &u);
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

// ── RECEIVER_THIS_FNS regression (Issue #4) ─────────────────────────────────

#[test]
fn this_type_run_infers_receiver() {
    // user.run {
    //     this.   ← cursor here (line 1, col 9)
    // }
    let src = "val user: User = User()";
    let (u, idx) = indexed("/t.kt", src);
    let lines: Vec<String> = vec!["user.run {".to_owned(), "    this.".to_owned(), "}".to_owned()];
    assert_eq!(
        find_this_element_type_in_lines(&lines, CursorPos { line: 1, utf16_col: 9 }, &idx, &u).as_deref(),
        Some("User"),
        "run: this should resolve to User"
    );
}

#[test]
fn this_type_apply_infers_receiver() {
    // user.apply {
    //     this.   ← cursor here (line 1, col 9)
    // }
    let src = "val user: User = User()";
    let (u, idx) = indexed("/t.kt", src);
    let lines: Vec<String> = vec!["user.apply {".to_owned(), "    this.".to_owned(), "}".to_owned()];
    assert_eq!(
        find_this_element_type_in_lines(&lines, CursorPos { line: 1, utf16_col: 9 }, &idx, &u).as_deref(),
        Some("User"),
        "apply: this should resolve to User"
    );
}

#[test]
fn this_type_let_does_not_infer_receiver() {
    // `let` exposes the receiver as `it`, not `this`.
    // `this` inside a let{} block should NOT resolve to User via RECEIVER_THIS_FNS.
    let src = "val user: User = User()";
    let (u, idx) = indexed("/t.kt", src);
    let lines: Vec<String> = vec!["user.let {".to_owned(), "    this.".to_owned(), "}".to_owned()];
    let result = find_this_element_type_in_lines(&lines, CursorPos { line: 1, utf16_col: 9 }, &idx, &u);
    assert_eq!(
        result.as_deref(),
        None,
        "let: `this` must not resolve to any receiver type (let exposes receiver as `it`, not `this`)"
    );
}

#[test]
fn this_type_also_does_not_infer_receiver() {
    // `also` exposes the receiver as `it`, not `this`.
    let src = "val user: User = User()";
    let (u, idx) = indexed("/t.kt", src);
    let lines: Vec<String> = vec!["user.also {".to_owned(), "    this.".to_owned(), "}".to_owned()];
    let result = find_this_element_type_in_lines(&lines, CursorPos { line: 1, utf16_col: 9 }, &idx, &u);
    assert_eq!(
        result.as_deref(),
        None,
        "also: `this` must not resolve to any receiver type (also exposes receiver as `it`, not `this`)"
    );
}

#[test]
fn it_type_let_still_infers_receiver() {
    // `user.let { it.` — `let` exposes receiver as `it` → should still infer User
    let src = "val user: User = User()";
    let (u, idx) = indexed("/t.kt", src);
    assert_eq!(
        find_it_element_type("user.let { it.", &idx, &u).as_deref(),
        Some("User"),
        "let: it should still resolve to User"
    );
}

/// When setState IS indexed and the live tree is available, the simple
/// trailing-lambda case (Case B) must still resolve via the EXISTING path
/// — `cst_lambda_param_type_via_call` must NOT be called or, if it is,
/// must not interfere.
#[test]
fn it_type_indexed_inner_fn_cst_still_works() {
    let sig_src = "fun setState(block: (State) -> Unit) {}";
    let code_src = "setState { it }";
    let (u, idx, lines) = indexed_with_live("/t.kt", sig_src, code_src);
    // "setState { " = 11 chars → `it` at col 11
    let pos = crate::types::CursorPos { line: 0, utf16_col: 11 };
    let result = find_it_element_type_in_lines(&lines, pos, &idx, &u);
    assert_eq!(result.as_deref(), Some("State"),
        "simple trailing-lambda with live tree must still resolve via Case B");
}
