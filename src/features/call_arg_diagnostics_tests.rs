use tower_lsp::lsp_types::Url;

use crate::indexer::Indexer;

use super::call_arg_diagnostics;

fn uri(path: &str) -> Url {
    Url::parse(&format!("file:///test{path}")).unwrap()
}

fn setup(sources: &[(&str, &str)]) -> (Url, Indexer) {
    let idx = Indexer::new();
    let mut last_uri = uri("/test.kt");
    for (path, src) in sources {
        let u = uri(path);
        idx.index_content(&u, src);
        idx.store_live_tree(&u, src);
        last_uri = u;
    }
    (last_uri, idx)
}

#[test]
fn no_diagnostic_when_args_match() {
    let (uri, idx) = setup(&[(
        "/a.kt",
        concat!(
            "fun greet(name: String, age: Int) {}\n",
            "fun main() {\n",
            "    greet(\"Alice\", 30)\n",
            "}\n",
        ),
    )]);
    let diags = call_arg_diagnostics(&idx, &uri);
    assert!(diags.is_empty(), "expected no diagnostics: {diags:?}");
}

#[test]
fn too_few_args_warns() {
    let (uri, idx) = setup(&[(
        "/a.kt",
        concat!(
            "fun greet(name: String, age: Int) {}\n",
            "fun main() {\n",
            "    greet(\"Alice\")\n",
            "}\n",
        ),
    )]);
    let diags = call_arg_diagnostics(&idx, &uri);
    assert_eq!(diags.len(), 1, "expected 1 diagnostic: {diags:?}");
    assert!(
        diags[0].message.contains("expected 2"),
        "msg: {}",
        diags[0].message
    );
    assert!(
        diags[0].message.contains("found 1"),
        "msg: {}",
        diags[0].message
    );
}

#[test]
fn too_many_args_warns() {
    let (uri, idx) = setup(&[(
        "/a.kt",
        concat!(
            "fun greet(name: String) {}\n",
            "fun main() {\n",
            "    greet(\"Alice\", 30, true)\n",
            "}\n",
        ),
    )]);
    let diags = call_arg_diagnostics(&idx, &uri);
    assert_eq!(diags.len(), 1, "expected 1 diagnostic: {diags:?}");
    assert!(
        diags[0].message.contains("at most 1"),
        "msg: {}",
        diags[0].message
    );
    assert!(
        diags[0].message.contains("found 3"),
        "msg: {}",
        diags[0].message
    );
}

#[test]
fn default_params_not_required() {
    let (uri, idx) = setup(&[(
        "/a.kt",
        concat!(
            "fun greet(name: String, greeting: String = \"Hello\") {}\n",
            "fun main() {\n",
            "    greet(\"Alice\")\n",
            "}\n",
        ),
    )]);
    let diags = call_arg_diagnostics(&idx, &uri);
    assert!(
        diags.is_empty(),
        "default param should not be required: {diags:?}"
    );
}

#[test]
fn default_params_still_cap_max() {
    let (uri, idx) = setup(&[(
        "/a.kt",
        concat!(
            "fun greet(name: String, greeting: String = \"Hello\") {}\n",
            "fun main() {\n",
            "    greet(\"Alice\", \"Hi\", \"extra\")\n",
            "}\n",
        ),
    )]);
    let diags = call_arg_diagnostics(&idx, &uri);
    assert_eq!(diags.len(), 1, "too many args: {diags:?}");
    assert!(
        diags[0].message.contains("at most 2"),
        "msg: {}",
        diags[0].message
    );
}

#[test]
fn named_args_skipped() {
    let (uri, idx) = setup(&[(
        "/a.kt",
        concat!(
            "fun greet(name: String, age: Int) {}\n",
            "fun main() {\n",
            "    greet(name = \"Alice\")\n",
            "}\n",
        ),
    )]);
    let diags = call_arg_diagnostics(&idx, &uri);
    assert!(diags.is_empty(), "named args should be skipped: {diags:?}");
}

#[test]
fn trailing_lambda_skipped() {
    let (uri, idx) = setup(&[(
        "/a.kt",
        concat!(
            "fun run(action: () -> Unit) {}\n",
            "fun main() {\n",
            "    run { println(\"hi\") }\n",
            "}\n",
        ),
    )]);
    let diags = call_arg_diagnostics(&idx, &uri);
    assert!(
        diags.is_empty(),
        "trailing lambda should be skipped: {diags:?}"
    );
}

#[test]
fn vararg_skipped() {
    let (uri, idx) = setup(&[(
        "/a.kt",
        concat!(
            "fun log(vararg messages: String) {}\n",
            "fun main() {\n",
            "    log(\"a\", \"b\", \"c\", \"d\")\n",
            "}\n",
        ),
    )]);
    let diags = call_arg_diagnostics(&idx, &uri);
    assert!(diags.is_empty(), "vararg should be skipped: {diags:?}");
}

