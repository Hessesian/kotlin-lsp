use std::cell::RefCell;
use std::sync::OnceLock;

use tree_sitter::{Node, Parser, Query, QueryCursor};
use tower_lsp::lsp_types::{Position, Range, SymbolKind};

use crate::indexer::NodeExt;
use crate::StrExt;
use crate::queries::{self, KOTLIN_DEFINITIONS, SWIFT_DEFINITIONS,
    KIND_SIMPLE_IDENT, KIND_TYPE_IDENT, KIND_IDENTIFIER, KIND_SCOPED_IDENT,
    KIND_USER_TYPE, KIND_FUN_DECL,
    KIND_CLASS_DECL, KIND_ENUM_DECL, KIND_INTERFACE_DECL,
    KIND_OBJECT_DECL, KIND_DELEGATION_SPEC,
    KIND_RECORD_DECL, KIND_METHOD_DECL, KIND_CTOR_DECL, KIND_FIELD_DECL,
    KIND_IMPORT_DECL, KIND_PACKAGE_DECL,
    KIND_SUPERCLASS, KIND_SUPER_INTERFACES, KIND_EXTENDS_INTERFACES, KIND_TYPE_LIST,
    KIND_PROTOCOL_DECL, KIND_INHERITANCE_SPEC};
use crate::types::{FileData, ImportEntry, SymbolEntry, SyntaxError, Visibility};

type MatchEntry = (usize, [Option<(String, Range, Range, Vec<String>)>; 2]);

// ─── cached query objects ────────────────────────────────────────────────────
//
// Query compilation (parsing the S-expression DSL + building the automaton) is
// expensive — O(query²) — and identical across every file parse.  Cache the
// compiled query *and* its capture indices in a process-wide OnceLock so we pay
// that cost once.  Query is Send+Sync in tree-sitter ≥0.22.

struct DefQueryCache {
    query:    Query,
    def_idx:  u32,
    name_idx: u32,
}

static KOTLIN_DEF_QUERY_CACHE: OnceLock<Option<DefQueryCache>> = OnceLock::new();
static SWIFT_DEF_QUERY_CACHE:  OnceLock<Option<DefQueryCache>> = OnceLock::new();

fn kotlin_def_query() -> Option<&'static DefQueryCache> {
    KOTLIN_DEF_QUERY_CACHE.get_or_init(|| {
        match Query::new(&tree_sitter_kotlin::language(), KOTLIN_DEFINITIONS) {
            Ok(query) => {
                let def_idx  = query.capture_index_for_name("def").unwrap_or(0);
                let name_idx = query.capture_index_for_name("name").unwrap_or(1);
                Some(DefQueryCache { query, def_idx, name_idx })
            }
            Err(e) => { log::error!("Kotlin definitions query compile error: {e}"); None }
        }
    }).as_ref()
}

fn swift_def_query() -> Option<&'static DefQueryCache> {
    SWIFT_DEF_QUERY_CACHE.get_or_init(|| {
        match Query::new(&tree_sitter_swift_bundled::language(), SWIFT_DEFINITIONS) {
            Ok(query) => {
                let def_idx  = query.capture_index_for_name("def").unwrap_or(0);
                let name_idx = query.capture_index_for_name("name").unwrap_or(1);
                Some(DefQueryCache { query, def_idx, name_idx })
            }
            Err(e) => { log::error!("Swift definitions query compile error: {e}"); None }
        }
    }).as_ref()
}

// ─── per-thread parser instances ─────────────────────────────────────────────
//
// Parser::new() + set_language() allocates internal state each time.  Re-using
// a Parser across parse() calls is safe — parse(content, None) with no prior
// tree passes no incremental state.  Thread-local storage gives each worker
// thread its own Parser without any locking overhead.

thread_local! {
    static KOTLIN_PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        let _ = p.set_language(&tree_sitter_kotlin::language());
        p
    });
    static SWIFT_PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        let _ = p.set_language(&tree_sitter_swift_bundled::language());
        p
    });
    static JAVA_PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        let _ = p.set_language(&tree_sitter_java::language());
        p
    });
}

// ─── public entry points ────────────────────────────────────────────────────

