//! Fill missing `when` branches for sealed classes and enums.
//!
//! This module provides a code action that detects an incomplete `when` expression
//! over a sealed class or enum, and generates the missing branches.
//!
//! Entry point: [`build_fill_when_action`].

use tower_lsp::lsp_types::*;

use crate::indexer::live_tree::utf16_col_to_byte;
use crate::indexer::Indexer;
use crate::queries::{
    KIND_BOOLEAN_LITERAL, KIND_CLASS_DECL, KIND_CLASS_PARAM, KIND_ELSE, KIND_FUN_DECL,
    KIND_FUN_VALUE_PARAMS, KIND_LBRACE, KIND_NAV_EXPR, KIND_NAV_SUFFIX, KIND_NULLABLE_TYPE,
    KIND_PARAMETER, KIND_PRIMARY_CTOR, KIND_PROP_DECL, KIND_RBRACE, KIND_SIMPLE_IDENT,
    KIND_STATEMENTS, KIND_TYPE_IDENT, KIND_TYPE_TEST, KIND_USER_TYPE, KIND_VAR_DECL,
    KIND_WHEN_CONDITION, KIND_WHEN_ENTRY, KIND_WHEN_EXPR, KIND_WHEN_SUBJECT,
};

/// Analysis result for incomplete when expressions — shared by code actions and diagnostics.
struct WhenAnalysis<'a> {
    when_node: tree_sitter::Node<'a>,
    subject_type: String,
    type_kind: TypeKind,
    missing: Vec<WhenMember>,
}

/// Analyze a single when expression for missing branches.
fn analyze_when<'a>(
    indexer: &Indexer,
    uri: &Url,
    when_node: tree_sitter::Node<'a>,
    source_bytes: &[u8],
) -> Option<WhenAnalysis<'a>> {
    let subject_node = when_node
        .children(&mut when_node.walk())
        .find(|c| c.kind() == KIND_WHEN_SUBJECT)?;

    let subject_var = extract_subject_identifier(&subject_node, source_bytes)?;

    let subject_type = resolve_subject_type_from_cst(&when_node, &subject_var, source_bytes)
        .or_else(|| crate::resolver::infer::infer_variable_type(indexer, &subject_var, uri))?;
    let subject_type = strip_nullable(&subject_type).to_string();

    let (type_kind, members) = resolve_type_members(indexer, &subject_type)?;

    let existing = collect_existing_branches(&when_node, source_bytes);

    if existing.iter().any(|b| b == "else") {
        return None;
    }

    let missing: Vec<WhenMember> = members
        .into_iter()
        .filter(|m| !existing.contains(&m.name))
        .collect();

    if missing.is_empty() {
        return None;
    }

    Some(WhenAnalysis {
        when_node,
        subject_type,
        type_kind,
        missing,
    })
}

/// Try to build a "fill missing when branches" code action for the cursor position.
///
/// Returns `None` if the cursor is not inside a `when` expression, the subject type
/// cannot be resolved, or all branches are already covered.
pub(crate) fn build_fill_when_action(
    indexer: &Indexer,
    uri: &Url,
    range: Range,
) -> Option<CodeActionOrCommand> {
    let live_doc = indexer.live_doc(uri)?;
    let source_bytes = &live_doc.bytes;
    let lines = indexer.mem_lines_for(uri.as_str())?;

    let cursor_byte = byte_offset_for_position(&lines, range.start)?;
    let when_node = find_enclosing_when(&live_doc.tree, source_bytes, cursor_byte)?;

    let analysis = analyze_when(indexer, uri, when_node, source_bytes)?;

    let indent = detect_indent(&analysis.when_node, source_bytes);
    let (replace_range, brace_indent) =
        find_insert_position(&analysis.when_node, source_bytes, &lines)?;
    let missing_refs: Vec<&WhenMember> = analysis.missing.iter().collect();
    let mut insert_text = build_branch_text(
        &missing_refs,
        &analysis.subject_type,
        analysis.type_kind,
        &indent,
    );
    insert_text.push_str(&brace_indent);
    insert_text.push('}');

    let edit = TextEdit {
        range: replace_range,
        new_text: insert_text,
    };

    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    let action = CodeAction {
        title: format!("Fill missing '{}' branches", analysis.subject_type),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    };

    Some(CodeActionOrCommand::CodeAction(action))
}

