// Unified resolution service for symbol lookup, substitution, and extraction.
// Phase 2: Core `resolve_symbol_info` pipeline implementation.

use std::collections::HashMap;
use std::sync::Arc;
use tower_lsp::lsp_types::{Url, SymbolKind};

use crate::indexer::Location;
use crate::resolver::ReceiverType;
use crate::types::{FileData, SymbolEntry};
use crate::LinesExt;
use crate::indexer::doc::extract_doc_comment;

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
    pub fn goto_def() -> Self { Self { allow_rg: true, include_doc: false, apply_subst: false } }
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
    fn get_file_data(&self, uri: &str) -> Option<Arc<FileData>>;

    /// Resolve definition locations for `name` with qualifier and import context.
    /// Default implementation uses the global definitions map (no import awareness).
    /// Production `Indexer` overrides this with the full resolver.
    fn resolve_locations(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
        allow_rg: bool,
    ) -> Vec<Location> {
        let _ = (qualifier, from_uri, allow_rg);
        self.get_definitions(name)
            .unwrap_or_default()
    }
}

// ─── Pipeline Entry Point (thin coordinator) ───────────────────────────────

/// Core resolution pipeline: locate → load → enrich → substitute → extract.
/// Thin coordinator that delegates to pure functions and trait methods.
pub fn resolve_symbol_info<I: IndexRead>(
    index: &I,
    name: &str,
    qualifier: Option<&str>,
    from_uri: &Url,
    subst_ctx: SubstitutionContext<'_>,
    options: &ResolveOptions,
) -> Option<ResolvedSymbol> {
    let location = locate_symbol(index, name, qualifier, from_uri, options.allow_rg)?;
    let data = index.get_file_data(location.uri.as_str())?;
    enrich_symbol(index, &data, &location, name, subst_ctx, options)
}

/// Resolve contextual receiver information (it/this).
pub fn resolve_contextual_info<I: IndexRead>(
    index: &I,
    rt: &ReceiverType,
    from_uri: &Url,
    cursor_line: u32,
    options: &ResolveOptions,
) -> Option<ResolvedSymbol> {
    resolve_symbol_info(
        index,
        &rt.leaf,
        None,
        from_uri,
        SubstitutionContext::EnclosingClass {
            uri: from_uri.as_str(),
            cursor_line,
        },
        options,
    )
}

/// Build substitution map for enclosing class at cursor position.
pub fn build_subst_map<I: IndexRead>(index: &I, uri: &str, cursor_line: u32) -> HashMap<String, String> {
    build_enclosing_class_subst_impl(index, uri, cursor_line)
}

// ─── Pure Data Transformation Functions ──────────────────────────────────

/// Extract canonical signature (prefer cached detail, fall back to source).
fn extract_canonical_signature(sym: &SymbolEntry, data: &FileData) -> String {
    if !sym.detail.is_empty() {
        sym.detail.clone()
    } else {
        data.lines.collect_signature(sym.selection_range.start.line as usize)
    }
}

/// Apply type-parameter substitution to a signature string.
fn apply_subst(sig: &str, subst: &HashMap<String, String>) -> String {
    super::apply_type_subst(sig, subst)
}

// ─── Glue Functions (coordinate I/O + data transformation) ──────────────────

/// Locate first definition of a symbol using import-aware resolution.
fn locate_symbol<I: IndexRead>(
    index: &I,
    name: &str,
    qualifier: Option<&str>,
    from_uri: &Url,
    allow_rg: bool,
) -> Option<Location> {
    index.resolve_locations(name, qualifier, from_uri, allow_rg).into_iter().next()
}

/// Find SymbolEntry in FileData by range or name.
fn find_symbol_entry<'a>(data: &'a FileData, location: &Location, name: &str) -> Option<&'a SymbolEntry> {
    data.symbols
        .iter()
        .find(|s| s.selection_range == location.range)
        .or_else(|| data.symbols.iter().find(|s| s.name == name))
}

