use tree_sitter::{Node, Parser, Query, QueryCursor};
use tower_lsp::lsp_types::{Position, Range, SymbolKind};

use crate::queries::{self, KOTLIN_DEFINITIONS};
use crate::types::{FileData, ImportEntry, SymbolEntry, Visibility};

// ─── public entry points ────────────────────────────────────────────────────

pub fn parse_kotlin(content: &str) -> FileData {
    let lang  = tree_sitter_kotlin::language();
    let lines = std::sync::Arc::new(content.lines().map(str::to_owned).collect());
    let mut data = FileData { lines, ..Default::default() };

    let mut parser = Parser::new();
    if parser.set_language(&lang).is_err() { return data; }
    let Some(tree) = parser.parse(content, None) else { return data; };

    let bytes = content.as_bytes();
    let root  = tree.root_node();

    // ── definitions ──────────────────────────────────────────────────────────
    let def_q = match Query::new(&lang, KOTLIN_DEFINITIONS) {
        Ok(q)  => q,
        Err(e) => { log::error!("definitions query error: {e}"); return data; }
    };
    let def_idx  = def_q.capture_index_for_name("def").unwrap_or(0);
    let name_idx = def_q.capture_index_for_name("name").unwrap_or(1);

    let mut cur = QueryCursor::new();
    let matches: Vec<(usize, [Option<(String, Range, Range)>; 2])> = cur
        .matches(&def_q, root, bytes)
        .map(|m| {
            let pidx = m.pattern_index;
            let mut def_range:  Option<Range> = None;
            let mut name_text:  Option<String> = None;
            let mut name_range: Option<Range> = None;
            for cap in m.captures {
                if cap.index == def_idx {
                    def_range = Some(ts_to_lsp(cap.node.range()));
                } else if cap.index == name_idx {
                    name_text  = cap.node.utf8_text(bytes).ok().map(str::to_owned);
                    name_range = Some(ts_to_lsp(cap.node.range()));
                }
            }
            let slot = if let (Some(dr), Some(nt), Some(nr)) = (def_range, name_text, name_range) {
                [Some((nt, dr, nr)), None]
            } else {
                [None, None]
            };
            (pidx, slot)
        })
        .collect();

    // Deduplicate: multiple patterns can fire on the same node
    // (e.g. enum class matches both pattern 0 "enum" AND pattern 2 "class").
    //
    // Use a BTreeMap keyed by the @name node's start position.
    // The value records the LOWEST pattern index seen so far — lower index = more
    // specific pattern = wins.  This is correct regardless of the order in which
    // tree-sitter returns overlapping matches (not guaranteed by the API).
    let mut best: std::collections::BTreeMap<(u32, u32), (usize, String, Range, Range)> =
        std::collections::BTreeMap::new();

    for (pidx, slot) in matches {
        if let Some((name, range, sel)) = slot[0].clone() {
            let key = (sel.start.line, sel.start.character);
            let is_better = best.get(&key).map(|(ep, _, _, _)| pidx < *ep).unwrap_or(true);
            if is_better {
                best.insert(key, (pidx, name, range, sel));
            }
        }
    }

    for (_, (pidx, name, range, sel)) in best {
        let (kind, _detail) = queries::def_pattern_meta(pidx);
        if kind != SymbolKind::NULL {
            let visibility = visibility_at_line(&data.lines, sel.start.line as usize);
            data.symbols.push(SymbolEntry { name, kind, visibility, range, selection_range: sel });
        }
    }

    // ── package + imports (manual tree walk — avoids query overlap issues) ────
    extract_package_and_imports(root, bytes, &mut data);

    // ── declared_names: scan lines once for `ident:` patterns ───────────────
    data.declared_names = extract_declared_names(&data.lines);

    data
}

pub fn parse_java(content: &str) -> FileData {
    let lang  = tree_sitter_java::language();
    let lines = std::sync::Arc::new(content.lines().map(str::to_owned).collect());
    let mut data = FileData { lines, ..Default::default() };

    let mut parser = Parser::new();
    if parser.set_language(&lang).is_err() { return data; }
    let Some(tree) = parser.parse(content, None) else { return data; };

    let bytes = content.as_bytes();
    let mut queue = vec![tree.root_node()];
    while let Some(node) = queue.pop() {
        extract_java(&node, bytes, &mut data);
        let mut cur = node.walk();
        for child in node.children(&mut cur) { queue.push(child); }
    }
    data.declared_names = extract_declared_names(&data.lines);
    data
}