pub fn parse_kotlin(content: &str) -> FileData {
    let lines = std::sync::Arc::new(content.lines().map(str::to_owned).collect());
    let mut data = FileData { lines, ..Default::default() };

    let Some(tree) = KOTLIN_PARSER.with(|p| p.borrow_mut().parse(content, None)) else { return data; };

    let bytes = content.as_bytes();
    let root  = tree.root_node();

    // ── definitions ──────────────────────────────────────────────────────────
    let Some(qc) = kotlin_def_query() else { return data; };
    let mut cur = QueryCursor::new();
    let matches: Vec<MatchEntry> = cur
        .matches(&qc.query, root, bytes)
        .map(|m| map_def_captures(&m, qc.def_idx, qc.name_idx, bytes))
        .collect();

    // Deduplicate: multiple patterns can fire on the same node
    // (e.g. enum class matches both pattern 0 "enum" AND pattern 2 "class").
    let best = dedup_matches(&matches);
    push_def_symbols(best, queries::def_pattern_meta, visibility_at_line, &data.lines, &mut data.symbols);

    // ── package + imports (manual tree walk — avoids query overlap issues) ────
    data.extract_package_and_imports(root, bytes);

    // ── fun interface (tree-sitter parses these as ERROR + lambda_literal) ───
    data.extract_fun_interfaces(root, bytes);

    // ── supertype relationships (delegation specifiers) ──────────────────────
    data.extract_supers_kotlin(root, bytes);

    // ── declared_names: scan lines once for `ident:` patterns ───────────────
    data.declared_names = extract_declared_names(&data.lines);

    // ── syntax errors (ERROR / MISSING nodes) ────────────────────────────────
    data.syntax_errors = collect_syntax_errors(root, bytes);

    data
}

pub fn parse_java(content: &str) -> FileData {
    let lines = std::sync::Arc::new(content.lines().map(str::to_owned).collect());
    let mut data = FileData { lines, ..Default::default() };

    let Some(tree) = JAVA_PARSER.with(|p| p.borrow_mut().parse(content, None)) else { return data; };

    let bytes = content.as_bytes();
    let mut queue = vec![tree.root_node()];
    while let Some(node) = queue.pop() {
        data.extract_java(&node, bytes);
        data.extract_supers_java(&node, bytes);
        let mut cur = node.walk();
        for child in node.children(&mut cur) { queue.push(child); }
    }
    data.declared_names = extract_declared_names(&data.lines);
    data.syntax_errors = collect_syntax_errors(tree.root_node(), bytes);
    data
}

pub fn parse_swift(content: &str) -> FileData {
    let lines = std::sync::Arc::new(content.lines().map(str::to_owned).collect());
    let mut data = FileData { lines, ..Default::default() };

    let Some(tree) = SWIFT_PARSER.with(|p| p.borrow_mut().parse(content, None)) else { return data; };

    let bytes = content.as_bytes();
    let root  = tree.root_node();

    // ── definitions ──────────────────────────────────────────────────────────
    let Some(qc) = swift_def_query() else { return data; };
    let def_idx  = qc.def_idx;
    let name_idx = qc.name_idx;
    let mut cur = QueryCursor::new();
    let matches: Vec<MatchEntry> = cur
        .matches(&qc.query, root, bytes)
        .map(|m| {
            let (pidx, slot) = map_def_captures(&m, def_idx, name_idx, bytes);
            if pidx == queries::SWIFT_INIT_PATTERN_IDX && slot[0].is_none() {
                // init_declaration — no @name, synthesize "init"; type_params from @def node.
                let def_cap = m.captures.iter().find(|cap| cap.index == def_idx);
                if let Some(cap) = def_cap {
                    let dr = ts_to_lsp(cap.node.range());
                    let sel = Range::new(
                        Position::new(dr.start.line, dr.start.character),
                        Position::new(dr.start.line, dr.start.character + queries::SWIFT_INIT_NAME.len() as u32),
                    );
                    let type_params = cap.node.extract_type_params(bytes);
                    return (pidx, [Some((queries::SWIFT_INIT_NAME.to_owned(), dr, sel, type_params)), None]);
                }
            }
            (pidx, slot)
        })
        .collect();

    // Deduplicate: use same BTreeMap strategy as Kotlin parser.
    let best = dedup_matches(&matches);
    push_def_symbols(best, queries::swift_def_pattern_meta, swift_visibility_at_line, &data.lines, &mut data.symbols);

    // ── imports (manual tree walk — Swift imports are simpler) ────────────────
    data.extract_swift_imports(root, bytes);

    // ── supertype relationships (inheritance specifiers) ──────────────────────
    data.extract_supers_swift(root, bytes);

    // ── declared_names ───────────────────────────────────────────────────────
    data.declared_names = extract_declared_names(&data.lines);

    // ── syntax errors ────────────────────────────────────────────────────────
    data.syntax_errors = collect_syntax_errors(root, bytes);

    data
}

/// Dispatch to the correct parser based on file extension.
pub fn parse_by_extension(path: &str, content: &str) -> FileData {
    if path.ends_with(".swift") {
        parse_swift(content)
    } else if path.ends_with(".java") {
        parse_java(content)
    } else {
        parse_kotlin(content)
    }
}

