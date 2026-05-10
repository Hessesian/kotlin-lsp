//! Java language provider.

use tower_lsp::lsp_types::SymbolKind;

use super::LanguageParser;
use crate::indexer::lookup::symbol_kw;
use crate::types::FileData;

pub(crate) struct JavaParser;

impl LanguageParser for JavaParser {
    fn language_id(&self) -> &'static str {
        "java"
    }

    fn file_extensions(&self) -> &[&'static str] {
        &["java"]
    }

    fn parse(&self, source: &str) -> FileData {
        crate::parser::parse_java(source)
    }

    fn symbol_keyword(&self, kind: SymbolKind) -> &'static str {
        symbol_kw(kind)
    }
}

#[cfg(test)]
#[path = "java_tests.rs"]
mod tests;