/// Produce diagnostics for all incomplete `when` expressions in a file.
///
/// Scans the CST for every `when_expression` node and emits a warning
/// diagnostic on each one that has missing branches.
pub(crate) fn when_diagnostics(indexer: &Indexer, uri: &Url) -> Vec<Diagnostic> {
    if crate::Language::from_path(uri.path()) != crate::Language::Kotlin {
        return Vec::new();
    }
    let live_doc = match indexer.live_doc(uri) {
        Some(doc) => doc,
        None => return Vec::new(),
    };
    let source_bytes = &live_doc.bytes;
    let root = live_doc.tree.root_node();

    let mut diagnostics = Vec::new();
    collect_when_nodes(root, source_bytes, indexer, uri, &mut diagnostics);
    diagnostics
}

fn collect_when_nodes(
    node: tree_sitter::Node,
    source: &[u8],
    indexer: &Indexer,
    uri: &Url,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if node.kind() == KIND_WHEN_EXPR {
        if let Some(analysis) = analyze_when(indexer, uri, node, source) {
            let missing_names: Vec<&str> =
                analysis.missing.iter().map(|m| m.name.as_str()).collect();
            let message = format!("'when' is missing branches: {}", missing_names.join(", "));
            let start = node.start_position();
            let keyword_end_col = start.column + 4; // "when" is 4 chars
            diagnostics.push(Diagnostic {
                range: Range::new(
                    Position::new(start.row as u32, start.column as u32),
                    Position::new(start.row as u32, keyword_end_col as u32),
                ),
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("kotlin-lsp".into()),
                message,
                ..Default::default()
            });
        }
    }

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_when_nodes(child, source, indexer, uri, diagnostics);
        }
    }
}

// ─── helpers ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeKind {
    Enum,
    Sealed,
    Boolean,
}

#[derive(Debug)]
struct WhenMember {
    name: String,
    is_object: bool,
    /// True if subtype is nested inside the parent type (for sealed classes).
    is_nested: bool,
}

fn byte_offset_for_position(lines: &[String], pos: Position) -> Option<usize> {
    let line = pos.line as usize;
    if line >= lines.len() {
        return None;
    }
    let mut offset = 0;
    for l in &lines[..line] {
        offset += l.len() + 1; // +1 for '\n'
    }
    let col_byte = utf16_col_to_byte(&lines[line], pos.character as usize);
    Some(offset + col_byte)
}

fn find_enclosing_when<'a>(
    tree: &'a tree_sitter::Tree,
    _source: &[u8],
    cursor_byte: usize,
) -> Option<tree_sitter::Node<'a>> {
    let node = tree
        .root_node()
        .descendant_for_byte_range(cursor_byte, cursor_byte)?;
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == KIND_WHEN_EXPR {
            return Some(n);
        }
        current = n.parent();
    }
    None
}

/// Resolve the when subject's type from the CST by searching:
/// 1. Sibling property_declarations in the same statements block (local vals)
/// 2. Enclosing function's parameters
/// 3. Enclosing class constructor parameters
fn resolve_subject_type_from_cst(
    when_node: &tree_sitter::Node,
    var_name: &str,
    source: &[u8],
) -> Option<String> {
    // Walk up to find statements block or function_declaration
    let mut current = when_node.parent();
    while let Some(node) = current {
        match node.kind() {
            KIND_STATEMENTS => {
                if let Some(ty) = find_type_in_sibling_declarations(&node, var_name, source) {
                    return Some(ty);
                }
            }
            KIND_FUN_DECL => {
                if let Some(ty) = find_type_in_parameters(&node, var_name, source) {
                    return Some(ty);
                }
            }
            KIND_CLASS_DECL => {
                if let Some(ty) = find_type_in_constructor(&node, var_name, source) {
                    return Some(ty);
                }
            }
            _ => {}
        }
        current = node.parent();
    }
    None
}

/// Search sibling property_declarations for `val <var_name> : Type`
fn find_type_in_sibling_declarations(
    statements: &tree_sitter::Node,
    var_name: &str,
    source: &[u8],
) -> Option<String> {
    for child in statements.children(&mut statements.walk()) {
        if child.kind() != KIND_PROP_DECL {
            continue;
        }
        if let Some(ty) = extract_var_type_from_declaration(&child, var_name, source) {
            return Some(ty);
        }
    }
    None
}

/// Search function parameters for `<var_name>: Type`
fn find_type_in_parameters(
    func_node: &tree_sitter::Node,
    var_name: &str,
    source: &[u8],
) -> Option<String> {
    for child in func_node.children(&mut func_node.walk()) {
        if child.kind() != KIND_FUN_VALUE_PARAMS {
            continue;
        }
        for param in child.children(&mut child.walk()) {
            if param.kind() != KIND_PARAMETER {
                continue;
            }
            if let Some(ty) = extract_param_type(&param, var_name, source) {
                return Some(ty);
            }
        }
    }
    None
}

