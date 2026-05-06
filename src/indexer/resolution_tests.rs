use super::*;
use std::collections::HashMap;
use tower_lsp::lsp_types::Url;

// ── Minimal stub (for tests that don't need real data) ───────────────────

struct TestIndex;
impl IndexRead for TestIndex {
    fn get_definitions(&self, _name: &str) -> Option<Vec<Location>> {
        None
    }
    fn get_file_data(&self, _uri: &str) -> Option<Arc<FileData>> {
        None
    }
}

// ── Fully-populated index for end-to-end tests ───────────────────────────

struct RealTestIndex {
    files: HashMap<String, Arc<FileData>>,
    definitions: HashMap<String, Vec<Location>>,
}

impl IndexRead for RealTestIndex {
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
        start: Position {
            line: start_line,
            character: 0,
        },
        end: Position {
            line: end_line,
            character: 0,
        },
    }
}

fn make_sym(name: &str, kind: SymbolKind, start_line: u32, end_line: u32) -> SymbolEntry {
    use crate::types::Visibility;
    SymbolEntry {
        name: name.to_owned(),
        kind,
        visibility: Visibility::Public,
        range: make_range(start_line, end_line),
        selection_range: make_range(start_line, start_line),
        detail: String::new(),
        type_params: Vec::new(),
        extension_receiver: String::new(),
    }
}

fn make_location(uri: &str, line: u32) -> Location {
    Location {
        uri: Url::parse(uri).unwrap(),
        range: make_range(line, line),
    }
}

// ── Basic stub tests ──────────────────────────────────────────────────────

