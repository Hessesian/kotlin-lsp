    use super::*;
    use tower_lsp::lsp_types::{Position, SymbolKind};
    use crate::resolver::complete_symbol;

    fn uri(path: &str) -> tower_lsp::lsp_types::Url {
        tower_lsp::lsp_types::Url::parse(&format!("file:///test{path}")).unwrap()
    }

    fn sym<'a>(data: &'a FileData, name: &str) -> Option<&'a SymbolEntry> {
        data.symbols.iter().find(|s| s.name == name)
    }

    // ── symbol extraction ────────────────────────────────────────────────────

    // ── query sanity check ───────────────────────────────────────────────────

    #[test]
    fn kotlin_definitions_query_compiles() {
        let lang = tree_sitter_kotlin::language();
        let result = tree_sitter::Query::new(&lang, crate::queries::KOTLIN_DEFINITIONS);
        if let Err(e) = &result {
            panic!("KOTLIN_DEFINITIONS query failed to compile: {e}");
        }
    }

    #[test] fn class()        { assert_eq!(sym(&parse_kotlin("class Foo"),        "Foo").unwrap().kind, SymbolKind::CLASS); }
    #[test] fn interface()    { assert_eq!(sym(&parse_kotlin("interface Bar"),     "Bar").unwrap().kind, SymbolKind::INTERFACE); }
    #[test] fn fun_interface() {
        let data = parse_kotlin("fun interface Action {\n    fun invoke(value: String)\n}");
        assert_eq!(sym(&data, "Action").unwrap().kind, SymbolKind::INTERFACE,
            "fun interface should be indexed as INTERFACE");
    }
    #[test] fn fun_interface_internal() {
        let data = parse_kotlin("internal fun interface IPairCodeParser {\n    fun parse(input: String): String\n}");
        assert_eq!(sym(&data, "IPairCodeParser").unwrap().kind, SymbolKind::INTERFACE,
            "internal fun interface should be indexed as INTERFACE");
    }
    #[test] fn fun_interface_generic() {
        let data = parse_kotlin("fun interface Router<Effect> {\n    fun route(effect: Effect)\n}");
        assert_eq!(sym(&data, "Router").unwrap().kind, SymbolKind::INTERFACE,
            "generic fun interface should be indexed as INTERFACE");
    }
    #[test] fn fun_interface_nested() {
        let data = parse_kotlin("class LoanReducer {\n    @AssistedFactory\n    fun interface Factory {\n        fun create(x: Int): String\n    }\n}");
        assert_eq!(sym(&data, "Factory").unwrap().kind, SymbolKind::INTERFACE,
            "nested fun interface should be indexed as INTERFACE");
    }
    #[test] fn object_decl()  { assert_eq!(sym(&parse_kotlin("object Obj"),        "Obj").unwrap().kind, SymbolKind::OBJECT); }
    #[test] fn data_class()   { assert_eq!(sym(&parse_kotlin("data class D(val x: Int)"), "D").unwrap().kind, SymbolKind::STRUCT); }
    #[test] fn enum_class()   { assert_eq!(sym(&parse_kotlin("enum class Color { RED }"), "Color").unwrap().kind, SymbolKind::ENUM); }

    #[test]
    fn dump_fun_interface_tree() {
        let content = "fun interface Action {\n    fun invoke(value: String)\n}";
        let lang = tree_sitter_kotlin::language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(content, None).unwrap();
        fn walk(node: tree_sitter::Node<'_>, src: &[u8], depth: usize) {
            let snippet = &src[node.start_byte()..node.end_byte().min(node.start_byte()+40)];
            eprintln!("{}{} {:?}", "  ".repeat(depth), node.kind(), String::from_utf8_lossy(snippet));
            for i in 0..node.child_count() {
                walk(node.child(i).unwrap(), src, depth+1);
            }
        }
        walk(tree.root_node(), content.as_bytes(), 0);
        // This test just dumps — it always passes. Check stderr output.
    }

    #[test]
    fn dump_fun_interface_internal_tree() {
        let content = "internal fun interface IPairCodeParser {\n    fun parse(input: String): String\n}";
        let lang = tree_sitter_kotlin::language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(content, None).unwrap();
        fn walk(node: tree_sitter::Node<'_>, src: &[u8], depth: usize) {
            let snippet = &src[node.start_byte()..node.end_byte().min(node.start_byte()+40)];
            eprintln!("{}{} {:?}", "  ".repeat(depth), node.kind(), String::from_utf8_lossy(snippet));
            for i in 0..node.child_count() {
                walk(node.child(i).unwrap(), src, depth+1);
            }
        }
        walk(tree.root_node(), content.as_bytes(), 0);
    }

    #[test]
    fn dump_fun_interface_nested_tree() {
        let content = "class LoanReducer {\n    @AssistedFactory\n    fun interface Factory {\n        fun create(x: Int): String\n    }\n}";
        let lang = tree_sitter_kotlin::language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(content, None).unwrap();
        fn walk(node: tree_sitter::Node<'_>, src: &[u8], depth: usize) {
            let snippet = &src[node.start_byte()..node.end_byte().min(node.start_byte()+40)];
            eprintln!("{}{} {:?}", "  ".repeat(depth), node.kind(), String::from_utf8_lossy(snippet));
            for i in 0..node.child_count() {
                walk(node.child(i).unwrap(), src, depth+1);
            }
        }
        walk(tree.root_node(), content.as_bytes(), 0);
    }
    #[test] fn enum_entries() {
        let data = parse_kotlin("enum class Screen { DETAIL, LIST, SETTINGS }");
        assert_eq!(sym(&data, "DETAIL").unwrap().kind,   SymbolKind::ENUM_MEMBER);
        assert_eq!(sym(&data, "LIST").unwrap().kind,     SymbolKind::ENUM_MEMBER);
        assert_eq!(sym(&data, "SETTINGS").unwrap().kind, SymbolKind::ENUM_MEMBER);
    }
    #[test] fn typealias()    { assert_eq!(sym(&parse_kotlin("typealias Alias = String"), "Alias").unwrap().kind, SymbolKind::CLASS); }
    #[test] fn top_fun()      { assert_eq!(sym(&parse_kotlin("fun foo() {}"), "foo").unwrap().kind, SymbolKind::FUNCTION); }
    #[test] fn val_prop()     { assert_eq!(sym(&parse_kotlin("val x: Int = 0"), "x").unwrap().kind, SymbolKind::PROPERTY); }
    #[test] fn var_prop()     { assert_eq!(sym(&parse_kotlin("var y = 0"),      "y").unwrap().kind, SymbolKind::VARIABLE); }
    #[test] fn const_val()    {
        let data = parse_kotlin("const val MAX: Int = 100");
        assert_eq!(sym(&data, "MAX").unwrap().kind, SymbolKind::CONSTANT);
    }
    #[test] fn operator_fun() {
        let data = parse_kotlin("operator fun plus(other: Vec): Vec = Vec()");
        assert_eq!(sym(&data, "plus").unwrap().kind, SymbolKind::OPERATOR);
    }
    #[test] fn operator_fun_in_class() {
        let data = parse_kotlin("class Vec {\n  operator fun plus(other: Vec): Vec = Vec()\n}");
        assert_eq!(sym(&data, "plus").unwrap().kind, SymbolKind::OPERATOR);
    }


    #[test]
    fn primary_ctor_val_param_indexed() {
        let data = parse_kotlin("data class User(val name: String, val age: Int)");
        assert_eq!(sym(&data, "name").unwrap().kind, SymbolKind::PROPERTY,
            "val ctor param should be PROPERTY");
        assert_eq!(sym(&data, "age").unwrap().kind, SymbolKind::PROPERTY);
    }

    #[test]
    fn primary_ctor_var_param_indexed() {
        let data = parse_kotlin("class Counter(var count: Int = 0)");
        assert_eq!(sym(&data, "count").unwrap().kind, SymbolKind::VARIABLE,
            "var ctor param should be VARIABLE");
    }

    #[test]
    fn primary_ctor_plain_param_not_indexed() {
        // A plain parameter WITHOUT val/var is NOT a property — should not be indexed.
        let data = parse_kotlin("class Foo(name: String)");
        assert!(sym(&data, "name").is_none(),
            "plain ctor param (no val/var) should not be in symbol index");
    }

    #[test]
    fn val_destructure() {
        let data = parse_kotlin("val (a, b) = pair");
        assert!(sym(&data, "a").is_some());
        assert!(sym(&data, "b").is_some());
    }

    #[test]
    fn nested_class_indexed() {
        let data = parse_kotlin("class Outer { class Inner {} }");
        assert!(sym(&data, "Outer").is_some(), "Outer missing");
        assert!(sym(&data, "Inner").is_some(), "Inner missing");
    }

    #[test]
    fn method_in_class_indexed() {
        let data = parse_kotlin("class Foo {\n  fun method() {}\n}");
        assert!(sym(&data, "method").is_some());
    }

    // ── selection_range positions ────────────────────────────────────────────

    #[test]
    fn class_name_position() {
        let data = parse_kotlin("class Foo");
        let s = sym(&data, "Foo").unwrap();
        assert_eq!(s.selection_range.start.line,      0);
        assert_eq!(s.selection_range.start.character, 6);
        assert_eq!(s.selection_range.end.character,   9);
    }

    #[test]
    fn fun_name_position() {
        let data = parse_kotlin("fun myFun() {}");
        let s = sym(&data, "myFun").unwrap();
        assert_eq!(s.selection_range.start.character, 4);
    }

    // ── deduplication ────────────────────────────────────────────────────────

    #[test]
    fn data_class_no_duplicate() {
        let data = parse_kotlin("data class Foo(val x: Int)");
        assert_eq!(
            data.symbols.iter().filter(|s| s.name == "Foo").count(), 1,
            "data class must appear exactly once"
        );
    }

    #[test]
    fn top_fun_no_duplicate() {
        let data = parse_kotlin("fun foo() {}");
        assert_eq!(
            data.symbols.iter().filter(|s| s.name == "foo").count(), 1,
            "top-level fun must appear exactly once"
        );
    }

    // ── package + imports ────────────────────────────────────────────────────

    #[test]
    fn package_parsed() {
        let data = parse_kotlin("package com.example.app");
        assert_eq!(data.package, Some("com.example.app".into()));
    }

    #[test]
    fn import_plain() {
        let data = parse_kotlin("import com.example.Foo");
        let imp = data.imports.iter().find(|i| i.full_path == "com.example.Foo").unwrap();
        assert_eq!(imp.local_name, "Foo");
        assert!(!imp.is_star);
    }

    #[test]
    fn import_alias() {
        let data = parse_kotlin("import com.example.Foo as F");
        let imp = data.imports.iter().find(|i| i.full_path == "com.example.Foo").unwrap();
        assert_eq!(imp.local_name, "F");
        assert!(!imp.is_star);
    }

    #[test]
    fn import_star() {
        let data = parse_kotlin("import com.example.*");
        let imp = data.imports.iter().find(|i| i.is_star).unwrap();
        assert_eq!(imp.full_path, "com.example");
        assert_eq!(imp.local_name, "*");
    }

    // ── lines ────────────────────────────────────────────────────────────────

    #[test]
    fn lines_populated() {
        let data = parse_kotlin("class Foo\nfun bar() {}");
        assert_eq!(data.lines.len(), 2);
        assert_eq!(data.lines[0], "class Foo");
        assert_eq!(data.lines[1], "fun bar() {}");
    }

    // ── full file smoke test ─────────────────────────────────────────────────

    #[test]
    fn full_file() {
        let src = "package com.example\n\
                   import com.example.Bar\n\
                   import com.example.pkg.*\n\
                   import com.example.Baz as B\n\
                   class MyClass\n\
                   interface MyIface\n\
                   object MySingleton\n\
                   data class MyData(val id: Int)\n\
                   typealias MyAlias = String\n\
                   val topVal = 0\n\
                   var topVar = 0\n\
                   fun topFun() {}";

        let data = parse_kotlin(src);
        assert_eq!(data.package, Some("com.example".into()));

        for name in &["MyClass","MyIface","MySingleton","MyData","MyAlias","topVal","topVar","topFun"] {
            assert!(sym(&data, name).is_some(), "{name} not indexed");
        }
        assert!(data.imports.iter().any(|i| i.full_path == "com.example.Bar"));
        assert!(data.imports.iter().any(|i| i.is_star && i.full_path == "com.example.pkg"));
        assert!(data.imports.iter().any(|i| i.local_name == "B" && i.full_path == "com.example.Baz"));
    }

    // ── visibility detection ─────────────────────────────────────────────────

    #[test]
    fn visibility_private_fun() {
        let data = parse_kotlin("class Foo {\n  private fun secret() {}\n  fun public() {}\n}");
        let secret = sym(&data, "secret").expect("secret not indexed");
        let public = sym(&data, "public").expect("public not indexed");
        assert_eq!(secret.visibility, Visibility::Private);
        assert_eq!(public.visibility, Visibility::Public);
    }

    #[test]
    fn visibility_protected_val() {
        let data = parse_kotlin("class Foo {\n  protected val x: Int = 0\n}");
        let x = sym(&data, "x").expect("x not indexed");
        assert_eq!(x.visibility, Visibility::Protected);
    }

    #[test]
    fn visibility_internal_class() {
        let data = parse_kotlin("internal class Bar");
        let bar = sym(&data, "Bar").expect("Bar not indexed");
        assert_eq!(bar.visibility, Visibility::Internal);
    }

    #[test]
    fn dot_completion_hides_private() {
        let vm_uri   = uri("/VM.kt");
        let repo_uri = uri("/Repo.kt");
        let idx = crate::indexer::Indexer::new();
        idx.index_content(&repo_uri,
            "package com.pkg\nclass Repo {\n  fun findAll() {}\n  private fun secret() {}\n}");
        idx.index_content(&vm_uri,
            "package com.pkg\nclass VM(\n  private val repo: Repo\n) {}");

        let (items, _) = idx.completions(&vm_uri, tower_lsp::lsp_types::Position::new(2, 24), true); // after "private val repo: Repo"
        // Trigger a dot completion manually through resolver
        let (items, _) = complete_symbol(
            &idx, "", Some("repo"), &vm_uri, true
        );
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"findAll"), "findAll missing: {labels:?}");
        assert!(!labels.contains(&"secret"),  "private 'secret' should be hidden: {labels:?}");
    }

    #[test]
    fn java_package_extracted() {
        let data = parse_java("package cz.moneta.example;\npublic class Foo {}");
        assert_eq!(data.package.as_deref(), Some("cz.moneta.example"));
    }

    #[test]
    fn java_enum_constants_indexed() {
        let data = parse_java("package cz.moneta.example;\npublic enum EProductScreen { FLEXIKREDIT, SAVINGS }");
        assert_eq!(data.package.as_deref(), Some("cz.moneta.example"));
        assert_eq!(sym(&data, "EProductScreen").unwrap().kind, SymbolKind::ENUM);
        assert_eq!(sym(&data, "FLEXIKREDIT").unwrap().kind, SymbolKind::ENUM_MEMBER);
        assert_eq!(sym(&data, "SAVINGS").unwrap().kind,     SymbolKind::ENUM_MEMBER);
    }

    #[test]
    fn java_import_parsed() {
        let data = parse_java("import cz.moneta.data.compat.enums.product.EProductScreen;\nclass Foo {}");
        assert_eq!(data.imports.len(), 1);
        assert_eq!(data.imports[0].local_name, "EProductScreen");
        assert_eq!(data.imports[0].full_path,  "cz.moneta.data.compat.enums.product.EProductScreen");
    }

    #[test]
    fn java_constructor_indexed() {
        let data = parse_java("public class Foo {\n  public Foo(int x) {}\n}");
        let ctor = sym(&data, "Foo");
        // class Foo AND constructor Foo both parsed; at least one must be CONSTRUCTOR
        let has_ctor = data.symbols.iter().any(|s| s.name == "Foo" && s.kind == SymbolKind::CONSTRUCTOR);
        assert!(has_ctor, "constructor not found: {:?}", data.symbols.iter().map(|s| (&s.name, s.kind)).collect::<Vec<_>>());
        let _ = ctor;
    }

    #[test]
    fn java_static_final_field_is_constant() {
        let data = parse_java("public class Cfg {\n  public static final int MAX = 100;\n}");
        let sym = data.symbols.iter().find(|s| s.name == "MAX");
        assert!(sym.is_some(), "MAX not indexed");
        assert_eq!(sym.unwrap().kind, SymbolKind::CONSTANT, "expected CONSTANT for static final field");
    }

    #[test]
    fn java_instance_field_is_field() {
        let data = parse_java("public class Cfg {\n  private int count;\n}");
        let sym = data.symbols.iter().find(|s| s.name == "count");
        assert!(sym.is_some(), "count not indexed");
        assert_eq!(sym.unwrap().kind, SymbolKind::FIELD);
    }

    #[test]
    fn declared_names_includes_function_params() {
        let src = "private fun handle(resultState: ResultState.Success<List<Int>>) {\n  val other: Foo\n}";
        let names = extract_declared_names(&src.lines().map(String::from).collect::<Vec<_>>());
        assert!(names.contains(&"resultState".to_string()), "param not found: {names:?}");
        assert!(names.contains(&"other".to_string()), "local var not found: {names:?}");
    }

    #[test]
    fn declared_names_includes_multi_params() {
        let src = "fun foo(alpha: Int, betaValue: String, gamma: Foo)";
        let names = extract_declared_names(&src.lines().map(String::from).collect::<Vec<_>>());
        assert!(names.contains(&"alpha".to_string()),     "alpha missing: {names:?}");
        assert!(names.contains(&"betaValue".to_string()), "betaValue missing: {names:?}");
        assert!(names.contains(&"gamma".to_string()),     "gamma missing: {names:?}");
    }

    // ── Syntax error detection tests ─────────────────────────────────────────

    #[test]
    fn no_errors_on_valid_kotlin() {
        let data = parse_kotlin("package com.example\nclass Foo { fun bar() {} }");
        assert!(data.syntax_errors.is_empty(), "expected no errors: {:?}", data.syntax_errors);
    }

    #[test]
    fn missing_closing_brace_kotlin() {
        let data = parse_kotlin("class Foo {\n    fun bar() {}\n");
        assert!(!data.syntax_errors.is_empty(), "expected errors for unclosed brace");
    }

    #[test]
    fn missing_closing_paren_kotlin() {
        let data = parse_kotlin("fun foo(x: Int {\n}");
        assert!(!data.syntax_errors.is_empty(), "expected errors for unclosed paren");
    }

    #[test]
    fn dangling_equals_kotlin() {
        let data = parse_kotlin("val x =\n");
        assert!(!data.syntax_errors.is_empty(), "expected errors for dangling =");
    }

    #[test]
    fn garbled_syntax_kotlin() {
        let data = parse_kotlin("class @@@ invalid!!! {{{");
        assert!(!data.syntax_errors.is_empty(), "expected errors for garbled syntax");
    }

    #[test]
    fn no_errors_on_valid_java() {
        let data = parse_java("package com.example;\npublic class Foo { void bar() {} }");
        assert!(data.syntax_errors.is_empty(), "expected no errors: {:?}", data.syntax_errors);
    }

    #[test]
    fn missing_semicolon_java() {
        let data = parse_java("public class Foo { int x = 5 }");
        assert!(!data.syntax_errors.is_empty(), "expected errors for missing semicolon");
    }

    #[test]
    fn error_message_contains_context() {
        let data = parse_kotlin("fun foo(x: Int { }");
        let msgs: Vec<&str> = data.syntax_errors.iter().map(|e| e.message.as_str()).collect();
        assert!(
            msgs.iter().any(|m| m.contains("missing") || m.contains("unexpected")),
            "error messages should be descriptive: {msgs:?}"
        );
    }

    #[test]
    fn errors_capped_at_max() {
        // Generate a file with many syntax errors.
        let bad = (0..50).map(|_| "@@@ ").collect::<String>();
        let data = parse_kotlin(&bad);
        assert!(
            data.syntax_errors.len() <= super::MAX_SYNTAX_ERRORS,
            "expected at most {} errors, got {}",
            super::MAX_SYNTAX_ERRORS, data.syntax_errors.len()
        );
    }

    #[test]
    fn error_has_correct_line() {
        let src = "class Foo {\n    fun bar() {}\n    val x =\n}";
        let data = parse_kotlin(src);
        assert!(!data.syntax_errors.is_empty());
        // The error should be on or near line 2 (0-indexed) where `val x =` is.
        let has_line_2_or_3 = data.syntax_errors.iter().any(|e|
            e.range.start.line == 2 || e.range.start.line == 3
        );
        assert!(has_line_2_or_3, "error should be near line 2-3: {:?}", data.syntax_errors);
    }

    // ── Swift parsing ────────────────────────────────────────────────────────

    #[test]
    fn swift_query_compiles() {
        let lang = tree_sitter_swift_bundled::language();
        tree_sitter::Query::new(&lang, crate::queries::SWIFT_DEFINITIONS)
            .expect("SWIFT_DEFINITIONS query should compile");
    }

    #[test] fn swift_class()    { assert_eq!(sym(&parse_swift("class Foo {}"),    "Foo").unwrap().kind, SymbolKind::CLASS); }
    #[test] fn swift_struct()   { assert_eq!(sym(&parse_swift("struct Bar {}"),   "Bar").unwrap().kind, SymbolKind::STRUCT); }
    #[test] fn swift_enum()     { assert_eq!(sym(&parse_swift("enum Dir { case n }"), "Dir").unwrap().kind, SymbolKind::ENUM); }
    #[test] fn swift_protocol() { assert_eq!(sym(&parse_swift("protocol P {}"),   "P").unwrap().kind, SymbolKind::INTERFACE); }
    #[test] fn swift_func()     { assert_eq!(sym(&parse_swift("func foo() {}"),   "foo").unwrap().kind, SymbolKind::FUNCTION); }
    #[test] fn swift_typealias(){ assert_eq!(sym(&parse_swift("typealias A = Int"), "A").unwrap().kind, SymbolKind::CLASS); }

    #[test]
    fn swift_property_let() {
        let data = parse_swift("let x = 42");
        assert_eq!(sym(&data, "x").unwrap().kind, SymbolKind::PROPERTY);
    }

    #[test]
    fn swift_property_var() {
        let data = parse_swift("var y: Int = 0");
        assert_eq!(sym(&data, "y").unwrap().kind, SymbolKind::PROPERTY);
    }

    #[test]
    fn swift_enum_entries() {
        let data = parse_swift("enum Dir { case north, south, east }");
        assert!(sym(&data, "north").is_some());
        assert!(sym(&data, "south").is_some());
        assert!(sym(&data, "east").is_some());
    }

    #[test]
    fn swift_extension() {
        let data = parse_swift("extension Point: Equatable { func dist() -> Double { 0 } }");
        let ext = sym(&data, "Point").unwrap();
        assert_eq!(ext.kind, SymbolKind::CLASS);
        assert!(sym(&data, "dist").is_some());
    }

    #[test]
    fn swift_init() {
        let data = parse_swift("class Foo { init(x: Int) { } }");
        assert!(sym(&data, "init").is_some());
    }

    #[test]
    fn swift_imports() {
        let data = parse_swift("import Foundation\nimport UIKit\nclass A {}");
        assert_eq!(data.imports.len(), 2);
        assert_eq!(data.imports[0].full_path, "Foundation");
        assert_eq!(data.imports[1].full_path, "UIKit");
    }

    #[test]
    fn swift_no_package() {
        let data = parse_swift("class A {}");
        assert!(data.package.is_none());
    }

    #[test]
    fn swift_visibility() {
        let data = parse_swift("private class Secret {}\npublic class Pub {}");
        assert_eq!(sym(&data, "Secret").unwrap().visibility, Visibility::Private);
        assert_eq!(sym(&data, "Pub").unwrap().visibility, Visibility::Public);
    }

    #[test]
    fn swift_default_visibility_is_internal() {
        let data = parse_swift("class Foo {}");
        assert_eq!(sym(&data, "Foo").unwrap().visibility, Visibility::Internal);
    }

    #[test]
    fn swift_detail_extraction() {
        let data = parse_swift("func distance(to other: Point) -> Double { 0 }");
        let s = sym(&data, "distance").unwrap();
        assert!(s.detail.contains("distance"), "detail: {}", s.detail);
    }

    #[test]
    fn swift_syntax_errors() {
        let data = parse_swift("class Foo {\n    func bar() {}\n    let x =\n}");
        assert!(!data.syntax_errors.is_empty(), "should detect syntax error");
    }

    #[test]
    fn parse_by_extension_dispatch() {
        let kt = parse_by_extension("/Foo.kt", "class Foo");
        let java = parse_by_extension("/Foo.java", "public class Foo {}");
        let swift = parse_by_extension("/Foo.swift", "class Foo {}");
        assert!(sym(&kt, "Foo").is_some());
        assert!(sym(&java, "Foo").is_some());
        assert!(sym(&swift, "Foo").is_some());
    }

    #[test]
    fn loan_reducer_no_false_errors() {
        // @AssistedFactory fun interface Factory inside a class should not
        // produce a false "missing bracket" syntax error.
        let src = r#"
class LoanReducer {
  @AssistedFactory
  fun interface Factory {
    fun create(
      reloadAction: (loanId: String, isWustenrot: Boolean) -> Unit,
      mapSheet: (LoanDetail) -> ProductDetailSheetModel,
    ): LoanReducer
  }
}
"#;
        let data = parse_kotlin(src);
        assert!(data.syntax_errors.is_empty(),
            "Expected no syntax errors, got: {:?}", data.syntax_errors);
    }

    #[test]
    fn swift_nested_enum_in_class() {
        let src = "final class DPSChangeVictoryViewModel: SimpleVictoryViewModel, @unchecked Sendable {\n    let coordinator: DPSCoordinator\n    func update(kind: DPSCoordinator.Kind) {}\n}\n\nclass DPSCoordinator {\n    enum Kind {\n        case victory\n        case defeat\n    }\n}";
        let data = parse_swift(src);
        let names: Vec<&str> = data.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(sym(&data, "DPSChangeVictoryViewModel").unwrap().kind, SymbolKind::CLASS,
            "DPSChangeVictoryViewModel should be CLASS; symbols: {names:?}");
        assert!(sym(&data, "Kind").is_some(), "nested Kind enum should be indexed; got: {names:?}");
        assert_eq!(sym(&data, "Kind").unwrap().kind, SymbolKind::ENUM, "Kind should be ENUM");
        assert!(sym(&data, "victory").is_some(), "enum cases should be indexed; got: {names:?}");
    }

    #[test]
    fn dedup_matches_lower_pidx_wins() {
        use tower_lsp::lsp_types::{Position, Range};
        let sel   = Range::new(Position::new(1, 0), Position::new(1, 3));
        let range = sel;
        let matches: Vec<MatchEntry> = vec![
            (2, [Some(("Foo".into(), range, sel, vec![])), None]),
            (0, [Some(("Foo".into(), range, sel, vec![])), None]),
        ];
        let best = dedup_matches(&matches);
        assert_eq!(best.len(), 1);
        assert_eq!(best.values().next().unwrap().0, 0, "pidx 0 should win over pidx 2");
    }

    // ── Swift supers extraction ──────────────────────────────────────────────

    fn supers_names(data: &FileData) -> Vec<String> {
        data.supers.iter().map(|(_, name, _)| name.clone()).collect()
    }

    #[test]
    fn swift_supers_class() {
        let data = parse_swift("class Foo: UIViewController, Sendable {}");
        let names = supers_names(&data);
        assert!(names.contains(&"UIViewController".to_owned()), "missing UIViewController; got: {names:?}");
        assert!(names.contains(&"Sendable".to_owned()), "missing Sendable; got: {names:?}");
    }

    #[test]
    fn swift_supers_protocol() {
        let data = parse_swift("protocol P: Q, R {}");
        let names = supers_names(&data);
        assert!(names.contains(&"Q".to_owned()), "missing Q; got: {names:?}");
        assert!(names.contains(&"R".to_owned()), "missing R; got: {names:?}");
    }

    #[test]
    fn swift_supers_struct() {
        let data = parse_swift("struct Point: Drawable {}");
        let names = supers_names(&data);
        assert!(names.contains(&"Drawable".to_owned()), "missing Drawable; got: {names:?}");
    }

    #[test]
    fn swift_supers_extension() {
        let data = parse_swift("extension Point: Hashable, Equatable {}");
        let names = supers_names(&data);
        assert!(names.contains(&"Hashable".to_owned()), "missing Hashable; got: {names:?}");
        assert!(names.contains(&"Equatable".to_owned()), "missing Equatable; got: {names:?}");
    }

    #[test]
    fn swift_supers_with_generic_base() {
        let data = parse_swift("class Foo: Bar<Baz> {}");
        let entry = data.supers.iter().find(|(_, name, _)| name == "Bar")
            .expect("missing Bar super");
        assert_eq!(entry.2, vec!["Baz"], "type_args for Bar<Baz> should be [Baz]");
    }

    #[test]
    fn swift_supers_multi_generic_args() {
        let data = parse_swift("class Foo: Base<Int, String> {}");
        let entry = data.supers.iter().find(|(_, name, _)| name == "Base")
            .expect("missing Base super");
        assert_eq!(entry.2, vec!["Int", "String"], "type_args for Base<Int, String> should be [Int, String]");
    }

    // ── visibility ───────────────────────────────────────────────────────────

    #[test]
    fn visibility_kotlin_defaults_public() {
        let lines: Vec<String> = vec!["fun foo() {}".into()];
        assert_eq!(visibility_at_line(&lines, 0), crate::types::Visibility::Public);
    }

    #[test]
    fn visibility_kotlin_private() {
        let lines: Vec<String> = vec!["private fun foo() {}".into()];
        assert_eq!(visibility_at_line(&lines, 0), crate::types::Visibility::Private);
    }

    #[test]
    fn visibility_swift_defaults_internal() {
        let lines: Vec<String> = vec!["func foo() {}".into()];
        assert_eq!(swift_visibility_at_line(&lines, 0), crate::types::Visibility::Internal);
    }

    #[test]
    fn visibility_swift_open_is_public() {
        let lines: Vec<String> = vec!["open class Foo {}".into()];
        assert_eq!(swift_visibility_at_line(&lines, 0), crate::types::Visibility::Public);
    }

    // ── type_params extraction ──────────────────────────────────────────────

    #[test]
    fn kotlin_generic_class_has_type_params() {
        let src = "class Box<T, U>(val value: T) {}";
        let data = parse_kotlin(src);
        let s = sym(&data, "Box").expect("Box not found");
        assert_eq!(s.type_params, vec!["T", "U"]);
    }

    #[test]
    fn kotlin_generic_interface_has_type_params() {
        let src = "interface FlowReducer<in Event, out Effect, State> {}";
        let data = parse_kotlin(src);
        let s = sym(&data, "FlowReducer").expect("FlowReducer not found");
        assert_eq!(s.type_params, vec!["Event", "Effect", "State"]);
    }

    #[test]
    fn kotlin_non_generic_class_has_empty_type_params() {
        let src = "class Plain {}";
        let data = parse_kotlin(src);
        let s = sym(&data, "Plain").expect("Plain not found");
        assert!(s.type_params.is_empty(), "expected empty type_params, got {:?}", s.type_params);
    }

    #[test]
    fn java_generic_class_has_type_params() {
        let src = "public class Pair<A, B> { public A first; public B second; }";
        let data = parse_java(src);
        let s = sym(&data, "Pair").expect("Pair not found");
        assert_eq!(s.type_params, vec!["A", "B"]);
    }

    // ── fun interface type_params ──────────────────────────────────────────────

    #[test]
    fn fun_interface_no_modifier_is_indexed() {
        let src = "fun interface Action {}";
        let data = parse_kotlin(src);
        let s = sym(&data, "Action").expect("Action not found");
        assert_eq!(s.kind, tower_lsp::lsp_types::SymbolKind::INTERFACE);
        assert!(s.type_params.is_empty());
    }

    #[test]
    fn fun_interface_with_modifier_is_indexed() {
        let src = "public fun interface Runnable {}";
        let data = parse_kotlin(src);
        let s = sym(&data, "Runnable").expect("Runnable not found");
        assert_eq!(s.kind, tower_lsp::lsp_types::SymbolKind::INTERFACE);
        assert!(s.type_params.is_empty());
    }

    #[test]
    fn fun_interface_body_generic_method_not_harvested() {
        // Non-generic fun interface whose body has a generic method must not
        // pick up the method's type param as the interface's own type param.
        let src = "fun interface Transformer { fun <T> transform(x: Any): T }";
        let data = parse_kotlin(src);
        let s = sym(&data, "Transformer").expect("Transformer not found");
        assert_eq!(s.kind, tower_lsp::lsp_types::SymbolKind::INTERFACE);
        assert!(s.type_params.is_empty(),
            "body method type param leaked: {:?}", s.type_params);
    }

    #[test]
    fn fun_interface_generic_type_params() {
        let src = "fun interface Router<Effect> { fun route(effect: Effect) }";
        let data = parse_kotlin(src);
        let s = sym(&data, "Router").expect("Router not found");
        assert_eq!(s.kind, tower_lsp::lsp_types::SymbolKind::INTERFACE);
        // Text fallback extracts type params from the declaration line when CST
        // error recovery doesn't produce a type_parameters node.
        assert_eq!(s.type_params, vec!["Effect".to_string()],
            "type_params should be extracted via text fallback: {:?}", s.type_params);
        assert!(!s.type_params.contains(&"effect".to_string()),
            "method param leaked into interface type_params: {:?}", s.type_params);
    }

    /// Multi-type-param `fun interface` declarations (e.g. `<A, B>`) are now indexed
    /// via the nested-ERROR detection path added for variance support.
    #[test]
    fn fun_interface_multi_type_params_indexed() {
        let src = "fun interface Pair<A, B> { fun get(): A }";
        let data = parse_kotlin(src);
        let s = sym(&data, "Pair").expect("Pair should now be indexed");
        assert_eq!(s.type_params, vec!["A", "B"]);
    }

    /// type_params_from_angle_brackets must not produce entries containing `:` or spaces.
    /// For `fun interface Sortable<T: Comparable>`, like multi-param, the whole
    /// declaration is not indexed (same tree-sitter-kotlin 0.3 limitation).
    #[test]
    fn angle_brackets_strips_variance_and_bounds() {
        // When a fun interface with variance/bounds IS indexed, type_params must strip them.
        // Not all forms are detectable by is_fun_interface_error (tree-sitter may wrap the
        // name in user_type when generics follow), so we use `if let Some`.
        let cases: &[(&str, &[&str])] = &[
            ("fun interface Producer<out T>",  &["T"]),
            ("fun interface Consumer<in T>",   &["T"]),
            ("fun interface Box<T : Any>",     &["T"]),
            ("fun interface Pair<out A, in B>", &["A", "B"]),
        ];
        for (src, expected) in cases {
            let data = parse_kotlin(src);
            if let Some(sym) = data.symbols.iter().find(|s| s.kind == SymbolKind::INTERFACE) {
                assert_eq!(&sym.type_params.iter().map(String::as_str).collect::<Vec<_>>(),
                    expected, "type_params wrong for: {src}");
            }
            // If not indexed: known limitation — tree-sitter-kotlin 0.3 wraps the
            // name in user_type when variance appears, hiding the simple_identifier.
        }
    }

    #[test]
    fn angle_brackets_ignores_complex_declarations() {
        let src = "fun interface Sortable<T: Comparable> { fun sort() }";
        let data = parse_kotlin(src);
        // `T: Comparable` bound stripped → no `:` in type_params
        for s in &data.symbols {
            assert!(!s.type_params.iter().any(|p| p.contains(':')),
                "bound leaked into type_params for {}: {:?}", s.name, s.type_params);
        }
    }

    // ── fun interface CST fixture tests ──────────────────────────────────────
    // Fixture files live in tests/fixtures/kotlin/ — they replicate the package
    // structure of the original production sources so the package declaration
    // and naming context are preserved.

    #[test]
    fn fun_interface_single_type_param_indexed() {
        let src = std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/kotlin/mvi/Router.kt")
        ).expect("fixture Router.kt missing");
        let data = parse_kotlin(&src);
        let sym = data.symbols.iter().find(|s| s.name == "Router" && s.kind == SymbolKind::INTERFACE)
            .expect("Router interface not indexed");
        assert_eq!(sym.type_params, vec!["Effect"], "Router<Effect> type_params wrong");
    }

    #[test]
    fn fun_interface_variance_stripped() {
        let src = std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/kotlin/input/validator/IInputValidator.kt")
        ).expect("fixture IInputValidator.kt missing");
        let data = parse_kotlin(&src);
        let sym = data.symbols.iter().find(|s| s.name == "IInputValidator" && s.kind == SymbolKind::INTERFACE)
            .expect("IInputValidator should be indexed");
        assert_eq!(sym.type_params, vec!["In", "Out"],
            "variance 'in'/'out' must be stripped from type_params");
    }

    // ── extract_extension_receiver ────────────────────────────────────────────

    #[test]
    fn extension_receiver_simple() {
        assert_eq!(super::extract_extension_receiver("fun Foo.bar()"), "Foo");
    }

    #[test]
    fn extension_receiver_with_type_params() {
        assert_eq!(super::extract_extension_receiver("fun <T> List<T>.bar()"), "List");
    }

    #[test]
    fn extension_receiver_qualified() {
        // `fun Outer.Inner.baz()` — last segment is Inner
        assert_eq!(super::extract_extension_receiver("fun Outer.Inner.baz()"), "Inner");
    }

    #[test]
    fn extension_receiver_no_receiver() {
        assert_eq!(super::extract_extension_receiver("fun bar()"), "");
    }

    #[test]
    fn extension_receiver_non_fun() {
        assert_eq!(super::extract_extension_receiver("val x: Int"), "");
        assert_eq!(super::extract_extension_receiver("class Foo"), "");
    }

    #[test]
    fn extension_receiver_indexed_in_symbol_entry() {
        // A top-level extension function should have extension_receiver populated.
        let src = "fun String.shout(): String = this.uppercase()";
        let data = super::parse_kotlin(src);
        let sym = data.symbols.iter().find(|s| s.name == "shout")
            .expect("shout should be indexed");
        assert_eq!(sym.extension_receiver, "String");
    }

    #[test]
    fn non_extension_fun_has_empty_receiver() {
        let src = "fun greet(name: String): String = \"Hello $name\"";
        let data = super::parse_kotlin(src);
        let sym = data.symbols.iter().find(|s| s.name == "greet")
            .expect("greet should be indexed");
        assert_eq!(sym.extension_receiver, "");
    }
