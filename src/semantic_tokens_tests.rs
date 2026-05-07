use tower_lsp::lsp_types::{Position, Range, SemanticTokenModifier, SemanticTokenType, Url};

use crate::indexer::{live_tree::parse_live, Indexer};
use crate::semantic_tokens::{
    full_tokens, full_tokens_cst_only, range_tokens_cst_only, TOKEN_MODIFIERS, TOKEN_TYPES,
};
use crate::Language;

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn parse_kotlin(src: &str) -> crate::indexer::LiveDoc {
    parse_live(src, tree_sitter_kotlin::language()).expect("parse failed")
}

fn parse_java(src: &str) -> crate::indexer::LiveDoc {
    parse_live(src, tree_sitter_java::language()).expect("parse failed")
}

/// Find token type id by name in the legend.
fn type_id(t: &SemanticTokenType) -> u32 {
    TOKEN_TYPES.iter().position(|x| x == t).unwrap() as u32
}

fn decode_tokens(tokens: &tower_lsp::lsp_types::SemanticTokens) -> Vec<(u32, u32, u32, u32, u32)> {
    let mut line = 0u32;
    let mut col = 0u32;
    tokens
        .data
        .iter()
        .map(|t| {
            line += t.delta_line;
            if t.delta_line > 0 {
                col = t.delta_start;
            } else {
                col += t.delta_start;
            }
            (line, col, t.length, t.token_type, t.token_modifiers_bitset)
        })
        .collect()
}

/// Decode one SemanticToken from the delta-encoded stream.
fn decode_all(doc: &crate::indexer::LiveDoc, language: Language) -> Vec<(u32, u32, u32, u32, u32)> {
    decode_tokens(&full_tokens_cst_only(doc, language))
}

fn decode_all_indexed(
    indexer: &Indexer,
    uri: &Url,
    doc: &crate::indexer::LiveDoc,
    language: Language,
) -> Vec<(u32, u32, u32, u32, u32)> {
    decode_tokens(&full_tokens(indexer, uri, doc, language))
}

fn assert_token_at(
    tokens: &[(u32, u32, u32, u32, u32)],
    line: u32,
    col: u32,
    token_type: u32,
    label: &str,
) {
    assert!(
        tokens
            .iter()
            .any(|&(token_line, token_col, _, kind, _)| token_line == line
                && token_col == col
                && kind == token_type),
        "expected {label} token at {line}:{col}, got: {tokens:?}"
    );
}

// ─── Kotlin tests ─────────────────────────────────────────────────────────────

#[test]
fn kotlin_class_decl() {
    let src = "class Foo";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let class_id = type_id(&SemanticTokenType::CLASS);
    let found = tokens.iter().any(|&(_, _, _, tt, _)| tt == class_id);
    assert!(
        found,
        "expected CLASS token for 'class Foo', got: {tokens:?}"
    );
}

#[test]
fn kotlin_interface_decl() {
    let src = "interface Runnable";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let iface_id = type_id(&SemanticTokenType::INTERFACE);
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == iface_id),
        "expected INTERFACE token, got: {tokens:?}"
    );
}

#[test]
fn kotlin_enum_class() {
    let src = "enum class Color { RED, GREEN, BLUE }";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let enum_id = type_id(&SemanticTokenType::ENUM);
    let member_id = type_id(&SemanticTokenType::ENUM_MEMBER);
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == enum_id),
        "expected ENUM token, got: {tokens:?}"
    );
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == member_id),
        "expected ENUM_MEMBER token, got: {tokens:?}"
    );
}

#[test]
fn kotlin_top_level_function() {
    let src = "fun greet() {}";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let fun_id = type_id(&SemanticTokenType::FUNCTION);
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == fun_id),
        "expected FUNCTION token for top-level fun, got: {tokens:?}"
    );
}

#[test]
fn kotlin_method_vs_function() {
    let src = r#"
class Foo {
    fun bar() {}
}
fun topLevel() {}
"#;
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let method_id = type_id(&SemanticTokenType::METHOD);
    let fun_id = type_id(&SemanticTokenType::FUNCTION);
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == method_id),
        "expected METHOD token for class member fun, got: {tokens:?}"
    );
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == fun_id),
        "expected FUNCTION token for top-level fun, got: {tokens:?}"
    );
}

#[test]
fn kotlin_val_is_readonly() {
    let src = "val x: Int = 1";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    // READONLY modifier bit = 1
    let readonly_bit = 1u32 << 1;
    assert!(
        tokens
            .iter()
            .any(|&(_, _, _, _, mods)| mods & readonly_bit != 0),
        "expected READONLY modifier for val, got: {tokens:?}"
    );
}

#[test]
fn kotlin_var_not_readonly() {
    let src = "var x: Int = 1";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let var_id = type_id(&SemanticTokenType::VARIABLE);
    let readonly_bit = 1u32 << 1;
    // Find the token for `x` at col 4 (after "var ")
    let x_token = tokens
        .iter()
        .find(|&&(line, col, _, tt, _)| line == 0 && col == 4 && tt == var_id);
    assert!(
        x_token.is_some(),
        "expected VARIABLE token for x at 0:4, got: {tokens:?}"
    );
    let (_, _, _, _, mods) = *x_token.unwrap();
    assert_eq!(
        mods & readonly_bit,
        0,
        "var x should NOT have READONLY modifier, mods={mods:#b}"
    );
}

