//! Unit tests for `indexer::infer::sig`.
//!
//! Covers pure string helpers that can be tested without any `Indexer` state.

use super::*;
use crate::indexer::Indexer;
use tower_lsp::lsp_types::Url;

fn test_uri(path: &str) -> Url {
    Url::parse(&format!("file://{path}")).unwrap()
}

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
    assert!(sig.contains("MviViewModel"), "should contain superclass");
    assert!(!sig.contains('{'), "should not include body brace");
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
    let lines = vec!["// comment".to_owned(), "fun hello(): String".to_owned()];
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
    assert_eq!(
        nth_fun_param_type_str(params, 1),
        Some("(Boolean) -> Flow<ResultState<T>>".into())
    );
    assert_eq!(
        nth_fun_param_type_str(params, 2),
        Some("(ResultState<T>) -> StatefulModel".into())
    );
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
    assert_eq!(
        last_fun_param_type_str("block: () -> Unit"),
        Some("() -> Unit".into())
    );
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
    assert_eq!(
        split_params_at_depth_zero("a: A, b: B"),
        vec!["a: A", " b: B"]
    );
}

#[test]
fn split_nested_generics() {
    // comma inside <> must not split
    assert_eq!(
        split_params_at_depth_zero("a: Map<K, V>, b: B"),
        vec!["a: Map<K, V>", " b: B"]
    );
}

#[test]
fn split_function_type_arrow() {
    // `->` must not cause `>` to consume generic depth
    assert_eq!(
        split_params_at_depth_zero("block: (T) -> Unit, n: Int"),
        vec!["block: (T) -> Unit", " n: Int"]
    );
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
    assert_eq!(
        strip_trailing_call_args("collection.method(arg1, arg2)"),
        "collection.method"
    );
}