#[test]
fn stub_resolve_returns_none() {
    let idx = TestIndex;
    let res = resolve_symbol_info(
        &idx,
        "Foo",
        None,
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
    assert_eq!(data.containing_class_at(7).as_deref(), Some("Inner"));
}

#[test]
fn find_containing_class_returns_none_for_top_level() {
    use crate::types::FileData;
    let data = FileData {
        symbols: vec![make_sym("Outer", SymbolKind::CLASS, 5, 15)],
        ..Default::default()
    };
    assert!(data.containing_class_at(1).is_none());
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
    assert_eq!(data.containing_class_at(5).as_deref(), Some("MyEnum"));
    assert_eq!(data.containing_class_at(15).as_deref(), Some("MyObject"));
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
        supers: vec![(
            0,
            "Base".to_owned(),
            vec!["String".to_owned(), "Int".to_owned()],
        )],
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

// ── enrich_at_line tests ──────────────────────────────────────────────────

fn make_sym_col(
    name: &str,
    kind: SymbolKind,
    line: u32,
    col_start: u32,
    col_end: u32,
) -> SymbolEntry {
    use crate::types::Visibility;
    use tower_lsp::lsp_types::{Position, Range};
    SymbolEntry {
        name: name.to_owned(),
        kind,
        visibility: Visibility::Public,
        range: Range {
            start: Position {
                line,
                character: col_start,
            },
            end: Position {
                line,
                character: col_end,
            },
        },
        selection_range: Range {
            start: Position {
                line,
                character: col_start,
            },
            end: Position {
                line,
                character: col_end,
            },
        },
        detail: format!("fun {}()", name),
        type_params: Vec::new(),
        extension_receiver: String::new(),
    }
}

/// Two overloads on different lines: enrich_at_line selects the right one by line.
#[test]
fn enrich_at_line_picks_by_line() {
    let file_uri = "file:///overloads.kt";
    let sym_a = make_sym_col("process", SymbolKind::FUNCTION, 0, 4, 11);
    let sym_b = make_sym_col("process", SymbolKind::FUNCTION, 5, 4, 11);

    let file_data = Arc::new(crate::types::FileData {
        symbols: vec![sym_a, sym_b],
        lines: std::sync::Arc::new(vec![
            "fun process() {}".to_owned(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            "fun process() {}".to_owned(),
        ]),
        ..Default::default()
    });
    let mut files = HashMap::new();
    files.insert(file_uri.to_owned(), file_data);
    let idx = RealTestIndex {
        files,
        definitions: HashMap::new(),
    };

    // Picking line 0 returns the first symbol; line 5 returns the second.
    let res0 = enrich_at_line(
        &idx,
        file_uri,
        0,
        6,
        SubstitutionContext::None,
        &ResolveOptions::hover(),
    );
    assert!(res0.is_some(), "should find symbol on line 0");

    let res5 = enrich_at_line(
        &idx,
        file_uri,
        5,
        6,
        SubstitutionContext::None,
        &ResolveOptions::hover(),
    );
    assert!(res5.is_some(), "should find symbol on line 5");

    // Both have the same name but came from different symbol entries.
    assert_eq!(res0.unwrap().location.range.start.line, 0);
    assert_eq!(res5.unwrap().location.range.start.line, 5);
}

/// Column outside any symbol on the line → falls back to first sym on that line.
#[test]
fn enrich_at_line_col_fallback() {
    let file_uri = "file:///fb.kt";
    let sym = make_sym_col("fetch", SymbolKind::FUNCTION, 2, 4, 9);

    let file_data = Arc::new(crate::types::FileData {
        symbols: vec![sym],
        lines: std::sync::Arc::new(vec![
            String::new(),
            String::new(),
            "fun fetch() {}".to_owned(),
        ]),
        ..Default::default()
    });
    let mut files = HashMap::new();
    files.insert(file_uri.to_owned(), file_data);
    let idx = RealTestIndex {
        files,
        definitions: HashMap::new(),
    };

    // col 99 is far outside [4,9) — should still resolve via fallback.
    let res = enrich_at_line(
        &idx,
        file_uri,
        2,
        99,
        SubstitutionContext::None,
        &ResolveOptions::hover(),
    );
    assert!(res.is_some(), "fallback should work when col misses");
    assert_eq!(res.unwrap().name, "fetch");
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
        &idx,
        "compute",
        None,
        &Url::parse("file:///caller.kt").unwrap(),
        SubstitutionContext::None,
        &ResolveOptions::goto_def(),
    );

    assert!(result.is_some());
    let r = result.unwrap();
    assert_eq!(r.location.uri.as_str(), file_uri);
    // collect_signature reads from source lines and should prefer those
    assert!(
        r.raw_signature.contains("compute"),
        "raw_signature: {}",
        r.raw_signature
    );
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
        &idx,
        "process",
        None,
        &Url::parse("file:///caller.kt").unwrap(),
        SubstitutionContext::Precomputed(&subst),
        &ResolveOptions::hover(),
    );

    assert!(result.is_some());
    let r = result.unwrap();
    assert!(
        r.signature.contains("String"),
        "signature should have substituted T→String: {}",
        r.signature
    );
    assert!(
        !r.signature.contains(": T"),
        "raw T should be replaced: {}",
        r.signature
    );
}

// ── Unb5/TRjS regression: CrossFile with cursor_line ─────────────────────

/// When two classes in the same file extend the same base with different type
/// args, `CrossFile { cursor_line }` must pick the right class for substitution.
#[test]
fn crossfile_cursor_line_disambiguates_multiple_callers() {
    use crate::types::Visibility;

    let base_uri = "file:///base.kt";
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
            (
                0,
                "FlowReducer".to_owned(),
                vec!["DashEvent".to_owned(), "DashState".to_owned()],
            ),
            (
                10,
                "FlowReducer".to_owned(),
                vec!["SettEvent".to_owned(), "SettState".to_owned()],
            ),
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
        &idx,
        "reduce",
        None,
        &Url::parse(caller_uri).unwrap(),
        SubstitutionContext::CrossFile {
            calling_uri: caller_uri,
            cursor_line: Some(4),
        },
        &ResolveOptions::hover(),
    );
    let dash = result_dash.expect("should resolve reduce");
    assert!(
        dash.signature.contains("DashEvent"),
        "dash: {}",
        dash.signature
    );
    assert!(
        dash.signature.contains("DashState"),
        "dash: {}",
        dash.signature
    );

    // Cursor inside SettingsReducer (line 14): should use SettEvent/SettState
    let result_sett = resolve_symbol_info(
        &idx,
        "reduce",
        None,
        &Url::parse(caller_uri).unwrap(),
        SubstitutionContext::CrossFile {
            calling_uri: caller_uri,
            cursor_line: Some(14),
        },
        &ResolveOptions::hover(),
    );
    let sett = result_sett.expect("should resolve reduce");
    assert!(
        sett.signature.contains("SettEvent"),
        "sett: {}",
        sett.signature
    );
    assert!(
        sett.signature.contains("SettState"),
        "sett: {}",
        sett.signature
    );
}