#[test]
fn kotlin_type_parameter() {
    let src = "fun <T> identity(x: T): T = x";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let tp_id = type_id(&SemanticTokenType::TYPE_PARAMETER);
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == tp_id),
        "expected TYPE_PARAMETER token, got: {tokens:?}"
    );
}

#[test]
fn kotlin_companion_object() {
    let src = r#"
class Foo {
    companion object {
        val CONST = 1
    }
}
"#;
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let ns_id = type_id(&SemanticTokenType::NAMESPACE);
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == ns_id),
        "expected NAMESPACE token for companion object, got: {tokens:?}"
    );
}

#[test]
fn kotlin_object_decl() {
    let src = "object Singleton { val x = 1 }";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let ns_id = type_id(&SemanticTokenType::NAMESPACE);
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == ns_id),
        "expected NAMESPACE token for object decl, got: {tokens:?}"
    );
}

#[test]
fn kotlin_range_restricts_to_range() {
    let src = r#"
class Foo
fun bar() {}
"#;
    let doc = parse_kotlin(src);
    // Only request line 1 (0-indexed) — "class Foo"
    let range = Range {
        start: Position {
            line: 1,
            character: 0,
        },
        end: Position {
            line: 1,
            character: 9,
        },
    };
    let tokens = range_tokens_cst_only(&doc, Language::Kotlin, &range);
    // Should have tokens only on line 1
    let decoded: Vec<_> = {
        let mut line = 0u32;
        let mut col = 0u32;
        tokens
            .data
            .iter()
            .map(|t| {
                line += t.delta_line;
                if t.delta_line > 0 {
                    col = t.delta_start;
                } else {
                    col += t.delta_start;
                }
                line
            })
            .collect()
    };
    assert!(
        decoded.iter().all(|&l| l == 1),
        "range_tokens should only return tokens on line 1, got lines: {decoded:?}"
    );
}

#[test]
fn kotlin_range_honors_character_bounds() {
    let src = "fun foo(x: Int) = x\n";
    let doc = parse_kotlin(src);
    let range = Range {
        start: Position {
            line: 0,
            character: 7,
        },
        end: Position {
            line: 0,
            character: 9,
        },
    };
    let decoded = decode_tokens(&range_tokens_cst_only(&doc, Language::Kotlin, &range));
    let param_type = type_id(&SemanticTokenType::PARAMETER);
    assert_eq!(
        decoded.len(),
        1,
        "expected exactly one token in range, got: {decoded:?}"
    );
    assert_eq!(
        decoded[0].0, 0,
        "expected token on line 0, got: {decoded:?}"
    );
    assert_eq!(
        decoded[0].1, 8,
        "expected token at parameter column, got: {decoded:?}"
    );
    assert_eq!(
        decoded[0].3, param_type,
        "expected PARAMETER token, got: {decoded:?}"
    );
}

#[test]
fn kotlin_reference_sites_resolve_types_functions_and_namespaces() {
    let src = "class User\nobject Utils { fun run() {} }\nfun greet(): User = User()\nfun use(): User {\n    greet()\n    Utils.run()\n    return User()\n}\n";
    let uri = Url::parse("file:///semantic_tokens_refs.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);

    assert_token_at(
        &tokens,
        2,
        13,
        type_id(&SemanticTokenType::CLASS),
        "CLASS return type",
    );
    assert_token_at(
        &tokens,
        2,
        20,
        type_id(&SemanticTokenType::CLASS),
        "CLASS constructor call",
    );
    assert_token_at(
        &tokens,
        3,
        11,
        type_id(&SemanticTokenType::CLASS),
        "CLASS function return type",
    );
    assert_token_at(
        &tokens,
        4,
        4,
        type_id(&SemanticTokenType::FUNCTION),
        "FUNCTION call",
    );
    assert_token_at(
        &tokens,
        5,
        4,
        type_id(&SemanticTokenType::NAMESPACE),
        "NAMESPACE receiver",
    );
    assert_token_at(
        &tokens,
        6,
        11,
        type_id(&SemanticTokenType::CLASS),
        "CLASS return expression",
    );
}

#[test]
fn kotlin_reference_sites_resolve_annotations_and_enum_entries() {
    let src = "annotation class Fancy\nenum class Color { RED }\n@Fancy\nfun color(): Color = Color.RED\n";
    let uri = Url::parse("file:///semantic_tokens_annotations.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);

    assert_token_at(
        &tokens,
        2,
        0,
        type_id(&SemanticTokenType::DECORATOR),
        "DECORATOR annotation reference",
    );
    assert_token_at(
        &tokens,
        3,
        13,
        type_id(&SemanticTokenType::ENUM),
        "ENUM return type",
    );
    assert_token_at(
        &tokens,
        3,
        27,
        type_id(&SemanticTokenType::ENUM_MEMBER),
        "ENUM_MEMBER reference",
    );
}

// ─── Java tests ───────────────────────────────────────────────────────────────

#[test]
fn java_class_decl() {
    let src = "class Foo {}";
    let doc = parse_java(src);
    let tokens = decode_all(&doc, Language::Java);
    let class_id = type_id(&SemanticTokenType::CLASS);
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == class_id),
        "expected CLASS token for Java class, got: {tokens:?}"
    );
}

