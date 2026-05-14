use tower_lsp::lsp_types::Url;

use crate::indexer::live_tree::parse_live;
use crate::indexer::Indexer;

use super::call_arg_diagnostics;

fn uri(path: &str) -> Url {
    Url::parse(&format!("file:///test{path}")).unwrap()
}

fn setup(sources: &[(&str, &str)]) -> (Url, Indexer, String) {
    let idx = Indexer::new();
    let mut last_uri = uri("/test.kt");
    let mut last_src = String::new();
    for (path, src) in sources {
        let u = uri(path);
        idx.index_content(&u, src);
        idx.store_live_tree(&u, src);
        last_uri = u;
        last_src = (*src).to_string();
    }
    (last_uri, idx, last_src)
}

/// Run diagnostics using a locally-parsed tree (mirrors production flow).
fn run_diagnostics(
    idx: &Indexer,
    uri: &Url,
    source: &str,
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let doc = parse_live(source, tree_sitter_kotlin::language()).unwrap();
    call_arg_diagnostics(idx, uri, &doc)
}

#[test]
fn no_diagnostic_when_args_match() {
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun greet(name: String, age: Int) {}\n",
            "fun main() {\n",
            "    greet(\"Alice\", 30)\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert!(diags.is_empty(), "expected no diagnostics: {diags:?}");
}

#[test]
fn too_few_args_warns() {
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun greet(name: String, age: Int) {}\n",
            "fun main() {\n",
            "    greet(\"Alice\")\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
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
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun greet(name: String) {}\n",
            "fun main() {\n",
            "    greet(\"Alice\", 30, true)\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
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
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun greet(name: String, greeting: String = \"Hello\") {}\n",
            "fun main() {\n",
            "    greet(\"Alice\")\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert!(
        diags.is_empty(),
        "default param should not be required: {diags:?}"
    );
}

#[test]
fn default_params_still_cap_max() {
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun greet(name: String, greeting: String = \"Hello\") {}\n",
            "fun main() {\n",
            "    greet(\"Alice\", \"Hi\", \"extra\")\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert_eq!(diags.len(), 1, "too many args: {diags:?}");
    assert!(
        diags[0].message.contains("at most 2"),
        "msg: {}",
        diags[0].message
    );
}

#[test]
fn named_args_skipped() {
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun greet(name: String, age: Int) {}\n",
            "fun main() {\n",
            "    greet(name = \"Alice\")\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert!(diags.is_empty(), "named args should be skipped: {diags:?}");
}

#[test]
fn trailing_lambda_skipped() {
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun run(action: () -> Unit) {}\n",
            "fun main() {\n",
            "    run { println(\"hi\") }\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert!(
        diags.is_empty(),
        "trailing lambda should be skipped: {diags:?}"
    );
}

#[test]
fn vararg_skipped() {
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun log(vararg messages: String) {}\n",
            "fun main() {\n",
            "    log(\"a\", \"b\", \"c\", \"d\")\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert!(diags.is_empty(), "vararg should be skipped: {diags:?}");
}

#[test]
fn cross_file_resolution() {
    let (uri, idx, src) = setup(&[
        ("/lib.kt", "fun helper(x: Int, y: Int, z: Int) {}\n"),
        (
            "/main.kt",
            concat!("fun main() {\n", "    helper(1)\n", "}\n",),
        ),
    ]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert_eq!(diags.len(), 1, "cross-file: {diags:?}");
    assert!(
        diags[0].message.contains("expected 3"),
        "msg: {}",
        diags[0].message
    );
}

#[test]
fn zero_args_when_params_required() {
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun process(data: String) {}\n",
            "fun main() {\n",
            "    process()\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert_eq!(diags.len(), 1, "zero args: {diags:?}");
    assert!(
        diags[0].message.contains("found 0"),
        "msg: {}",
        diags[0].message
    );
}

#[test]
fn no_params_no_args_ok() {
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!("fun noop() {}\n", "fun main() {\n", "    noop()\n", "}\n",),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert!(diags.is_empty(), "no params, no args: {diags:?}");
}

#[test]
fn complex_default_value_detected() {
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun config(timeout: Int = 30, retries: Int = 3, label: String) {}\n",
            "fun main() {\n",
            "    config(label = \"x\")\n",
            "}\n",
        ),
    )]);
    // Named arg → skipped
    let diags = run_diagnostics(&idx, &uri, &src);
    assert!(diags.is_empty(), "named arg with defaults: {diags:?}");
}

#[test]
fn function_type_default_not_confused() {
    // `=` inside a function type like `(Int) -> String` should not be treated as default
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun transform(mapper: (Int) -> String, fallback: String) {}\n",
            "fun main() {\n",
            "    transform({ it.toString() }, \"none\")\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert!(
        diags.is_empty(),
        "function type param not confused: {diags:?}"
    );
}

