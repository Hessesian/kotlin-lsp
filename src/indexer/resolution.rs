// Unified resolution service for symbol lookup, substitution, and extraction.
// Phase 1: types, traits and stubs. Will be filled in incrementally.

use std::collections::HashMap;
use url::Url;

use crate::types::{Location, SymbolKind, ReceiverType};

/// Domain-level resolution result. Small, owned data suitable for LSP adapters.
pub struct ResolvedSymbol {
    pub location: Location,
    pub kind: SymbolKind,
    pub raw_signature: String,
    pub signature: String,
    pub subst: HashMap<String, String>,
    pub doc: String,
}

/// Options controlling resolution behaviour and allowed fallbacks.
pub struct ResolveOptions {
    pub allow_rg: bool,
    pub include_doc: bool,
    pub apply_subst: bool,
}

impl ResolveOptions {
    pub fn hover() -> Self { Self { allow_rg: true, include_doc: true, apply_subst: true } }
    pub fn inlay() -> Self { Self { allow_rg: false, include_doc: false, apply_subst: true } }
    pub fn completion() -> Self { Self { allow_rg: false, include_doc: true, apply_subst: true } }
}

/// Substitution context used by the pipeline.
pub enum SubstitutionContext<'a> {
    None,
    CrossFile { calling_uri: &'a str },
    EnclosingClass { uri: &'a str, cursor_line: u32 },
    Precomputed(&'a HashMap<String, String>),
}

/// Test seam trait: read-only view into index state. Keep this lightweight for tests.
pub trait IndexRead {
    fn get_file_lines(&self, uri: &str) -> Option<Vec<String>>;
    fn get_definitions(&self, name: &str) -> Option<Vec<Location>>;
}

/// Resolve a named symbol from a cursor position. Returns None when not found.
pub fn resolve_symbol_info<I: IndexRead>(
    _index: &I,
    _name: &str,
    _qualifier: Option<&str>,
    _from_uri: &Url,
    _subst_ctx: SubstitutionContext<'_>,
    _options: &ResolveOptions,
) -> Option<ResolvedSymbol> {
    // Stub: implementation will be added in next phases.
    None
}

/// Resolve contextual receiver information (it/this).
pub fn resolve_contextual_info<I: IndexRead>(
    _index: &I,
    _rt: &ReceiverType,
    _from_uri: &Url,
    _cursor_line: u32,
    _options: &ResolveOptions,
) -> Option<ResolvedSymbol> {
    None
}

/// Build substitution map for enclosing class (wrapper stub).
pub fn build_subst_map<I: IndexRead>(_index: &I, _uri: &str, _cursor_line: u32) -> HashMap<String, String> {
    HashMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    struct TestIndex;
    impl IndexRead for TestIndex {
        fn get_file_lines(&self, _uri: &str) -> Option<Vec<String>> { None }
        fn get_definitions(&self, _name: &str) -> Option<Vec<Location>> { None }
    }

    #[test]
    fn stub_resolve_returns_none() {
        let idx = TestIndex;
        let res = resolve_symbol_info(&idx, "Foo", None, &Url::parse("file:///x").unwrap(), SubstitutionContext::None, &ResolveOptions::hover());
        assert!(res.is_none());
    }
}