/// Search class constructor parameters
fn find_type_in_constructor(
    class_node: &tree_sitter::Node,
    var_name: &str,
    source: &[u8],
) -> Option<String> {
    for child in class_node.children(&mut class_node.walk()) {
        if child.kind() != KIND_PRIMARY_CTOR {
            continue;
        }
        for param in child.children(&mut child.walk()) {
            if param.kind() != KIND_CLASS_PARAM {
                continue;
            }
            if let Some(ty) = extract_param_type(&param, var_name, source) {
                return Some(ty);
            }
        }
    }
    None
}

/// Extract type from `variable_declaration` inside a property_declaration.
/// CST: property_declaration → variable_declaration → simple_identifier + ":" + user_type
/// Also handles inferred Boolean from literal: `val x = false` → Boolean
fn extract_var_type_from_declaration(
    prop: &tree_sitter::Node,
    var_name: &str,
    source: &[u8],
) -> Option<String> {
    let mut name_matched = false;
    for child in prop.children(&mut prop.walk()) {
        if child.kind() == KIND_VAR_DECL {
            for vc in child.children(&mut child.walk()) {
                if vc.kind() == KIND_SIMPLE_IDENT && vc.utf8_text(source).ok() == Some(var_name) {
                    name_matched = true;
                }
                if name_matched {
                    if vc.kind() == KIND_USER_TYPE {
                        return extract_full_type_name(&vc, source);
                    }
                    if vc.kind() == KIND_NULLABLE_TYPE {
                        return extract_user_type_from_nullable(&vc, source);
                    }
                }
            }
        }
        if name_matched && child.kind() == KIND_BOOLEAN_LITERAL {
            return Some("Boolean".to_string());
        }
    }
    None
}

/// Extract type from a `parameter` or `class_parameter` node.
/// CST: parameter → simple_identifier + ":" + user_type | nullable_type
fn extract_param_type(param: &tree_sitter::Node, var_name: &str, source: &[u8]) -> Option<String> {
    let mut found_name = false;
    for child in param.children(&mut param.walk()) {
        if child.kind() == KIND_SIMPLE_IDENT && child.utf8_text(source).ok() == Some(var_name) {
            found_name = true;
        }
        if found_name {
            if child.kind() == KIND_USER_TYPE {
                return extract_full_type_name(&child, source);
            }
            if child.kind() == KIND_NULLABLE_TYPE {
                return extract_user_type_from_nullable(&child, source);
            }
        }
    }
    None
}

/// Extract user_type from a nullable_type node (nullable_type → user_type + "?").
fn extract_user_type_from_nullable(nullable: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    for child in nullable.children(&mut nullable.walk()) {
        if child.kind() == KIND_USER_TYPE {
            return extract_full_type_name(&child, source);
        }
    }
    None
}