#[test]
fn diagnostic_on_correct_call_not_next_line() {
    let src = concat!(
        "class FamilyAccount(val members: List<String>)\n",
        "fun loadData(account: FamilyAccount, refresh: Boolean) {}\n",
        "suspend fun test() {\n",
        "    loadData(FamilyAccount(listOf()))\n",
        "    return withContext(ioDispatcher) {\n",
        "    }\n",
        "}\n",
    );
    let (uri, idx, _) = setup(&[("/a.kt", src)]);
    let diags = run_diagnostics(&idx, &uri, src);
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
    let idx = Indexer::new();

    let test_uri = uri("/src/test/kotlin/MyTest.kt");
    idx.index_content(&test_uri, "fun loadData() { /* test helper */ }\n");

    let main_uri = uri("/src/main/kotlin/Main.kt");
    let main_src = concat!(
        "fun loadData(account: String, refresh: Boolean) {}\n",
        "fun caller() {\n",
        "    loadData()\n",
        "}\n",
    );
    idx.index_content(&main_uri, main_src);

    let diags = run_diagnostics(&idx, &main_uri, main_src);
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

#[test]
fn no_stale_diagnostic_after_deleting_bad_call() {
    let idx = Indexer::new();

    let lib_uri = uri("/lib.kt");
    let lib_src = "fun loadData(account: String, refresh: Boolean) {}\n";
    idx.index_content(&lib_uri, lib_src);

    let main_uri = uri("/main.kt");

    // Step 1: file has the bad call
    let src_before = concat!(
        "suspend fun test() {\n",
        "    loadData()\n",
        "    withContext(ioDispatcher) {\n",
        "        doWork()\n",
        "    }\n",
        "}\n",
    );
    idx.index_content(&main_uri, src_before);
    let diags = run_diagnostics(&idx, &main_uri, src_before);
    assert_eq!(diags.len(), 1, "before deletion: {diags:?}");
    assert!(
        diags[0].message.contains("expected 2"),
        "before: {}",
        diags[0].message
    );

    // Step 2: user deletes loadData() line
    let src_after = concat!(
        "suspend fun test() {\n",
        "    withContext(ioDispatcher) {\n",
        "        doWork()\n",
        "    }\n",
        "}\n",
    );
    idx.index_content(&main_uri, src_after);
    let diags = run_diagnostics(&idx, &main_uri, src_after);
    assert!(
        diags.is_empty(),
        "after deletion, no diagnostic should remain: {diags:?}"
    );
}

#[test]
fn no_false_diagnostic_on_incomplete_trailing_lambda() {
    let idx = Indexer::new();

    let lib_uri = uri("/lib.kt");
    idx.index_content(
        &lib_uri,
        "suspend fun <T> withContext(context: CoroutineContext, block: suspend CoroutineScope.() -> T): T {}\n",
    );

    let main_uri = uri("/a.kt");
    let src = concat!(
        "override suspend fun loadData(args: FamilyAccount): TipsResult {\n",
        "    loadData()\n",
        "    return withContext(ioDispatcher) {\n",
    );
    idx.index_content(&main_uri, src);

    let diags = run_diagnostics(&idx, &main_uri, src);
    for d in &diags {
        eprintln!(
            "  diag line={} col={}: {}",
            d.range.start.line, d.range.start.character, d.message
        );
    }
    // withContext should NOT be flagged (trailing lambda, even if unclosed)
    let flagged_lines: Vec<_> = diags.iter().map(|d| d.range.start.line).collect();
    assert!(
        !flagged_lines.contains(&2),
        "withContext on line 2 should not be flagged: {diags:?}"
    );
}

#[test]
fn no_diagnostic_on_withcontext_after_deletion() {
    // After user deletes a bad call, withContext(x) { ... } must not be flagged.
    let idx = Indexer::new();

    let lib_uri = uri("/lib.kt");
    idx.index_content(
        &lib_uri,
        "suspend fun <T> withContext(context: CoroutineContext, block: suspend CoroutineScope.() -> T): T {}\n",
    );

    let def_uri = uri("/def.kt");
    idx.index_content(
        &def_uri,
        "fun loadData(args: String, refresh: Boolean) {}\n",
    );

    let main_uri = uri("/main.kt");

    // Step 1: verify the "after deletion" state has no diagnostics
    let src_after = concat!(
        "override suspend fun doWork(): String {\n",
        "    return withContext(ioDispatcher) {\n",
        "        \"result\"\n",
        "    }\n",
        "}\n",
    );
    idx.index_content(&main_uri, src_after);
    let diags = run_diagnostics(&idx, &main_uri, src_after);
    for d in &diags {
        eprintln!(
            "  UNEXPECTED diag line={} col={}: {}",
            d.range.start.line, d.range.start.character, d.message
        );
    }
    assert!(
        diags.is_empty(),
        "withContext with trailing lambda should not be flagged: {diags:?}"
    );
}

#[test]
fn no_false_diagnostic_on_let_lambda_chain() {
    let src = concat!(
        "fun toMillis(days: Int): Long = 0L\n",
        "class Foo {\n",
        "  var familyCreationDate: Long? = null\n",
        "  fun test() {\n",
        "    val result = familyCreationDate\n",
        "      ?.let {\n",
        "        if (it == 0L) System.currentTimeMillis().also {\n",
        "          familyCreationDate = it\n",
        "        } else it\n",
        "      }\n",
        "      ?.let { System.currentTimeMillis() - it }\n",
        "      ?.let { it > toMillis(2) } ?: false\n",
        "  }\n",
        "}\n",
    );
    let (uri, idx, _) = setup(&[("/chain.kt", src)]);
    let diags = run_diagnostics(&idx, &uri, src);
    for d in &diags {
        eprintln!(
            "  UNEXPECTED diag line={} col={}: {}",
            d.range.start.line, d.range.start.character, d.message
        );
    }
    assert!(
        diags.is_empty(),
        "let/also lambda chains should not produce diagnostics: {diags:?}"
    );
}