// ─── Java extraction (manual traversal — Java grammar has named fields) ──────

fn extract_java(node: &Node, bytes: &[u8], data: &mut FileData) {
    match node.kind() {
        "package_declaration" => {
            // (package_declaration "package" (scoped_identifier | identifier) ";")
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                if matches!(child.kind(), "scoped_identifier" | "identifier") {
                    if let Ok(txt) = child.utf8_text(bytes) {
                        data.package = Some(txt.to_owned());
                    }
                    break;
                }
            }
        }
        "class_declaration"     => push_named(node, bytes, SymbolKind::CLASS,     data),
        "record_declaration"    => push_named(node, bytes, SymbolKind::STRUCT,    data),
        "interface_declaration" => push_named(node, bytes, SymbolKind::INTERFACE, data),
        "annotation_type_declaration" => push_named(node, bytes, SymbolKind::INTERFACE, data),
        "enum_declaration"      => push_named(node, bytes, SymbolKind::ENUM,      data),
        "method_declaration"    => push_named(node, bytes, SymbolKind::METHOD,    data),
        "constructor_declaration" => push_named(node, bytes, SymbolKind::CONSTRUCTOR, data),
        "enum_constant" => {
            // Direct child of enum_body — the first identifier child is the constant name.
            let nr = ts_to_lsp(node.range());
            let line_no = node.range().start_point.row;
            let vis = visibility_at_line(&data.lines, line_no);
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                if child.kind() == "identifier" {
                    if let Ok(txt) = child.utf8_text(bytes) {
                        data.symbols.push(SymbolEntry {
                            name: txt.to_owned(),
                            kind: SymbolKind::ENUM_MEMBER,
                            visibility: vis,
                            range: nr,
                            selection_range: ts_to_lsp(child.range()),
                        });
                    }
                    break;
                }
            }
        }
        "field_declaration"     => {
            let nr = ts_to_lsp(node.range());
            let line_no = node.range().start_point.row;
            let vis = visibility_at_line(&data.lines, line_no);
            // Detect `static final` → CONSTANT, anything else → FIELD.
            let kind = if java_node_has_modifiers(node, &["static", "final"]) {
                SymbolKind::CONSTANT
            } else {
                SymbolKind::FIELD
            };
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                if child.kind() == "variable_declarator" {
                    if let Some((name, sel)) = first_identifier(&child, bytes) {
                        data.symbols.push(SymbolEntry { name, kind, visibility: vis, range: nr, selection_range: sel });
                    }
                }
            }
        }
        "import_declaration" => {
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                if matches!(child.kind(), "scoped_identifier" | "identifier") {
                    if let Ok(txt) = child.utf8_text(bytes) {
                        let full_path  = txt.to_owned();
                        let local_name = last_segment(&full_path).to_owned();
                        data.imports.push(ImportEntry { full_path, local_name, is_star: false });
                        return;
                    }
                }
            }
        }
        _ => {}
    }
}

/// Returns true if the Java node's modifiers child contains ALL of `required` keywords.
fn java_node_has_modifiers(node: &Node, required: &[&str]) -> bool {
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if child.kind() == "modifiers" {
            // Collect modifier keyword kinds into a Vec first to avoid walker lifetime issues.
            let mut mc = child.walk();
            let found_kinds: Vec<&str> = child.children(&mut mc).map(|k| k.kind()).collect();
            return required.iter().all(|&req| found_kinds.contains(&req));
        }
    }
    false
}

fn push_named(node: &Node, bytes: &[u8], kind: SymbolKind, data: &mut FileData) {
    if let Some((name, sel)) = first_identifier(node, bytes) {
        let visibility = visibility_at_line(&data.lines, node.range().start_point.row);
        data.symbols.push(SymbolEntry { name, kind, visibility, range: ts_to_lsp(node.range()), selection_range: sel });
    }
}