// ── enrich_at_line (completion resolve) ──────────────────────────────────

#[test]
fn enrich_at_line_returns_detail_for_completion_resolve() {
    let uri = "file:///Foo.kt";
    let mut sym = make_sym("add", SymbolKind::FUNCTION, 0, 0);
    sym.detail = "fun add(a: Int, b: Int): Int".to_owned();

    let data = Arc::new(crate::types::FileData {
        symbols: vec![sym],
        ..Default::default()
    });
    let mut files = HashMap::new();
    files.insert(uri.to_owned(), data);
    let idx = RealTestIndex {
        files,
        definitions: HashMap::new(),
    };

    let result = enrich_at_line(
        &idx,
        uri,
        0, // line
        0, // col
        SubstitutionContext::None,
        &ResolveOptions::completion(),
    );
    assert!(
        result.is_some(),
        "enrich_at_line should return Some for documented function"
    );
    let info = result.unwrap();
    assert!(!info.signature.is_empty(), "signature should not be empty");
    assert_eq!(info.signature, "fun add(a: Int, b: Int): Int");
    // doc should be empty by default for completion() options
    assert_eq!(info.doc, "");
}

#[test]
fn enrich_at_line_falls_back_to_line_only_match_for_completion() {
    let uri = "file:///Bar.kt";
    let sym = make_sym("multiply", SymbolKind::FUNCTION, 0, 0);
    let data = Arc::new(crate::types::FileData {
        symbols: vec![sym],
        ..Default::default()
    });
    let mut files = HashMap::new();
    files.insert(uri.to_owned(), data);
    let idx = RealTestIndex {
        files,
        definitions: HashMap::new(),
    };

    // Query with col=5, but symbol is at col=0: should fall back to line-only match
    let result = enrich_at_line(
        &idx,
        uri,
        0, // line (matches)
        5, // col (doesn't match, but fallback should find it)
        SubstitutionContext::None,
        &ResolveOptions::completion(),
    );
    assert!(
        result.is_some(),
        "enrich_at_line should fall back to line-only match"
    );
    assert_eq!(result.unwrap().kind, SymbolKind::FUNCTION);
}

#[test]
fn enrich_at_line_exact_position_match_preferred() {
    // Verify that exact col match takes precedence over line-only match
    let uri = "file:///Baz.kt";
    let sym1 = make_sym_col("first", SymbolKind::FUNCTION, 0, 0, 5);
    let sym2 = make_sym_col("second", SymbolKind::FUNCTION, 0, 7, 13);

    let data = Arc::new(crate::types::FileData {
        symbols: vec![sym1, sym2],
        ..Default::default()
    });
    let mut files = HashMap::new();
    files.insert(uri.to_owned(), data);
    let idx = RealTestIndex {
        files,
        definitions: HashMap::new(),
    };

    // Query at col=8 (within "second"): should match second, not first
    let result = enrich_at_line(
        &idx,
        uri,
        0,
        8,
        SubstitutionContext::None,
        &ResolveOptions::completion(),
    );
    assert!(result.is_some());
    assert_eq!(
        result.unwrap().name,
        "second",
        "should prefer exact position match"
    );
}