#[test]
fn cross_file_resolution() {
    let (uri, idx) = setup(&[
        ("/lib.kt", "fun helper(x: Int, y: Int, z: Int) {}\n"),
        (
            "/main.kt",
            concat!("fun main() {\n", "    helper(1)\n", "}\n",),
        ),
    ]);
    let diags = call_arg_diagnostics(&idx, &uri);
    assert_eq!(diags.len(), 1, "cross-file: {diags:?}");
    assert!(
        diags[0].message.contains("expected 3"),
        "msg: {}",
        diags[0].message
    );
}

#[test]
fn zero_args_when_params_required() {
    let (uri, idx) = setup(&[(
        "/a.kt",
        concat!(
            "fun process(data: String) {}\n",
            "fun main() {\n",
            "    process()\n",
            "}\n",
        ),
    )]);
    let diags = call_arg_diagnostics(&idx, &uri);
    assert_eq!(diags.len(), 1, "zero args: {diags:?}");
    assert!(
        diags[0].message.contains("found 0"),
        "msg: {}",
        diags[0].message
    );
}

#[test]
fn no_params_no_args_ok() {
    let (uri, idx) = setup(&[(
        "/a.kt",
        concat!("fun noop() {}\n", "fun main() {\n", "    noop()\n", "}\n",),
    )]);
    let diags = call_arg_diagnostics(&idx, &uri);
    assert!(diags.is_empty(), "no params, no args: {diags:?}");
}

#[test]
fn complex_default_value_detected() {
    let (uri, idx) = setup(&[(
        "/a.kt",
        concat!(
            "fun config(timeout: Int = 30, retries: Int = 3, label: String) {}\n",
            "fun main() {\n",
            "    config(label = \"x\")\n",
            "}\n",
        ),
    )]);
    // Named arg → skipped
    let diags = call_arg_diagnostics(&idx, &uri);
    assert!(diags.is_empty(), "named arg with defaults: {diags:?}");
}

#[test]
fn function_type_default_not_confused() {
    // `=` inside a function type like `(Int) -> String` should not be treated as default
    let (uri, idx) = setup(&[(
        "/a.kt",
        concat!(
            "fun transform(mapper: (Int) -> String, fallback: String) {}\n",
            "fun main() {\n",
            "    transform({ it.toString() }, \"none\")\n",
            "}\n",
        ),
    )]);
    let diags = call_arg_diagnostics(&idx, &uri);
    assert!(
        diags.is_empty(),
        "function type param not confused: {diags:?}"
    );
}

#[test]
fn diagnostic_on_correct_call_not_next_line() {
    // Reproduces: diagnostic should be on `loadData()` (0 args, expects 2),
    // NOT on `withContext(ioDispatcher) { }` (trailing lambda, should be skipped).
    let (uri, idx) = setup(&[(
        "/a.kt",
        concat!(
            "class FamilyAccount(val members: List<String>)\n",
            "fun loadData(account: FamilyAccount, refresh: Boolean) {}\n",
            "suspend fun test() {\n",
            "    loadData(FamilyAccount(listOf()))\n",
            "    return withContext(ioDispatcher) {\n",
            "    }\n",
            "}\n",
        ),
    )]);
    let diags = call_arg_diagnostics(&idx, &uri);
    // loadData gets 1 arg, expects 2 → diagnostic
    // withContext has trailing lambda → skipped
    assert_eq!(
        diags.len(),
        1,
        "should be exactly one diagnostic: {diags:?}"
    );
    assert!(
        diags[0].message.contains("expected 2"),
        "should expect 2 args: {}",
        diags[0].message
    );
    // Diagnostic must be on line 3 (loadData), not line 4 (withContext)
    assert_eq!(
        diags[0].range.start.line, 3,
        "diagnostic should be on loadData line, got line {}",
        diags[0].range.start.line
    );
}

#[test]
fn test_file_functions_excluded_from_resolution() {
    // A test file defines `loadData()` with 0 params.
    // Production file defines `loadData(account: String, refresh: Boolean)`.
    // The test-file overload should be excluded so the call site gets a
    // clean single-signature match instead of being skipped as "overloaded".
    let idx = Indexer::new();

    let test_uri = uri("/src/test/kotlin/MyTest.kt");
    idx.index_content(&test_uri, "fun loadData() { /* test helper */ }\n");
    idx.store_live_tree(&test_uri, "fun loadData() { /* test helper */ }\n");

    let main_uri = uri("/src/main/kotlin/Main.kt");
    let main_src = concat!(
        "fun loadData(account: String, refresh: Boolean) {}\n",
        "fun caller() {\n",
        "    loadData()\n",
        "}\n",
    );
    idx.index_content(&main_uri, main_src);
    idx.store_live_tree(&main_uri, main_src);

    let diags = call_arg_diagnostics(&idx, &main_uri);
    assert_eq!(
        diags.len(),
        1,
        "test file overload should be excluded: {diags:?}"
    );
    assert!(
        diags[0].message.contains("expected 2"),
        "should see production signature: {}",
        diags[0].message
    );
}