#[test]
fn java_interface_decl() {
    let src = "interface Runnable { void run(); }";
    let doc = parse_java(src);
    let tokens = decode_all(&doc, Language::Java);
    let iface_id = type_id(&SemanticTokenType::INTERFACE);
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == iface_id),
        "expected INTERFACE token for Java interface, got: {tokens:?}"
    );
}

#[test]
fn java_method_decl() {
    let src = r#"
class Foo {
    void bar() {}
}
"#;
    let doc = parse_java(src);
    let tokens = decode_all(&doc, Language::Java);
    let method_id = type_id(&SemanticTokenType::METHOD);
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == method_id),
        "expected METHOD token for Java method, got: {tokens:?}"
    );
}

#[test]
fn java_enum_constant() {
    let src = "enum Color { RED, GREEN, BLUE }";
    let doc = parse_java(src);
    let tokens = decode_all(&doc, Language::Java);
    let enum_id = type_id(&SemanticTokenType::ENUM);
    let member_id = type_id(&SemanticTokenType::ENUM_MEMBER);
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == enum_id),
        "expected ENUM token for Java enum, got: {tokens:?}"
    );
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == member_id),
        "expected ENUM_MEMBER token for Java enum constant, got: {tokens:?}"
    );
}

#[test]
fn java_type_parameter() {
    let src = r#"
class Box<T> {
    T value;
}
"#;
    let doc = parse_java(src);
    let tokens = decode_all(&doc, Language::Java);
    let tp_id = type_id(&SemanticTokenType::TYPE_PARAMETER);
    assert!(
        tokens.iter().any(|&(_, _, _, tt, _)| tt == tp_id),
        "expected TYPE_PARAMETER token for Java generic, got: {tokens:?}"
    );
}

#[test]
fn delta_encoding_is_sorted() {
    let src = r#"
class Foo {
    val x: Int = 1
    fun bar(): Int = x
}
"#;
    let doc = parse_kotlin(src);
    let tokens = full_tokens_cst_only(&doc, Language::Kotlin);
    // Decode back to absolute positions and verify non-decreasing order
    let mut abs_line: u32 = 0;
    let mut abs_col: u32 = 0;
    for t in &tokens.data {
        if t.delta_line > 0 {
            abs_line += t.delta_line;
            abs_col = t.delta_start;
        } else {
            abs_col += t.delta_start;
        }
        assert!(
            t.delta_line < 0x8000_0000,
            "delta_line overflow suggests unsorted tokens: {t:?}"
        );
        // Same-line tokens must have non-zero delta_start (or be the first on this line)
        if t.delta_line == 0 && abs_col > 0 {
            assert!(
                t.delta_start > 0 || abs_col == t.delta_start,
                "same-line delta_start=0 implies duplicate/unsorted at line {abs_line}: {t:?}"
            );
        }
    }
    // Verify we have tokens spanning multiple lines
    assert!(abs_line > 0, "test should have tokens on multiple lines");
}

#[test]
fn empty_file_returns_no_tokens() {
    let doc = parse_kotlin("");
    let tokens = full_tokens_cst_only(&doc, Language::Kotlin);
    assert!(
        tokens.data.is_empty(),
        "empty file should produce no tokens"
    );
}

#[test]
fn named_arg_cst_detail() {
    let src = "fun main() { foo(name = 42) }\n";
    let doc = parse_kotlin(src);
    fn find_value_arg(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
        if node.kind() == "value_argument" {
            return Some(node);
        }
        let mut c = node.walk();
        if c.goto_first_child() {
            loop {
                if let Some(r) = find_value_arg(c.node()) {
                    return Some(r);
                }
                if !c.goto_next_sibling() {
                    break;
                }
            }
        }
        None
    }
    let va = find_value_arg(doc.tree.root_node()).unwrap();
    eprintln!("value_argument children:");
    for i in 0..va.child_count() {
        let c = va.child(i).unwrap();
        eprintln!(
            "  child({i}): kind={:?} named={} text={:?}",
            c.kind(),
            c.is_named(),
            c.utf8_text(src.as_bytes()).unwrap_or("?")
        );
    }
    eprintln!("value_argument named_children:");
    for i in 0..va.named_child_count() {
        let c = va.named_child(i).unwrap();
        eprintln!(
            "  named_child({i}): kind={:?} text={:?}",
            c.kind(),
            c.utf8_text(src.as_bytes()).unwrap_or("?")
        );
    }
    // Check our detection function
    let label = va.named_child(0).unwrap();
    let next_sib = label.next_sibling();
    eprintln!("label next_sibling: {:?}", next_sib.map(|n| n.kind()));
}

