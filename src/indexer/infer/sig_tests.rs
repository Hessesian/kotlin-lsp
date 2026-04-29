//! Unit tests for `indexer::infer::sig`.
//!
//! Covers pure string helpers that can be tested without any `Indexer` state.

use super::*;

// ─── collect_signature ────────────────────────────────────────────────────────

#[test]
fn collect_signature_single_line_with_brace() {
    let lines = vec!["sealed interface NewsFeedUiState {".to_owned()];
    // The `{` should be stripped; result is just the declaration.
    assert_eq!(
        collect_signature(&lines, 0),
        "sealed interface NewsFeedUiState"
    );
}

#[test]
fn collect_signature_single_line_no_brace() {
    let lines = vec!["fun doSomething(x: Int): Boolean".to_owned()];
    assert_eq!(
        collect_signature(&lines, 0),
        "fun doSomething(x: Int): Boolean"
    );
}

#[test]
fn collect_signature_multiline_constructor() {
    let lines = vec![
        "class DetailViewModel @Inject constructor(".to_owned(),
        "  private val mapper: DetailMapper,".to_owned(),
        "  private val loadUseCase: LoadDataUseCase,".to_owned(),
        ") : MviViewModel<Event, State, Effect>() {".to_owned(),
    ];
    let sig = collect_signature(&lines, 0);
    assert!(sig.contains("DetailViewModel"), "should contain class name");
    assert!(sig.contains("MviViewModel"),    "should contain superclass");
    assert!(!sig.contains('{'),              "should not include body brace");
}

#[test]
fn collect_signature_brace_on_own_line() {
    // `{` on its own line — body opener, must not appear in output.
    let lines = vec![
        "class Foo(val x: Int)".to_owned(),
        "    : Bar() {".to_owned(),
    ];
    let sig = collect_signature(&lines, 0);
    assert!(!sig.contains('{'), "brace should be stripped");
    assert!(sig.contains("Foo"), "class name must be present");
}

#[test]
fn collect_signature_starts_at_offset() {
    let lines = vec![
        "// comment".to_owned(),
        "fun hello(): String".to_owned(),
    ];
    assert_eq!(collect_signature(&lines, 1), "fun hello(): String");
}

#[test]
fn collect_signature_caps_at_15_lines() {
    // A function spanning more than 15 lines must not cause a panic.
    let mut lines: Vec<String> = vec!["fun f(".to_owned()];
    for i in 0..20 {
        lines.push(format!("  p{i}: Int,"));
    }
    lines.push(")".to_owned());
    let sig = collect_signature(&lines, 0);
    // Should have collected up to the 15-line cap without panicking.
    assert!(sig.contains("fun f("), "should start with fun f(");
}

// ─── nth_fun_param_type_str ───────────────────────────────────────────────────

#[test]
fn nth_param_type_first() {
    let params = "key: String, value: Int";
    assert_eq!(nth_fun_param_type_str(params, 0), Some("String".into()));
}

#[test]
fn nth_param_type_second() {
    let params = "key: String, value: Int";
    assert_eq!(nth_fun_param_type_str(params, 1), Some("Int".into()));
}

#[test]
fn nth_param_type_out_of_range_falls_back_to_last() {
    let params = "key: String, value: Int";
    assert_eq!(nth_fun_param_type_str(params, 99), Some("Int".into()));
}

#[test]
fn nth_param_type_lambda_type_arg() {
    // `->` must not upset `<>` depth counter.
    let params = "key: ProductKey, flow: (Boolean) -> Flow<ResultState<T>>, map: (ResultState<T>) -> StatefulModel";
    assert_eq!(nth_fun_param_type_str(params, 0), Some("ProductKey".into()));
    assert_eq!(nth_fun_param_type_str(params, 1), Some("(Boolean) -> Flow<ResultState<T>>".into()));
    assert_eq!(nth_fun_param_type_str(params, 2), Some("(ResultState<T>) -> StatefulModel".into()));
}

#[test]
fn nth_param_type_single_param() {
    let params = "block: () -> Unit";
    assert_eq!(nth_fun_param_type_str(params, 0), Some("() -> Unit".into()));
}

#[test]
fn nth_param_type_empty_returns_none() {
    assert_eq!(nth_fun_param_type_str("", 0), None);
}

