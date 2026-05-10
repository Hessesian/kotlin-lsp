//! Language provider traits and implementations.
//!
//! Each supported language implements [`LanguageParser`], which is the single
//! dispatch point for language-specific parsing and keyword behaviour.
//! Use [`crate::Language::parser()`] to get the provider for a given file.

pub(crate) mod java;
pub(crate) mod kotlin;
pub(crate) mod swift;

use tower_lsp::lsp_types::SymbolKind;

use crate::types::FileData;

/// Language-specific capabilities exposed to the rest of the codebase.
///
/// Implementations are stateless singletons; tree-sitter parser state and query
/// caches remain in module-level statics inside `parser.rs`.
pub(crate) trait LanguageParser: Send + Sync {
    /// LSP language identifier string (e.g. `"kotlin"`, `"java"`, `"swift"`).
    fn language_id(&self) -> &'static str;

    /// File extensions handled by this provider (without leading dot).
    fn file_extensions(&self) -> &[&'static str];

    /// Parse `source` and return extracted symbols, imports, supertypes, etc.
    ///
    /// Delegates to the existing `parse_kotlin` / `parse_java` / `parse_swift`
    /// free functions — no logic has moved, this is purely dispatch unification.
    fn parse(&self, source: &str) -> FileData;

    /// Return the declaration keyword for `kind` in this language.
    ///
    /// Examples: Kotlin/Java → `"fun"`, Swift → `"func"`;
    ///           Kotlin/Java → `"val"`, Swift → `"let"`.
    fn symbol_keyword(&self, kind: SymbolKind) -> &'static str;
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
