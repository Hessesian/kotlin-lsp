    use super::*;
    use crate::indexer::Indexer;
    use tower_lsp::lsp_types::Url;

    fn uri(path: &str) -> Url {
        Url::parse(&format!("file:///test{path}")).unwrap()
    }

    fn import_file_candidates(import_path: &str) -> Vec<String> {
        import_file_stems(import_path)
            .into_iter()
            .flat_map(|stem| {
                crate::rg::SOURCE_EXTENSIONS
                    .iter()
                    .map(move |ext| format!("{stem}.{ext}"))
            })
            .collect()
    }

    // ── pure helpers ─────────────────────────────────────────────────────────

    #[test]
    fn package_prefix_standard() {
        assert_eq!(package_prefix("com.example.app.MyClass"),              "com.example.app");
        assert_eq!(package_prefix("com.example.OuterClass.InnerClass"),    "com.example");
        assert_eq!(package_prefix("MyClass"),                               "");
        assert_eq!(package_prefix("com.example.Foo"),                      "com.example");
    }

    #[test]
    fn import_candidates_top_level() {
        let c = import_file_candidates("com.example.Foo");
        assert_eq!(c[0], "Foo.kt");
        assert_eq!(c[1], "Foo.java");
        assert_eq!(c[2], "Foo.swift");
    }

    #[test]
    fn import_candidates_nested() {
        let c = import_file_candidates("com.example.OuterClass.InnerClass");
        assert_eq!(c[0], "OuterClass.kt");   // outer class file tried first
        assert_eq!(c[1], "OuterClass.java");
        assert_eq!(c[2], "OuterClass.swift");
        assert_eq!(c[3], "InnerClass.kt");
        assert_eq!(c[4], "InnerClass.java");
        assert_eq!(c[5], "InnerClass.swift");
    }

    #[test]
    fn import_candidates_deeply_nested() {
        let c = import_file_candidates("a.b.Outer.Middle.Inner");
        assert_eq!(c[0], "Middle.kt");
        assert_eq!(c[1], "Middle.java");
        assert_eq!(c[2], "Middle.swift");
        assert_eq!(c[3], "Inner.kt");
        assert_eq!(c[4], "Inner.java");
        assert_eq!(c[5], "Inner.swift");
    }

    #[test]
    fn import_candidates_no_uppercase() {
        assert!(import_file_candidates("com.example.pkg").is_empty());
    }

    // ── resolve_local ────────────────────────────────────────────────────────

    #[test]
    fn resolve_local_finds_own_symbols() {
        let u = uri("/Foo.kt");
        let idx = Indexer::new();
        idx.index_content(&u, "class Foo\nclass Bar");
        let locs = resolve_symbol(&idx, "Foo", None, &u);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].uri, u);
    }

    #[test]
    fn resolve_local_not_found_returns_empty_without_rg() {
        // Symbol that doesn't exist anywhere in the index; rg will find nothing
        // in the (empty) working tree — acceptable to return vec![]
        let u = uri("/Foo.kt");
        let idx = Indexer::new();
        idx.index_content(&u, "class Foo");
        // "Xyz" is not in the index; rg likely returns nothing in tests
        let locs = resolve_symbol(&idx, "Xyz", None, &u);
        // We can't guarantee rg returns nothing in all environments,
        // so just verify local didn't find it in index.
        assert!(!locs.iter().any(|l| l.uri == u));
    }

    // ── resolve_via_imports (qualified index) ────────────────────────────────

    #[test]
    fn resolve_via_explicit_import() {
        let src_uri = uri("/src/Source.kt");
        let def_uri = uri("/src/Target.kt");
        let idx = Indexer::new();
        idx.index_content(&def_uri, "package com.example\nclass Target");
        idx.index_content(&src_uri, "package com.example\nimport com.example.Target\nval x: Target = TODO()");

        let locs = resolve_symbol(&idx, "Target", None, &src_uri);
        assert!(!locs.is_empty(), "Target not found via import");
        assert_eq!(locs[0].uri, def_uri);
    }

    #[test]
    fn resolve_via_alias_import() {
        let src_uri = uri("/src/A.kt");
        let def_uri = uri("/src/B.kt");
        let idx = Indexer::new();
        idx.index_content(&def_uri, "package com.example\nclass LongName");
        idx.index_content(&src_uri, "package com.example\nimport com.example.LongName as LN\nval x: LN = TODO()");

        // Looking up "LN" should find "LongName" in def_uri
        let locs = resolve_symbol(&idx, "LN", None, &src_uri);
        assert!(!locs.is_empty(), "aliased import not resolved");
        assert_eq!(locs[0].uri, def_uri);
    }

    // ── resolve_same_package ─────────────────────────────────────────────────

    #[test]
    fn resolve_same_package() {
        let a_uri = uri("/pkg/A.kt");
        let b_uri = uri("/pkg/B.kt");
        let idx = Indexer::new();
        idx.index_content(&a_uri, "package com.example\nclass A");
        idx.index_content(&b_uri, "package com.example\nval x: A = TODO()");

        let locs = resolve_symbol(&idx, "A", None, &b_uri);
        assert!(!locs.is_empty(), "same-package class not found");
        assert_eq!(locs[0].uri, a_uri);
    }

    #[test]
    fn resolve_does_not_cross_packages_without_import() {
        let a_uri = uri("/pkg1/A.kt");
        let b_uri = uri("/pkg2/B.kt");
        let idx = Indexer::new();
        idx.index_content(&a_uri, "package com.example.pkg1\nclass A");
        idx.index_content(&b_uri, "package com.example.pkg2"); // no import

        // rg might find it; test that same-package step doesn't leak
        let locs: Vec<_> = resolve_symbol(&idx, "A", None, &b_uri)
            .into_iter()
            .filter(|l| l.uri == a_uri)
            .collect();
        // If rg finds it that's fine, but same-package shouldn't (different packages)
        // We verify by checking the packages map didn't bridge pkg1 and pkg2
        assert!(
            idx.packages.get("com.example.pkg2").map(|u| !u.contains(&a_uri.to_string())).unwrap_or(true),
            "pkg1 URI leaked into pkg2 packages map"
        );
    }

    // ── resolve_qualified (dot accessor) ────────────────────────────────────

    #[test]
    fn resolve_qualifier_dot_access() {
        let host_uri = uri("/Host.kt");
        let outer_uri = uri("/Outer.kt");
        let idx = Indexer::new();
        idx.index_content(&outer_uri, "package com.pkg\nclass Outer {\n  class Inner\n}");
        idx.index_content(&host_uri,  "package com.pkg\nval x: Outer.Inner = TODO()");

        // Cursor on "Inner" with qualifier "Outer"
        let locs = resolve_symbol(&idx, "Inner", Some("Outer"), &host_uri);
        assert!(!locs.is_empty(), "Inner not found via qualifier");
        assert_eq!(locs[0].uri, outer_uri);
    }

    #[test]
    fn resolve_deep_qualifier_chain() {
        // A.B.C.D cursor on D → qualifier = "A.B.C"
        // resolve_qualified should resolve root "A", find its file, locate "D" in it.
        let host_uri = uri("/Host.kt");
        let root_uri = uri("/Root.kt");
        let idx = Indexer::new();
        // Root.kt defines class Root with nested class Deep
        idx.index_content(&root_uri, "package com.pkg\nclass Root {\n  class Mid {\n    class Deep\n  }\n}");
        idx.index_content(&host_uri, "package com.pkg\nval x: Root.Mid.Deep = TODO()");

        // qualifier = "Root.Mid" (full chain minus last segment), word = "Deep"
        let locs = resolve_symbol(&idx, "Deep", Some("Root.Mid"), &host_uri);
        assert!(!locs.is_empty(), "Deep not found via full qualifier chain");
        assert_eq!(locs[0].uri, root_uri);
    }

    #[test]
    fn resolve_nested_type_via_variable_annotation() {
        // `val factory: DashboardProductsReducer.Factory` — goto-def of `factory.create(...)`
        // should navigate to the `create` fun inside the `Factory` interface.
        let host_uri = uri("/Host.kt");
        let reducer_uri = uri("/DashboardProductsReducer.kt");
        let idx = Indexer::new();
        idx.index_content(&reducer_uri, concat!(
            "package com.pkg\n",
            "class DashboardProductsReducer {\n",
            "  interface Factory {\n",
            "    fun create(scope: Any): DashboardProductsReducer\n",
            "  }\n",
            "}\n",
        ));
        idx.index_content(&host_uri, concat!(
            "package com.pkg\n",
            "val factory: DashboardProductsReducer.Factory = TODO()\n",
            "fun foo() { factory.create(this) }\n",
        ));

        // Qualifier = "factory" (lowercase), word = "create"
        let locs = resolve_symbol(&idx, "create", Some("factory"), &host_uri);
        assert!(!locs.is_empty(), "create not found via nested type Factory");
        assert_eq!(locs[0].uri, reducer_uri);
    }

    #[test]
    fn infer_type_in_lines_dotted() {
        // Ensure infer_type_in_lines handles `Outer.Inner` dotted types.
        let lines: Vec<String> = vec![
            "  private val factory: DashboardProductsReducer.Factory,".to_owned()
        ];
        let t = super::infer_type_in_lines(&lines, "factory");
        assert_eq!(t.as_deref(), Some("DashboardProductsReducer.Factory"));
    }

    // ── infer_variable_type + method resolution ──────────────────────────────

    #[test]
    fn resolve_multi_hop_field_chain() {
        // vm.account.interestPlanCode where:
        //   fun foo(vm: ViewModel) – vm has field account: AccountModel
        //   AccountModel has field interestPlanCode: String
        let host_uri = uri("/Host.kt");
        let vm_uri   = uri("/ViewModel.kt");
        let acc_uri  = uri("/AccountModel.kt");
        let idx = Indexer::new();
        idx.index_content(&acc_uri,
            "package com.pkg\nclass AccountModel {\n  val interestPlanCode: String = \"\"\n}");
        idx.index_content(&vm_uri,
            "package com.pkg\nclass ViewModel {\n  val account: AccountModel = AccountModel()\n}");
        idx.index_content(&host_uri,
            "package com.pkg\nfun foo(vm: ViewModel) { vm.account.interestPlanCode }");

        // qualifier = "vm.account", name = "interestPlanCode"
        let locs = resolve_symbol(&idx, "interestPlanCode", Some("vm.account"), &host_uri);
        assert!(!locs.is_empty(), "interestPlanCode not found via multi-hop field chain");
        assert_eq!(locs[0].uri, acc_uri);
    }

    #[test]
    fn resolve_local_param_declaration() {
        // Cursor on `account` (function param without val/var) should return the
        // declaration line in the same file.
        let u = uri("/Foo.kt");
        let idx = Indexer::new();
        idx.index_content(&u, "package com.pkg\nfun foo(account: AccountModel) {\n  account.something\n}");

        let locs = resolve_symbol(&idx, "account", None, &u);
        assert!(!locs.is_empty(), "local param declaration not found");
        assert_eq!(locs[0].uri, u);
        // Line 1 (0-indexed) contains the parameter declaration
        assert_eq!(locs[0].range.start.line, 1);
    }


    #[test]
    fn resolve_method_via_variable_type_inference() {
        // repo.findById(1) where repo: UserRepository
        let vm_uri   = uri("/ViewModel.kt");
        let repo_uri = uri("/UserRepository.kt");
        let idx = Indexer::new();
        idx.index_content(&repo_uri, "package com.pkg\nclass UserRepository {\n  fun findById(id: Int) {}\n}");
        idx.index_content(&vm_uri,
            "package com.pkg\nclass ViewModel(\n  private val repo: UserRepository\n) {\n  fun load() { repo.findById(1) }\n}");

        // qualifier = "repo" (lowercase), name = "findById"
        // infer_variable_type should extract "UserRepository" from "val repo: UserRepository"
        // then resolve_qualified finds findById in UserRepository.kt
        let locs = resolve_symbol(&idx, "findById", Some("repo"), &vm_uri);
        assert!(!locs.is_empty(), "findById not found via variable type inference");
        assert_eq!(locs[0].uri, repo_uri);
    }

    #[test]
    fn resolve_method_via_constructor_param_type() {
        // interactor.loadDataFlow(x) where interactor: ShowChildNewTipsInteractor
        let vm_uri  = uri("/SomeViewModel.kt");
        let int_uri = uri("/ShowChildNewTipsInteractor.kt");
        let idx = Indexer::new();
        idx.index_content(&int_uri,
            "package com.feature\nclass ShowChildNewTipsInteractor {\n  fun loadDataFlow(account: Any) {}\n}");
        idx.index_content(&vm_uri,
            "package com.feature\nclass SomeViewModel(\n  private val interactor: ShowChildNewTipsInteractor\n) {\n  fun init() { interactor.loadDataFlow(x) }\n}");

        let locs = resolve_symbol(&idx, "loadDataFlow", Some("interactor"), &vm_uri);
        assert!(!locs.is_empty(), "loadDataFlow not found via constructor param type inference");
        assert_eq!(locs[0].uri, int_uri);
    }

    #[test]
    fn resolve_method_via_interface_hierarchy() {
        // repo.contactAddressSetup() where repo: IGoldConversionRepository
        // contactAddressSetup is defined in IBaseRepository (superinterface)
        let vm_uri   = uri("/ViewModel.kt");
        let repo_uri = uri("/IGoldConversionRepository.kt");
        let base_uri = uri("/IBaseRepository.kt");
        let idx = Indexer::new();
        idx.index_content(&base_uri,
            "package com.pkg\ninterface IBaseRepository {\n  fun contactAddressSetup(): String\n}");
        idx.index_content(&repo_uri,
            "package com.pkg\ninterface IGoldConversionRepository : IBaseRepository {\n  fun goldPrice(): Double\n}");
        idx.index_content(&vm_uri,
            "package com.pkg\nclass ViewModel(\n  private val repo: IGoldConversionRepository\n) {\n  fun init() { repo.contactAddressSetup() }\n}");

        let locs = resolve_symbol(&idx, "contactAddressSetup", Some("repo"), &vm_uri);
        assert!(!locs.is_empty(), "contactAddressSetup not found via interface hierarchy");
        assert_eq!(locs[0].uri, base_uri, "should resolve to IBaseRepository");
    }

    // ── build_rg_pattern ─────────────────────────────────────────────────────
    // Use rg itself to validate patterns (it's always available in the dev env).

    fn rg_available() -> bool {
        std::process::Command::new("rg")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn rg_matches(pattern: &str, text: &str) -> bool {
        std::process::Command::new("rg")
            .args(["--quiet", "-e", pattern, "--"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .ok()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin.as_mut()?.write_all(text.as_bytes()).ok()?;
                Some(c.wait().ok()?.success())
            })
            .unwrap_or(false)
    }

    #[test]
    fn rg_pattern_matches_kotlin_class() {
        if !rg_available() { eprintln!("skipping: rg not available"); return; }
        let pat = build_rg_pattern("Foo");
        assert!(rg_matches(&pat, "class Foo {"));
        assert!(rg_matches(&pat, "sealed class Foo"));
    }

    #[test]
    fn rg_pattern_matches_kotlin_enum() {
        if !rg_available() { eprintln!("skipping: rg not available"); return; }
        let pat = build_rg_pattern("EScreen");
        assert!(rg_matches(&pat, "enum class EScreen {"));
    }

    #[test]
    fn rg_pattern_matches_java_enum() {
        if !rg_available() { eprintln!("skipping: rg not available"); return; }
        let pat = build_rg_pattern("EProductScreen");
        assert!(rg_matches(&pat, "public enum EProductScreen {"));
        assert!(rg_matches(&pat, "  enum EProductScreen {"));
        assert!(rg_matches(&pat, "private static enum EProductScreen {"));
    }

    #[test]
    fn rg_pattern_no_false_positive_on_usage() {
        if !rg_available() { eprintln!("skipping: rg not available"); return; }
        let pat = build_rg_pattern("EProductScreen");
        // Should NOT match a plain usage (not a declaration)
        assert!(!rg_matches(&pat, "EProductScreen.SOMETHING"));
        assert!(!rg_matches(&pat, "val x: EProductScreen = "));
    }

    #[test]
    fn rg_pattern_matches_java_class() {
        if !rg_available() { eprintln!("skipping: rg not available"); return; }
        let pat = build_rg_pattern("FlexiEntryVM");
        assert!(rg_matches(&pat, "public class FlexiEntryVM extends Base {"));
    }

    // ── import_file_stems ────────────────────────────────────────────────────

    #[test]
    fn file_stems_top_level() {
        assert_eq!(import_file_stems("cz.moneta.data.EProductScreen"), vec!["EProductScreen"]);
    }

    #[test]
    fn file_stems_nested() {
        let s = import_file_stems("com.example.OuterClass.InnerClass");
        assert_eq!(s, vec!["OuterClass", "InnerClass"]);
    }

    // ── supers CST extraction (via parse_kotlin / parse_java) ────────────────

    fn kotlin_supers(src: &str) -> Vec<String> {
        crate::parser::parse_kotlin(src).supers.into_iter().map(|(_, n)| n).collect()
    }

    #[test]
    fn supers_kotlin_single_line() {
        let s = kotlin_supers("class DetailViewModel : MviViewModel<Event, State, Effect>() {}");
        assert!(s.contains(&"MviViewModel".to_string()), "got {s:?}");
    }

    #[test]
    fn supers_kotlin_multi_line_ctor() {
        let src = "class DetailViewModel @Inject constructor(\n  private val useCase: UseCase,\n) : MviViewModel<Event, State, Effect>() {}";
        let s = kotlin_supers(src);
        assert!(s.contains(&"MviViewModel".to_string()), "got {s:?}");
    }

    #[test]
    fn supers_kotlin_multiple() {
        let src = "class Foo : BaseClass(), SomeInterface, AnotherInterface {}";
        let s = kotlin_supers(src);
        assert!(s.contains(&"BaseClass".to_string()),       "got {s:?}");
        assert!(s.contains(&"SomeInterface".to_string()),   "got {s:?}");
        assert!(s.contains(&"AnotherInterface".to_string()), "got {s:?}");
    }

    #[test]
    fn supers_java_extends() {
        let src = "public class FlexiEntryVM extends BaseFlexikreditVM {}";
        let s: Vec<String> = crate::parser::parse_java(src).supers.into_iter().map(|(_, n)| n).collect();
        assert!(s.contains(&"BaseFlexikreditVM".to_string()), "got {s:?}");
    }

    #[test]
    fn supers_java_implements() {
        let src = "public class Foo extends Base implements Runnable, Serializable {}";
        let s: Vec<String> = crate::parser::parse_java(src).supers.into_iter().map(|(_, n)| n).collect();
        assert!(s.contains(&"Base".to_string()),         "got {s:?}");
        assert!(s.contains(&"Runnable".to_string()),     "got {s:?}");
        assert!(s.contains(&"Serializable".to_string()), "got {s:?}");
    }

    #[test]
    fn supers_does_not_pick_up_type_annotations() {
        let src = "class Foo {\n  val x: Int = 0\n  fun f(): String = \"\"\n}";
        let s = kotlin_supers(src);
        assert!(s.is_empty(), "should have no supers, got {s:?}");
    }

    // ── resolve_from_class_hierarchy ─────────────────────────────────────────

    #[test]
    fn resolve_inherited_method() {
        let base_uri = uri("/Base.kt");
        let child_uri = uri("/Child.kt");
        let idx = Indexer::new();
        idx.index_content(&base_uri,  "package com.example\nopen class Base {\n  fun baseMethod() {}\n}");
        idx.index_content(&child_uri, "package com.example\nclass Child : Base() {}\n");

        // `baseMethod` is not declared in Child — must be found via hierarchy
        let locs = resolve_symbol(&idx, "baseMethod", None, &child_uri);
        assert!(!locs.is_empty(), "inherited method not found");
        assert_eq!(locs[0].uri, base_uri);
    }

    #[test]
    fn resolve_inherited_method_via_import() {
        let base_uri  = uri("/lib/Base.kt");
        let child_uri = uri("/app/Child.kt");
        let idx = Indexer::new();
        idx.index_content(&base_uri,  "package com.lib\nopen class Base {\n  fun doStuff() {}\n}");
        idx.index_content(&child_uri,
            "package com.app\nimport com.lib.Base\nclass Child : Base() {}\n");

        let locs = resolve_symbol(&idx, "doStuff", None, &child_uri);
        assert!(!locs.is_empty(), "inherited method not found via import");
        assert_eq!(locs[0].uri, base_uri);
    }

    // ── this / super resolution ───────────────────────────────────────────────

    #[test]
    fn resolve_this_dot_method() {
        let u = uri("/Foo.kt");
        let idx = Indexer::new();
        idx.index_content(&u, "package com.example\nclass Foo {\n  fun doThing() {}\n  fun other() { this.doThing() }\n}");
        let locs = resolve_symbol(&idx, "doThing", Some("this"), &u);
        assert!(!locs.is_empty(), "this.doThing() not resolved");
        assert_eq!(locs[0].uri, u);
    }

    #[test]
    fn resolve_super_dot_method() {
        let base_uri  = uri("/Base.kt");
        let child_uri = uri("/Child.kt");
        let idx = Indexer::new();
        idx.index_content(&base_uri,  "package com.example\nopen class Base { fun init() {} }");
        idx.index_content(&child_uri, "package com.example\nclass Child : Base() { fun x() { super.init() } }");
        let locs = resolve_symbol(&idx, "init", Some("super"), &child_uri);
        assert!(!locs.is_empty(), "super.init() not resolved");
        assert_eq!(locs[0].uri, base_uri);
    }

    // ── lambda parameter recognition ─────────────────────────────────────────

    #[test]
    fn local_decl_lambda_untyped() {
        let lines: Vec<String> = vec![
            "list.forEach { account ->".to_string(),
            "  println(account)".to_string(),
        ];
        let range = find_declaration_range_in_lines(&lines, "account");
        assert!(range.is_some(), "untyped lambda param not found");
        assert_eq!(range.unwrap().start.line, 0);
    }

    #[test]
    fn local_decl_lambda_typed() {
        let lines: Vec<String> = vec![
            "items.map { item: DetailItem ->".to_string(),
        ];
        let range = find_declaration_range_in_lines(&lines, "item");
        assert!(range.is_some(), "typed lambda param not found");
    }

    #[test]
    fn local_decl_no_false_positive_usage() {
        // A usage of `account` on a non-declaration line must not be returned
        let lines: Vec<String> = vec![
            "val result = account.name".to_string(),
        ];
        let range = find_declaration_range_in_lines(&lines, "account");
        assert!(range.is_none(), "false positive on usage line");
    }

    // ── primary constructor val/var parameter resolution ─────────────────────

    #[test]
    fn resolve_data_class_field_via_dot_access() {
        // user.name should resolve to `val name: String` in User's primary ctor
        let user_uri = uri("/User.kt");
        let caller_uri = uri("/Caller.kt");
        let idx = Indexer::new();
        idx.index_content(&user_uri,
            "package com.example\ndata class User(val name: String, val age: Int)");
        idx.index_content(&caller_uri,
            "package com.example\nfun greet(user: User) { println(user.name) }");

        let locs = resolve_symbol(&idx, "name", Some("user"), &caller_uri);
        assert!(!locs.is_empty(), "name not found via user.name");
        assert_eq!(locs[0].uri, user_uri, "should point to User.kt");
    }

    #[test]
    fn resolve_ctor_param_no_qualifier() {
        // Inside the class itself, `name` should resolve to the ctor param.
        let uri = uri("/User.kt");
        let idx = Indexer::new();
        idx.index_content(&uri,
            "package com.example\ndata class User(val name: String) {\n  fun display() = name\n}");

        let locs = resolve_symbol(&idx, "name", None, &uri);
        assert!(!locs.is_empty(), "ctor param not found locally");
        assert_eq!(locs[0].uri, uri, "should stay in same file");
    }

    #[test]
    fn resolve_named_arg_to_ctor_param() {
        // User(name = "Alice") — qualifier is "User" (detected by word_and_qualifier_at).
        // resolve_symbol with qualifier="User" must find `val name` in User's primary ctor.
        let user_uri   = uri("/User.kt");
        let caller_uri = uri("/Caller.kt");
        let idx = Indexer::new();
        idx.index_content(&user_uri,
            "package com.example\ndata class User(val name: String, val age: Int)");
        idx.index_content(&caller_uri,
            "package com.example\nfun test() { val u = User(name = \"Alice\", age = 30) }");

        // Simulate what the backend does after word_and_qualifier_at returns ("name", "User")
        let locs = resolve_symbol(&idx, "name", Some("User"), &caller_uri);
        assert!(!locs.is_empty(), "named arg 'name' not resolved to User ctor param");
        assert_eq!(locs[0].uri, user_uri, "should point to User.kt, not caller");
    }

    #[test]
    fn named_arg_same_name_different_classes_same_file() {
        // Regression: Contract.kt has both State(val toastModel: ...) and
        // OnClick(val toastModel: ...) in the same file.
        // Resolving State(toastModel = ...) should land on State's field,
        // not OnClick's (which appears later but might be returned first).
        let contract_uri = uri("/Contract.kt");
        let caller_uri   = uri("/Caller.kt");
        let idx = Indexer::new();
        idx.index_content(&contract_uri, "\
package com.example
sealed class Effect {
    data class OnClick(val toastModel: String) : Effect()
}
data class State(
    val toastModel: String? = null,
)");
        idx.index_content(&caller_uri,
            "package com.example\nfun test() { State(toastModel = \"hi\") }");

        let locs = resolve_symbol(&idx, "toastModel", Some("State"), &caller_uri);
        assert!(!locs.is_empty(), "toastModel not resolved");
        // Must point to State's toastModel (line 4), NOT OnClick's (line 2)
        let line = locs[0].range.start.line;
        assert!(line >= 4, "resolved to OnClick.toastModel (line {line}) instead of State.toastModel");
    }

    // ── it-completion helpers ─────────────────────────────────────────────────

    #[test]
    fn extract_collection_element_list() {
        assert_eq!(extract_collection_element_type("List<Product>"), Some("Product".into()));
    }

    #[test]
    fn extract_collection_element_mutable_list() {
        assert_eq!(extract_collection_element_type("MutableList<User>"), Some("User".into()));
    }

    #[test]
    fn extract_collection_element_flow() {
        assert_eq!(extract_collection_element_type("Flow<Event>"), Some("Event".into()));
    }

    #[test]
    fn extract_collection_element_state_flow() {
        assert_eq!(extract_collection_element_type("StateFlow<UiState>"), Some("UiState".into()));
    }

    #[test]
    fn extract_collection_element_map_returns_first() {
        // Map is not in the collection list → returns None (it's more complex).
        // forEach on Map gives Map.Entry, not the first type arg.
        assert_eq!(extract_collection_element_type("Map<String, Int>"), None);
    }

    #[test]
    fn extract_collection_element_non_collection() {
        // Plain class → not a collection, returns None.
        assert_eq!(extract_collection_element_type("User"), None);
    }

    #[test]
    fn infer_type_in_lines_raw_keeps_generics() {
        let lines: Vec<String> = vec![
            "val items: List<Product> = emptyList()".into(),
        ];
        assert_eq!(infer_type_in_lines_raw(&lines, "items"), Some("List<Product>".into()));
    }

    #[test]
    fn infer_type_in_lines_raw_state_flow() {
        let lines: Vec<String> = vec![
            "    private val _state: StateFlow<UiState>".into(),
        ];
        assert_eq!(infer_type_in_lines_raw(&lines, "_state"), Some("StateFlow<UiState>".into()));
    }

    #[test]
    fn goto_def_on_named_lambda_param_resolves_to_declaration_line() {
        // items.forEach { product ->
        //     product.name   ← gd on `product` here
        // go-to-def should jump to the `{ product ->` declaration line (line 2)
        let caller_uri  = uri("/Caller.kt");
        let product_uri = uri("/Product.kt");
        let idx = Indexer::new();
        idx.index_content(&product_uri, "package com.example\ndata class Product(val name: String)");
        idx.index_content(&caller_uri,
            "package com.example\nval items: List<Product> = emptyList()\nitems.forEach { product ->\n    product.name\n}");

        // step 1.5 finds `{ product ->` via the lambda arrow pattern
        let locs = resolve_symbol(&idx, "product", None, &caller_uri);
        assert!(!locs.is_empty(), "lambda param 'product' not found");
        // Must land in the same file (the lambda declaration), NOT in rg results
        assert_eq!(locs[0].uri, caller_uri, "should stay in Caller.kt at the lambda decl");
        // Line 2 is where `items.forEach { product ->` is declared
        assert_eq!(locs[0].range.start.line, 2, "should point to the lambda arrow line");
    }

    // ── complete_dot scoping — no local fns leak ─────────────────────────────

    #[test]
    fn dot_complete_does_not_leak_top_level_fns() {
        let mut idx = Indexer::new();
        let uri = Url::parse("file:///a/Keys.kt").unwrap();
        idx.index_content(&uri, "package a\n\nobject ProductKey {\n    val CARD = \"card\"\n    val LOAN = \"loan\"\n    fun fromString(s: String) = s\n}\n\nfun topLevelHelper() {}\n");

        // Simulate a variable typed as ProductKey in another file.
        let caller_uri = Url::parse("file:///a/Caller.kt").unwrap();
        idx.index_content(&caller_uri, "package a\nval key: ProductKey = TODO()");

        let items = complete_dot(&idx, "ProductKey", &caller_uri, false);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();

        assert!(labels.contains(&"fromString"),    "member fun should appear");
        assert!(labels.contains(&"CARD"),          "member val should appear");
        assert!(!labels.contains(&"topLevelHelper"), "top-level fn must NOT leak into dot completions");
    }

    // ── complete_bare distance sorting ───────────────────────────────────────

    #[test]
    fn complete_bare_local_before_same_pkg() {
        let mut idx = Indexer::new();
        let local_uri = Url::parse("file:///pkg/a/Local.kt").unwrap();
        let other_uri = Url::parse("file:///pkg/a/Other.kt").unwrap();
        // local file has "localFoo"
        idx.index_content(&local_uri, "package a\nfun localFoo() {}");
        // same-package file has "pkgBar"
        idx.index_content(&other_uri, "package a\nfun pkgBar() {}");

        let (items, _) = complete_bare(&idx, "", &local_uri, false, false);

        let local_pos = items.iter().position(|i| i.label == "localFoo");
        let pkg_pos   = items.iter().position(|i| i.label == "pkgBar");
        assert!(local_pos.is_some(), "localFoo should appear");
        assert!(pkg_pos.is_some(),   "pkgBar should appear");

        // sort_text with tier prefix means local (0:…) sorts before same-pkg (1:…).
        let local_sort = items[local_pos.unwrap()].sort_text.as_deref().unwrap_or("");
        let pkg_sort   = items[pkg_pos.unwrap()].sort_text.as_deref().unwrap_or("");
        assert!(local_sort < pkg_sort, "local tier sort_text should be less than same-pkg tier");
    }

    // ── dot_completions_for type filtering ────────────────────────────────────

    #[test]
    fn dot_completions_string_receiver_has_string_fns() {
        let items = crate::stdlib::dot_completions_for("String", false);
        let names: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(names.contains(&"trim"),    "String should have trim()");
        assert!(names.contains(&"split"),   "String should have split()");
        assert!(names.contains(&"let"),     "String should have scope fn let()");
        // Collection fns should NOT appear on String
        assert!(!names.contains(&"map"),    "String should NOT have map()");
        assert!(!names.contains(&"filter"), "String should NOT have filter()");
    }

    #[test]
    fn dot_completions_list_receiver_has_collection_fns() {
        let items = crate::stdlib::dot_completions_for("List", false);
        let names: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(names.contains(&"map"),      "List should have map()");
        assert!(names.contains(&"filter"),   "List should have filter()");
        assert!(names.contains(&"forEach"),  "List should have forEach()");
        assert!(names.contains(&"let"),      "List should have scope fn let()");
        // String-only fns should NOT appear on List
        assert!(!names.contains(&"trim"),    "List should NOT have trim()");
        assert!(!names.contains(&"split"),   "List should NOT have split()");
    }

    #[test]
    fn dot_completions_custom_type_has_scope_fns_only() {
        let items = crate::stdlib::dot_completions_for("MyDomainClass", false);
        let names: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(names.contains(&"let"),     "domain type should have let()");
        assert!(names.contains(&"apply"),   "domain type should have apply()");
        assert!(!names.contains(&"trim"),   "domain type should NOT have trim()");
        assert!(!names.contains(&"map"),    "domain type should NOT have map()");
        assert!(!names.contains(&"filter"), "domain type should NOT have filter()");
    }

    // ── supers CST extraction – annotation handling ──────────────────────────

    #[test]
    fn extract_supers_annotation_same_line() {
        let s = kotlin_supers("@Suppress(\"unused\") class Foo : Bar {}");
        assert!(s.contains(&"Bar".to_string()), "got {s:?}");
    }

    #[test]
    fn extract_supers_annotation_separate_line() {
        let src = "@Module\nclass Foo : Bar, Baz {}";
        let s = kotlin_supers(src);
        assert!(s.contains(&"Bar".to_string()), "got {s:?}");
        assert!(s.contains(&"Baz".to_string()), "got {s:?}");
    }

    #[test]
    fn extract_supers_field_inject_annotation() {
        let s = kotlin_supers("@field:Inject\nclass Foo {}");
        assert!(s.is_empty(), "annotation-only line should produce no supers, got {s:?}");
    }

    #[test]
    fn extract_supers_multiple_annotations() {
        let src = "@Module\n@Provides\nclass FooModule : BaseModule() {}";
        let s = kotlin_supers(src);
        assert!(s.contains(&"BaseModule".to_string()), "got {s:?}");
    }

    // ── auto-import helpers ───────────────────────────────────────────────────

    fn make_import_entry(full_path: &str, local_name: &str, is_star: bool) -> crate::types::ImportEntry {
        crate::types::ImportEntry {
            full_path: full_path.to_string(),
            local_name: local_name.to_string(),
            is_star,
        }
    }

    #[test]
    fn already_imported_exact() {
        let imports = vec![make_import_entry("com.example.Foo", "Foo", false)];
        assert!(already_imported("com.example.Foo", &imports));
    }

    #[test]
    fn already_imported_alias_not_counted() {
        // `import com.example.Foo as Bar` — Foo is not usable as Foo
        let imports = vec![make_import_entry("com.example.Foo", "Bar", false)];
        assert!(!already_imported("com.example.Foo", &imports));
    }

    #[test]
    fn already_imported_star() {
        let imports = vec![make_import_entry("com.example", "*", true)];
        assert!(already_imported("com.example.Foo", &imports));
    }

    #[test]
    fn already_imported_star_wrong_pkg() {
        let imports = vec![make_import_entry("com.other", "*", true)];
        assert!(!already_imported("com.example.Foo", &imports));
    }

    #[test]
    fn import_insertion_after_last_import() {
        let lines = vec![
            "package com.example".to_string(),
            "".to_string(),
            "import com.example.Bar".to_string(),
            "import com.example.Baz".to_string(),
            "".to_string(),
            "class Foo {}".to_string(),
        ];
        assert_eq!(import_insertion_line(&lines), 4); // line after last import
    }

    #[test]
    fn import_insertion_after_package_no_imports() {
        let lines = vec![
            "package com.example".to_string(),
            "".to_string(),
            "class Foo {}".to_string(),
        ];
        assert_eq!(import_insertion_line(&lines), 1); // line after package
    }

    #[test]
    fn import_insertion_at_top_no_package_no_imports() {
        let lines = vec!["class Foo {}".to_string()];
        assert_eq!(import_insertion_line(&lines), 0);
    }

    #[test]
    fn auto_import_completion_adds_edit() {
        let idx = Indexer::new();
        // Library file in a different package.
        let lib_uri = uri("/lib/Composable.kt");
        idx.index_content(&lib_uri, "package androidx.compose.runtime\nannotation class Composable");
        // Current file — different package, no imports.
        let cur_uri = uri("/app/Screen.kt");
        idx.index_content(&cur_uri, "package com.example.app\n\nfun Screen() {\n    Comp\n}");

        let (items, _) = complete_symbol(&idx, "Comp", None, &cur_uri, false);
        let import_item = items.iter().find(|i| i.label == "Composable");
        assert!(import_item.is_some(), "Composable should appear in completions");
        let edits = import_item.unwrap().additional_text_edits.as_ref();
        assert!(edits.is_some(), "additionalTextEdits should be present");
        let edit_text = &edits.unwrap()[0].new_text;
        assert!(edit_text.contains("import androidx.compose.runtime.Composable"), 
            "edit should add correct import, got: {edit_text}");
    }

    #[test]
    fn auto_import_skipped_when_already_imported() {
        let idx = Indexer::new();
        let lib_uri = uri("/lib/Foo.kt");
        idx.index_content(&lib_uri, "package com.lib\nclass Foo");
        let cur_uri = uri("/app/Bar.kt");
        // Already imports com.lib.Foo.
        idx.index_content(&cur_uri, "package com.app\nimport com.lib.Foo\nclass Bar { val f: Foo = Foo() }");

        let (items, _) = complete_symbol(&idx, "Foo", None, &cur_uri, false);
        let foo_items: Vec<_> = items.iter().filter(|i| i.label == "Foo").collect();
        // May appear (from tier-0/1 or tier-2 without edit) but must not have an import edit.
        for item in &foo_items {
            assert!(item.additional_text_edits.is_none() 
                || item.additional_text_edits.as_ref().unwrap().is_empty(),
                "already-imported symbol must not carry an import edit");
        }
    }

    #[test]
    fn auto_import_skipped_same_package() {
        let idx = Indexer::new();
        let lib_uri = uri("/app/Foo.kt");
        idx.index_content(&lib_uri, "package com.example\nclass Foo");
        let cur_uri = uri("/app/Bar.kt");
        idx.index_content(&cur_uri, "package com.example\nclass Bar");

        let (items, _) = complete_symbol(&idx, "Foo", None, &cur_uri, false);
        // Foo is in the same package — any completion item for it must have no import edit.
        for item in items.iter().filter(|i| i.label == "Foo") {
            assert!(item.additional_text_edits.is_none()
                || item.additional_text_edits.as_ref().unwrap().is_empty(),
                "same-package symbol must not carry an import edit");
        }
    }

    #[test]
    fn auto_import_two_packages_two_items() {
        let idx = Indexer::new();
        idx.index_content(&uri("/m3/Button.kt"), "package androidx.compose.material3\nclass Button");
        idx.index_content(&uri("/m1/Button.kt"), "package androidx.compose.material\nclass Button");
        let cur_uri = uri("/app/Screen.kt");
        idx.index_content(&cur_uri, "package com.example\nfun screen() {}");

        let (items, _) = complete_symbol(&idx, "Button", None, &cur_uri, false);
        let button_items: Vec<_> = items.iter().filter(|i| i.label == "Button").collect();
        assert_eq!(button_items.len(), 2, "Two Button symbols from different packages should yield two items");
        let details: Vec<_> = button_items.iter().filter_map(|i| i.detail.as_deref()).collect();
        assert!(details.iter().any(|d| d.contains("material3")), "One item should mention material3");
        assert!(details.iter().any(|d| d.contains("material") && !d.contains("material3")), "One item should mention material");
    }

    #[test]
    fn caps_mode_hides_lowercase_functions() {
        let idx = Indexer::new();
        let cur_uri = uri("/app/Screen.kt");
        // File with both a class and a lowercase function.
        idx.index_content(&cur_uri, "package com.example\nclass Column\nfun collectAsState() {}");

        let (items, _) = complete_symbol(&idx, "Col", None, &cur_uri, false);
        // Column (uppercase) should appear.
        assert!(items.iter().any(|i| i.label == "Column"), "Column should appear in caps mode");
        // collectAsState (lowercase) should NOT appear when typing uppercase prefix.
        assert!(!items.iter().any(|i| i.label == "collectAsState"),
            "lowercase function must not appear when typing uppercase prefix");
    }

    #[test]
    fn lowercase_mode_hides_classes() {
        let idx = Indexer::new();
        let cur_uri = uri("/app/Screen.kt");
        idx.index_content(&cur_uri, "package com.example\nclass Column\nfun collectAsState() {}");

        let (items, _) = complete_symbol(&idx, "col", None, &cur_uri, false);
        // collectAsState (lowercase) should appear.
        assert!(items.iter().any(|i| i.label == "collectAsState"), "lowercase function should appear in lowercase mode");
        // Column (uppercase) should NOT appear when typing lowercase prefix.
        assert!(!items.iter().any(|i| i.label == "Column"),
            "CamelCase class must not appear when typing lowercase prefix");
    }

    #[test]
    fn tier2_suppressed_when_name_visible_in_current_file() {
        let idx = Indexer::new();
        idx.index_content(&uri("/lib/Foo.kt"), "package com.lib\nclass Foo");
        let cur_uri = uri("/app/Bar.kt");
        idx.index_content(&cur_uri, "package com.example\nclass Foo");

        let (items, _) = complete_symbol(&idx, "Foo", None, &cur_uri, false);
        let foo_items: Vec<_> = items.iter().filter(|i| i.label == "Foo").collect();
        assert_eq!(foo_items.len(), 1, "Foo defined in current file must not generate a duplicate tier-2 item");
        assert!(foo_items[0].additional_text_edits.is_none()
            || foo_items[0].additional_text_edits.as_ref().unwrap().is_empty(),
            "tier-0 item must not carry an import edit");
    }

    // ── match_score ────────────────────────────────────────────────────────────

    #[test]
    fn match_score_prefix_is_best() {
        assert_eq!(match_score("Column", "Col"), Some(0));
        assert_eq!(match_score("column", "col"), Some(0));
    }

    #[test]
    fn match_score_acronym_is_second() {
        // CB → ColumnButton (C=Column, B=Button)
        assert_eq!(match_score("ColumnButton", "CB"), Some(1));
        // mSF → myStateFlow
        assert_eq!(match_score("myStateFlow", "mSF"), Some(1));
        // underscore-prefixed private fields: _ColumnButton, _myStateFlow
        assert_eq!(match_score("_ColumnButton", "CB"), Some(1));
        assert_eq!(match_score("_myStateFlow", "mSF"), Some(1));
    }

    #[test]
    fn match_score_substring_is_third() {
        assert_eq!(match_score("RecyclerView", "View"), Some(2));
    }

    #[test]
    fn match_score_no_match_returns_none() {
        assert_eq!(match_score("Column", "xyz"), None);
    }

    #[test]
    fn match_score_prefix_beats_acronym_in_sort() {
        let idx = Indexer::new();
        let cur_uri = uri("/app/Screen.kt");
        // Column → prefix match for "Col"; ColumnButton → acronym for "CB" but prefix for "Col"
        idx.index_content(&cur_uri, "package com.example\nclass Column\nclass ColumnButton");

        let (items, _) = complete_symbol(&idx, "Col", None, &cur_uri, false);
        let col_pos    = items.iter().position(|i| i.label == "Column").unwrap();
        let colbtn_pos = items.iter().position(|i| i.label == "ColumnButton").unwrap();
        // Both are prefix matches; Column (shorter) should sort before ColumnButton lexicographically.
        assert!(col_pos < colbtn_pos || {
            // Accept either order — both are score-0, lexicographic tie-break.
            let a = items[col_pos].sort_text.as_deref().unwrap_or("");
            let b = items[colbtn_pos].sort_text.as_deref().unwrap_or("");
            a <= b
        }, "Column should sort ≤ ColumnButton for prefix 'Col'");
    }

    #[test]
    fn tier2_requires_prefix_length_2() {
        let idx = Indexer::new();
        idx.index_content(&uri("/lib/Foo.kt"), "package com.lib\nclass Column");
        let cur_uri = uri("/app/Bar.kt");
        idx.index_content(&cur_uri, "package com.example\n");

        // Single char 'C' — tier-2 should NOT fire, so Column (cross-pkg) not returned.
        let (items, _) = complete_symbol(&idx, "C", None, &cur_uri, false);
        assert!(!items.iter().any(|i| i.label == "Column" && i.additional_text_edits.is_some()),
            "tier-2 must not fire for single-char prefix");

        // Two chars 'Co' — tier-2 SHOULD fire.
        let (items, _) = complete_symbol(&idx, "Co", None, &cur_uri, false);
        assert!(items.iter().any(|i| i.label == "Column"),
            "tier-2 must fire for prefix length >= 2");
    }

    #[test]
    fn result_cap_sets_hit_cap() {
        let idx = Indexer::new();
        let cur_uri = uri("/app/Screen.kt");
        // Generate 200 unique class names → exceeds COMPLETION_CAP (150).
        let src = (0..200)
            .map(|i| format!("class Cls{i:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        idx.index_content(&cur_uri, &format!("package com.example\n{src}"));

        let (items, hit_cap) = complete_symbol(&idx, "Cls", None, &cur_uri, false);
        assert!(hit_cap, "hit_cap should be true when result count exceeds COMPLETION_CAP");
        assert_eq!(items.len(), crate::resolver::COMPLETION_CAP, "items must be truncated to cap");
    }

    #[test]
    fn annotation_context_hides_functions() {
        let idx = Indexer::new();
        let cur_uri = uri("/app/Screen.kt");
        idx.index_content(&cur_uri,
            "package com.example\nannotation class Composable\nfun composable() {}");

        let line = "@Composable";
        let prefix = "Composable";
        let annotation_only = is_annotation_context(line, prefix);
        assert!(annotation_only, "should detect annotation context");

        let (items, _) = complete_symbol_with_context(&idx, prefix, None, &cur_uri, false, true);
        // Annotation class should appear.
        assert!(items.iter().any(|i| i.label == "Composable"),
            "annotation class Composable must appear");
        // Lowercase function should not appear.
        assert!(!items.iter().any(|i| i.label == "composable"),
            "function composable must not appear in annotation context");
    }

    #[test]
    fn camel_mode_hides_screaming_snake() {
        let idx = Indexer::new();
        let cur_uri = uri("/app/Screen.kt");
        idx.index_content(&cur_uri,
            "package com.example\nclass ChildDashboardViewModel\nconst val CHILD_DASHBOARD_MAX = 10\nval CHILD_COUNT = 5");

        // Typing CamelCase prefix — SCREAMING_SNAKE constants must not appear.
        let (items, _) = complete_symbol(&idx, "Child", None, &cur_uri, false);
        assert!(items.iter().any(|i| i.label == "ChildDashboardViewModel"),
            "CamelCase class must appear");
        assert!(!items.iter().any(|i| i.label == "CHILD_DASHBOARD_MAX"),
            "SCREAMING_SNAKE constant must be hidden in camel_mode");
        assert!(!items.iter().any(|i| i.label == "CHILD_COUNT"),
            "SCREAMING_SNAKE val must be hidden in camel_mode");

        // Typing all-uppercase prefix — SCREAMING_SNAKE constants may appear.
        let (items2, _) = complete_symbol(&idx, "CHILD", None, &cur_uri, false);
        assert!(items2.iter().any(|i| i.label == "CHILD_DASHBOARD_MAX"),
            "SCREAMING_SNAKE constant must appear when prefix is uppercase");
    }

    #[test]
    fn long_prefix_tier2_not_crowded_out() {
        // Even when the same-package has many substring-matching symbols,
        // a cross-package prefix match must survive with a 4+ char prefix.
        let idx = Indexer::new();
        let cur_uri  = uri("/app/pkg/Screen.kt");
        let other_uri = uri("/app/pkg/Other.kt");
        let cross_uri = uri("/app/other/Cross.kt");

        // 60 same-pkg classes that contain "child" as substring but don't start with it.
        let same_pkg: String = (0..60)
            .map(|i| format!("class Something{i}Child"))
            .collect::<Vec<_>>()
            .join("\n");
        idx.index_content(&cur_uri,  "package com.example\n");
        idx.index_content(&other_uri, &format!("package com.example\n{same_pkg}"));
        // Cross-package class with prefix match.
        idx.index_content(&cross_uri, "package com.other\nclass ChildDashboardViewModel");

        // Short prefix (2 chars): substring allowed, cross-pkg fires.
        let (short, _) = complete_symbol(&idx, "Ch", None, &cur_uri, false);
        assert!(short.iter().any(|i| i.label == "ChildDashboardViewModel"),
            "cross-pkg must appear for short prefix");

        // Long prefix (5 chars): substring suppressed for tier-0/1 — cross-pkg prefix match wins.
        let (long, _) = complete_symbol(&idx, "Child", None, &cur_uri, false);
        assert!(long.iter().any(|i| i.label == "ChildDashboardViewModel"),
            "cross-pkg prefix match must survive long prefix even with many same-pkg substring hits");
        // Same-pkg substring hits (Something*Child) must be absent for long prefix.
        assert!(!long.iter().any(|i| i.label.ends_with("Child") && i.label.starts_with("Something")),
            "same-pkg substring matches must be filtered for long prefix");
    }

    #[test]
    fn is_screaming_snake_cases() {
        assert!(is_screaming_snake("MAX_SIZE"));
        assert!(is_screaming_snake("CHILD_DASHBOARD_MAX"));
        assert!(is_screaming_snake("A"));
        assert!(!is_screaming_snake("ChildDashboard"));
        assert!(!is_screaming_snake("maxSize"));
        assert!(!is_screaming_snake("_"));      // no letters
        assert!(!is_screaming_snake("123"));    // no letters
    }

    #[test]
    fn is_annotation_context_detection() {
        assert!(is_annotation_context("@Composable", "Composable"));
        assert!(is_annotation_context("  @Comp", "Comp"));
        assert!(!is_annotation_context("Composable", "Composable")); // no @
        // "x@Comp" — technically matches, real code won't have identifiers directly before @
    }