/// Enrich symbol with signature, substitution, and docs.
fn enrich_symbol<I: IndexRead>(
    index: &I,
    data: &FileData,
    location: &Location,
    name: &str,
    subst_ctx: SubstitutionContext<'_>,
    options: &ResolveOptions,
) -> Option<ResolvedSymbol> {
    let sym = find_symbol_entry(data, location, name)?;

    let raw_signature = extract_canonical_signature(sym, data);
    let subst = build_subst_if_needed(index, location, sym, &raw_signature, subst_ctx, options);
    let signature = apply_subst(&raw_signature, &subst);
    let doc = if options.include_doc {
        extract_doc_comment(&data.lines, sym.selection_range.start.line as usize).unwrap_or_default()
    } else {
        String::new()
    };

    Some(ResolvedSymbol {
        location: location.clone(),
        kind: sym.kind,
        raw_signature,
        signature,
        subst,
        doc,
    })
}

/// Build substitution map if requested by options and context.
fn build_subst_if_needed<I: IndexRead>(
    index: &I,
    location: &Location,
    _sym: &SymbolEntry,
    _raw_sig: &str,
    subst_ctx: SubstitutionContext<'_>,
    options: &ResolveOptions,
) -> HashMap<String, String> {
    if !options.apply_subst {
        return HashMap::new();
    }

    match subst_ctx {
        SubstitutionContext::None => HashMap::new(),
        SubstitutionContext::CrossFile { calling_uri } => {
            build_type_param_subst_impl(index, location.uri.as_str(), location.range.start.line, calling_uri)
        }
        SubstitutionContext::EnclosingClass { uri, cursor_line } => {
            build_enclosing_class_subst_impl(index, uri, cursor_line)
        }
        SubstitutionContext::Precomputed(m) => m.clone(),
    }
}

// ─── Substitution Builders (coordinate I/O + pure logic) ────────────────────