/// Extract the full type name from a user_type node (e.g. "TipsResult", "Effect").
/// For dotted types like `Outer.Inner`, concatenates with dots.
fn extract_full_type_name(user_type: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut parts = Vec::new();
    for child in user_type.children(&mut user_type.walk()) {
        if child.kind() == KIND_TYPE_IDENT {
            if let Ok(text) = child.utf8_text(source) {
                parts.push(text.to_string());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("."))
    }
}

fn extract_subject_identifier(subject_node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    // when_subject → "(" simple_identifier ")"
    for child in subject_node.children(&mut subject_node.walk()) {
        if child.kind() == KIND_SIMPLE_IDENT {
            return child.utf8_text(source).ok().map(|s| s.to_string());
        }
    }
    None
}

fn strip_nullable(type_name: &str) -> &str {
    type_name.strip_suffix('?').unwrap_or(type_name)
}

/// Resolve whether the type is an enum, sealed class, or Boolean, and return its members.
fn resolve_type_members(indexer: &Indexer, type_name: &str) -> Option<(TypeKind, Vec<WhenMember>)> {
    // Boolean is a built-in — no index lookup needed
    if type_name == "Boolean" {
        let members = vec![
            WhenMember {
                name: "true".to_string(),
                is_object: true,
                is_nested: false,
            },
            WhenMember {
                name: "false".to_string(),
                is_object: true,
                is_nested: false,
            },
        ];
        return Some((TypeKind::Boolean, members));
    }

    let locations = indexer.definition_locations(type_name);
    if locations.is_empty() {
        return None;
    }

    for location in &locations {
        let file_data = indexer.file_data_for(location.uri.as_str())?;
        let symbol = find_symbol_at(&file_data, location)?;

        if symbol.kind == SymbolKind::ENUM {
            let members = collect_enum_members(&file_data, &symbol);
            if !members.is_empty() {
                return Some((TypeKind::Enum, members));
            }
        }

        if is_sealed(&symbol) {
            let members = collect_sealed_members(indexer, type_name);
            if !members.is_empty() {
                return Some((TypeKind::Sealed, members));
            }
        }
    }

    None
}

fn find_symbol_at(
    file_data: &crate::types::FileData,
    location: &Location,
) -> Option<crate::types::SymbolEntry> {
    file_data
        .symbols
        .iter()
        .find(|s| s.selection_range == location.range)
        .cloned()
}

fn is_sealed(symbol: &crate::types::SymbolEntry) -> bool {
    let detail = &symbol.detail;
    detail.starts_with("sealed class")
        || detail.starts_with("sealed interface")
        || detail.starts_with("abstract sealed")
}

fn collect_enum_members(
    file_data: &crate::types::FileData,
    enum_symbol: &crate::types::SymbolEntry,
) -> Vec<WhenMember> {
    let enum_range = &enum_symbol.range;
    file_data
        .symbols
        .iter()
        .filter(|s| {
            s.kind == SymbolKind::ENUM_MEMBER
                && s.range.start.line > enum_range.start.line
                && s.range.end.line <= enum_range.end.line
        })
        .map(|s| WhenMember {
            name: s.name.clone(),
            is_object: true, // enum entries are always object-like
            is_nested: true,
        })
        .collect()
}

fn collect_sealed_members(indexer: &Indexer, sealed_name: &str) -> Vec<WhenMember> {
    // Find the sealed parent's full declaration range to detect nested subtypes.
    // `definitions` stores selection_range (identifier only), so look up the full
    // SymbolEntry.range from the file data.
    let parent_info = indexer.definitions.get(sealed_name).and_then(|locs| {
        let loc = locs.first()?;
        let file_data = indexer.file_data_for(loc.uri.as_str())?;
        let parent_sym = file_data
            .symbols
            .iter()
            .find(|s| s.name == sealed_name && s.selection_range == loc.range)?;
        Some((loc.uri.clone(), parent_sym.range))
    });

    let subtype_locations = indexer.subtypes_of(sealed_name);
    let mut members = Vec::new();

    for location in &subtype_locations {
        let Some(file_data) = indexer.file_data_for(location.uri.as_str()) else {
            continue;
        };
        if let Some(symbol) = find_symbol_at(&file_data, location) {
            let is_object = symbol.kind == SymbolKind::OBJECT;
            let is_nested = parent_info
                .as_ref()
                .is_some_and(|(parent_uri, parent_range)| {
                    location.uri == *parent_uri
                        && symbol.range.start.line > parent_range.start.line
                        && symbol.range.end.line <= parent_range.end.line
                });
            members.push(WhenMember {
                name: symbol.name.clone(),
                is_object,
                is_nested,
            });
        }
    }

    members
}

fn collect_existing_branches(when_node: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut branches = Vec::new();
    for child in when_node.children(&mut when_node.walk()) {
        if child.kind() != KIND_WHEN_ENTRY {
            continue;
        }
        // Check for `else` branch
        for entry_child in child.children(&mut child.walk()) {
            if entry_child.kind() == KIND_ELSE {
                branches.push("else".to_string());
                continue;
            }
            if entry_child.kind() != KIND_WHEN_CONDITION {
                continue;
            }
            if let Some(name) = extract_branch_name(&entry_child, source) {
                branches.push(name);
            }
        }
    }
    branches
}

/// Extract the type/value name from a when_condition.
///
/// Handles:
/// - `is Effect.ShowToast` → "ShowToast"
/// - `Color.RED` → "RED"
/// - `is ShowToast` → "ShowToast"
fn extract_branch_name(condition: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    for child in condition.children(&mut condition.walk()) {
        match child.kind() {
            KIND_TYPE_TEST => {
                // type_test → "is" user_type → type_identifier ("." type_identifier)*
                return extract_last_type_identifier(&child, source);
            }
            KIND_NAV_EXPR => {
                // navigation_expression → simple_identifier "." simple_identifier
                return extract_nav_last_ident(&child, source);
            }
            // Boolean literals: `true` / `false`
            KIND_BOOLEAN_LITERAL => {
                return child.utf8_text(source).ok().map(|s| s.to_string());
            }
            _ => {}
        }
    }
    None
}

/// Extract the last type_identifier from a type_test node.
/// e.g. `is Effect.ShowToast` → "ShowToast", `is ShowToast` → "ShowToast"
fn extract_last_type_identifier(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut last_ident = None;
    for child in node.children(&mut node.walk()) {
        if child.kind() == KIND_USER_TYPE {
            last_ident = extract_last_type_from_user_type(&child, source);
        }
    }
    last_ident
}

fn extract_last_type_from_user_type(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut last = None;
    for child in node.children(&mut node.walk()) {
        if child.kind() == KIND_TYPE_IDENT {
            last = child.utf8_text(source).ok().map(|s| s.to_string());
        }
    }
    last
}

/// Extract the last identifier from a navigation_expression.
/// e.g. `Color.RED` → "RED"
fn extract_nav_last_ident(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    for child in node.children(&mut node.walk()) {
        if child.kind() == KIND_NAV_SUFFIX {
            for suffix_child in child.children(&mut child.walk()) {
                if suffix_child.kind() == KIND_SIMPLE_IDENT {
                    return suffix_child.utf8_text(source).ok().map(|s| s.to_string());
                }
            }
        }
    }
    None
}

fn build_branch_text(
    missing: &[&WhenMember],
    parent_type: &str,
    type_kind: TypeKind,
    indent: &str,
) -> String {
    let mut text = String::new();
    for member in missing {
        match type_kind {
            TypeKind::Boolean => {
                // Bare value: `true -> TODO()`, `false -> TODO()`
                text.push_str(&format!("{}{} -> TODO()\n", indent, member.name));
            }
            TypeKind::Enum => {
                text.push_str(&format!(
                    "{}{}.{} -> TODO()\n",
                    indent, parent_type, member.name
                ));
            }
            TypeKind::Sealed => {
                let qualified = if member.is_nested {
                    format!("{}.{}", parent_type, member.name)
                } else {
                    member.name.clone()
                };
                if member.is_object {
                    text.push_str(&format!("{}{} -> TODO()\n", indent, qualified));
                } else {
                    text.push_str(&format!("{}is {} -> TODO()\n", indent, qualified));
                }
            }
        }
    }
    text
}

/// Detect indentation for new branches.
/// Uses the first existing `when_entry`'s column, or falls back to when_expression column + 4.
fn detect_indent(when_node: &tree_sitter::Node, _source: &[u8]) -> String {
    for child in when_node.children(&mut when_node.walk()) {
        if child.kind() == KIND_WHEN_ENTRY {
            let col = child.start_position().column;
            return " ".repeat(col);
        }
    }
    let base = when_node.start_position().column;
    " ".repeat(base + 4)
}

/// Find the replace range for new branches.
///
/// When the block is empty (no existing entries), replaces from line after `{`
/// through `}` — cleaning up blank lines. When entries exist, replaces from
/// the line after the last entry through `}`.
///
/// Returns `(replace_range, closing_brace_indent)`.
fn find_insert_position(
    when_node: &tree_sitter::Node,
    _source: &[u8],
    _lines: &[String],
) -> Option<(Range, String)> {
    let child_count = when_node.child_count();
    if child_count == 0 {
        return None;
    }
    let last_child = when_node.child(child_count - 1)?;
    if last_child.kind() != KIND_RBRACE {
        return None;
    }
    let close_line = last_child.start_position().row as u32;
    let close_col = last_child.start_position().column as u32;

    // Find the last when_entry to insert after it, or `{` if none
    let last_entry = when_node
        .children(&mut when_node.walk())
        .filter(|c| c.kind() == KIND_WHEN_ENTRY)
        .last();

    let start_line = if let Some(entry) = last_entry {
        entry.end_position().row as u32 + 1
    } else {
        // No entries — find `{` and start after it
        let open = when_node
            .children(&mut when_node.walk())
            .find(|c| c.kind() == KIND_LBRACE)?;
        open.start_position().row as u32 + 1
    };

    // Clamp: if when is compact (single line), start at close_line
    let start_line = start_line.min(close_line);

    let start = Position::new(start_line, 0);
    let end = Position::new(close_line, close_col + 1);
    let brace_indent = " ".repeat(close_col as usize);
    Some((Range::new(start, end), brace_indent))
}

#[cfg(test)]
#[path = "fill_when_tests.rs"]
mod tests;