#[test]
fn named_arg_label_gets_property_token() {
    let src = "fun foo(name: Int) {}\nfun main() { foo(name = 42) }\n";
    let uri = Url::parse("file:///named_arg_test.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    // "name" in foo(name = 42) is at line 1, col 17 — emitted as PARAMETER (JetBrains behaviour)
    let param_type = type_id(&SemanticTokenType::PARAMETER);
    assert!(
        tokens
            .iter()
            .any(|t| t.0 == 1 && t.1 == 17 && t.3 == param_type),
        "Expected PARAMETER at (1,17) for named arg 'name', got: {tokens:?}"
    );
}

#[test]
fn named_arg_multiline_call() {
    let src = "fun main() {\n    Scaffold(\n        modifier = 1,\n        topBar = 2\n    )\n}\n";
    let uri = Url::parse("file:///multiline_named.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let param_type = type_id(&SemanticTokenType::PARAMETER);
    // "modifier" at line 2, col 8
    assert!(
        tokens.iter().any(|t| t.0 == 2 && t.3 == param_type),
        "Expected PARAMETER on line 2 for 'modifier', got: {tokens:?}"
    );
    // "topBar" at line 3, col 8
    assert!(
        tokens.iter().any(|t| t.0 == 3 && t.3 == param_type),
        "Expected PARAMETER on line 3 for 'topBar', got: {tokens:?}"
    );
}

// ─── Phase 2: Comprehensive semantic token coverage ──────────────────────────

// --- Declaration-site tokens (CST-only, no indexer) ---

#[test]
fn decl_function_parameter() {
    let src = "fun greet(name: String, age: Int) {}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let param_type = type_id(&SemanticTokenType::PARAMETER);
    // "name" at col 10, "age" at col 24
    assert!(
        tokens
            .iter()
            .any(|t| t.0 == 0 && t.1 == 10 && t.3 == param_type),
        "Expected PARAMETER for 'name' at col 10, got: {tokens:?}"
    );
    assert!(
        tokens
            .iter()
            .any(|t| t.0 == 0 && t.1 == 24 && t.3 == param_type),
        "Expected PARAMETER for 'age' at col 24, got: {tokens:?}"
    );
}

#[test]
fn decl_annotation_simple() {
    let src = "@Composable\nfun Screen() {}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let deco_type = type_id(&SemanticTokenType::DECORATOR);
    assert!(
        tokens.iter().any(|t| t.0 == 0 && t.3 == deco_type),
        "Expected DECORATOR for @Composable, got: {tokens:?}"
    );
}

#[test]
fn decl_annotation_with_args() {
    let src = "@Named(\"key\")\nfun provide() {}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let deco_type = type_id(&SemanticTokenType::DECORATOR);
    assert!(
        tokens.iter().any(|t| t.0 == 0 && t.3 == deco_type),
        "Expected DECORATOR for @Named(...), got: {tokens:?}"
    );
}

#[test]
fn decl_annotation_inline_on_parameter() {
    let src = "fun test(@Inject param: String) {}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let deco_type = type_id(&SemanticTokenType::DECORATOR);
    assert!(
        tokens.iter().any(|t| t.0 == 0 && t.3 == deco_type),
        "Expected DECORATOR for inline @Inject on parameter, got: {tokens:?}"
    );
}

#[test]
fn decl_annotation_inline_with_args_on_parameter() {
    let src = "fun test(@Named(\"key\") param: String) {}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let deco_type = type_id(&SemanticTokenType::DECORATOR);
    assert!(
        tokens.iter().any(|t| t.0 == 0 && t.3 == deco_type),
        "Expected DECORATOR for inline @Named(\"key\") on param, got: {tokens:?}"
    );
}

#[test]
fn param_use_in_body_gets_parameter_token() {
    // Parameters referenced in the function body should be colored as PARAMETER
    // (without declaration modifier) — matches rust-analyzer behaviour.
    let src = "fun greet(name: String, count: Int) {\n    println(name)\n    println(count)\n}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let param_type = type_id(&SemanticTokenType::PARAMETER);
    let decl_mod = 1u32
        << TOKEN_MODIFIERS
            .iter()
            .position(|m| m == &SemanticTokenModifier::DECLARATION)
            .unwrap();
    // Use-site tokens: line 1 col 12 "name", line 2 col 12 "count"
    assert!(
        tokens.iter().any(|&(line, col, _, tt, mods)| line == 1
            && col == 12
            && tt == param_type
            && (mods & decl_mod) == 0),
        "Expected PARAMETER (no declaration) for 'name' at 1:12, got: {tokens:?}"
    );
    assert!(
        tokens.iter().any(|&(line, col, _, tt, mods)| line == 2
            && col == 12
            && tt == param_type
            && (mods & decl_mod) == 0),
        "Expected PARAMETER (no declaration) for 'count' at 2:12, got: {tokens:?}"
    );
}

#[test]
fn param_use_in_string_template_gets_parameter_token() {
    let src = "fun greet(name: String) {\n    val msg = \"Hello $name\"\n}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let param_type = type_id(&SemanticTokenType::PARAMETER);
    assert!(
        tokens
            .iter()
            .any(|&(line, _, _, tt, _)| line == 1 && tt == param_type),
        "Expected PARAMETER for '$name' in string template, got: {tokens:?}"
    );
}

#[test]
fn param_use_in_nested_function_captures_outer_param() {
    let src = "fun outer(x: Int) {\n    fun inner(y: Int) {\n        println(x)\n    }\n}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let param_type = type_id(&SemanticTokenType::PARAMETER);
    // outer's x used inside inner's body
    assert!(
        tokens
            .iter()
            .any(|&(line, col, _, tt, _)| line == 2 && col == 16 && tt == param_type),
        "Expected PARAMETER for outer 'x' captured in inner fn at 2:16, got: {tokens:?}"
    );
}

#[test]
fn decl_data_class_is_struct() {
    let src = "data class Point(val x: Int, val y: Int)\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let struct_type = type_id(&SemanticTokenType::STRUCT);
    assert!(
        tokens.iter().any(|t| t.3 == struct_type),
        "Expected STRUCT for data class, got: {tokens:?}"
    );
}

#[test]
fn decl_operator_fun() {
    let src = "class Vec {\n    operator fun plus(other: Vec): Vec = this\n}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let op_type = type_id(&SemanticTokenType::OPERATOR);
    assert!(
        tokens.iter().any(|t| t.3 == op_type),
        "Expected OPERATOR for operator fun, got: {tokens:?}"
    );
}

#[test]
fn decl_suspend_has_async_modifier() {
    let src = "suspend fun fetch() {}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let async_bit = 1u32 << 4; // ASYNC modifier
    assert!(
        tokens.iter().any(|t| t.4 & async_bit != 0),
        "Expected ASYNC modifier for suspend fun, got: {tokens:?}"
    );
}

#[test]
fn decl_abstract_has_abstract_modifier() {
    let src = "abstract class Base {\n    abstract fun compute(): Int\n}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let abstract_bit = 1u32 << 3; // ABSTRACT modifier
    assert!(
        tokens.iter().any(|t| t.4 & abstract_bit != 0),
        "Expected ABSTRACT modifier, got: {tokens:?}"
    );
}

#[test]
fn decl_class_property_vs_top_level_variable() {
    let src = "val top = 1\nclass C {\n    val member = 2\n}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let var_type = type_id(&SemanticTokenType::VARIABLE);
    let prop_type = type_id(&SemanticTokenType::PROPERTY);
    assert!(
        tokens.iter().any(|t| t.0 == 0 && t.3 == var_type),
        "Expected VARIABLE for top-level val, got: {tokens:?}"
    );
    assert!(
        tokens.iter().any(|t| t.0 == 2 && t.3 == prop_type),
        "Expected PROPERTY for class member val, got: {tokens:?}"
    );
}

// --- Reference-site tokens (requires indexer) ---

#[test]
fn ref_type_reference_in_return_type() {
    let src = "class User\nfun getUser(): User = User()\n";
    let uri = Url::parse("file:///ref_return.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let class_type = type_id(&SemanticTokenType::CLASS);
    // "User" return type at line 1, col 15
    assert_token_at(&tokens, 1, 15, class_type, "CLASS return type ref");
}

#[test]
fn ref_type_reference_in_parameter_type() {
    let src = "class Config\nfun load(cfg: Config) {}\n";
    let uri = Url::parse("file:///ref_param_type.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let class_type = type_id(&SemanticTokenType::CLASS);
    // "Config" at line 1, col 14
    assert_token_at(&tokens, 1, 14, class_type, "CLASS param type ref");
}

#[test]
fn ref_interface_type_reference() {
    let src = "interface Repo\nfun use(r: Repo) {}\n";
    let uri = Url::parse("file:///ref_iface.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let iface_type = type_id(&SemanticTokenType::INTERFACE);
    // "Repo" at line 1, col 11
    assert_token_at(&tokens, 1, 11, iface_type, "INTERFACE type ref");
}

#[test]
fn ref_enum_type_reference() {
    let src = "enum class Dir { UP }\nfun go(): Dir = Dir.UP\n";
    let uri = Url::parse("file:///ref_enum.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let enum_type = type_id(&SemanticTokenType::ENUM);
    // "Dir" return type at line 1, col 10
    assert_token_at(&tokens, 1, 10, enum_type, "ENUM type ref");
}

#[test]
fn ref_function_call_top_level() {
    let src = "fun helper() {}\nfun main() {\n    helper()\n}\n";
    let uri = Url::parse("file:///ref_call.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let fun_type = type_id(&SemanticTokenType::FUNCTION);
    // "helper" call at line 2, col 4
    assert_token_at(&tokens, 2, 4, fun_type, "FUNCTION call ref");
}

#[test]
fn ref_constructor_call_as_class() {
    let src = "class Item\nfun make() = Item()\n";
    let uri = Url::parse("file:///ref_ctor.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let class_type = type_id(&SemanticTokenType::CLASS);
    // "Item" constructor call at line 1, col 13
    assert_token_at(&tokens, 1, 13, class_type, "CLASS constructor call");
}

#[test]
fn ref_object_as_namespace() {
    let src = "object Utils { fun run() {} }\nfun main() { Utils.run() }\n";
    let uri = Url::parse("file:///ref_obj.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let ns_type = type_id(&SemanticTokenType::NAMESPACE);
    // "Utils" at line 1, col 13
    assert_token_at(&tokens, 1, 13, ns_type, "NAMESPACE object ref");
}

#[test]
fn ref_annotation_unresolved_still_decorator() {
    // Annotation class not in index — should still get DECORATOR
    let src = "@Composable\nfun Screen() {}\n";
    let uri = Url::parse("file:///ref_anno_unresolved.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let deco_type = type_id(&SemanticTokenType::DECORATOR);
    assert_token_at(&tokens, 0, 0, deco_type, "DECORATOR unresolved annotation");
}

#[test]
fn ref_enum_member_via_dot() {
    let src = "enum class Color { RED, GREEN }\nval c = Color.RED\n";
    let uri = Url::parse("file:///ref_enum_member.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let member_type = type_id(&SemanticTokenType::ENUM_MEMBER);
    // "RED" at line 1, col 14
    assert_token_at(&tokens, 1, 14, member_type, "ENUM_MEMBER dot access");
}

// --- Keyword tokens ---

#[test]
fn keyword_by_delegation() {
    let src = "interface I\nclass Impl : I\nclass Wrapper(i: I) : I by i\n";
    let uri = Url::parse("file:///kw_by.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let kw_type = type_id(&SemanticTokenType::KEYWORD);
    assert!(
        tokens.iter().any(|t| t.3 == kw_type),
        "Expected KEYWORD for 'by', got: {tokens:?}"
    );
}

#[test]
fn keyword_is_check() {
    let src = "fun check(x: Any) {\n    if (x is String) {}\n}\n";
    let uri = Url::parse("file:///kw_is.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let kw_type = type_id(&SemanticTokenType::KEYWORD);
    assert!(
        tokens.iter().any(|t| t.3 == kw_type),
        "Expected KEYWORD for 'is', got: {tokens:?}"
    );
}

#[test]
fn keyword_as_cast() {
    let src = "fun cast(x: Any) {\n    val s = x as String\n}\n";
    let uri = Url::parse("file:///kw_as.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let kw_type = type_id(&SemanticTokenType::KEYWORD);
    assert!(
        tokens.iter().any(|t| t.3 == kw_type),
        "Expected KEYWORD for 'as', got: {tokens:?}"
    );
}

#[test]
fn keyword_in_loop() {
    let src = "fun loop() {\n    for (i in 1..10) {}\n}\n";
    let uri = Url::parse("file:///kw_in.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let kw_type = type_id(&SemanticTokenType::KEYWORD);
    assert!(
        tokens.iter().any(|t| t.3 == kw_type),
        "Expected KEYWORD for 'in', got: {tokens:?}"
    );
}

#[test]
fn keyword_constructor() {
    let src = "class Foo @Inject constructor(val x: Int)\n";
    let uri = Url::parse("file:///kw_ctor.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let kw_type = type_id(&SemanticTokenType::KEYWORD);
    assert!(
        tokens.iter().any(|t| t.3 == kw_type),
        "Expected KEYWORD for 'constructor', got: {tokens:?}"
    );
}

// --- Named argument labels ---

#[test]
fn named_arg_simple_call() {
    let src = "fun foo(x: Int) {}\nfun main() { foo(x = 1) }\n";
    let uri = Url::parse("file:///na_simple.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let param_type = type_id(&SemanticTokenType::PARAMETER);
    // "x" named arg at line 1, col 17
    assert_token_at(&tokens, 1, 17, param_type, "PARAMETER named arg label");
}

#[test]
fn named_arg_not_emitted_for_positional() {
    let src = "fun foo(x: Int) {}\nfun main() { foo(42) }\n";
    let uri = Url::parse("file:///na_positional.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    // No PARAMETER token on line 1 for positional arg "42"
    let param_type = type_id(&SemanticTokenType::PARAMETER);
    assert!(
        !tokens
            .iter()
            .any(|t| t.0 == 1 && t.1 == 17 && t.3 == param_type),
        "Should NOT emit PARAMETER for positional arg, got: {tokens:?}"
    );
}

#[test]
fn named_arg_multiple_in_one_call() {
    let src = "fun f(a: Int, b: Int) {}\nfun main() { f(a = 1, b = 2) }\n";
    let uri = Url::parse("file:///na_multi.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let param_type = type_id(&SemanticTokenType::PARAMETER);
    // "a" at line 1, col 15; "b" at line 1, col 22
    assert_token_at(&tokens, 1, 15, param_type, "PARAMETER first named arg 'a'");
    assert_token_at(&tokens, 1, 22, param_type, "PARAMETER second named arg 'b'");
}

// --- Member access (Tier 2) ---

#[test]
fn ref_method_call_on_receiver() {
    let src = "class Svc { fun run() {} }\nfun use(s: Svc) {\n    s.run()\n}\n";
    let uri = Url::parse("file:///ref_method_call.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let method_type = type_id(&SemanticTokenType::METHOD);
    // "run" at line 2, col 6
    assert_token_at(&tokens, 2, 6, method_type, "METHOD call on receiver");
}

#[test]
fn ref_property_access_on_receiver() {
    let src = "class Box { val value: Int = 0 }\nfun use(b: Box) {\n    b.value\n}\n";
    let uri = Url::parse("file:///ref_prop_access.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let prop_type = type_id(&SemanticTokenType::PROPERTY);
    // "value" at line 2, col 6
    assert_token_at(&tokens, 2, 6, prop_type, "PROPERTY access on receiver");
}

// --- Extension functions ---

#[test]
fn decl_extension_function_receiver_type_colored() {
    // The receiver type "String" in the declaration should get CLASS token
    let src = "fun String.capitalize(): String = this\n";
    let uri = Url::parse("file:///ext_decl.kt").unwrap();
    let indexer = Indexer::new();
    indexer.index_content(&uri, src);
    let doc = parse_kotlin(src);
    let tokens = decode_all_indexed(&indexer, &uri, &doc, Language::Kotlin);
    let class_type = type_id(&SemanticTokenType::CLASS);
    // "String" receiver at col 4 — classified as CLASS (unresolved type ref fallback)
    assert!(
        tokens
            .iter()
            .any(|t| t.0 == 0 && t.1 == 4 && t.3 == class_type),
        "Expected CLASS for extension receiver type 'String', got: {tokens:?}"
    );
}

// --- byte_col_to_utf16 correctness ---

#[test]
fn single_line_columns_correct() {
    let src = "fun f(x: Int) {}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    // "f" at col 4, "x" at col 6
    let fun_type = type_id(&SemanticTokenType::FUNCTION);
    let param_type = type_id(&SemanticTokenType::PARAMETER);
    assert_token_at(&tokens, 0, 4, fun_type, "FUNCTION 'f' at col 4");
    assert_token_at(&tokens, 0, 6, param_type, "PARAMETER 'x' at col 6");
}

#[test]
fn multiline_columns_correct() {
    let src = "class A\nclass B\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let class_type = type_id(&SemanticTokenType::CLASS);
    assert_token_at(&tokens, 0, 6, class_type, "CLASS 'A' at line 0 col 6");
    assert_token_at(&tokens, 1, 6, class_type, "CLASS 'B' at line 1 col 6");
}

// ─── Modifier coverage ────────────────────────────────────────────────────────

#[test]
fn companion_object_has_static_modifier() {
    // companion object itself gets NAMESPACE + STATIC|DECLARATION
    let src = "class Foo {\n    companion object {}\n}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let ns_type = type_id(&SemanticTokenType::NAMESPACE);
    let static_bit = 1u32 << 2;
    let decl_bit = 1u32 << 0;
    let companion = tokens.iter().find(|&&(_, _, _, tt, _)| tt == ns_type);
    assert!(
        companion.is_some(),
        "expected NAMESPACE token for companion object"
    );
    let (_, _, _, _, mods) = *companion.unwrap();
    assert_ne!(
        mods & static_bit,
        0,
        "companion object should have STATIC modifier, mods={mods:#b}"
    );
    assert_ne!(
        mods & decl_bit,
        0,
        "companion object should have DECLARATION modifier, mods={mods:#b}"
    );
}

#[test]
fn java_field_final_is_property_readonly() {
    let src = "class Foo { private final int count = 0; }\n";
    let doc = parse_java(src);
    let tokens = decode_all(&doc, Language::Java);
    let prop_type = type_id(&SemanticTokenType::PROPERTY);
    let readonly_bit = 1u32 << 1;
    let decl_bit = 1u32 << 0;
    // "count" at col 30
    let field = tokens
        .iter()
        .find(|&&(_, col, _, tt, _)| col == 30 && tt == prop_type);
    assert!(
        field.is_some(),
        "expected PROPERTY for 'count', tokens: {tokens:?}"
    );
    let (_, _, _, _, mods) = *field.unwrap();
    assert_ne!(
        mods & readonly_bit,
        0,
        "final field should have READONLY modifier, mods={mods:#b}"
    );
    assert_ne!(
        mods & decl_bit,
        0,
        "field should have DECLARATION modifier, mods={mods:#b}"
    );
}

#[test]
fn java_field_static_has_static_modifier() {
    let src = "class Foo { static int count = 0; }\n";
    let doc = parse_java(src);
    let tokens = decode_all(&doc, Language::Java);
    let prop_type = type_id(&SemanticTokenType::PROPERTY);
    let static_bit = 1u32 << 2;
    let field = tokens.iter().find(|&&(_, _, _, tt, _)| tt == prop_type);
    assert!(field.is_some(), "expected PROPERTY for 'count'");
    let (_, _, _, _, mods) = *field.unwrap();
    assert_ne!(
        mods & static_bit,
        0,
        "static field should have STATIC modifier, mods={mods:#b}"
    );
}

#[test]
fn java_field_non_final_non_static_has_no_readonly_static() {
    let src = "class Foo { int count = 0; }\n";
    let doc = parse_java(src);
    let tokens = decode_all(&doc, Language::Java);
    let prop_type = type_id(&SemanticTokenType::PROPERTY);
    let readonly_bit = 1u32 << 1;
    let static_bit = 1u32 << 2;
    let field = tokens.iter().find(|&&(_, _, _, tt, _)| tt == prop_type);
    assert!(field.is_some(), "expected PROPERTY for 'count'");
    let (_, _, _, _, mods) = *field.unwrap();
    assert_eq!(
        mods & readonly_bit,
        0,
        "mutable field should NOT have READONLY, mods={mods:#b}"
    );
    assert_eq!(
        mods & static_bit,
        0,
        "instance field should NOT have STATIC, mods={mods:#b}"
    );
}

#[test]
fn deprecated_modifier_not_set_without_annotation() {
    // Negative test: regular function should never get DEPRECATED
    let src = "fun normal(): Unit {}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let deprecated_bit = 1u32 << 5;
    for &(_, _, _, _, mods) in &tokens {
        assert_eq!(
            mods & deprecated_bit,
            0,
            "DEPRECATED should not be set without @Deprecated, mods={mods:#b}"
        );
    }
}

#[test]
fn deprecated_modifier_set_on_kotlin_declarations() {
    let src = "@Deprecated(\"x\") class OldClass\n@Deprecated(\"x\")\nfun oldFun() {}\n@Deprecated(\"x\")\nobject OldObject\n@Deprecated(\"x\") val oldValue = 1\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let deprecated_bit = 1u32 << 5;
    let class_type = type_id(&SemanticTokenType::CLASS);
    let namespace_type = type_id(&SemanticTokenType::NAMESPACE);
    let function_type = type_id(&SemanticTokenType::FUNCTION);
    let variable_type = type_id(&SemanticTokenType::VARIABLE);

    for token_type in [class_type, namespace_type, function_type, variable_type] {
        let token = tokens
            .iter()
            .find(|&&(_, _, _, actual_type, _)| actual_type == token_type)
            .expect("expected declaration token");
        assert_ne!(
            token.4 & deprecated_bit,
            0,
            "expected DEPRECATED modifier, token={token:?}, all={tokens:?}"
        );
    }
}

#[test]
fn companion_members_have_static_modifier() {
    let src = "class Foo {\n    companion object {\n        fun make() = Foo()\n        val value = 1\n    }\n}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let static_bit = 1u32 << 2;
    let method_type = type_id(&SemanticTokenType::METHOD);
    let property_type = type_id(&SemanticTokenType::PROPERTY);

    let method = tokens
        .iter()
        .find(|&&(line, _, _, token_type, _)| line == 2 && token_type == method_type)
        .expect("expected METHOD token for companion member");
    assert_ne!(
        method.4 & static_bit,
        0,
        "companion method should have STATIC modifier, token={method:?}, all={tokens:?}"
    );

    let property = tokens
        .iter()
        .find(|&&(line, _, _, token_type, _)| line == 3 && token_type == property_type)
        .expect("expected PROPERTY token for companion member");
    assert_ne!(
        property.4 & static_bit,
        0,
        "companion property should have STATIC modifier, token={property:?}, all={tokens:?}"
    );
}

#[test]
fn abstract_class_method_both_abstract() {
    let src = "abstract class Base {\n    abstract fun doIt()\n}\n";
    let doc = parse_kotlin(src);
    let tokens = decode_all(&doc, Language::Kotlin);
    let class_type = type_id(&SemanticTokenType::CLASS);
    let method_type = type_id(&SemanticTokenType::METHOD);
    let abstract_bit = 1u32 << 3;
    let decl_bit = 1u32 << 0;
    // class Base
    let cls = tokens.iter().find(|&&(_, _, _, tt, _)| tt == class_type);
    assert!(cls.is_some(), "expected CLASS token");
    let (_, _, _, _, class_mods) = *cls.unwrap();
    assert_ne!(
        class_mods & abstract_bit,
        0,
        "abstract class should have ABSTRACT, mods={class_mods:#b}"
    );
    // fun doIt
    let method = tokens.iter().find(|&&(_, _, _, tt, _)| tt == method_type);
    assert!(method.is_some(), "expected METHOD token");
    let (_, _, _, _, method_mods) = *method.unwrap();
    assert_ne!(
        method_mods & abstract_bit,
        0,
        "abstract fun should have ABSTRACT, mods={method_mods:#b}"
    );
    assert_ne!(
        method_mods & decl_bit,
        0,
        "method should have DECLARATION, mods={method_mods:#b}"
    );
}