// ─── shared query pipeline helpers ───────────────────────────────────────────

/// Extract def/name captures from a single `QueryMatch` into a `MatchEntry`.
///
/// Handles the common case shared by both Kotlin and Swift definition queries:
/// each match has a `@def` capture (full node range) and a `@name` capture
/// (identifier text + range).  Returns `[None, None]` when either is absent.
fn map_def_captures<'c, 't>(
    m: &tree_sitter::QueryMatch<'c, 't>,
    def_idx: u32,
    name_idx: u32,
    bytes: &[u8],
) -> MatchEntry {
    let pidx = m.pattern_index;
    let mut def_node:   Option<tree_sitter::Node> = None;
    let mut def_range:  Option<Range> = None;
    let mut name_text:  Option<String> = None;
    let mut name_range: Option<Range> = None;
    for cap in m.captures.iter() {
        if cap.index == def_idx {
            def_node  = Some(cap.node);
            def_range = Some(ts_to_lsp(cap.node.range()));
        } else if cap.index == name_idx {
            name_text  = cap.node.utf8_text_owned(bytes);
            name_range = Some(ts_to_lsp(cap.node.range()));
        }
    }
    let slot = if let (Some(dn), Some(dr), Some(nt), Some(nr)) = (def_node, def_range, name_text, name_range) {
        let type_params = dn.extract_type_params(bytes);
        [Some((nt, dr, nr, type_params)), None]
    } else {
        [None, None]
    };
    (pidx, slot)
}

/// Deduplicate a list of `MatchEntry` values by `@name` start position.
///
/// Multiple patterns can fire on the same node (e.g. an enum class matches both
/// the "enum class" pattern and the plain "class" pattern).  Keeps the entry
/// with the **lowest** pattern index — lower index = more specific pattern.
fn dedup_matches(
    matches: &[MatchEntry],
) -> std::collections::BTreeMap<(u32, u32), (usize, String, Range, Range, Vec<String>)> {
    let mut best = std::collections::BTreeMap::new();
    for (pidx, slot) in matches {
        if let Some((name, range, sel, type_params)) = slot[0].clone() {
            let key = (sel.start.line, sel.start.character);
            let is_better = best.get(&key).map(|(ep, _, _, _, _)| pidx < ep).unwrap_or(true);
            if is_better {
                best.insert(key, (*pidx, name, range, sel, type_params));
            }
        }
    }
    best
}

