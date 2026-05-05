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
    CrossFile { calling_uri: &'a str, cursor_line: u32 },
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
    // For qualified types like `Outer.Inner`, pass `outer` as qualifier so the
    // resolver can narrow to the right file before searching for `leaf`.
    let qualifier = if rt.leaf != rt.qualified { Some(rt.outer.as_str()) } else { None };
    resolve_symbol_info(
        index,
        &rt.leaf,
        qualifier,
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

/// Extract canonical signature (prefer full source, fall back to cached detail).
///
/// `detail` is truncated to ~120 chars at index time; `collect_signature` reads
/// the original source lines and returns the complete declaration up to `{`/`=`.
/// We prefer the full source version so callers see untruncated signatures.
fn extract_canonical_signature(sym: &SymbolEntry, data: &FileData) -> String {
    let full = data.lines.collect_signature(sym.selection_range.start.line as usize);
    if !full.is_empty() { full } else { sym.detail.clone() }
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
        SubstitutionContext::CrossFile { calling_uri, cursor_line } => {
            build_type_param_subst_impl(index, location.uri.as_str(), location.range.start.line, calling_uri, cursor_line)
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
    caller_cursor_line: u32,
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

    // Use `caller_cursor_line` to identify the specific calling class — when multiple
    // classes in the same file extend the same base with different type args,
    // this ensures we pick the correct substitution for the caller.
    let calling_class_line = find_containing_class_name(&calling_data, caller_cursor_line)
        .and_then(|name| calling_data.symbols.iter().find(|s| s.name == name))
        .map(|s| s.selection_range.start.line);

    let type_args = calling_data.supers.iter()
        .find(|(line, base, _)| {
            base == &container_name
                && calling_class_line.is_none_or(|class_line| *line == class_line)
        })
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

    let class_line = class_sym.selection_range.start.line;
    let class_end_line = class_sym.range.end.line;

    // For each supertype with concrete type args, look up the BASE class's own
    // type parameters (e.g., `[T, U]` from `class Base<T, U>`), then zip with
    // the concrete args (e.g., `[Event, State]`) to build `{T→Event, U→State}`.
    // Using the enclosing class's own type_params here would be wrong: for
    // `class Child : Base<Event, State>`, Child itself has no type params.
    let mut result = HashMap::new();
    for (line, base_name, type_args) in data.supers.iter() {
        if *line != class_line || type_args.is_empty() {
            continue;
        }

        let base_type_params: Vec<String> = index
            .get_definitions(base_name)
            .and_then(|locs| locs.into_iter().next())
            .and_then(|loc| {
                index.get_file_data(loc.uri.as_str()).and_then(|base_data| {
                    base_data.symbols
                        .iter()
                        .find(|s| s.name == *base_name)
                        .map(|s| s.type_params.clone())
                })
            })
            .unwrap_or_default();

        if base_type_params.is_empty() {
            continue;
        }
        for (param, arg) in base_type_params.iter().zip(type_args.iter()) {
            result.entry(param.clone()).or_insert_with(|| arg.clone());
        }
    }
    // Phase 2: collect substitutions from member property types.
    // E.g. if the enclosing class has `val reducer: DashboardProductsReducer`
    // and that type extends `FlowReducer<Event, State>`, include
    // `{Event→…, State→…}` mappings from FlowReducer's type params.
    for sym in data.symbols.iter() {
        if sym.selection_range.start.line <= class_line { continue; }
        if sym.selection_range.start.line > class_end_line { continue; }
        if !matches!(sym.kind, SymbolKind::FIELD | SymbolKind::PROPERTY) { continue; }
        let type_name = super::lookup::extract_property_type_name(&sym.detail);
        if type_name.is_empty() { continue; }
        let Some(locs) = index.get_definitions(type_name) else { continue };
        let Some(loc) = locs.into_iter().next() else { continue };
        let Some(prop_type_data) = index.get_file_data(loc.uri.as_str()) else { continue };
        let Some(prop_sym) = prop_type_data.symbols.iter().find(|s| s.name == type_name) else { continue };
        let prop_class_line = prop_sym.selection_range.start.line;
        for (line, super_name, type_args) in prop_type_data.supers.iter() {
            if *line != prop_class_line || type_args.is_empty() { continue; }
            let Some(super_locs) = index.get_definitions(super_name) else { continue };
            let Some(super_loc) = super_locs.into_iter().next() else { continue };
            let Some(super_file_data) = index.get_file_data(super_loc.uri.as_str()) else { continue };
            let Some(super_sym) = super_file_data.symbols.iter().find(|s| s.name == *super_name) else { continue };
            for (param, arg) in super_sym.type_params.iter().zip(type_args.iter()) {
                result.entry(param.clone()).or_insert_with(|| arg.clone());
            }
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
            if let Some(qual) = qualifier {
                // Index-only qualified resolution: resolve the qualifier without rg,
                // then search that file for `name`. Avoids silently dropping the
                // qualifier and returning results for an unrelated top-level symbol.
                let qual_locs = self.resolve_symbol_no_rg(qual, from_uri);
                if let Some(loc) = qual_locs.into_iter().next() {
                    let locs = self.find_name_in_uri(name, loc.uri.as_str());
                    if !locs.is_empty() {
                        return locs;
                    }
                }
            }
            self.resolve_symbol_no_rg(name, from_uri)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Url;
    use std::collections::HashMap;

    // ── Minimal stub (for tests that don't need real data) ───────────────────

    struct TestIndex;
    impl IndexRead for TestIndex {
        fn get_file_lines(&self, _uri: &str) -> Option<Vec<String>> { None }
        fn get_definitions(&self, _name: &str) -> Option<Vec<Location>> { None }
        fn get_file_data(&self, _uri: &str) -> Option<Arc<FileData>> { None }
    }

    // ── Fully-populated index for end-to-end tests ───────────────────────────

    struct RealTestIndex {
        files: HashMap<String, Arc<FileData>>,
        definitions: HashMap<String, Vec<Location>>,
    }

    impl IndexRead for RealTestIndex {
        fn get_file_lines(&self, uri: &str) -> Option<Vec<String>> {
            self.files.get(uri).map(|f| f.lines.as_ref().to_vec())
        }
        fn get_definitions(&self, name: &str) -> Option<Vec<Location>> {
            self.definitions.get(name).cloned()
        }
        fn get_file_data(&self, uri: &str) -> Option<Arc<FileData>> {
            self.files.get(uri).cloned()
        }
        fn resolve_locations(
            &self,
            name: &str,
            _qualifier: Option<&str>,
            _from_uri: &Url,
            _allow_rg: bool,
        ) -> Vec<Location> {
            self.definitions.get(name).cloned().unwrap_or_default()
        }
    }

    // ── Shared helpers ────────────────────────────────────────────────────────

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
            name:               name.to_owned(),
            kind,
            visibility:         Visibility::Public,
            range:              make_range(start_line, end_line),
            selection_range:    make_range(start_line, start_line),
            detail:             String::new(),
            type_params:        Vec::new(),
            extension_receiver: String::new(),
        }
    }

    fn make_location(uri: &str, line: u32) -> Location {
        Location { uri: Url::parse(uri).unwrap(), range: make_range(line, line) }
    }

    // ── Basic stub tests ──────────────────────────────────────────────────────

    #[test]
    fn stub_resolve_returns_none() {
        let idx = TestIndex;
        let res = resolve_symbol_info(
            &idx, "Foo", None,
            &Url::parse("file:///x").unwrap(),
            SubstitutionContext::None,
            &ResolveOptions::hover(),
        );
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

    // ── find_containing_class tests ───────────────────────────────────────────

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
        assert_eq!(find_containing_class_name(&data, 7).as_deref(), Some("Inner"));
    }

    #[test]
    fn find_containing_class_returns_none_for_top_level() {
        use crate::types::FileData;
        let data = FileData {
            symbols: vec![make_sym("Outer", SymbolKind::CLASS, 5, 15)],
            ..Default::default()
        };
        assert!(find_containing_class_name(&data, 1).is_none());
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

    // ── build_subst_map end-to-end tests ─────────────────────────────────────

    /// `class Child : Base<String, Int>` — subst should be `{T→String, U→Int}`
    /// where T and U come from Base's declaration, NOT from Child.
    #[test]
    fn build_subst_map_uses_base_class_type_params() {
        let base_uri = "file:///base.kt";
        let child_uri = "file:///child.kt";

        let mut base_sym = make_sym("Base", SymbolKind::CLASS, 0, 10);
        base_sym.type_params = vec!["T".to_owned(), "U".to_owned()];

        let base_data = Arc::new(crate::types::FileData {
            symbols: vec![base_sym],
            ..Default::default()
        });

        let child_data = Arc::new(crate::types::FileData {
            symbols: vec![make_sym("Child", SymbolKind::CLASS, 0, 20)],
            supers: vec![(0, "Base".to_owned(), vec!["String".to_owned(), "Int".to_owned()])],
            ..Default::default()
        });

        let mut files = HashMap::new();
        files.insert(base_uri.to_owned(), base_data);
        files.insert(child_uri.to_owned(), child_data);

        let mut definitions = HashMap::new();
        definitions.insert("Base".to_owned(), vec![make_location(base_uri, 0)]);

        let idx = RealTestIndex { files, definitions };

        let subst = build_subst_map(&idx, child_uri, 5);
        assert_eq!(subst.get("T").map(|s| s.as_str()), Some("String"));
        assert_eq!(subst.get("U").map(|s| s.as_str()), Some("Int"));
    }

    /// A class with no type params itself but inheriting a generic base.
    /// Previously the bug caused an empty map; now it correctly builds the map.
    #[test]
    fn build_subst_map_child_has_no_own_type_params() {
        let base_uri = "file:///reducer.kt";
        let child_uri = "file:///dashboard.kt";

        let mut base_sym = make_sym("FlowReducer", SymbolKind::CLASS, 0, 5);
        base_sym.type_params = vec!["Event".to_owned(), "State".to_owned()];

        let base_data = Arc::new(crate::types::FileData {
            symbols: vec![base_sym],
            ..Default::default()
        });

        // DashboardReducer has NO own type params, inherits FlowReducer<DashEvent, DashState>
        let child_data = Arc::new(crate::types::FileData {
            symbols: vec![make_sym("DashboardReducer", SymbolKind::CLASS, 0, 50)],
            supers: vec![(
                0,
                "FlowReducer".to_owned(),
                vec!["DashEvent".to_owned(), "DashState".to_owned()],
            )],
            ..Default::default()
        });

        let mut files = HashMap::new();
        files.insert(base_uri.to_owned(), base_data);
        files.insert(child_uri.to_owned(), child_data);

        let mut definitions = HashMap::new();
        definitions.insert("FlowReducer".to_owned(), vec![make_location(base_uri, 0)]);

        let idx = RealTestIndex { files, definitions };

        let subst = build_subst_map(&idx, child_uri, 10);
        assert_eq!(subst.get("Event").map(|s| s.as_str()), Some("DashEvent"));
        assert_eq!(subst.get("State").map(|s| s.as_str()), Some("DashState"));
    }

    // ── resolve_symbol_info end-to-end tests ─────────────────────────────────

    /// Basic lookup: symbol in a file with source lines, no substitution.
    #[test]
    fn resolve_symbol_info_basic_lookup() {
        let file_uri = "file:///utils.kt";

        let mut sym = make_sym("compute", SymbolKind::FUNCTION, 2, 5);
        sym.detail = "fun compute(x: Int): String".to_owned();

        let file_data = Arc::new(crate::types::FileData {
            symbols: vec![sym],
            lines: std::sync::Arc::new(vec![
                "package com.example".to_owned(),
                String::new(),
                "fun compute(x: Int): String = x.toString()".to_owned(),
            ]),
            ..Default::default()
        });

        let mut files = HashMap::new();
        files.insert(file_uri.to_owned(), file_data);

        let mut definitions = HashMap::new();
        definitions.insert("compute".to_owned(), vec![make_location(file_uri, 2)]);

        let idx = RealTestIndex { files, definitions };

        let result = resolve_symbol_info(
            &idx, "compute", None,
            &Url::parse("file:///caller.kt").unwrap(),
            SubstitutionContext::None,
            &ResolveOptions::goto_def(),
        );

        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.location.uri.as_str(), file_uri);
        // collect_signature reads from source lines and should prefer those
        assert!(r.raw_signature.contains("compute"), "raw_signature: {}", r.raw_signature);
    }

    /// With substitution context: `{T→String}` applied to the signature.
    #[test]
    fn resolve_symbol_info_applies_precomputed_subst() {
        let file_uri = "file:///base.kt";

        let mut sym = make_sym("process", SymbolKind::FUNCTION, 3, 5);
        sym.detail = "fun process(item: T): T".to_owned();

        let file_data = Arc::new(crate::types::FileData {
            symbols: vec![sym],
            lines: std::sync::Arc::new(vec![
                "package com.example".to_owned(),
                String::new(),
                String::new(),
                "fun process(item: T): T {".to_owned(),
            ]),
            ..Default::default()
        });

        let mut files = HashMap::new();
        files.insert(file_uri.to_owned(), file_data);

        let mut definitions = HashMap::new();
        definitions.insert("process".to_owned(), vec![make_location(file_uri, 3)]);

        let idx = RealTestIndex { files, definitions };

        let mut subst = HashMap::new();
        subst.insert("T".to_owned(), "String".to_owned());

        let result = resolve_symbol_info(
            &idx, "process", None,
            &Url::parse("file:///caller.kt").unwrap(),
            SubstitutionContext::Precomputed(&subst),
            &ResolveOptions::hover(),
        );

        assert!(result.is_some());
        let r = result.unwrap();
        assert!(r.signature.contains("String"), "signature should have substituted T→String: {}", r.signature);
        assert!(!r.signature.contains(": T"), "raw T should be replaced: {}", r.signature);
    }

    // ── Unb5/TRjS regression: CrossFile with cursor_line ─────────────────────

    /// When two classes in the same file extend the same base with different type
    /// args, `CrossFile { cursor_line }` must pick the right class for substitution.
    #[test]
    fn crossfile_cursor_line_disambiguates_multiple_callers() {
        use crate::types::Visibility;

        let base_uri   = "file:///base.kt";
        let caller_uri = "file:///caller.kt";

        // Base: class FlowReducer<E, S>  with fun reduce(e: E): S
        let base_class = {
            let mut s = make_sym("FlowReducer", SymbolKind::CLASS, 0, 10);
            s.type_params = vec!["E".to_owned(), "S".to_owned()];
            s
        };
        let base_method = {
            let mut s = make_sym("reduce", SymbolKind::FUNCTION, 5, 7);
            s.detail = "fun reduce(e: E): S".to_owned();
            s
        };
        let base_data = Arc::new(crate::types::FileData {
            symbols: vec![base_class, base_method],
            lines: std::sync::Arc::new(vec![
                "class FlowReducer<E, S> {".to_owned(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                "    fun reduce(e: E): S {}".to_owned(),
                "}".to_owned(),
            ]),
            ..Default::default()
        });

        // Caller file has TWO classes extending FlowReducer with different args:
        //   class DashReducer : FlowReducer<DashEvent, DashState>  (line 0)
        //   class SettingsReducer : FlowReducer<SettEvent, SettState> (line 10)
        let dash_class = {
            let mut s = make_sym("DashReducer", SymbolKind::CLASS, 0, 8);
            s.selection_range = make_range(0, 0);
            s
        };
        let sett_class = {
            let mut s = make_sym("SettingsReducer", SymbolKind::CLASS, 10, 18);
            s.selection_range = make_range(10, 10);
            s
        };
        let caller_data = Arc::new(crate::types::FileData {
            symbols: vec![dash_class, sett_class],
            supers: vec![
                (0, "FlowReducer".to_owned(), vec!["DashEvent".to_owned(), "DashState".to_owned()]),
                (10, "FlowReducer".to_owned(), vec!["SettEvent".to_owned(), "SettState".to_owned()]),
            ],
            ..Default::default()
        });

        let mut files = HashMap::new();
        files.insert(base_uri.to_owned(), base_data);
        files.insert(caller_uri.to_owned(), caller_data);
        let mut definitions = HashMap::new();
        definitions.insert("FlowReducer".to_owned(), vec![make_location(base_uri, 0)]);
        definitions.insert("reduce".to_owned(), vec![make_location(base_uri, 5)]);
        let idx = RealTestIndex { files, definitions };

        // Cursor inside DashReducer (line 4): should use DashEvent/DashState
        let result_dash = resolve_symbol_info(
            &idx, "reduce", None,
            &Url::parse(caller_uri).unwrap(),
            SubstitutionContext::CrossFile { calling_uri: caller_uri, cursor_line: 4 },
            &ResolveOptions::hover(),
        );
        let dash = result_dash.expect("should resolve reduce");
        assert!(dash.signature.contains("DashEvent"), "dash: {}", dash.signature);
        assert!(dash.signature.contains("DashState"), "dash: {}", dash.signature);

        // Cursor inside SettingsReducer (line 14): should use SettEvent/SettState
        let result_sett = resolve_symbol_info(
            &idx, "reduce", None,
            &Url::parse(caller_uri).unwrap(),
            SubstitutionContext::CrossFile { calling_uri: caller_uri, cursor_line: 14 },
            &ResolveOptions::hover(),
        );
        let sett = result_sett.expect("should resolve reduce");
        assert!(sett.signature.contains("SettEvent"), "sett: {}", sett.signature);
        assert!(sett.signature.contains("SettState"), "sett: {}", sett.signature);
    }
}