#[test]
fn nth_param_type_val_var_prefix_stripped() {
    // Constructor params: `val repo: IRepo, var counter: Int`.
    let params = "val repo: IRepo, var counter: Int";
    assert_eq!(nth_fun_param_type_str(params, 0), Some("IRepo".into()));
    assert_eq!(nth_fun_param_type_str(params, 1), Some("Int".into()));
}

// ─── last_fun_param_type_str ─────────────────────────────────────────────────

#[test]
fn last_param_type_single_param() {
    assert_eq!(last_fun_param_type_str("block: () -> Unit"), Some("() -> Unit".into()));
}

#[test]
fn last_param_type_multiple_params() {
    let params = "a: String, b: Int, c: Boolean";
    assert_eq!(last_fun_param_type_str(params), Some("Boolean".into()));
}

#[test]
fn last_param_type_lambda_last() {
    // The trailing lambda param after a `->` in the type must be parsed correctly.
    let params = "key: ProductKey, map: (ResultState<T>) -> StatefulModel";
    assert_eq!(
        last_fun_param_type_str(params),
        Some("(ResultState<T>) -> StatefulModel".into())
    );
}

#[test]
fn last_param_type_arrow_depth_not_confused() {
    // `reloadableProduct` has two functional-type params; the `>` of `->` must
    // not throw off the depth counter so that the last param is picked correctly.
    let params =
        "key: ProductKey, productFlow: (isRefresh: Boolean) -> Flow<ResultState<T>>, map: (ResultState<T>) -> StatefulModel<SortableProducts>";
    assert_eq!(
        last_fun_param_type_str(params),
        Some("(ResultState<T>) -> StatefulModel<SortableProducts>".into())
    );
}

#[test]
fn last_param_type_empty_returns_none() {
    assert_eq!(last_fun_param_type_str(""), None);
}

// ─── split_params_at_depth_zero ──────────────────────────────────────────────

use super::split_params_at_depth_zero;

#[test]
fn split_simple() {
    assert_eq!(split_params_at_depth_zero("a: A, b: B"), vec!["a: A", " b: B"]);
}

#[test]
fn split_nested_generics() {
    // comma inside <> must not split
    assert_eq!(split_params_at_depth_zero("a: Map<K, V>, b: B"), vec!["a: Map<K, V>", " b: B"]);
}

#[test]
fn split_function_type_arrow() {
    // `->` must not cause `>` to consume generic depth
    assert_eq!(split_params_at_depth_zero("block: (T) -> Unit, n: Int"),
               vec!["block: (T) -> Unit", " n: Int"]);
}

#[test]
fn split_empty() {
    assert_eq!(split_params_at_depth_zero(""), vec![""]);
}

#[test]
fn split_single() {
    assert_eq!(split_params_at_depth_zero("a: A"), vec!["a: A"]);
}

#[test]
fn split_trailing_comma() {
    let parts = split_params_at_depth_zero("a: A, b: B,");
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[2], "");
}

// ─── strip_trailing_call_args ─────────────────────────────────────────────────

#[test]
fn strip_args_with_trailing_parens() {
    assert_eq!(strip_trailing_call_args("collection.method(arg1, arg2)"), "collection.method");
}

#[test]
fn strip_args_no_trailing_parens() {
    assert_eq!(strip_trailing_call_args("collection.forEach"), "collection.forEach");
}

#[test]
fn strip_args_nested_parens() {
    assert_eq!(strip_trailing_call_args("fn(a, g(x))"), "fn");
}

#[test]
fn strip_args_empty_parens() {
    assert_eq!(strip_trailing_call_args("build()"), "build");
}

#[test]
fn strip_args_dotted_method_with_args() {
    assert_eq!(strip_trailing_call_args("state.copy(id = x)"), "state.copy");
}

#[test]
fn strip_args_unbalanced_no_crash() {
    // If parens are unbalanced, should not panic; returns original.
    assert_eq!(strip_trailing_call_args("fn("), "fn(");
}

// ─── Regression: `>` operator in default values must not go negative ─────────

#[test]
fn nth_param_type_gt_operator_in_default() {
    // `x: Int = a > b` — the `>` is a comparison, not a generic close.
    // Must not make depth go negative and break subsequent comma splitting.
    let params = "x: Int, y: String";
    assert_eq!(nth_fun_param_type_str(params, 1), Some("String".to_owned()));
}