/// Build type-parameter substitution for cross-file lookup.
fn build_type_param_subst_impl<I: IndexRead>(
    index: &I,
    sym_uri: &str,
    sym_line: u32,
    calling_uri: &str,
) -> HashMap<String, String> {
    if sym_uri == calling_uri {
        return HashMap::new();
    }

    let sym_data = match index.get_file_data(sym_uri) {
        Some(d) => d,
        None => return HashMap::new(),
    };

    let container_name = match find_containing_class_name(&sym_data, sym_line) {
        Some(n) => n,
        None => return HashMap::new(),
    };

    let container_sym = match sym_data.symbols.iter().find(|s| s.name == container_name) {
        Some(s) => s,
        None => return HashMap::new(),
    };

    let type_params = &container_sym.type_params;
    if type_params.is_empty() { return HashMap::new(); }

    let calling_data = match index.get_file_data(calling_uri) {
        Some(d) => d,
        None => return HashMap::new(),
    };
    let type_args = calling_data.supers.iter()
        .find(|(_, base, _)| base == &container_name)
        .map(|(_, _, args)| args.clone())
        .unwrap_or_default();

    if type_args.is_empty() { return HashMap::new(); }

    type_params.iter().zip(type_args.iter())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Build substitution for enclosing class's type parameters.
fn build_enclosing_class_subst_impl<I: IndexRead>(
    index: &I,
    uri: &str,
    cursor_line: u32,
) -> HashMap<String, String> {
    let data = match index.get_file_data(uri) {
        Some(d) => d,
        None => return HashMap::new(),
    };

    let class_name = match find_containing_class_name(&data, cursor_line) {
        Some(n) => n,
        None => return HashMap::new(),
    };

    let class_sym = match data.symbols.iter().find(|s| s.name == class_name) {
        Some(s) => s,
        None => return HashMap::new(),
    };

    let type_params = &class_sym.type_params;
    if type_params.is_empty() { return HashMap::new(); }

    let class_line = class_sym.selection_range.start.line;

    // Collect all supers of this class, zipping their type args against the class's type params.
    let mut result = HashMap::new();
    for (line, _base_name, type_args) in data.supers.iter() {
        if *line != class_line || type_args.is_empty() {
            continue;
        }
        for (param, arg) in type_params.iter().zip(type_args.iter()) {
            result.entry(param.clone()).or_insert_with(|| arg.clone());
        }
    }
    result
}

// ─── Helper: Find Enclosing Class ────────────────────────────────────────────

/// Find the name of the innermost class that contains a symbol at the given line.
fn find_containing_class_name(data: &FileData, sym_line: u32) -> Option<String> {
    data.symbols
        .iter()
        .filter(|s| s.range.start.line <= sym_line && sym_line <= s.range.end.line)
        .filter(|s| matches!(s.kind, SymbolKind::CLASS | SymbolKind::INTERFACE | SymbolKind::STRUCT | SymbolKind::ENUM | SymbolKind::OBJECT))
        .min_by_key(|s| s.range.end.line.saturating_sub(s.range.start.line))
        .map(|s| s.name.clone())
}

// ─── Indexer impl (production) ───────────────────────────────────────────────

// Implement IndexRead for Indexer: production code doesn't use the trait,
// but this enables unit tests to use a TestIndex stub.
impl IndexRead for super::Indexer {
    fn get_file_lines(&self, uri: &str) -> Option<Vec<String>> {
        self.files
            .get(uri)
            .map(|rf| rf.lines.as_ref().as_slice().to_vec())
    }

    fn get_definitions(&self, name: &str) -> Option<Vec<Location>> {
        self.definitions.get(name).map(|rf| rf.clone())
    }

    fn get_file_data(&self, uri: &str) -> Option<Arc<FileData>> {
        self.files.get(uri).map(|rf| rf.clone())
    }

    fn resolve_locations(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
        allow_rg: bool,
    ) -> Vec<Location> {
        if allow_rg {
            self.resolve_symbol(name, qualifier, from_uri)
        } else {
            self.resolve_symbol_no_rg(name, from_uri)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Url;

    struct TestIndex;
    impl IndexRead for TestIndex {
        fn get_file_lines(&self, _uri: &str) -> Option<Vec<String>> { None }
        fn get_definitions(&self, _name: &str) -> Option<Vec<Location>> { None }
        fn get_file_data(&self, _uri: &str) -> Option<Arc<FileData>> { None }
    }

    #[test]
    fn stub_resolve_returns_none() {
        let idx = TestIndex;
        let res = resolve_symbol_info(&idx, "Foo", None, &Url::parse("file:///x").unwrap(), SubstitutionContext::None, &ResolveOptions::hover());
        assert!(res.is_none());
    }

    #[test]
    fn apply_subst_replaces_identifiers() {
        let mut subst = HashMap::new();
        subst.insert("T".to_string(), "String".to_string());
        subst.insert("U".to_string(), "Int".to_string());
        let sig = "fun foo(x: T, y: U): T";
        let result = apply_subst(sig, &subst);
        assert_eq!(result, "fun foo(x: String, y: Int): String");
    }

    fn make_range(start_line: u32, end_line: u32) -> tower_lsp::lsp_types::Range {
        use tower_lsp::lsp_types::Position;
        tower_lsp::lsp_types::Range {
            start: Position { line: start_line, character: 0 },
            end:   Position { line: end_line,   character: 0 },
        }
    }

    fn make_sym(name: &str, kind: SymbolKind, start_line: u32, end_line: u32) -> SymbolEntry {
        use crate::types::Visibility;
        SymbolEntry {
            name:            name.to_owned(),
            kind,
            visibility:      Visibility::Public,
            range:           make_range(start_line, end_line),
            selection_range: make_range(start_line, start_line),
            detail:          String::new(),
            type_params:     Vec::new(),
        }
    }

    #[test]
    fn find_containing_class_returns_innermost() {
        use crate::types::FileData;
        let data = FileData {
            symbols: vec![
                make_sym("Outer", SymbolKind::CLASS, 0, 20),
                make_sym("Inner", SymbolKind::CLASS, 5, 15),
            ],
            ..Default::default()
        };
        // Line 7 is inside both Outer (0-20) and Inner (5-15); should return Inner.
        let result = find_containing_class_name(&data, 7);
        assert_eq!(result.as_deref(), Some("Inner"));
    }

    #[test]
    fn find_containing_class_returns_none_for_top_level() {
        use crate::types::FileData;
        let data = FileData {
            symbols: vec![make_sym("Outer", SymbolKind::CLASS, 5, 15)],
            ..Default::default()
        };
        // Line 1 is outside any class.
        let result = find_containing_class_name(&data, 1);
        assert!(result.is_none());
    }

    #[test]
    fn find_containing_class_includes_enum_and_object() {
        use crate::types::FileData;
        let data = FileData {
            symbols: vec![
                make_sym("MyEnum", SymbolKind::ENUM, 0, 10),
                make_sym("MyObject", SymbolKind::OBJECT, 12, 20),
            ],
            ..Default::default()
        };
        assert_eq!(find_containing_class_name(&data, 5).as_deref(), Some("MyEnum"));
        assert_eq!(find_containing_class_name(&data, 15).as_deref(), Some("MyObject"));
    }
}
