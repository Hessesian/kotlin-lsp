use tree_sitter::{Node, Parser, Query, QueryCursor};
use tower_lsp::lsp_types::{Position, Range, SymbolKind};

use crate::indexer::NodeExt;
use crate::queries::{self, KOTLIN_DEFINITIONS, SWIFT_DEFINITIONS};
use crate::types::{FileData, ImportEntry, SymbolEntry, SyntaxError, Visibility};

type MatchEntry = (usize, [Option<(String, Range, Range)>; 2]);

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
    let matches: Vec<MatchEntry> = cur
        .matches(&def_q, root, bytes)
        .map(|m| map_def_captures(&m, def_idx, name_idx, bytes))
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
    let lang  = tree_sitter_java::language();
    let lines = std::sync::Arc::new(content.lines().map(str::to_owned).collect());
    let mut data = FileData { lines, ..Default::default() };

    let mut parser = Parser::new();
    if parser.set_language(&lang).is_err() { return data; }
    let Some(tree) = parser.parse(content, None) else { return data; };

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
    let lang  = tree_sitter_swift_bundled::language();
    let lines = std::sync::Arc::new(content.lines().map(str::to_owned).collect());
    let mut data = FileData { lines, ..Default::default() };

    let mut parser = Parser::new();
    if parser.set_language(&lang).is_err() { return data; }
    let Some(tree) = parser.parse(content, None) else { return data; };

    let bytes = content.as_bytes();
    let root  = tree.root_node();

    // ── definitions ──────────────────────────────────────────────────────────
    let def_q = match Query::new(&lang, SWIFT_DEFINITIONS) {
        Ok(q)  => q,
        Err(e) => { log::error!("Swift definitions query error: {e}"); return data; }
    };
    let def_idx  = def_q.capture_index_for_name("def").unwrap_or(0);
    let name_idx = def_q.capture_index_for_name("name").unwrap_or(1);

    let mut cur = QueryCursor::new();
    let matches: Vec<MatchEntry> = cur
        .matches(&def_q, root, bytes)
        .map(|m| {
            let (pidx, slot) = map_def_captures(&m, def_idx, name_idx, bytes);
            if pidx == queries::SWIFT_INIT_PATTERN_IDX && slot[0].is_none() {
                // init_declaration — no @name, synthesize "init"
                let def_range = m.captures.iter()
                    .find(|cap| cap.index == def_idx)
                    .map(|cap| ts_to_lsp(cap.node.range()));
                if let Some(dr) = def_range {
                    let sel = Range::new(
                        Position::new(dr.start.line, dr.start.character),
                        Position::new(dr.start.line, dr.start.character + queries::SWIFT_INIT_NAME.len() as u32),
                    );
                    return (pidx, [Some((queries::SWIFT_INIT_NAME.to_owned(), dr, sel)), None]);
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
    let mut def_range:  Option<Range> = None;
    let mut name_text:  Option<String> = None;
    let mut name_range: Option<Range> = None;
    for cap in m.captures.iter() {
        if cap.index == def_idx {
            def_range = Some(ts_to_lsp(cap.node.range()));
        } else if cap.index == name_idx {
            name_text  = cap.node.utf8_text_owned(bytes);
            name_range = Some(ts_to_lsp(cap.node.range()));
        }
    }
    let slot = if let (Some(dr), Some(nt), Some(nr)) = (def_range, name_text, name_range) {
        [Some((nt, dr, nr)), None]
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
) -> std::collections::BTreeMap<(u32, u32), (usize, String, Range, Range)> {
    let mut best = std::collections::BTreeMap::new();
    for (pidx, slot) in matches {
        if let Some((name, range, sel)) = slot[0].clone() {
            let key = (sel.start.line, sel.start.character);
            let is_better = best.get(&key).map(|(ep, _, _, _)| pidx < ep).unwrap_or(true);
            if is_better {
                best.insert(key, (*pidx, name, range, sel));
            }
        }
    }
    best
}

/// Convert a deduplicated match map into `SymbolEntry` values and append them
/// to `symbols`.  `pattern_meta` maps a pattern index to `(SymbolKind, label)`;
/// `vis_fn` detects the visibility modifier from source lines.
fn push_def_symbols(
    best: std::collections::BTreeMap<(u32, u32), (usize, String, Range, Range)>,
    pattern_meta: fn(usize) -> (SymbolKind, Option<&'static str>),
    vis_fn: fn(&[String], usize) -> Visibility,
    lines: &[String],
    symbols: &mut Vec<SymbolEntry>,
) {
    for (_, (pidx, name, range, sel)) in best {
        let (kind, _) = pattern_meta(pidx);
        if kind != SymbolKind::NULL {
            let visibility = vis_fn(lines, sel.start.line as usize);
            let detail = extract_detail(lines, range.start.line, range.end.line);
            symbols.push(SymbolEntry { name, kind, visibility, range, selection_range: sel, detail });
        }
    }
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
        "enum_constant" => push_named(node, bytes, SymbolKind::ENUM_MEMBER, data),
        "field_declaration"     => push_field_declaration(node, bytes, data),
        "import_declaration" => push_java_import(node, bytes, data),
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

fn push_field_declaration(node: &Node, bytes: &[u8], data: &mut FileData) {
    // Detect `static final` → CONSTANT, anything else → FIELD.
    let kind = if java_node_has_modifiers(node, &["static", "final"]) {
        SymbolKind::CONSTANT
    } else {
        SymbolKind::FIELD
    };
    let nr = ts_to_lsp(node.range());
    let vis = visibility_at_line(&data.lines, node.range().start_point.row);
    let detail = extract_detail(&data.lines, nr.start.line, nr.end.line);
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if child.kind() == "variable_declarator" {
            if let Some((name, sel)) = first_identifier(&child, bytes) {
                data.symbols.push(SymbolEntry {
                    name,
                    kind,
                    visibility: vis,
                    range: nr,
                    selection_range: sel,
                    detail: detail.clone(),
                });
            }
        }
    }
}

fn push_java_import(node: &Node, bytes: &[u8], data: &mut FileData) {
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if matches!(child.kind(), "scoped_identifier" | "identifier") {
            if let Ok(txt) = child.utf8_text(bytes) {
                let full_path  = txt.to_owned();
                let local_name = last_segment(&full_path).to_owned();
                data.imports.push(ImportEntry { full_path, local_name, is_star: false });
            }
            return;
        }
    }
}

fn push_named(node: &Node, bytes: &[u8], kind: SymbolKind, data: &mut FileData) {
    if let Some((name, sel)) = first_identifier(node, bytes) {
        let visibility = visibility_at_line(&data.lines, node.range().start_point.row);
        let range = ts_to_lsp(node.range());
        let detail = extract_detail(&data.lines, range.start.line, range.end.line);
        data.symbols.push(SymbolEntry { name, kind, visibility, range, selection_range: sel, detail });
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
            _ => {}
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
    if node.kind() != "function_declaration" { return None; }
    if !node.has_error() { return None; }
    let mut after_interface = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if after_interface {
            // Direct simple_identifier child (@annotation case: "simple_identifier Factory")
            if child.kind() == "simple_identifier" {
                return Some((child.start_byte(), child.end_byte(), child.range()));
            }
            // ERROR child containing simple_identifier as first meaningful child
            // (internal case: ERROR { simple_identifier("IPairCodeParser"), "{", "fun", ... })
            if child.is_error() {
                let mut ec = child.walk();
                let info = child.children(&mut ec)
                    .next()
                    .filter(|c| c.kind() == "simple_identifier")
                    .map(|c| (c.start_byte(), c.end_byte(), c.range()));
                if let Some(loc) = info {
                    return Some(loc);
                }
            }
        }
        if child.kind() == "user_type" && child.utf8_text(bytes).unwrap_or("") == "interface" {
            after_interface = true;
        }
    }
    None
}

fn push_interface_symbol(name: &str, node: &Node, sel_node_range: tree_sitter::Range, data: &mut FileData) {
    let visibility = visibility_at_line(&data.lines, node.range().start_point.row);
    let range      = ts_to_lsp(node.range());
    let sel        = ts_to_lsp(sel_node_range);
    let detail     = extract_detail(&data.lines, range.start.line, range.end.line);
    data.symbols.push(SymbolEntry { name: name.to_owned(), kind: SymbolKind::INTERFACE, visibility, range, selection_range: sel, detail });
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
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                if child.kind() == "simple_identifier" {
                    if let Ok(name) = child.utf8_text(bytes) {
                        push_interface_symbol(name, &node, child.range(), data);
                    }
                    break;
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
                push_interface_symbol(name, &node, name_ts_range, data);
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

fn report_missing_node(node: &Node, seen: &mut std::collections::HashSet<(u32, u32)>, errors: &mut Vec<SyntaxError>) {
    let range = ts_to_lsp(node.range());
    let key = (range.start.line, range.start.character);
    if seen.insert(key) {
        let kind = node.kind();
        errors.push(SyntaxError { range, message: format!("missing `{kind}`") });
    }
}

fn report_error_node(node: &Node, bytes: &[u8], seen: &mut std::collections::HashSet<(u32, u32)>, errors: &mut Vec<SyntaxError>) {
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
}

fn collect_syntax_errors(root: Node, bytes: &[u8]) -> Vec<SyntaxError> {
    if !root.has_error() { return Vec::new(); }

    let mut errors = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![root];

    while let Some(node) = stack.pop() {
        if errors.len() >= MAX_SYNTAX_ERRORS { break; }

        if node.is_missing() {
            report_missing_node(&node, &mut seen, &mut errors);
        } else if node.is_error() {
            // Skip errors that are actually valid `fun interface` declarations.
            if is_fun_interface_error(&node, bytes) {
                continue;
            }
            report_error_node(&node, bytes, &mut seen, &mut errors);
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
                path_text = child.utf8_text_owned(bytes);
            }
            "import_alias" => {
                // (import_alias "as" (type_identifier))
                let mut ac = child.walk();
                for ac_child in child.children(&mut ac) {
                    if ac_child.kind() == "type_identifier" {
                        alias_text = ac_child.utf8_text_owned(bytes);
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

/// Lightweight import scanner for live (unsaved) buffer lines.
/// Handles: `import pkg.Name`, `import pkg.Name as Alias`, `import pkg.*`
/// Used by completion to read the current buffer state without a full re-parse.
pub fn parse_imports_from_lines(lines: &[String]) -> Vec<crate::types::ImportEntry> {
    let mut imports = Vec::new();
    for line in lines {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("import ") { continue; }
        let rest_raw = trimmed["import ".len()..].trim();
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
        let rest = rest.strip_prefix("static ").map(str::trim_start).unwrap_or(rest);
        let is_star = rest.ends_with(".*");
        let (path_part, alias) = if let Some(idx) = rest.find(" as ") {
            (&rest[..idx], Some(rest[idx + " as ".len()..].trim().to_owned()))
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
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if child.kind() == "identifier" {
            return child.utf8_text(bytes).ok();
        }
    }
    None
}

fn extract_swift_imports(root: tree_sitter::Node, bytes: &[u8], data: &mut FileData) {
    let mut cur = root.walk();
    for node in root.children(&mut cur) {
        if node.kind() == "import_declaration" {
            if let Some(txt) = swift_import_path(node, bytes) {
                let local = last_segment(txt);
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

/// Returns the portion of `line` before any `{` or `=` — the modifiers region.
fn decl_prefix(line: &str) -> &str {
    line.split_once('{').map(|(l, _)| l)
        .unwrap_or(line)
        .split_once('=').map(|(l, _)| l)
        .unwrap_or(line)
}

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
    let decl = decl_prefix(line);

    // Check whole-word tokens.
    if contains_word(decl, "private")   { return Visibility::Private; }
    if contains_word(decl, "protected") { return Visibility::Protected; }
    if contains_word(decl, "internal")  { return Visibility::Internal; }
    Visibility::Public
}

/// Swift visibility detection.
///
/// Swift modifiers: `private`, `fileprivate`, `internal`, `public`, `open`.
/// Default is `internal` (unlike Kotlin which defaults to `public`).
pub(crate) fn swift_visibility_at_line(lines: &[String], line_no: usize) -> Visibility {
    let line = match lines.get(line_no) {
        Some(l) => l,
        None    => return Visibility::Internal,
    };
    let decl = decl_prefix(line);

    if contains_word(decl, "private")     { return Visibility::Private; }
    if contains_word(decl, "fileprivate") { return Visibility::Private; }
    if contains_word(decl, "public")      { return Visibility::Public; }
    if contains_word(decl, "open")        { return Visibility::Public; }
    Visibility::Internal // Swift default
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
                && crate::indexer::starts_with_lowercase(&word)
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

/// Walk the Kotlin CST and populate `data.supers` with `(class_name_line, supertype_name)`
/// for every `class_declaration` and `object_declaration` that has delegation specifiers.
fn extract_supers_kotlin(root: Node, bytes: &[u8], data: &mut FileData) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "class_declaration" | "object_declaration" => {
                let name_line = node.name_line();
                let mut cur = node.walk();
                for child in node.children(&mut cur) {
                    if child.kind() == "delegation_specifier" {
                        if let Some(name) = super_name_from_delegation(&child, bytes) {
                            data.supers.push((name_line, name));
                        }
                    }
                }
            }
            _ => {}
        }
        let mut cur = node.walk();
        for child in node.children(&mut cur) { stack.push(child); }
    }
}

/// Walk the Java CST and populate `data.supers` for class/interface/enum/record
/// declarations that extend or implement other types.
fn extract_supers_java(node: &Node, bytes: &[u8], data: &mut FileData) {
    match node.kind() {
        "class_declaration" | "record_declaration" | "enum_declaration" => {
            let name_line = node.name_line();
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                match child.kind() {
                    "superclass" => {
                        // (superclass "extends" _type)
                        if let Some(name) = java_first_type_name(&child, bytes) {
                            data.supers.push((name_line, name));
                        }
                    }
                    "super_interfaces" => {
                        // (super_interfaces "implements" (type_list _type, ...))
                        java_collect_type_list(&child, bytes, name_line, data);
                    }
                    _ => {}
                }
            }
        }
        "interface_declaration" => {
            let name_line = node.name_line();
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                if child.kind() == "extends_interfaces" {
                    // (extends_interfaces "extends" (type_list ...))
                    java_collect_type_list(&child, bytes, name_line, data);
                }
            }
        }
        _ => {}
    }
}

/// Returns the name of the first `user_type` child of `node`, if any.
fn first_user_type_child_name(node: &Node, bytes: &[u8]) -> Option<String> {
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if child.kind() == "user_type" {
            return user_type_name(&child, bytes);
        }
    }
    None
}

/// Extract the supertype name from a `delegation_specifier` node.
fn super_name_from_delegation(node: &Node, bytes: &[u8]) -> Option<String> {
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        match child.kind() {
            "constructor_invocation" | "explicit_delegation" => {
                return first_user_type_child_name(&child, bytes);
            }
            "user_type" => {
                return user_type_name(&child, bytes);
            }
            _ => {}
        }
    }
    None
}

/// Collect the identifier segments of a `user_type` node, ignoring
/// `type_arguments` subtrees, so generics don't interfere with dotted paths.
/// `Bar<Event, State>` → `["Bar"]`;  `Outer<T>.Inner` → `["Outer", "Inner"]`.
fn collect_user_type_segments(node: &Node, bytes: &[u8], segments: &mut Vec<String>) {
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        match child.kind() {
            "type_arguments" => {}  // skip generic parameters entirely
            "simple_identifier" | "type_identifier" | "identifier" => {
                if let Ok(text) = child.utf8_text(bytes) {
                    let text = text.trim();
                    if !text.is_empty() { segments.push(text.to_owned()); }
                }
            }
            _ if child.is_named() => collect_user_type_segments(&child, bytes, segments),
            _ => {}
        }
    }
}

/// Get the canonical type name from a `user_type` node, stripping generic args.
/// `Bar<Event, State>` → `"Bar"`;  `Outer<T>.Inner` → `"Outer.Inner"`.
fn user_type_name(node: &Node, bytes: &[u8]) -> Option<String> {
    let mut segments = Vec::new();
    collect_user_type_segments(node, bytes, &mut segments);
    if segments.is_empty() { None } else { Some(segments.join(".")) }
}

/// Extract the outermost type name from a Java type node.
///
/// Handles leaf `type_identifier`, `scoped_type_identifier`, and wrapper nodes
/// like `generic_type` (`Base<String>` → `"Base"`) by descending until
/// a `type_identifier` is found. `type_arguments` nodes are skipped so generic
/// parameters don't shadow the base type name.
fn java_first_type_name(node: &Node, bytes: &[u8]) -> Option<String> {
    let mut stack = vec![*node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "type_identifier" => {
                return n.utf8_text_owned(bytes);
            }
            "scoped_type_identifier" => {
                // Return the full dotted name (e.g. `pkg.Base`), stripping any trailing
                // generic args that may appear inside the scope chain.
                let text = n.utf8_text(bytes).ok()?;
                let name = text.split('<').next().unwrap_or(text).trim();
                return if name.is_empty() { None } else { Some(name.to_owned()) };
            }
            // Skip type_arguments entirely — they contain the generic params, not the base name.
            "type_arguments" => continue,
            _ => {}
        }
        let mut cur = n.walk();
        for child in n.children(&mut cur) {
            if child.is_named() { stack.push(child); }
        }
    }
    None
}

/// Walk a `super_interfaces` or `extends_interfaces` node, collecting all type names
/// from its `type_list` child into `data.supers`.
fn java_collect_type_list(node: &Node, bytes: &[u8], name_line: u32, data: &mut FileData) {
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if child.kind() == "type_list" {
            let mut cc = child.walk();
            for type_node in child.children(&mut cc) {
                // type_list children may be leaf type_identifier nodes directly,
                // or wrapper nodes (generic_type, scoped_type_identifier) containing one.
                let name = if type_node.kind() == "type_identifier" {
                    type_node.utf8_text_owned(bytes)
                } else {
                    java_first_type_name(&type_node, bytes)
                };
                if let Some(n) = name { data.supers.push((name_line, n)); }
            }
        }
    }
}

// ─── FileData methods (thin wrappers around the free functions above) ────────

impl crate::types::FileData {
    fn extract_package_and_imports(&mut self, root: tree_sitter::Node, bytes: &[u8]) {
        extract_package_and_imports(root, bytes, self)
    }
    fn extract_fun_interfaces(&mut self, root: tree_sitter::Node, bytes: &[u8]) {
        extract_fun_interfaces(root, bytes, self)
    }
    fn extract_supers_kotlin(&mut self, root: tree_sitter::Node, bytes: &[u8]) {
        extract_supers_kotlin(root, bytes, self)
    }
    fn extract_java(&mut self, node: &Node, bytes: &[u8]) {
        extract_java(node, bytes, self)
    }
    fn extract_supers_java(&mut self, node: &Node, bytes: &[u8]) {
        extract_supers_java(node, bytes, self)
    }
    fn extract_swift_imports(&mut self, root: tree_sitter::Node, bytes: &[u8]) {
        extract_swift_imports(root, bytes, self)
    }
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
    #[test] fn fun_interface() {
        let data = parse_kotlin("fun interface Action {\n    fun invoke(value: String)\n}");
        assert_eq!(sym(&data, "Action").unwrap().kind, SymbolKind::INTERFACE,
            "fun interface should be indexed as INTERFACE");
    }
    #[test] fn fun_interface_internal() {
        let data = parse_kotlin("internal fun interface IPairCodeParser {\n    fun parse(input: String): String\n}");
        assert_eq!(sym(&data, "IPairCodeParser").unwrap().kind, SymbolKind::INTERFACE,
            "internal fun interface should be indexed as INTERFACE");
    }
    #[test] fn fun_interface_generic() {
        let data = parse_kotlin("fun interface Router<Effect> {\n    fun route(effect: Effect)\n}");
        assert_eq!(sym(&data, "Router").unwrap().kind, SymbolKind::INTERFACE,
            "generic fun interface should be indexed as INTERFACE");
    }
    #[test] fn fun_interface_nested() {
        let data = parse_kotlin("class LoanReducer {\n    @AssistedFactory\n    fun interface Factory {\n        fun create(x: Int): String\n    }\n}");
        assert_eq!(sym(&data, "Factory").unwrap().kind, SymbolKind::INTERFACE,
            "nested fun interface should be indexed as INTERFACE");
    }
    #[test] fn object_decl()  { assert_eq!(sym(&parse_kotlin("object Obj"),        "Obj").unwrap().kind, SymbolKind::OBJECT); }
    #[test] fn data_class()   { assert_eq!(sym(&parse_kotlin("data class D(val x: Int)"), "D").unwrap().kind, SymbolKind::STRUCT); }
    #[test] fn enum_class()   { assert_eq!(sym(&parse_kotlin("enum class Color { RED }"), "Color").unwrap().kind, SymbolKind::ENUM); }

    #[test]
    fn dump_fun_interface_tree() {
        let content = "fun interface Action {\n    fun invoke(value: String)\n}";
        let lang = tree_sitter_kotlin::language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(content, None).unwrap();
        fn walk(node: tree_sitter::Node<'_>, src: &[u8], depth: usize) {
            let snippet = &src[node.start_byte()..node.end_byte().min(node.start_byte()+40)];
            eprintln!("{}{} {:?}", "  ".repeat(depth), node.kind(), String::from_utf8_lossy(snippet));
            for i in 0..node.child_count() {
                walk(node.child(i).unwrap(), src, depth+1);
            }
        }
        walk(tree.root_node(), content.as_bytes(), 0);
        // This test just dumps — it always passes. Check stderr output.
    }

    #[test]
    fn dump_fun_interface_internal_tree() {
        let content = "internal fun interface IPairCodeParser {\n    fun parse(input: String): String\n}";
        let lang = tree_sitter_kotlin::language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(content, None).unwrap();
        fn walk(node: tree_sitter::Node<'_>, src: &[u8], depth: usize) {
            let snippet = &src[node.start_byte()..node.end_byte().min(node.start_byte()+40)];
            eprintln!("{}{} {:?}", "  ".repeat(depth), node.kind(), String::from_utf8_lossy(snippet));
            for i in 0..node.child_count() {
                walk(node.child(i).unwrap(), src, depth+1);
            }
        }
        walk(tree.root_node(), content.as_bytes(), 0);
    }

    #[test]
    fn dump_fun_interface_nested_tree() {
        let content = "class LoanReducer {\n    @AssistedFactory\n    fun interface Factory {\n        fun create(x: Int): String\n    }\n}";
        let lang = tree_sitter_kotlin::language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(content, None).unwrap();
        fn walk(node: tree_sitter::Node<'_>, src: &[u8], depth: usize) {
            let snippet = &src[node.start_byte()..node.end_byte().min(node.start_byte()+40)];
            eprintln!("{}{} {:?}", "  ".repeat(depth), node.kind(), String::from_utf8_lossy(snippet));
            for i in 0..node.child_count() {
                walk(node.child(i).unwrap(), src, depth+1);
            }
        }
        walk(tree.root_node(), content.as_bytes(), 0);
    }
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

        let (items, _) = idx.completions(&vm_uri, tower_lsp::lsp_types::Position::new(2, 24), true); // after "private val repo: Repo"
        // Trigger a dot completion manually through resolver
        let (items, _) = crate::resolver::complete_symbol(
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

    // ── Syntax error detection tests ─────────────────────────────────────────

    #[test]
    fn no_errors_on_valid_kotlin() {
        let data = parse_kotlin("package com.example\nclass Foo { fun bar() {} }");
        assert!(data.syntax_errors.is_empty(), "expected no errors: {:?}", data.syntax_errors);
    }

    #[test]
    fn missing_closing_brace_kotlin() {
        let data = parse_kotlin("class Foo {\n    fun bar() {}\n");
        assert!(!data.syntax_errors.is_empty(), "expected errors for unclosed brace");
    }

    #[test]
    fn missing_closing_paren_kotlin() {
        let data = parse_kotlin("fun foo(x: Int {\n}");
        assert!(!data.syntax_errors.is_empty(), "expected errors for unclosed paren");
    }

    #[test]
    fn dangling_equals_kotlin() {
        let data = parse_kotlin("val x =\n");
        assert!(!data.syntax_errors.is_empty(), "expected errors for dangling =");
    }

    #[test]
    fn garbled_syntax_kotlin() {
        let data = parse_kotlin("class @@@ invalid!!! {{{");
        assert!(!data.syntax_errors.is_empty(), "expected errors for garbled syntax");
    }

    #[test]
    fn no_errors_on_valid_java() {
        let data = parse_java("package com.example;\npublic class Foo { void bar() {} }");
        assert!(data.syntax_errors.is_empty(), "expected no errors: {:?}", data.syntax_errors);
    }

    #[test]
    fn missing_semicolon_java() {
        let data = parse_java("public class Foo { int x = 5 }");
        assert!(!data.syntax_errors.is_empty(), "expected errors for missing semicolon");
    }

    #[test]
    fn error_message_contains_context() {
        let data = parse_kotlin("fun foo(x: Int { }");
        let msgs: Vec<&str> = data.syntax_errors.iter().map(|e| e.message.as_str()).collect();
        assert!(
            msgs.iter().any(|m| m.contains("missing") || m.contains("unexpected")),
            "error messages should be descriptive: {msgs:?}"
        );
    }

    #[test]
    fn errors_capped_at_max() {
        // Generate a file with many syntax errors.
        let bad = (0..50).map(|_| "@@@ ").collect::<String>();
        let data = parse_kotlin(&bad);
        assert!(
            data.syntax_errors.len() <= super::MAX_SYNTAX_ERRORS,
            "expected at most {} errors, got {}",
            super::MAX_SYNTAX_ERRORS, data.syntax_errors.len()
        );
    }

    #[test]
    fn error_has_correct_line() {
        let src = "class Foo {\n    fun bar() {}\n    val x =\n}";
        let data = parse_kotlin(src);
        assert!(!data.syntax_errors.is_empty());
        // The error should be on or near line 2 (0-indexed) where `val x =` is.
        let has_line_2_or_3 = data.syntax_errors.iter().any(|e|
            e.range.start.line == 2 || e.range.start.line == 3
        );
        assert!(has_line_2_or_3, "error should be near line 2-3: {:?}", data.syntax_errors);
    }

    // ── Swift parsing ────────────────────────────────────────────────────────

    #[test]
    fn swift_query_compiles() {
        let lang = tree_sitter_swift_bundled::language();
        tree_sitter::Query::new(&lang, crate::queries::SWIFT_DEFINITIONS)
            .expect("SWIFT_DEFINITIONS query should compile");
    }

    #[test] fn swift_class()    { assert_eq!(sym(&parse_swift("class Foo {}"),    "Foo").unwrap().kind, SymbolKind::CLASS); }
    #[test] fn swift_struct()   { assert_eq!(sym(&parse_swift("struct Bar {}"),   "Bar").unwrap().kind, SymbolKind::STRUCT); }
    #[test] fn swift_enum()     { assert_eq!(sym(&parse_swift("enum Dir { case n }"), "Dir").unwrap().kind, SymbolKind::ENUM); }
    #[test] fn swift_protocol() { assert_eq!(sym(&parse_swift("protocol P {}"),   "P").unwrap().kind, SymbolKind::INTERFACE); }
    #[test] fn swift_func()     { assert_eq!(sym(&parse_swift("func foo() {}"),   "foo").unwrap().kind, SymbolKind::FUNCTION); }
    #[test] fn swift_typealias(){ assert_eq!(sym(&parse_swift("typealias A = Int"), "A").unwrap().kind, SymbolKind::CLASS); }

    #[test]
    fn swift_property_let() {
        let data = parse_swift("let x = 42");
        assert_eq!(sym(&data, "x").unwrap().kind, SymbolKind::PROPERTY);
    }

    #[test]
    fn swift_property_var() {
        let data = parse_swift("var y: Int = 0");
        assert_eq!(sym(&data, "y").unwrap().kind, SymbolKind::PROPERTY);
    }

    #[test]
    fn swift_enum_entries() {
        let data = parse_swift("enum Dir { case north, south, east }");
        assert!(sym(&data, "north").is_some());
        assert!(sym(&data, "south").is_some());
        assert!(sym(&data, "east").is_some());
    }

    #[test]
    fn swift_extension() {
        let data = parse_swift("extension Point: Equatable { func dist() -> Double { 0 } }");
        let ext = sym(&data, "Point").unwrap();
        assert_eq!(ext.kind, SymbolKind::CLASS);
        assert!(sym(&data, "dist").is_some());
    }

    #[test]
    fn swift_init() {
        let data = parse_swift("class Foo { init(x: Int) { } }");
        assert!(sym(&data, "init").is_some());
    }

    #[test]
    fn swift_imports() {
        let data = parse_swift("import Foundation\nimport UIKit\nclass A {}");
        assert_eq!(data.imports.len(), 2);
        assert_eq!(data.imports[0].full_path, "Foundation");
        assert_eq!(data.imports[1].full_path, "UIKit");
    }

    #[test]
    fn swift_no_package() {
        let data = parse_swift("class A {}");
        assert!(data.package.is_none());
    }

    #[test]
    fn swift_visibility() {
        let data = parse_swift("private class Secret {}\npublic class Pub {}");
        assert_eq!(sym(&data, "Secret").unwrap().visibility, Visibility::Private);
        assert_eq!(sym(&data, "Pub").unwrap().visibility, Visibility::Public);
    }

    #[test]
    fn swift_default_visibility_is_internal() {
        let data = parse_swift("class Foo {}");
        assert_eq!(sym(&data, "Foo").unwrap().visibility, Visibility::Internal);
    }

    #[test]
    fn swift_detail_extraction() {
        let data = parse_swift("func distance(to other: Point) -> Double { 0 }");
        let s = sym(&data, "distance").unwrap();
        assert!(s.detail.contains("distance"), "detail: {}", s.detail);
    }

    #[test]
    fn swift_syntax_errors() {
        let data = parse_swift("class Foo {\n    func bar() {}\n    let x =\n}");
        assert!(!data.syntax_errors.is_empty(), "should detect syntax error");
    }

    #[test]
    fn parse_by_extension_dispatch() {
        let kt = parse_by_extension("/Foo.kt", "class Foo");
        let java = parse_by_extension("/Foo.java", "public class Foo {}");
        let swift = parse_by_extension("/Foo.swift", "class Foo {}");
        assert!(sym(&kt, "Foo").is_some());
        assert!(sym(&java, "Foo").is_some());
        assert!(sym(&swift, "Foo").is_some());
    }

    #[test]
    fn loan_reducer_no_false_errors() {
        // @AssistedFactory fun interface Factory inside a class should not
        // produce a false "missing bracket" syntax error.
        let src = r#"
class LoanReducer {
  @AssistedFactory
  fun interface Factory {
    fun create(
      reloadAction: (loanId: String, isWustenrot: Boolean) -> Unit,
      mapSheet: (LoanDetail) -> ProductDetailSheetModel,
    ): LoanReducer
  }
}
"#;
        let data = parse_kotlin(src);
        assert!(data.syntax_errors.is_empty(),
            "Expected no syntax errors, got: {:?}", data.syntax_errors);
    }

    #[test]
    fn swift_nested_enum_in_class() {
        let src = "final class DPSChangeVictoryViewModel: SimpleVictoryViewModel, @unchecked Sendable {\n    let coordinator: DPSCoordinator\n    func update(kind: DPSCoordinator.Kind) {}\n}\n\nclass DPSCoordinator {\n    enum Kind {\n        case victory\n        case defeat\n    }\n}";
        let data = parse_swift(src);
        let names: Vec<&str> = data.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(sym(&data, "DPSChangeVictoryViewModel").unwrap().kind, SymbolKind::CLASS,
            "DPSChangeVictoryViewModel should be CLASS; symbols: {names:?}");
        assert!(sym(&data, "Kind").is_some(), "nested Kind enum should be indexed; got: {names:?}");
        assert_eq!(sym(&data, "Kind").unwrap().kind, SymbolKind::ENUM, "Kind should be ENUM");
        assert!(sym(&data, "victory").is_some(), "enum cases should be indexed; got: {names:?}");
    }

    #[test]
    fn dedup_matches_lower_pidx_wins() {
        use tower_lsp::lsp_types::{Position, Range};
        let sel   = Range::new(Position::new(1, 0), Position::new(1, 3));
        let range = sel;
        let matches: Vec<MatchEntry> = vec![
            (2, [Some(("Foo".into(), range, sel)), None]),
            (0, [Some(("Foo".into(), range, sel)), None]),
        ];
        let best = dedup_matches(&matches);
        assert_eq!(best.len(), 1);
        assert_eq!(best.values().next().unwrap().0, 0, "pidx 0 should win over pidx 2");
    }
}