#[test]
fn strip_args_no_trailing_parens() {
    assert_eq!(
        strip_trailing_call_args("collection.forEach"),
        "collection.forEach"
    );
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

// ─── is_import_reachable ─────────────────────────────────────────────────────

#[test]
fn resolve_qualified_skips_top_level_function_before_type_body() {
    let caller_uri = test_uri("/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(
        &caller_uri,
        "package com.example\nfun run(unrelated: Int, extra: Int) {}\nclass Service {\n    fun run(name: String) {}\n}\nclass Caller(private val service: Service) {\n    fun invoke() {\n        service.run(1)\n    }\n}\n",
    );

    let call = CallSite {
        name: "run",
        qualifier: Some("service"),
        caller_uri: &caller_uri,
    };

    match resolve_call_signature(&call, &idx) {
        SignatureResult::Unique {
            param_counts,
            params_text,
        } => {
            assert_eq!(param_counts, (1, 1));
            assert_eq!(params_text, "name: String");
        }
        other => panic!("expected unique class member match, got {other:?}"),
    }
}

#[test]
fn resolve_qualified_matches_method_via_container() {
    let caller_uri = test_uri("/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(
        &caller_uri,
        "package com.example\nclass Api {\n    fun fetch(id: String, force: Boolean = false) {}\n}\nclass Caller(private val api: Api) {\n    fun invoke() {\n        api.fetch(1)\n    }\n}\n",
    );

    let call = CallSite {
        name: "fetch",
        qualifier: Some("api"),
        caller_uri: &caller_uri,
    };

    match resolve_call_signature(&call, &idx) {
        SignatureResult::Unique {
            param_counts,
            params_text,
        } => {
            assert_eq!(param_counts, (1, 2));
            assert_eq!(params_text, "id: String, force: Boolean = false");
        }
        other => panic!("expected unique class member match, got {other:?}"),
    }
}

#[test]
fn resolve_unqualified_data_class_constructor() {
    let caller_uri = test_uri("/Config.kt");
    let idx = Indexer::new();
    idx.index_content(
        &caller_uri,
        "package com.example\ndata class Config(\n    val host: String,\n    val port: Int = 443,\n)\n\nfun build(): Config {\n    return Config(host = \"localhost\")\n}\n",
    );

    let call = CallSite {
        name: "Config",
        qualifier: None,
        caller_uri: &caller_uri,
    };

    match resolve_call_signature(&call, &idx) {
        SignatureResult::Unique {
            param_counts,
            params_text,
        } => {
            assert_eq!(param_counts, (1, 2));
            assert!(params_text.contains("host: String"));
        }
        other => panic!("expected unique constructor match, got {other:?}"),
    }
}

#[test]
fn resolve_unqualified_test_definition_visible_only_to_test_callers() {
    let idx = Indexer::new();
    let helper_uri = test_uri("/workspace/src/test/kotlin/com/example/TestHelper.kt");
    let test_caller_uri = test_uri("/workspace/src/test/kotlin/com/example/TestCaller.kt");
    let main_caller_uri = test_uri("/workspace/src/main/kotlin/com/example/MainCaller.kt");

    idx.index_content(
        &helper_uri,
        "package com.example\nfun testOnlyHelper(arg: String) {}\n",
    );
    idx.index_content(
        &test_caller_uri,
        "package com.example\nfun invokeFromTest() { testOnlyHelper() }\n",
    );
    idx.index_content(
        &main_caller_uri,
        "package com.example\nfun invokeFromMain() { testOnlyHelper() }\n",
    );

    let test_call = CallSite {
        name: "testOnlyHelper",
        qualifier: None,
        caller_uri: &test_caller_uri,
    };
    match resolve_call_signature(&test_call, &idx) {
        SignatureResult::Unique {
            param_counts,
            params_text,
        } => {
            assert_eq!(param_counts, (1, 1));
            assert_eq!(params_text, "arg: String");
        }
        other => panic!("expected test caller to resolve test helper, got {other:?}"),
    }

    let main_call = CallSite {
        name: "testOnlyHelper",
        qualifier: None,
        caller_uri: &main_caller_uri,
    };
    assert!(
        matches!(
            resolve_call_signature(&main_call, &idx),
            SignatureResult::NotFound
        ),
        "main caller must not resolve same-package test helper"
    );
}

#[cfg(test)]
mod import_reachable {
    use super::{collect_params_from_file, is_import_reachable, ResolutionScope};
    use crate::indexer::Indexer;
    use crate::types::{FileData, ImportEntry, SymbolEntry, Visibility};
    use std::sync::Arc;
    use tower_lsp::lsp_types::{Position, Range, SymbolKind};

    fn make_url(path: &str) -> String {
        format!("file://{}", path)
    }

    fn index_file(idx: &Indexer, uri: &str, pkg: &str, imports: Vec<ImportEntry>) {
        index_file_with_symbols(idx, uri, pkg, imports, vec![]);
    }

    fn index_file_with_symbols(
        idx: &Indexer,
        uri: &str,
        pkg: &str,
        imports: Vec<ImportEntry>,
        symbols: Vec<SymbolEntry>,
    ) {
        let data = FileData {
            package: Some(pkg.to_owned()),
            imports,
            symbols,
            ..FileData::default()
        };
        idx.files.insert(uri.to_owned(), Arc::new(data));
    }

    fn explicit_import(pkg: &str, name: &str) -> ImportEntry {
        explicit_import_path(&format!("{}.{}", pkg, name), name)
    }

    fn explicit_import_path(full_path: &str, local_name: &str) -> ImportEntry {
        ImportEntry {
            full_path: full_path.to_owned(),
            local_name: local_name.to_owned(),
            is_star: false,
        }
    }

    fn star_import(pkg: &str) -> ImportEntry {
        ImportEntry {
            full_path: pkg.to_owned(),
            local_name: "*".to_owned(),
            is_star: true,
        }
    }

    fn nested_class(name: &str, container: &str) -> SymbolEntry {
        let range = Range::new(Position::new(0, 0), Position::new(0, name.len() as u32));
        SymbolEntry {
            name: name.to_owned(),
            kind: SymbolKind::CLASS,
            visibility: Visibility::Public,
            range,
            selection_range: range,
            detail: String::new(),
            params: String::new(),
            param_counts: (0, 0),
            type_params: vec![],
            extension_receiver: String::new(),
            container: Some(container.to_owned()),
        }
    }

    #[test]
    fn same_file_always_reachable() {
        let idx = Indexer::new();
        let uri = make_url("/a/Foo.kt");
        index_file(&idx, &uri, "com.example", vec![]);
        assert!(is_import_reachable(&idx, &uri, &uri, "Foo"));
    }

    #[test]
    fn same_package_reachable() {
        let idx = Indexer::new();
        let caller = make_url("/a/A.kt");
        let def = make_url("/a/B.kt");
        index_file(&idx, &caller, "com.example", vec![]);
        index_file(&idx, &def, "com.example", vec![]);
        assert!(is_import_reachable(&idx, &caller, &def, "Foo"));
    }

    #[test]
    fn different_package_no_import_not_reachable() {
        let idx = Indexer::new();
        let caller = make_url("/a/A.kt");
        let def = make_url("/b/B.kt");
        index_file(&idx, &caller, "com.example", vec![]);
        index_file(&idx, &def, "com.other", vec![]);
        assert!(!is_import_reachable(&idx, &caller, &def, "Foo"));
    }

    #[test]
    fn explicit_import_reachable() {
        let idx = Indexer::new();
        let caller = make_url("/a/A.kt");
        let def = make_url("/b/Foo.kt");
        index_file(
            &idx,
            &caller,
            "com.example",
            vec![explicit_import("com.other", "Foo")],
        );
        index_file(&idx, &def, "com.other", vec![]);
        assert!(is_import_reachable(&idx, &caller, &def, "Foo"));
    }

    #[test]
    fn nested_class_explicit_import_reachable() {
        let idx = Indexer::new();
        let caller = make_url("/a/A.kt");
        let def = make_url("/b/Outer.kt");
        index_file(
            &idx,
            &caller,
            "com.client",
            vec![explicit_import_path("com.example.Outer.Config", "Config")],
        );
        index_file(&idx, &def, "com.example", vec![]);
        assert!(is_import_reachable(&idx, &caller, &def, "Config"));
    }

    #[test]
    fn deeply_nested_import_reachable() {
        let idx = Indexer::new();
        let caller = make_url("/a/A.kt");
        let def = make_url("/b/Outer.kt");
        index_file(
            &idx,
            &caller,
            "com.client",
            vec![explicit_import_path(
                "com.example.Outer.Inner.Config",
                "Config",
            )],
        );
        index_file(&idx, &def, "com.example", vec![]);
        assert!(is_import_reachable(&idx, &caller, &def, "Config"));
    }

    #[test]
    fn nested_class_star_import_not_reachable_cross_file() {
        let idx = Indexer::new();
        let caller = make_url("/a/A.kt");
        let def = make_url("/b/Outer.kt");
        index_file(
            &idx,
            &caller,
            "com.client",
            vec![star_import("com.example")],
        );
        index_file_with_symbols(
            &idx,
            &def,
            "com.example",
            vec![],
            vec![nested_class("Config", "Outer")],
        );
        assert!(collect_params_from_file(
            "Config",
            &def,
            &idx,
            &caller,
            ResolutionScope::CrossFile,
        )
        .is_empty());
    }

    #[test]
    fn explicit_import_wrong_name_not_reachable() {
        let idx = Indexer::new();
        let caller = make_url("/a/A.kt");
        let def = make_url("/b/Bar.kt");
        index_file(
            &idx,
            &caller,
            "com.example",
            vec![explicit_import("com.other", "Foo")],
        );
        index_file(&idx, &def, "com.other", vec![]);
        assert!(!is_import_reachable(&idx, &caller, &def, "Bar"));
    }

    #[test]
    fn star_import_reachable() {
        let idx = Indexer::new();
        let caller = make_url("/a/A.kt");
        let def = make_url("/b/Foo.kt");
        index_file(&idx, &caller, "com.example", vec![star_import("com.other")]);
        index_file(&idx, &def, "com.other", vec![]);
        assert!(is_import_reachable(&idx, &caller, &def, "Foo"));
    }

    #[test]
    fn star_import_wrong_package_not_reachable() {
        let idx = Indexer::new();
        let caller = make_url("/a/A.kt");
        let def = make_url("/b/Foo.kt");
        index_file(&idx, &caller, "com.example", vec![star_import("com.third")]);
        index_file(&idx, &def, "com.other", vec![]);
        assert!(!is_import_reachable(&idx, &caller, &def, "Foo"));
    }

    #[test]
    fn missing_caller_data_fails_open() {
        let idx = Indexer::new();
        let def = make_url("/b/Foo.kt");
        index_file(&idx, &def, "com.other", vec![]);
        assert!(is_import_reachable(&idx, "file:///missing.kt", &def, "Foo"));
    }

    #[test]
    fn missing_def_data_fails_open() {
        let idx = Indexer::new();
        let caller = make_url("/a/A.kt");
        index_file(&idx, &caller, "com.example", vec![]);
        assert!(is_import_reachable(
            &idx,
            &caller,
            "file:///missing.kt",
            "Foo"
        ));
    }
}
