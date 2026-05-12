use tower_lsp::lsp_types::SymbolKind;

use crate::language::java::JavaParser;
use crate::language::LanguageParser;

#[test]
fn language_id_is_java() {
    assert_eq!(JavaParser.language_id(), "java");
}

#[test]
fn extensions_include_java() {
    assert!(JavaParser.file_extensions().contains(&"java"));
}

#[test]
fn parse_extracts_class_symbol() {
    let data = JavaParser.parse("public class Bar {}");
    assert!(data.symbols.iter().any(|s| s.name == "Bar"));
}

#[test]
fn symbol_keyword_function_is_fun() {
    // Java provider inherits Kotlin-style keywords; Swift is the only deviation.
    assert_eq!(JavaParser.symbol_keyword(SymbolKind::FUNCTION), "fun");
}

#[test]
fn symbol_keyword_class_is_class() {
    assert_eq!(JavaParser.symbol_keyword(SymbolKind::CLASS), "class");
}