/// Convert a deduplicated match map into `SymbolEntry` values and append them
/// to `symbols`.  `pattern_meta` maps a pattern index to `(SymbolKind, label)`;
/// `vis_fn` detects the visibility modifier from source lines.
fn push_def_symbols(
    best: std::collections::BTreeMap<(u32, u32), (usize, String, Range, Range, Vec<String>)>,
    pattern_meta: fn(usize) -> (SymbolKind, Option<&'static str>),
    vis_fn: fn(&[String], usize) -> Visibility,
    lines: &[String],
    symbols: &mut Vec<SymbolEntry>,
) {
    for (_, (pidx, name, range, sel, type_params)) in best {
        let (kind, _) = pattern_meta(pidx);
        if kind != SymbolKind::NULL {
            let visibility = vis_fn(lines, sel.start.line as usize);
            let detail = extract_detail(lines, range.start.line, range.end.line);
            symbols.push(SymbolEntry { name, kind, visibility, range, selection_range: sel, detail, type_params });
        }
    }
}

// ─── Java extraction (manual traversal — Java grammar has named fields) ──────

fn first_identifier(node: &Node, bytes: &[u8]) -> Option<(String, Range)> {
    if let Some(n) = node.child_by_field_name("name") {
        if let Ok(t) = n.utf8_text(bytes) { return Some((t.to_owned(), ts_to_lsp(n.range()))); }
    }
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if matches!(child.kind(), "type_identifier" | "simple_identifier" | "identifier") {
            if let Ok(t) = child.utf8_text(bytes) {
                if !t.is_empty()
                    && t.chars().next().map(|c| c.is_alphabetic() || c == '_').unwrap_or(false)
                    && t.chars().all(|c| c.is_alphanumeric() || c == '_')
                { return Some((t.to_owned(), ts_to_lsp(child.range()))); }
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

/// Walk the tree-sitter tree and collect ERROR / MISSING nodes as syntax errors.
///
/// Uses `has_error()` to prune clean subtrees (no wasted traversal).
/// Recurses into ERROR children to find nested MISSING nodes (more precise),
/// but deduplicates by `(start_line, start_col)` and caps at `MAX_ERRORS`.
const MAX_SYNTAX_ERRORS: usize = 20;

/// Returns true if this ERROR node is actually a valid `fun interface` declaration
/// that tree-sitter-kotlin just doesn't parse correctly.
/// Structure: ERROR { "fun", user_type("interface"), simple_identifier }
fn is_fun_interface_error(node: &Node, bytes: &[u8]) -> bool {
    if !node.is_error() { return false; }
    let mut has_fun = false;
    let mut has_interface = false;
    let mut has_name = false;
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        match child.kind() {
            "fun" => has_fun = true,
            "user_type" => {
                if child.utf8_text(bytes).unwrap_or("") == "interface" {
                    has_interface = true;
                }
            }
            "simple_identifier" => has_name = true,
            _ => {
                // Variance case: `fun interface Foo<in A, out B>` produces a nested
                // ERROR child that swallows `fun`, `interface`, and the name together:
                //   ERROR { ERROR(user_type("interface"), simple_identifier("Foo")),
                //           type_parameters(...) }
                if child.is_error() {
                    let mut ec = child.walk();
                    for gc in child.children(&mut ec) {
                        match gc.kind() {
                            "fun" => has_fun = true,
                            "user_type" => {
                                if gc.utf8_text(bytes).unwrap_or("") == "interface" {
                                    has_interface = true;
                                }
                            }
                            "simple_identifier" => has_name = true,
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    has_fun && has_interface && has_name
}

/// Returns the interface name if this `function_declaration` is actually a misparse
/// of `[modifiers] fun interface Foo { ... }`.
///
/// When a visibility/annotation modifier precedes `fun interface`, tree-sitter
/// misinterprets it as an extension function on the `interface` type:
///   `function_declaration { modifiers, "fun", user_type("interface"), simple_identifier("Foo"), ERROR }`
/// A real extension function would have a `.` between receiver type and name; the
/// mis-parsed one does not. We detect it by: user_type child = "interface" AND
/// simple_identifier present after it (directly or as first child of ERROR).
/// Returns (name_start_byte, name_end_byte, node_range) or None.
fn fun_interface_name_from_fn_decl(node: &Node, bytes: &[u8]) -> Option<(usize, usize, tree_sitter::Range)> {
    if node.kind() != KIND_FUN_DECL { return None; }
    if !node.has_error() { return None; }
    let mut after_interface = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if after_interface {
            // Direct simple_identifier child (@annotation case: "simple_identifier Factory")
            if child.kind() == KIND_SIMPLE_IDENT {
                return Some((child.start_byte(), child.end_byte(), child.range()));
            }
            // ERROR child containing simple_identifier as first meaningful child
            // (internal case: ERROR { simple_identifier("IPairCodeParser"), "{", "fun", ... })
            if child.is_error() {
                let mut ec = child.walk();
                let info = child.children(&mut ec)
                    .next()
                    .filter(|c| c.kind() == KIND_SIMPLE_IDENT)
                    .map(|c| (c.start_byte(), c.end_byte(), c.range()));
                if let Some(loc) = info {
                    return Some(loc);
                }
            }
        }
        if child.kind() == KIND_USER_TYPE && child.utf8_text(bytes).unwrap_or("") == "interface" {
            after_interface = true;
        }
    }
    None
}

fn push_interface_symbol(name: &str, node: &Node, sel_node_range: tree_sitter::Range, bytes: &[u8], data: &mut FileData) {
    let visibility  = visibility_at_line(&data.lines, node.range().start_point.row);
    let range       = ts_to_lsp(node.range());
    let sel         = ts_to_lsp(sel_node_range);
    let detail      = extract_detail(&data.lines, range.start.line, range.end.line);
    let type_params = node.extract_type_params_or_error_child(bytes);
    data.symbols.push(SymbolEntry { name: name.to_owned(), kind: SymbolKind::INTERFACE, visibility, range, selection_range: sel, detail, type_params });
}

/// Walk the parse tree and emit INTERFACE symbols for every `fun interface Foo` declaration.
///
/// Tree-sitter produces two different misparsings depending on whether modifiers precede:
/// - No modifiers: ERROR("fun", user_type("interface"), simple_identifier("Foo"))
/// - With modifiers: function_declaration(modifiers, "fun", user_type("interface"),
///   simple_identifier("Foo"), ERROR(...))
fn extract_fun_interfaces(root: Node, bytes: &[u8], data: &mut FileData) {
    if !root.has_error() { return; }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        // Case 1: no-modifier `fun interface` → ERROR node
        if node.is_error() && is_fun_interface_error(&node, bytes) {
            // Simple case: simple_identifier is a direct child.
            let name_node = node.first_child_of_kind(KIND_SIMPLE_IDENT)
                // Variance case: name is inside a nested ERROR child.
                .or_else(|| {
                    let mut cur = node.walk();
                    let inner_error = node.children(&mut cur).find(|c| c.is_error());
                    drop(cur);
                    inner_error.and_then(|inner| inner.first_child_of_kind(KIND_SIMPLE_IDENT))
                });
            if let Some(child) = name_node {
                if let Ok(name) = child.utf8_text(bytes) {
                    push_interface_symbol(name, &node, child.range(), bytes, data);
                }
            }
            // Don't recurse further into ERROR children.
            continue;
        }
        // Case 2: modifier-prefixed `fun interface` → misparse as function_declaration
        if let Some((name_start, name_end, name_ts_range)) = fun_interface_name_from_fn_decl(&node, bytes) {
            if let Ok(name) = std::str::from_utf8(&bytes[name_start..name_end]) {
                let sel = ts_to_lsp(name_ts_range);
                // Remove the incorrectly-added function/method symbol (same name, same line).
                data.symbols.retain(|s| {
                    !(s.name == name && s.selection_range.start.line == sel.start.line
                      && matches!(s.kind, SymbolKind::FUNCTION | SymbolKind::METHOD))
                });
                push_interface_symbol(name, &node, name_ts_range, bytes, data);
            }
            // Still recurse into children to find nested fun interfaces.
        }
        // Recurse only into subtrees that contain errors.
        if node.has_error() || node.is_error() {
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                stack.push(child);
            }
        }
    }
}

/// Returns true if `node` or any of its (error-containing) descendants is a
/// `fun interface` misparse — either the ERROR shape or the function_declaration shape.
/// Prunes clean subtrees (`!has_error()`) for efficiency.
fn has_fun_interface_descendant(node: &Node, bytes: &[u8]) -> bool {
    if fun_interface_name_from_fn_decl(node, bytes).is_some()
        || is_fun_interface_error(node, bytes)
    {
        return true;
    }
    if !node.has_error() { return false; }
    let mut cursor = node.walk();
    let children: Vec<_> = node.children(&mut cursor).collect();
    drop(cursor);
    children.iter().any(|c| has_fun_interface_descendant(c, bytes))
}

fn collect_syntax_errors(root: Node, bytes: &[u8]) -> Vec<SyntaxError> {
    if !root.has_error() { return Vec::new(); }

    let mut errors = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![root];

    while let Some(node) = stack.pop() {
        if errors.len() >= MAX_SYNTAX_ERRORS { break; }

        if node.is_missing() {
            let range = ts_to_lsp(node.range());
            let key = (range.start.line, range.start.character);
            if seen.insert(key) {
                let kind = node.kind();
                errors.push(SyntaxError { range, message: format!("missing `{kind}`") });
            }
        } else if node.is_error() {
            // Skip errors that are actually valid `fun interface` declarations.
            if is_fun_interface_error(&node, bytes) {
                continue;
            }
            let range = ts_to_lsp(node.range());
            let key = (range.start.line, range.start.character);
            if seen.insert(key) {
                let text: String = node.utf8_text(bytes).unwrap_or("").chars().take(30).collect();
                let first_line = text.lines().next().unwrap_or(&text);
                errors.push(SyntaxError {
                    range,
                    message: if first_line.is_empty() { "syntax error".into() } else { format!("unexpected `{first_line}`") },
                });
            }
            // Recurse into ERROR children to find nested MISSING nodes.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        } else if node.has_error() {
            // Skip recursing into function_declarations that are misparse of `fun interface`.
            if fun_interface_name_from_fn_decl(&node, bytes).is_some() {
                continue;
            }
            // Only recurse into subtrees that contain errors.
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            // If any sibling contains a fun-interface misparse, lone `}` ERROR nodes are
            // cascading false positives from that misparse — suppress them.
            let has_fun_iface_sibling = children.iter()
                .any(|c| has_fun_interface_descendant(c, bytes));
            for child in children {
                if has_fun_iface_sibling && child.is_error() {
                    let text = child.utf8_text(bytes).unwrap_or("").trim();
                    if text == "}" { continue; }
                }
                stack.push(child);
            }
        }
        // else: clean subtree — skip entirely.
    }

    errors
}

/// Extract a short declaration signature from source lines.
///
/// Concatenates lines starting at `start_line`, strips leading whitespace,
/// and truncates at the first `{` or `=` that begins a body — leaving just
/// the declaration header.  Result is capped at 120 characters.
///
/// Examples:
///   `fun addBiometryToPowerAuth(isAllowedForActiveOp: Boolean): Boolean`
///   `class CreatePinViewModel @Inject constructor(`
///   `val isChecked: Boolean`
/// Maximum number of characters in an extracted detail string before truncation.
const MAX_DETAIL_CHARS: usize = 120;

pub(crate) fn extract_detail(lines: &[String], start_line: u32, end_line: u32) -> String {
    let start = start_line as usize;
    let end   = (end_line as usize + 1).min(lines.len());
    let mut collected = String::new();
    for line in &lines[start..end] {
        if !collected.is_empty() {
            collected.push(' ');
        }
        collected.push_str(line.trim_start());
        // Stop collecting when we hit the body opener or annotation-only lines.
        if collected.contains('{') || collected.contains(" = ") || collected.ends_with('=') {
            break;
        }
    }
    // Trim at body opener `{` or ` =`.
    let trimmed = if let Some(pos) = collected.find('{') {
        collected[..pos].trim_end().to_owned()
    } else if let Some(pos) = collected.find(" = ") {
        collected[..pos].trim_end().to_owned()
    } else {
        collected
    };
    // Strip trailing `)` then `: ReturnType` to keep it compact, or keep if short.
    // Cap at 120 chars.
    if trimmed.chars().count() > MAX_DETAIL_CHARS {
        let s: String = trimmed.chars().take(MAX_DETAIL_CHARS - 1).collect();
        format!("{}…", s)
    } else {
        trimmed
    }
}

// ─── package + import extraction ─────────────────────────────────────────────
//
// Uses a manual BFS rather than queries to avoid the pattern-overlap problem
// (plain-import query would also fire on star / alias imports).

const IMPORT_KW: &str = "import ";
const STATIC_KW: &str = "static ";
const IMPORT_ALIAS_KW: &str = " as ";

fn extract_package_and_imports(root: tree_sitter::Node, bytes: &[u8], data: &mut FileData) {
    // Only need the top of the file: package_header and import_list are always
    // direct children of source_file, so one pass over root's children suffices.
    let mut cur = root.walk();
    for node in root.children(&mut cur) {
        match node.kind() {
            "package_header" => {
                // (package_header "package" (identifier ...))
                if let Some(child) = node.first_child_of_kind(KIND_IDENTIFIER) {
                    data.package = child.utf8_text_owned(bytes);
                }
            }
            "import_list" => {
                for header in node.children_of_kind("import_header") {
                    parse_import_header(&header, bytes, data);
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
                path_text = child.utf8_text_owned(bytes);
            }
            "import_alias" => {
                // (import_alias "as" (type_identifier))
                alias_text = child.first_child_of_kind(KIND_TYPE_IDENT)
                    .and_then(|c| c.utf8_text_owned(bytes));
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
            alias_text.unwrap_or_else(|| full_path.last_segment().to_owned())
        };
        data.imports.push(ImportEntry { full_path, local_name, is_star });
    }
}

/// Lightweight import scanner for live (unsaved) buffer lines.
/// Handles: `import pkg.Name`, `import pkg.Name as Alias`, `import pkg.*`
/// Used by completion to read the current buffer state without a full re-parse.
pub fn parse_imports_from_lines(lines: &[String]) -> Vec<crate::types::ImportEntry> {
    let mut imports = Vec::new();
    for line in lines {
        let trimmed = line.trim_start();
        if !trimmed.starts_with(IMPORT_KW) { continue; }
        let rest_raw = trimmed[IMPORT_KW.len()..].trim();
        if rest_raw.is_empty() { continue; }
        // Strip inline comments (e.g. `import foo.Bar // generated`)
        let rest = if let Some(ci) = rest_raw.find("//") {
            rest_raw[..ci].trim_end()
        } else {
            rest_raw
        };
        if rest.is_empty() { continue; }
        // Trim optional trailing `;` (Java-style imports) and skip Java's `static` modifier.
        let rest = rest.trim_end_matches(';').trim_end();
        let rest = rest.strip_prefix(STATIC_KW).map(str::trim_start).unwrap_or(rest);
        let is_star = rest.ends_with(".*");
        let (path_part, alias) = if let Some(idx) = rest.find(IMPORT_ALIAS_KW) {
            (&rest[..idx], Some(rest[idx + IMPORT_ALIAS_KW.len()..].trim().to_owned()))
        } else {
            (rest, None)
        };
        let full_path = if is_star {
            path_part.strip_suffix(".*").unwrap_or(path_part).to_owned()
        } else {
            path_part.to_owned()
        };
        let local_name = if is_star {
            "*".to_owned()
        } else {
            alias.unwrap_or_else(|| {
                full_path.rsplit('.').next().unwrap_or(&full_path).to_owned()
            })
        };
        imports.push(crate::types::ImportEntry { full_path, local_name, is_star });
    }
    imports
}

// ─── Swift import extraction ─────────────────────────────────────────────────

/// Extract the import path text from an `import_declaration` node, if present.
fn swift_import_path<'a>(node: tree_sitter::Node<'a>, bytes: &'a [u8]) -> Option<&'a str> {
    node.first_child_of_kind(KIND_IDENTIFIER)
        .and_then(|c| c.utf8_text(bytes).ok())
}

fn extract_swift_imports(root: tree_sitter::Node, bytes: &[u8], data: &mut FileData) {
    let mut cur = root.walk();
    for node in root.children(&mut cur) {
        if node.kind() == "import_declaration" {
            if let Some(txt) = swift_import_path(node, bytes) {
                let local = txt.last_segment();
                data.imports.push(ImportEntry {
                    full_path:  txt.to_owned(),
                    local_name: local.to_owned(),
                    is_star:    false,
                });
            }
        }
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
    const KOTLIN_JAVA_MODS: &[(&str, Visibility)] = &[
        ("private",   Visibility::Private),
        ("protected", Visibility::Protected),
        ("internal",  Visibility::Internal),
    ];
    visibility_at_line_impl(lines, line_no, Visibility::Public, KOTLIN_JAVA_MODS)
}

/// Swift visibility detection.
///
/// Swift modifiers: `private`, `fileprivate`, `internal`, `public`, `open`.
/// Default is `internal` (unlike Kotlin which defaults to `public`).
pub(crate) fn swift_visibility_at_line(lines: &[String], line_no: usize) -> Visibility {
    const SWIFT_MODS: &[(&str, Visibility)] = &[
        ("private",     Visibility::Private),
        ("fileprivate", Visibility::Private),
        ("public",      Visibility::Public),
        ("open",        Visibility::Public),
    ];
    visibility_at_line_impl(lines, line_no, Visibility::Internal, SWIFT_MODS)
}

fn visibility_at_line_impl(
    lines:     &[String],
    line_no:   usize,
    default:   Visibility,
    modifiers: &[(&str, Visibility)],
) -> Visibility {
    let Some(line) = lines.get(line_no) else { return default };
    // Work only on the part before any `=`, `{`, or `(` to avoid false positives
    // from string literals / bodies.
    let decl = line.decl_prefix();
    for &(kw, vis) in modifiers {
        if contains_word(decl, kw) { return vis; }
    }
    default
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
                && word.starts_with_lowercase()
                && seen.insert(word.clone())
            {
                names.push(word);
            }
            rest = &rest[ci + 1..];
        }
    }
    names
}


// ─── supertype CST extraction ────────────────────────────────────────────────

// ─── FileData methods ────────────────────────────────────────────────────────

impl crate::types::FileData {
    fn extract_package_and_imports(&mut self, root: tree_sitter::Node, bytes: &[u8]) {
        extract_package_and_imports(root, bytes, self)
    }
    fn extract_fun_interfaces(&mut self, root: tree_sitter::Node, bytes: &[u8]) {
        extract_fun_interfaces(root, bytes, self)
    }
    fn extract_swift_imports(&mut self, root: tree_sitter::Node, bytes: &[u8]) {
        extract_swift_imports(root, bytes, self)
    }

    fn extract_supers_kotlin(&mut self, root: Node, bytes: &[u8]) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == KIND_CLASS_DECL || node.kind() == KIND_OBJECT_DECL {
                let name_line = node.name_line();
                for child in node.children_of_kind(KIND_DELEGATION_SPEC) {
                    if let Some((name, type_args)) = child.super_from_delegation(bytes) {
                        self.supers.push((name_line, name, type_args));
                    }
                }
            }
            let mut cur = node.walk();
            for child in node.children(&mut cur) { stack.push(child); }
        }
    }

    fn extract_supers_java(&mut self, node: &Node, bytes: &[u8]) {
        let kind = node.kind();
        if kind == KIND_CLASS_DECL || kind == KIND_RECORD_DECL || kind == KIND_ENUM_DECL {
            let name_line = node.name_line();
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                if child.kind() == KIND_SUPERCLASS {
                    if let Some(name) = child.java_first_type_name(bytes) {
                        self.supers.push((name_line, name, child.type_arg_strings(bytes)));
                    }
                } else if child.kind() == KIND_SUPER_INTERFACES {
                    for (name, type_args) in child.java_type_list(bytes) {
                        self.supers.push((name_line, name, type_args));
                    }
                }
            }
        } else if kind == KIND_INTERFACE_DECL {
            let name_line = node.name_line();
            if let Some(ext) = node.first_child_of_kind(KIND_EXTENDS_INTERFACES) {
                for (name, type_args) in ext.java_type_list(bytes) {
                    self.supers.push((name_line, name, type_args));
                }
            }
        }
    }

    fn extract_supers_swift(&mut self, root: Node, bytes: &[u8]) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == KIND_CLASS_DECL || node.kind() == KIND_PROTOCOL_DECL {
                let name_line = node.name_line();
                for spec in node.children_of_kind(KIND_INHERITANCE_SPEC) {
                    if let Some(ut) = spec.first_child_of_kind(KIND_USER_TYPE) {
                        if let Some(name) = ut.user_type_name(bytes) {
                            // Swift generics are structurally nested inside user_type,
                            // not via type_arguments, so type_args are empty here.
                            self.supers.push((name_line, name, Vec::new()));
                        }
                    }
                }
            }
            let mut cur = node.walk();
            for child in node.children(&mut cur) { stack.push(child); }
        }
    }

    fn extract_java(&mut self, node: &Node, bytes: &[u8]) {
        match node.kind() {
            KIND_PACKAGE_DECL => {
                let mut cur = node.walk();
                for child in node.children(&mut cur) {
                    if matches!(child.kind(), "scoped_identifier" | "identifier") {
                        if let Ok(txt) = child.utf8_text(bytes) {
                            self.package = Some(txt.to_owned());
                        }
                        break;
                    }
                }
            }
            KIND_CLASS_DECL     => self.push_named(node, bytes, SymbolKind::CLASS),
            KIND_RECORD_DECL    => self.push_named(node, bytes, SymbolKind::STRUCT),
            KIND_INTERFACE_DECL => self.push_named(node, bytes, SymbolKind::INTERFACE),
            "annotation_type_declaration" => self.push_named(node, bytes, SymbolKind::INTERFACE),
            KIND_ENUM_DECL      => self.push_named(node, bytes, SymbolKind::ENUM),
            KIND_METHOD_DECL    => self.push_named(node, bytes, SymbolKind::METHOD),
            KIND_CTOR_DECL      => self.push_named(node, bytes, SymbolKind::CONSTRUCTOR),
            "enum_constant"     => self.push_named(node, bytes, SymbolKind::ENUM_MEMBER),
            KIND_FIELD_DECL     => self.push_field_declaration(node, bytes),
            KIND_IMPORT_DECL    => self.push_java_import(node, bytes),
            _ => {}
        }
    }

    fn push_named(&mut self, node: &Node, bytes: &[u8], kind: SymbolKind) {
        if let Some((name, sel)) = first_identifier(node, bytes) {
            let visibility  = visibility_at_line(&self.lines, node.range().start_point.row);
            let range       = ts_to_lsp(node.range());
            let detail      = extract_detail(&self.lines, range.start.line, range.end.line);
            let type_params = node.extract_type_params(bytes);
            self.symbols.push(SymbolEntry { name, kind, visibility, range, selection_range: sel, detail, type_params });
        }
    }

    fn push_field_declaration(&mut self, node: &Node, bytes: &[u8]) {
        let kind = if node.first_child_of_kind("modifiers").is_some_and(|mods| {
            let found_kinds: Vec<&str> = (0..mods.child_count())
                .filter_map(|i| mods.child(i))
                .map(|c| c.kind())
                .collect();
            ["static", "final"].iter().all(|&req| found_kinds.contains(&req))
        }) {
            SymbolKind::CONSTANT
        } else {
            SymbolKind::FIELD
        };
        let nr     = ts_to_lsp(node.range());
        let vis    = visibility_at_line(&self.lines, node.range().start_point.row);
        let detail = extract_detail(&self.lines, nr.start.line, nr.end.line);
        for child in node.children_of_kind("variable_declarator") {
            if let Some((name, sel)) = first_identifier(&child, bytes) {
                self.symbols.push(SymbolEntry {
                    name, kind, visibility: vis, range: nr,
                    selection_range: sel, detail: detail.clone(),
                    type_params: Vec::new(),
                });
            }
        }
    }

    fn push_java_import(&mut self, node: &Node, bytes: &[u8]) {
        let mut cur = node.walk();
        for child in node.children(&mut cur) {
            if matches!(child.kind(), KIND_SCOPED_IDENT | KIND_IDENTIFIER) {
                if let Ok(txt) = child.utf8_text(bytes) {
                    let full_path  = txt.to_owned();
                    let local_name = full_path.last_segment().to_owned();
                    self.imports.push(ImportEntry { full_path, local_name, is_star: false });
                }
                return;
            }
        }
    }
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "parser_tests.rs"]
mod tests;
