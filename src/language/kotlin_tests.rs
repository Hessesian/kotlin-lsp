use tower_lsp::lsp_types::SymbolKind;

use crate::language::LanguageParser;
use crate::language::kotlin::KotlinParser;

#[test]
fn language_id_is_kotlin() {
    assert_eq!(KotlinParser.language_id(), "kotlin");
}

#[test]
fn extensions_include_kt_and_kts() {
    let exts = KotlinParser.file_extensions();
    assert!(exts.contains(&"kt"));
    assert!(exts.contains(&"kts"));
}

#[test]
fn parse_extracts_class_symbol() {
    let data = KotlinParser.parse("class Foo");
    assert!(data.symbols.iter().any(|s| s.name == "Foo"));
}

#[test]
fn parse_extracts_function_symbol() {
    let data = KotlinParser.parse("fun bar() {}");
    assert!(data.symbols.iter().any(|s| s.name == "bar"));
}

#[test]
fn symbol_keyword_function_is_fun() {
    assert_eq!(KotlinParser.symbol_keyword(SymbolKind::FUNCTION), "fun");
}

#[test]
fn symbol_keyword_method_is_fun() {
    assert_eq!(KotlinParser.symbol_keyword(SymbolKind::METHOD), "fun");
}

#[test]
fn symbol_keyword_class_is_class() {
    assert_eq!(KotlinParser.symbol_keyword(SymbolKind::CLASS), "class");
}