fn first_identifier(node: &Node, bytes: &[u8]) -> Option<(String, Range)> {
    if let Some(n) = node.child_by_field_name("name") {
        if let Ok(t) = n.utf8_text(bytes) { return Some((t.to_owned(), ts_to_lsp(n.range()))); }
    }
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if matches!(child.kind(), "type_identifier" | "simple_identifier" | "identifier") {
            if let Ok(t) = child.utf8_text(bytes) {
                if is_ident(t) { return Some((t.to_owned(), ts_to_lsp(child.range()))); }
            }
        }
    }
    None
}

// ─── helpers ─────────────────────────────────────────────────────────────────

fn ts_to_lsp(r: tree_sitter::Range) -> Range {
    Range {
        start: Position { line: r.start_point.row as u32, character: r.start_point.column as u32 },
        end:   Position { line: r.end_point.row   as u32, character: r.end_point.column   as u32 },
    }
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().map(|c| c.is_alphabetic() || c == '_').unwrap_or(false)
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

fn last_segment(dotted: &str) -> &str {
    dotted.rsplit('.').next().unwrap_or(dotted)
}

// ─── package + import extraction ─────────────────────────────────────────────
//
// Uses a manual BFS rather than queries to avoid the pattern-overlap problem
// (plain-import query would also fire on star / alias imports).

fn extract_package_and_imports(root: tree_sitter::Node, bytes: &[u8], data: &mut FileData) {
    // Only need the top of the file: package_header and import_list are always
    // direct children of source_file, so one pass over root's children suffices.
    let mut cur = root.walk();
    for node in root.children(&mut cur) {
        match node.kind() {
            "package_header" => {
                // (package_header "package" (identifier ...))
                let mut c = node.walk();
                for child in node.children(&mut c) {
                    if child.kind() == "identifier" {
                        if let Ok(txt) = child.utf8_text(bytes) {
                            data.package = Some(txt.to_owned());
                        }
                        break;
                    }
                }
            }
            "import_list" => {
                let mut lc = node.walk();
                for header in node.children(&mut lc) {
                    if header.kind() == "import_header" {
                        parse_import_header(&header, bytes, data);
                    }
                }
            }
            _ => {}
        }
    }
}

fn parse_import_header(header: &tree_sitter::Node, bytes: &[u8], data: &mut FileData) {
    let mut path_text: Option<String> = None;
    let mut alias_text: Option<String> = None;
    let mut is_star = false;

    let mut cur = header.walk();
    for child in header.children(&mut cur) {
        match child.kind() {
            "identifier" => {
                path_text = child.utf8_text(bytes).ok().map(str::to_owned);
            }
            "import_alias" => {
                // (import_alias "as" (type_identifier))
                let mut ac = child.walk();
                for ac_child in child.children(&mut ac) {
                    if ac_child.kind() == "type_identifier" {
                        alias_text = ac_child.utf8_text(bytes).ok().map(str::to_owned);
                        break;
                    }
                }
            }
            "wildcard_import" => {
                is_star = true;
            }
            _ => {}
        }
    }

    if let Some(full_path) = path_text {
        let local_name = if is_star {
            "*".to_owned()
        } else {
            alias_text.unwrap_or_else(|| last_segment(&full_path).to_owned())
        };
        data.imports.push(ImportEntry { full_path, local_name, is_star });
    }
}

// ─── visibility detection ────────────────────────────────────────────────────

/// Detect the Kotlin/Java visibility modifier on `line_no` by scanning that
/// source line for modifier keywords.
///
/// Strategy: take the content *before* the symbol name (the modifiers region)
/// and check for visibility keywords.  Works for the common patterns:
///
/// ```kotlin
/// private fun foo()          → Private
/// protected val bar: T       → Protected
/// internal class Baz         → Internal
/// fun visible()              → Public (default)
/// override fun also()        → Public (no explicit visibility = public)
/// ```
///
/// Multi-line modifier blocks (rare) are NOT handled; they default to Public.
pub(crate) fn visibility_at_line(lines: &[String], line_no: usize) -> Visibility {
    let line = match lines.get(line_no) {
        Some(l) => l,
        None    => return Visibility::Public,
    };
    // Work only on the part before any `=`, `{`, or `(` to avoid false positives
    // from string literals / bodies.
    let decl = line.split_once('{').map(|(l, _)| l)
        .unwrap_or(line)
        .split_once('=').map(|(l, _)| l)
        .unwrap_or(line);

    // Check whole-word tokens.
    if contains_word(decl, "private")   { return Visibility::Private; }
    if contains_word(decl, "protected") { return Visibility::Protected; }
    if contains_word(decl, "internal")  { return Visibility::Internal; }
    Visibility::Public
}

fn contains_word(text: &str, word: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = text[start..].find(word) {
        let abs = start + pos;
        let before_ok = abs == 0
            || !text.as_bytes()[abs - 1].is_ascii_alphanumeric()
            && text.as_bytes()[abs - 1] != b'_';
        let after_ok  = abs + word.len() >= text.len()
            || !text.as_bytes()[abs + word.len()].is_ascii_alphanumeric()
            && text.as_bytes()[abs + word.len()] != b'_';
        if before_ok && after_ok { return true; }
        start = abs + 1;
    }
    false
}

// ─── declared_names extraction ───────────────────────────────────────────────

/// Scan source lines for `ident:` patterns (constructor params, properties, locals).
/// Called once at parse time; result cached in FileData so completion never re-scans.
pub(crate) fn extract_declared_names(lines: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in lines {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') || t.starts_with("/*") { continue; }
        let mut rest = t;
        while let Some(ci) = rest.find(':') {
            let before = &rest[..ci];
            // Extract the trailing identifier from `before` — handles both
            // `val foo:` (whitespace-separated) and `fun bar(foo:` (paren-separated).
            let word: String = before.chars()
                .rev()
                .take_while(|&c| c.is_alphanumeric() || c == '_')
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if word.len() > 1
                && word.chars().next().map(|c| c.is_lowercase()).unwrap_or(false)
                && seen.insert(word.clone())
            {
                names.push(word);
            }
            rest = &rest[ci + 1..];
        }
    }
    names
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::{Position, SymbolKind};

    fn uri(path: &str) -> tower_lsp::lsp_types::Url {
        tower_lsp::lsp_types::Url::parse(&format!("file:///test{path}")).unwrap()
    }

    fn sym<'a>(data: &'a FileData, name: &str) -> Option<&'a SymbolEntry> {
        data.symbols.iter().find(|s| s.name == name)
    }

    // ── symbol extraction ────────────────────────────────────────────────────

    // ── query sanity check ───────────────────────────────────────────────────

    #[test]
    fn kotlin_definitions_query_compiles() {
        let lang = tree_sitter_kotlin::language();
        let result = tree_sitter::Query::new(&lang, crate::queries::KOTLIN_DEFINITIONS);
        if let Err(e) = &result {
            panic!("KOTLIN_DEFINITIONS query failed to compile: {e}");
        }
    }

    #[test] fn class()        { assert_eq!(sym(&parse_kotlin("class Foo"),        "Foo").unwrap().kind, SymbolKind::CLASS); }
    #[test] fn interface()    { assert_eq!(sym(&parse_kotlin("interface Bar"),     "Bar").unwrap().kind, SymbolKind::INTERFACE); }
    #[test] fn object_decl()  { assert_eq!(sym(&parse_kotlin("object Obj"),        "Obj").unwrap().kind, SymbolKind::OBJECT); }
    #[test] fn data_class()   { assert_eq!(sym(&parse_kotlin("data class D(val x: Int)"), "D").unwrap().kind, SymbolKind::STRUCT); }
    #[test] fn enum_class()   { assert_eq!(sym(&parse_kotlin("enum class Color { RED }"), "Color").unwrap().kind, SymbolKind::ENUM); }
    #[test] fn enum_entries() {
        let data = parse_kotlin("enum class Screen { DETAIL, LIST, SETTINGS }");
        assert_eq!(sym(&data, "DETAIL").unwrap().kind,   SymbolKind::ENUM_MEMBER);
        assert_eq!(sym(&data, "LIST").unwrap().kind,     SymbolKind::ENUM_MEMBER);
        assert_eq!(sym(&data, "SETTINGS").unwrap().kind, SymbolKind::ENUM_MEMBER);
    }
    #[test] fn typealias()    { assert_eq!(sym(&parse_kotlin("typealias Alias = String"), "Alias").unwrap().kind, SymbolKind::CLASS); }
    #[test] fn top_fun()      { assert_eq!(sym(&parse_kotlin("fun foo() {}"), "foo").unwrap().kind, SymbolKind::FUNCTION); }
    #[test] fn val_prop()     { assert_eq!(sym(&parse_kotlin("val x: Int = 0"), "x").unwrap().kind, SymbolKind::PROPERTY); }
    #[test] fn var_prop()     { assert_eq!(sym(&parse_kotlin("var y = 0"),      "y").unwrap().kind, SymbolKind::VARIABLE); }
    #[test] fn const_val()    {
        let data = parse_kotlin("const val MAX: Int = 100");
        assert_eq!(sym(&data, "MAX").unwrap().kind, SymbolKind::CONSTANT);
    }
    #[test] fn operator_fun() {
        let data = parse_kotlin("operator fun plus(other: Vec): Vec = Vec()");
        assert_eq!(sym(&data, "plus").unwrap().kind, SymbolKind::OPERATOR);
    }
    #[test] fn operator_fun_in_class() {
        let data = parse_kotlin("class Vec {\n  operator fun plus(other: Vec): Vec = Vec()\n}");
        assert_eq!(sym(&data, "plus").unwrap().kind, SymbolKind::OPERATOR);
    }

    #[test]
    fn primary_ctor_val_param_indexed() {
        let data = parse_kotlin("data class User(val name: String, val age: Int)");
        assert_eq!(sym(&data, "name").unwrap().kind, SymbolKind::PROPERTY,
            "val ctor param should be PROPERTY");
        assert_eq!(sym(&data, "age").unwrap().kind, SymbolKind::PROPERTY);
    }

    #[test]
    fn primary_ctor_var_param_indexed() {
        let data = parse_kotlin("class Counter(var count: Int = 0)");
        assert_eq!(sym(&data, "count").unwrap().kind, SymbolKind::VARIABLE,
            "var ctor param should be VARIABLE");
    }

    #[test]
    fn primary_ctor_plain_param_not_indexed() {
        // A plain parameter WITHOUT val/var is NOT a property — should not be indexed.
        let data = parse_kotlin("class Foo(name: String)");
        assert!(sym(&data, "name").is_none(),
            "plain ctor param (no val/var) should not be in symbol index");
    }

    #[test]
    fn val_destructure() {
        let data = parse_kotlin("val (a, b) = pair");
        assert!(sym(&data, "a").is_some());
        assert!(sym(&data, "b").is_some());
    }

    #[test]
    fn nested_class_indexed() {
        let data = parse_kotlin("class Outer { class Inner {} }");
        assert!(sym(&data, "Outer").is_some(), "Outer missing");
        assert!(sym(&data, "Inner").is_some(), "Inner missing");
    }

    #[test]
    fn method_in_class_indexed() {
        let data = parse_kotlin("class Foo {\n  fun method() {}\n}");
        assert!(sym(&data, "method").is_some());
    }

    // ── selection_range positions ────────────────────────────────────────────

    #[test]
    fn class_name_position() {
        let data = parse_kotlin("class Foo");
        let s = sym(&data, "Foo").unwrap();
        assert_eq!(s.selection_range.start.line,      0);
        assert_eq!(s.selection_range.start.character, 6);
        assert_eq!(s.selection_range.end.character,   9);
    }

    #[test]
    fn fun_name_position() {
        let data = parse_kotlin("fun myFun() {}");
        let s = sym(&data, "myFun").unwrap();
        assert_eq!(s.selection_range.start.character, 4);
    }

    // ── deduplication ────────────────────────────────────────────────────────

    #[test]
    fn data_class_no_duplicate() {
        let data = parse_kotlin("data class Foo(val x: Int)");
        assert_eq!(
            data.symbols.iter().filter(|s| s.name == "Foo").count(), 1,
            "data class must appear exactly once"
        );
    }

    #[test]
    fn top_fun_no_duplicate() {
        let data = parse_kotlin("fun foo() {}");
        assert_eq!(
            data.symbols.iter().filter(|s| s.name == "foo").count(), 1,
            "top-level fun must appear exactly once"
        );
    }

    // ── package + imports ────────────────────────────────────────────────────

    #[test]
    fn package_parsed() {
        let data = parse_kotlin("package com.example.app");
        assert_eq!(data.package, Some("com.example.app".into()));
    }

    #[test]
    fn import_plain() {
        let data = parse_kotlin("import com.example.Foo");
        let imp = data.imports.iter().find(|i| i.full_path == "com.example.Foo").unwrap();
        assert_eq!(imp.local_name, "Foo");
        assert!(!imp.is_star);
    }

    #[test]
    fn import_alias() {
        let data = parse_kotlin("import com.example.Foo as F");
        let imp = data.imports.iter().find(|i| i.full_path == "com.example.Foo").unwrap();
        assert_eq!(imp.local_name, "F");
        assert!(!imp.is_star);
    }

    #[test]
    fn import_star() {
        let data = parse_kotlin("import com.example.*");
        let imp = data.imports.iter().find(|i| i.is_star).unwrap();
        assert_eq!(imp.full_path, "com.example");
        assert_eq!(imp.local_name, "*");
    }

    // ── lines ────────────────────────────────────────────────────────────────

    #[test]
    fn lines_populated() {
        let data = parse_kotlin("class Foo\nfun bar() {}");
        assert_eq!(data.lines.len(), 2);
        assert_eq!(data.lines[0], "class Foo");
        assert_eq!(data.lines[1], "fun bar() {}");
    }

    // ── full file smoke test ─────────────────────────────────────────────────

    #[test]
    fn full_file() {
        let src = "package com.example\n\
                   import com.example.Bar\n\
                   import com.example.pkg.*\n\
                   import com.example.Baz as B\n\
                   class MyClass\n\
                   interface MyIface\n\
                   object MySingleton\n\
                   data class MyData(val id: Int)\n\
                   typealias MyAlias = String\n\
                   val topVal = 0\n\
                   var topVar = 0\n\
                   fun topFun() {}";

        let data = parse_kotlin(src);
        assert_eq!(data.package, Some("com.example".into()));

        for name in &["MyClass","MyIface","MySingleton","MyData","MyAlias","topVal","topVar","topFun"] {
            assert!(sym(&data, name).is_some(), "{name} not indexed");
        }
        assert!(data.imports.iter().any(|i| i.full_path == "com.example.Bar"));
        assert!(data.imports.iter().any(|i| i.is_star && i.full_path == "com.example.pkg"));
        assert!(data.imports.iter().any(|i| i.local_name == "B" && i.full_path == "com.example.Baz"));
    }

    // ── visibility detection ─────────────────────────────────────────────────

    #[test]
    fn visibility_private_fun() {
        let data = parse_kotlin("class Foo {\n  private fun secret() {}\n  fun public() {}\n}");
        let secret = sym(&data, "secret").expect("secret not indexed");
        let public = sym(&data, "public").expect("public not indexed");
        assert_eq!(secret.visibility, Visibility::Private);
        assert_eq!(public.visibility, Visibility::Public);
    }

    #[test]
    fn visibility_protected_val() {
        let data = parse_kotlin("class Foo {\n  protected val x: Int = 0\n}");
        let x = sym(&data, "x").expect("x not indexed");
        assert_eq!(x.visibility, Visibility::Protected);
    }

    #[test]
    fn visibility_internal_class() {
        let data = parse_kotlin("internal class Bar");
        let bar = sym(&data, "Bar").expect("Bar not indexed");
        assert_eq!(bar.visibility, Visibility::Internal);
    }

    #[test]
    fn dot_completion_hides_private() {
        let vm_uri   = uri("/VM.kt");
        let repo_uri = uri("/Repo.kt");
        let idx = crate::indexer::Indexer::new();
        idx.index_content(&repo_uri,
            "package com.pkg\nclass Repo {\n  fun findAll() {}\n  private fun secret() {}\n}");
        idx.index_content(&vm_uri,
            "package com.pkg\nclass VM(\n  private val repo: Repo\n) {}");

        let items = idx.completions(&vm_uri, tower_lsp::lsp_types::Position::new(2, 24), true); // after "private val repo: Repo"
        // Trigger a dot completion manually through resolver
        let items = crate::resolver::complete_symbol(
            &idx, "", Some("repo"), &vm_uri, true
        );
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"findAll"), "findAll missing: {labels:?}");
        assert!(!labels.contains(&"secret"),  "private 'secret' should be hidden: {labels:?}");
    }

    #[test]
    fn java_package_extracted() {
        let data = parse_java("package cz.moneta.example;\npublic class Foo {}");
        assert_eq!(data.package.as_deref(), Some("cz.moneta.example"));
    }

    #[test]
    fn java_enum_constants_indexed() {
        let data = parse_java("package cz.moneta.example;\npublic enum EProductScreen { FLEXIKREDIT, SAVINGS }");
        assert_eq!(data.package.as_deref(), Some("cz.moneta.example"));
        assert_eq!(sym(&data, "EProductScreen").unwrap().kind, SymbolKind::ENUM);
        assert_eq!(sym(&data, "FLEXIKREDIT").unwrap().kind, SymbolKind::ENUM_MEMBER);
        assert_eq!(sym(&data, "SAVINGS").unwrap().kind,     SymbolKind::ENUM_MEMBER);
    }

    #[test]
    fn java_import_parsed() {
        let data = parse_java("import cz.moneta.data.compat.enums.product.EProductScreen;\nclass Foo {}");
        assert_eq!(data.imports.len(), 1);
        assert_eq!(data.imports[0].local_name, "EProductScreen");
        assert_eq!(data.imports[0].full_path,  "cz.moneta.data.compat.enums.product.EProductScreen");
    }

    #[test]
    fn java_constructor_indexed() {
        let data = parse_java("public class Foo {\n  public Foo(int x) {}\n}");
        let ctor = sym(&data, "Foo");
        // class Foo AND constructor Foo both parsed; at least one must be CONSTRUCTOR
        let has_ctor = data.symbols.iter().any(|s| s.name == "Foo" && s.kind == SymbolKind::CONSTRUCTOR);
        assert!(has_ctor, "constructor not found: {:?}", data.symbols.iter().map(|s| (&s.name, s.kind)).collect::<Vec<_>>());
        let _ = ctor;
    }

    #[test]
    fn java_static_final_field_is_constant() {
        let data = parse_java("public class Cfg {\n  public static final int MAX = 100;\n}");
        let sym = data.symbols.iter().find(|s| s.name == "MAX");
        assert!(sym.is_some(), "MAX not indexed");
        assert_eq!(sym.unwrap().kind, SymbolKind::CONSTANT, "expected CONSTANT for static final field");
    }

    #[test]
    fn java_instance_field_is_field() {
        let data = parse_java("public class Cfg {\n  private int count;\n}");
        let sym = data.symbols.iter().find(|s| s.name == "count");
        assert!(sym.is_some(), "count not indexed");
        assert_eq!(sym.unwrap().kind, SymbolKind::FIELD);
    }

    #[test]
    fn declared_names_includes_function_params() {
        let src = "private fun handle(resultState: ResultState.Success<List<Int>>) {\n  val other: Foo\n}";
        let names = extract_declared_names(&src.lines().map(String::from).collect::<Vec<_>>());
        assert!(names.contains(&"resultState".to_string()), "param not found: {names:?}");
        assert!(names.contains(&"other".to_string()), "local var not found: {names:?}");
    }

    #[test]
    fn declared_names_includes_multi_params() {
        let src = "fun foo(alpha: Int, betaValue: String, gamma: Foo)";
        let names = extract_declared_names(&src.lines().map(String::from).collect::<Vec<_>>());
        assert!(names.contains(&"alpha".to_string()),     "alpha missing: {names:?}");
        assert!(names.contains(&"betaValue".to_string()), "betaValue missing: {names:?}");
        assert!(names.contains(&"gamma".to_string()),     "gamma missing: {names:?}");
    }
}

