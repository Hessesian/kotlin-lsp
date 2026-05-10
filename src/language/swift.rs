//! Swift language provider.

use tower_lsp::lsp_types::SymbolKind;

use super::LanguageParser;
use crate::indexer::lookup::symbol_kw;
use crate::types::FileData;

pub(crate) struct SwiftParser;

impl LanguageParser for SwiftParser {
    fn language_id(&self) -> &'static str {
        "swift"
    }

    fn file_extensions(&self) -> &[&'static str] {
        &["swift"]
    }

    fn parse(&self, source: &str) -> FileData {
        crate::parser::parse_swift(source)
    }

    fn symbol_keyword(&self, kind: SymbolKind) -> &'static str {
        let kw = symbol_kw(kind);
        // Swift uses `func` and `let` instead of Kotlin's `fun` and `val`.
        match kw {
            "fun" => "func",
            "val" => "let",
            other => other,
        }
    }
}

#[cfg(test)]
#[path = "swift_tests.rs"]
mod tests;
