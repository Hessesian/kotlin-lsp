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
fn extension_fn_default_param_not_required() {
    // `cancel()` should be valid — `cause` has a default value.
    // This tests that extract_detail preserves the `= null` part even
    // when the signature is multiline.
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun CoroutineContext.cancel(cause: CancellationException? = null) {\n",
            "}\n",
            "fun test(ctx: CoroutineContext) {\n",
            "    ctx.cancel()\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert!(
        diags.is_empty(),
        "cancel() with default param should not error: {diags:?}"
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

#[test]
fn trailing_lambda_with_args_skipped() {
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun <T> withContext(context: Any, block: suspend () -> T): T = TODO()\n",
            "fun launch(context: Any, block: suspend () -> Unit): Unit = TODO()\n",
            "fun observe(owner: Any, observer: (String) -> Unit) {}\n",
            "class Vm {\n",
            "    fun load() {\n",
            "        withContext(dispatcher) {\n",
            "            doSomething()\n",
            "        }\n",
            "        launch(dispatcher) {\n",
            "            doSomething()\n",
            "        }\n",
            "        observe(this) { value ->\n",
            "            doSomething()\n",
            "        }\n",
            "    }\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert!(
        diags.is_empty(),
        "trailing lambda with preceding args should be skipped: {diags:?}"
    );
}

#[test]
fn trailing_lambda_same_line_three_args_skipped() {
    let (uri, idx, src) = setup(&[(
        "/a.kt",
        concat!(
            "fun <T> loadProduct(key: String, data: T, mapper: (T) -> Any) {}\n",
            "fun getData(): String = \"\"\n",
            "fun main() {\n",
            "    loadProduct(\"A\", getData()) { it.toString() }\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert!(
        diags.is_empty(),
        "trailing lambda same line should be skipped: {diags:?}"
    );
}

#[test]
fn same_file_diagnostic_retype_cycle() {
    // Regression: type loadData() → delete → retype should still emit diagnostic.
    // Tests that index_content + call_arg_diagnostics work correctly on re-index
    // when content returns to a previously-seen state.
    let idx = Indexer::new();
    let u = uri("/a.kt");

    let with_call = concat!(
        "fun loadData(arg: String) {}\n",
        "fun main() {\n",
        "    loadData()\n",
        "}\n",
    );
    let without_call = concat!("fun loadData(arg: String) {}\n", "fun main() {\n", "}\n",);

    // Step 1: type the call → should get diagnostic
    idx.index_content(&u, with_call);
    let diags1 = run_diagnostics(&idx, &u, with_call);
    assert!(!diags1.is_empty(), "step1: expected diagnostic, got none");

    // Step 2: delete the call → should be clear
    idx.index_content(&u, without_call);
    let diags2 = run_diagnostics(&idx, &u, without_call);
    assert!(
        diags2.is_empty(),
        "step2: expected no diagnostic after delete"
    );

    // Step 3: retype the call → should get diagnostic again
    idx.index_content(&u, with_call);
    let diags3 = run_diagnostics(&idx, &u, with_call);
    assert!(
        !diags3.is_empty(),
        "step3: expected diagnostic after retype, got none"
    );
}

#[test]
fn same_file_diagnostic_with_live_lines_cycle() {
    // Mirrors production: set_live_lines called before index_content.
    // Tests that even when index_content returns None (hash-cache hit),
    // call_arg_diagnostics still fires using diag_indexer.files.
    let idx = Indexer::new();
    let u = uri("/a.kt");

    let with_call = concat!(
        "fun loadData(arg: String) {}\n",
        "fun main() {\n",
        "    loadData()\n",
        "}\n",
    );
    let without_call = concat!("fun loadData(arg: String) {}\n", "fun main() {\n", "}\n",);

    // Step 1: type the call (production: set_live_lines THEN index_content)
    idx.set_live_lines(&u, with_call);
    idx.index_content(&u, with_call);
    let diags1 = run_diagnostics(&idx, &u, with_call);
    assert!(!diags1.is_empty(), "step1: expected diagnostic");

    // Step 2: delete call
    idx.set_live_lines(&u, without_call);
    idx.index_content(&u, without_call);
    let diags2 = run_diagnostics(&idx, &u, without_call);
    assert!(diags2.is_empty(), "step2: expected no diagnostic");

    // Step 3: simulate save from disk (re-indexes H1) — mimics handle_file_saved
    // racing with or preceding the debounce task
    idx.index_content(&u, with_call); // ← now content_hash = H1 again

    // Step 4: retype — index_content called with H1, but content_hash already H1
    // (from step 3), so it returns None. Diagnostics must still work via files.
    idx.set_live_lines(&u, with_call);
    let none_result = idx.index_content(&u, with_call); // should be None = hash-cache hit
                                                        // We still need diagnostics to fire using stale diag_indexer.files (set in step 3)
    let diags3 = if none_result.is_none() {
        // Production code: use diag_indexer.files when index_content returned None
        let doc = crate::indexer::live_tree::parse_live(with_call, tree_sitter_kotlin::language())
            .unwrap();
        call_arg_diagnostics(&idx, &u, &doc)
    } else {
        run_diagnostics(&idx, &u, with_call)
    };
    assert!(
        !diags3.is_empty(),
        "step4: expected diagnostic after retype (hash-cache hit path)"
    );
}

/// Regression: method call with wrong args inside a coroutine lambda (withContext)
/// should still produce a diagnostic. The function is defined in the same class.
#[test]
fn method_call_wrong_args_inside_coroutine_lambda() {
    let (uri, idx, src) = setup(&[(
        "/Interactor.kt",
        concat!(
            "class Interactor {\n",
            "  suspend fun loadData(args: String): String {\n",
            "    return withContext(Dispatchers.IO) {\n",
            "      loadData()\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert!(
        !diags.is_empty(),
        "expected diagnostic for loadData() inside withContext lambda: {diags:?}"
    );
}

/// Regression: override with base class also indexed — both define loadData with 1 arg.
/// The override in the derived class should still produce a diagnostic for loadData().
#[test]
fn method_call_wrong_args_with_base_class_indexed() {
    let idx = Indexer::new();
    let base_uri = uri("/LoadingInteractor.kt");
    let base_src = concat!(
        "abstract class LoadingInteractor<Args : Any, Result : Any> {\n",
        "  abstract suspend fun loadData(args: Args): Result\n",
        "}\n",
    );
    idx.index_content(&base_uri, base_src);
    idx.store_live_tree(&base_uri, base_src);

    let impl_src = concat!(
        "class ShowChildNewTipsInteractor : LoadingInteractor<String, String>() {\n",
        "  override suspend fun loadData(args: String): String {\n",
        "    return withContext(ioDispatcher) {\n",
        "      loadData()\n", // ← 0 args — should fire diagnostic
        "      \"\"\n",
        "    }\n",
        "  }\n",
        "}\n",
    );
    let impl_uri = uri("/ShowChildNewTipsInteractor.kt");
    idx.index_content(&impl_uri, impl_src);
    idx.store_live_tree(&impl_uri, impl_src);

    let doc = parse_live(impl_src, tree_sitter_kotlin::language()).unwrap();
    let diags = call_arg_diagnostics(&idx, &impl_uri, &doc);
    assert!(
        !diags.is_empty(),
        "expected diagnostic for loadData() with base class indexed: {diags:?}"
    );
}

/// Regression: workspace has many same-name functions with different arities.
/// Unqualified call should be resolved against the CURRENT FILE only,
/// not skipped because of 945+ same-name functions in the workspace.
#[test]
fn method_call_same_file_wins_over_workspace_overloads() {
    let idx = Indexer::new();

    // Simulate 3 other files with different arities for "loadData"
    for i in 0..3 {
        let other_uri = uri(&format!("/Other{i}.kt"));
        // Each has loadData with a different number of args
        let other_src = format!(
            "class Other{i} {{\n  fun loadData({}) {{}}\n}}\n",
            std::iter::repeat("x: Int")
                .take(i + 2)
                .collect::<Vec<_>>()
                .join(", ")
        );
        idx.index_content(&other_uri, &other_src);
    }

    // Current file has loadData with 1 required arg — call with 0 should diagnose
    let src = concat!(
        "class MyClass {\n",
        "  fun loadData(arg: String) {}\n",
        "  fun test() {\n",
        "    loadData()\n",
        "  }\n",
        "}\n",
    );
    let u = uri("/MyClass.kt");
    idx.index_content(&u, src);
    let doc = parse_live(src, tree_sitter_kotlin::language()).unwrap();
    let diags = call_arg_diagnostics(&idx, &u, &doc);
    assert!(
        !diags.is_empty(),
        "expected diagnostic even with many workspace overloads: {diags:?}"
    );
}

/// Regression: same as above but with multiple chained lambdas inside withContext,
/// mirroring the actual production file shape that was showing 0 diagnostics.
#[test]
fn method_call_wrong_args_inside_complex_coroutine_lambda() {
    let (uri, idx, src) = setup(&[(
        "/Interactor.kt",
        concat!(
            "class ShowChildNewTipsInteractor {\n",
            "  sealed interface TipsResult {\n",
            "    data object No : TipsResult\n",
            "  }\n",
            "  override suspend fun loadData(args: String): TipsResult {\n",
            "    return withContext(ioDispatcher) {\n",
            "      loadData()\n", // ← the call under test
            "      val x = settings?.let {\n",
            "        if (it == 0L) System.currentTimeMillis().also {\n",
            "          settings = it\n",
            "        } else it\n",
            "      }?.let { it > 0L } ?: false\n",
            "      TipsResult.No\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    )]);
    let diags = run_diagnostics(&idx, &uri, &src);
    assert!(
        !diags.is_empty(),
        "expected diagnostic for loadData() in complex coroutine context: {diags:?}"
    );
}
