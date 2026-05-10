use tower_lsp::lsp_types::SymbolKind;

use crate::language::LanguageParser;
use crate::language::swift::SwiftParser;

#[test]
fn language_id_is_swift() {
    assert_eq!(SwiftParser.language_id(), "swift");
}

#[test]
fn extensions_include_swift() {
    assert!(SwiftParser.file_extensions().contains(&"swift"));
}

#[test]
fn parse_extracts_class_symbol() {
    let data = SwiftParser.parse("class Baz {}");
    assert!(data.symbols.iter().any(|s| s.name == "Baz"));
}

#[test]
fn symbol_keyword_function_is_func() {
    assert_eq!(SwiftParser.symbol_keyword(SymbolKind::FUNCTION), "func");
}

#[test]
fn symbol_keyword_method_is_func() {
    assert_eq!(SwiftParser.symbol_keyword(SymbolKind::METHOD), "func");
}

#[test]
fn symbol_keyword_variable_is_val_not_let() {
    // `val` maps to "let" for Swift.
    assert_eq!(SwiftParser.symbol_keyword(SymbolKind::CONSTANT), "let");
}

#[test]
fn symbol_keyword_class_unchanged() {
    assert_eq!(SwiftParser.symbol_keyword(SymbolKind::CLASS), "class");
}
