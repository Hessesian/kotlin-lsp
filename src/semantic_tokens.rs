//! Semantic token classification for `textDocument/semanticTokens/full` and
//! `textDocument/semanticTokens/range`.
//!
//! Tokens are derived purely from the tree-sitter CST — no cross-file type
//! resolution.  This is sufficient to distinguish classes from functions,
//! parameters from properties, `val` (readonly) from `var`, etc.
//!
//! # Encoding
//! LSP semantic tokens are delta-encoded: each token stores the line *delta*
//! from the previous token and the column *delta* (from the previous token on
//! the same line, or from column 0 on a new line).  Tokens must be sorted by
//! (line, col) before encoding.

use tower_lsp::lsp_types::{
    Position, Range, SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens,
    SemanticTokensLegend, SymbolKind, Url,
};
use tree_sitter::Node;

use crate::indexer::{
    find_it_element_type_in_lines, find_this_element_type_in_lines, Indexer, LiveDoc, NodeExt,
};
use crate::queries::{
    KIND_ANNOTATION, KIND_ANNOTATION_TYPE_DECL, KIND_CALL_EXPR, KIND_CLASS_BODY, KIND_CLASS_DECL,
    KIND_COMPANION_OBJ, KIND_CONSTRUCTOR_INVOCATION, KIND_ENUM_CLASS_BODY, KIND_ENUM_CONSTANT,
    KIND_ENUM_ENTRY, KIND_ENUM_JAVA_DECL, KIND_EQ, KIND_FIELD_DECL, KIND_FORMAL_PARAM,
    KIND_FUN_DECL, KIND_IDENTIFIER, KIND_INTERFACE_BODY, KIND_INTERFACE_DECL, KIND_LAMBDA_LIT,
    KIND_LAMBDA_PARAMS, KIND_MARKER_ANNOTATION, KIND_METHOD_DECL, KIND_MODIFIERS,
    KIND_MULTI_ANNOTATION, KIND_MULTI_VAR_DECL, KIND_NAV_EXPR, KIND_NAV_SUFFIX, KIND_OBJECT_BODY,
    KIND_OBJECT_DECL, KIND_PARAMETER, KIND_PROP_DECL, KIND_RECORD_DECL, KIND_SIMPLE_IDENT,
    KIND_SPREAD_PARAM, KIND_THIS_EXPR, KIND_TYPE_IDENT, KIND_TYPE_PARAM, KIND_USER_TYPE,
    KIND_VALUE_ARG, KIND_VAR_DECL, KIND_VAR_DECLARATOR,
};
use crate::resolver::infer::{
    find_field_type_in_class, find_fun_return_type_by_name, find_method_return_type,
    infer_variable_type,
};
use crate::Language;

// ─── Legend ──────────────────────────────────────────────────────────────────

/// Ordered list of token types — index == LSP token type id.
pub(crate) const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::CLASS,          // 0
    SemanticTokenType::INTERFACE,      // 1
    SemanticTokenType::ENUM,           // 2
    SemanticTokenType::TYPE_PARAMETER, // 3
    SemanticTokenType::FUNCTION,       // 4
    SemanticTokenType::METHOD,         // 5
    SemanticTokenType::PROPERTY,       // 6
    SemanticTokenType::VARIABLE,       // 7
    SemanticTokenType::PARAMETER,      // 8
    SemanticTokenType::ENUM_MEMBER,    // 9
    SemanticTokenType::DECORATOR,      // 10  (annotations)
    SemanticTokenType::NAMESPACE,      // 11  (objects / companion objects used as namespaces)
    SemanticTokenType::STRUCT,         // 12  (data classes)
    SemanticTokenType::OPERATOR,       // 13  (operator fun)
    SemanticTokenType::KEYWORD,        // 14  (implicit it/this, future use)
];

/// Ordered list of modifiers — bit position == modifier id.
pub(crate) const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION,     // bit 0
    SemanticTokenModifier::READONLY,        // bit 1
    SemanticTokenModifier::STATIC,          // bit 2  (companion object members)
    SemanticTokenModifier::ABSTRACT,        // bit 3
    SemanticTokenModifier::ASYNC,           // bit 4  (suspend funs)
    SemanticTokenModifier::DEPRECATED,      // bit 5
    SemanticTokenModifier::DEFAULT_LIBRARY, // bit 6  (stdlib symbols, future use)
];

fn type_index(t: &SemanticTokenType) -> u32 {
    TOKEN_TYPES
        .iter()
        .position(|x| x == t)
        .expect("token type not in legend") as u32
}

fn modifier_bit(m: &SemanticTokenModifier) -> u32 {
    1 << TOKEN_MODIFIERS
        .iter()
        .position(|x| x == m)
        .expect("modifier not in legend") as u32
}

pub(crate) fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES.to_vec(),
        token_modifiers: TOKEN_MODIFIERS.to_vec(),
    }
}

// ─── Classification ───────────────────────────────────────────────────────────

#[derive(Clone)]
struct RawToken {
    line: u32,
    col: u32,  // UTF-16 column
    length: u32,
    token_type: u32,
    token_modifiers_bitset: u32,
}

/// Bundles source bytes with precomputed line-start offsets for O(1) UTF-16
/// column conversion (avoids re-scanning the entire file per token).
struct Source<'a> {
    bytes: &'a [u8],
    line_starts: Vec<usize>,
}

impl<'a> Source<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            line_starts: crate::inlay_hints::line_starts(bytes),
            bytes,
        }
    }

    fn col_utf16(&self, row: usize, byte_col: usize) -> u32 {
        crate::inlay_hints::ts_byte_col_to_utf16(self.bytes, &self.line_starts, row, byte_col)
            as u32
    }
}

/// Collect semantic tokens for `doc`, for the given `language`.
/// Returns delta-encoded `SemanticToken` values ready for the LSP response.
/// Filtered to `range` when `Some`.
///
/// When `indexer` and `uri` are provided, reference-site identifiers are
/// resolved against the cross-file index for richer coloring.
pub(crate) fn collect_tokens(
    doc: &LiveDoc,
    language: Language,
    range: Option<&Range>,
    indexer: Option<&Indexer>,
    uri: Option<&Url>,
) -> Vec<SemanticToken> {
    let mut raw: Vec<RawToken> = Vec::new();
    let src = Source::new(&doc.bytes);

    match language {
        Language::Kotlin => walk_kotlin(doc.tree.root_node(), &src, &mut raw),
        Language::Java => walk_java(doc.tree.root_node(), &src, &mut raw),
        _ => {}
    }

    // Phase 2: resolve reference-site identifiers against the index.
    if let (Some(idx), Some(file_uri)) = (indexer, uri) {
        walk_references(doc, &src, language, idx, file_uri, &mut raw);
    }

    // Sort by (line, col) — tree walk is depth-first so usually already sorted,
    // but not guaranteed for all node orderings.
    raw.sort_by_key(|t| (t.line, t.col));

    // Deduplicate: if both declaration walk and reference walk emitted a token
    // at the same position, keep only the first (declaration takes priority).
    raw.dedup_by_key(|t| (t.line, t.col));

    // Apply range filter before delta-encoding.
    if let Some(r) = range {
        let start_line = r.start.line;
        let end_line = r.end.line;
        raw.retain(|t| t.line >= start_line && t.line <= end_line);
    }

    delta_encode(raw)
}

// ─── Kotlin walker ────────────────────────────────────────────────────────────

fn walk_kotlin(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    classify_kotlin(node, src, out);
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk_kotlin(cursor.node(), src, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn classify_kotlin(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let kind = node.kind();
    match kind {
        k if k == KIND_CLASS_DECL => kotlin_class_token(node, src, out),
        k if k == KIND_OBJECT_DECL => kotlin_object_token(node, src, out),
        k if k == KIND_COMPANION_OBJ => kotlin_companion_token(node, src, out),
        k if k == KIND_FUN_DECL => kotlin_fun_token(node, src, out),
        k if k == KIND_PROP_DECL => kotlin_prop_token(node, src, out),
        k if k == KIND_TYPE_PARAM => kotlin_type_param_token(node, src, out),
        KIND_PARAMETER => {
            let mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
            if let Some(name) = child_ident(node) {
                push_token(name, type_index(&SemanticTokenType::PARAMETER), mods, src, out);
            }
        }
        KIND_ENUM_ENTRY => {
            let mods = modifier_bit(&SemanticTokenModifier::DECLARATION)
                | modifier_bit(&SemanticTokenModifier::READONLY);
            if let Some(name) = child_ident(node) {
                push_token(name, type_index(&SemanticTokenType::ENUM_MEMBER), mods, src, out);
            }
        }
        KIND_ANNOTATION | KIND_MULTI_ANNOTATION => {
            if let Some(ident) = find_annotation_ident(node) {
                push_token(ident, type_index(&SemanticTokenType::DECORATOR), 0, src, out);
            }
        }
        _ => {}
    }
}

fn kotlin_class_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let token_type = if has_keyword_child(node, "interface") {
        type_index(&SemanticTokenType::INTERFACE)
    } else if has_keyword_child(node, "enum") {
        type_index(&SemanticTokenType::ENUM)
    } else if has_modifier(node, src, "data") {
        type_index(&SemanticTokenType::STRUCT)
    } else {
        type_index(&SemanticTokenType::CLASS)
    };
    let mut mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
    if has_modifier(node, src, "abstract") {
        mods |= modifier_bit(&SemanticTokenModifier::ABSTRACT);
    }
    if let Some(name) = child_ident(node) {
        push_token(name, token_type, mods, src, out);
    }
}

fn kotlin_object_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
    if let Some(name) = child_ident(node) {
        push_token(name, type_index(&SemanticTokenType::NAMESPACE), mods, src, out);
    }
}

fn kotlin_companion_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let mods = modifier_bit(&SemanticTokenModifier::DECLARATION)
        | modifier_bit(&SemanticTokenModifier::STATIC);
    let ns_type = type_index(&SemanticTokenType::NAMESPACE);
    if let Some(name) = child_ident(node) {
        push_token(name, ns_type, mods, src, out);
    } else if let Some(obj_kw) = first_child_of_kind(node, "object") {
        push_token(obj_kw, ns_type, mods, src, out);
    }
}

fn kotlin_fun_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let token_type = if has_modifier(node, src, "operator") {
        type_index(&SemanticTokenType::OPERATOR)
    } else if is_inside_class_body(node) {
        type_index(&SemanticTokenType::METHOD)
    } else {
        type_index(&SemanticTokenType::FUNCTION)
    };
    let mut mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
    if has_modifier(node, src, "suspend") {
        mods |= modifier_bit(&SemanticTokenModifier::ASYNC);
    }
    if has_modifier(node, src, "abstract") {
        mods |= modifier_bit(&SemanticTokenModifier::ABSTRACT);
    }
    if let Some(name) = child_ident(node) {
        push_token(name, token_type, mods, src, out);
    }
}

fn kotlin_prop_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let is_val = first_child_of_kind(node, "binding_pattern_kind")
        .map(|bpk| has_keyword_child(bpk, "val"))
        .unwrap_or_else(|| has_keyword_child(node, "val"));
    let token_type = if is_inside_class_body(node) {
        type_index(&SemanticTokenType::PROPERTY)
    } else {
        type_index(&SemanticTokenType::VARIABLE)
    };
    let mut mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
    if is_val {
        mods |= modifier_bit(&SemanticTokenModifier::READONLY);
    }
    if let Some(var_decl) = first_child_of_kind(node, KIND_VAR_DECL) {
        if let Some(name) = child_ident(var_decl) {
            push_token(name, token_type, mods, src, out);
        }
    } else if let Some(multi) = first_child_of_kind(node, KIND_MULTI_VAR_DECL) {
        for i in 0..multi.named_child_count() {
            if let Some(vd) = multi.named_child(i) {
                if let Some(name) = child_ident(vd) {
                    push_token(name, token_type, mods, src, out);
                }
            }
        }
    }
}

fn kotlin_type_param_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
    if let Some(ident) = first_child_of_kind(node, KIND_TYPE_IDENT)
        .or_else(|| first_child_of_kind(node, KIND_SIMPLE_IDENT))
    {
        push_token(ident, type_index(&SemanticTokenType::TYPE_PARAMETER), mods, src, out);
    }
}

// ─── Java walker ─────────────────────────────────────────────────────────────

fn walk_java(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    classify_java(node, src, out);
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk_java(cursor.node(), src, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn classify_java(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let kind = node.kind();

    match kind {
        k if k == KIND_CLASS_DECL || k == KIND_RECORD_DECL => {
            let mut mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
            if has_java_modifier(node, src, "abstract") {
                mods |= modifier_bit(&SemanticTokenModifier::ABSTRACT);
            }
            if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
                push_token(name, type_index(&SemanticTokenType::CLASS), mods, src, out);
            }
        }

        k if k == KIND_INTERFACE_DECL || k == KIND_ANNOTATION_TYPE_DECL => {
            let token_type = if k == KIND_ANNOTATION_TYPE_DECL {
                type_index(&SemanticTokenType::DECORATOR)
            } else {
                type_index(&SemanticTokenType::INTERFACE)
            };
            let mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
            if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
                push_token(name, token_type, mods, src, out);
            }
        }

        KIND_ENUM_JAVA_DECL => {
            let mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
            if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
                push_token(name, type_index(&SemanticTokenType::ENUM), mods, src, out);
            }
        }

        k if k == KIND_METHOD_DECL => {
            let mut mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
            if has_java_modifier(node, src, "abstract") {
                mods |= modifier_bit(&SemanticTokenModifier::ABSTRACT);
            }
            if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
                push_token(name, type_index(&SemanticTokenType::METHOD), mods, src, out);
            }
        }

        k if k == KIND_FIELD_DECL => {
            let is_final = has_java_modifier(node, src, "final");
            let mut mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
            if is_final {
                mods |= modifier_bit(&SemanticTokenModifier::READONLY);
            }
            if has_java_modifier(node, src, "static") {
                mods |= modifier_bit(&SemanticTokenModifier::STATIC);
            }
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i) {
                    if child.kind() == KIND_VAR_DECLARATOR {
                        if let Some(name) = first_child_of_kind(child, KIND_IDENTIFIER) {
                            push_token(name, type_index(&SemanticTokenType::PROPERTY), mods, src, out);
                        }
                    }
                }
            }
        }

        KIND_FORMAL_PARAM | KIND_SPREAD_PARAM => {
            let mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
            if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
                push_token(name, type_index(&SemanticTokenType::PARAMETER), mods, src, out);
            }
        }

        k if k == KIND_ENUM_CONSTANT => {
            let mods = modifier_bit(&SemanticTokenModifier::DECLARATION)
                | modifier_bit(&SemanticTokenModifier::READONLY);
            if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
                push_token(name, type_index(&SemanticTokenType::ENUM_MEMBER), mods, src, out);
            }
        }

        k if k == KIND_TYPE_PARAM => {
            let mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
            // Java type_parameter has type_identifier child; Kotlin uses type_identifier too
            if let Some(name) = first_child_of_kind(node, KIND_TYPE_IDENT)
                .or_else(|| first_child_of_kind(node, KIND_IDENTIFIER))
            {
                push_token(name, type_index(&SemanticTokenType::TYPE_PARAMETER), mods, src, out);
            }
        }

        KIND_MARKER_ANNOTATION | KIND_ANNOTATION => {
            // @Override, @SuppressWarnings("...") etc.
            if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
                push_token(name, type_index(&SemanticTokenType::DECORATOR), 0, src, out);
            }
        }

        _ => {}
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Find the type/simple identifier inside an annotation node.
/// Handles both `annotation > user_type > type_identifier` (simple annotation)
/// and `annotation > constructor_invocation > user_type > type_identifier` (with args).
fn find_annotation_ident(annotation_node: Node<'_>) -> Option<Node<'_>> {
    // Direct: annotation > user_type > type_identifier  OR  annotation > type_identifier
    if let Some(ident) = first_child_of_kind(annotation_node, KIND_TYPE_IDENT)
        .or_else(|| first_child_of_kind(annotation_node, KIND_SIMPLE_IDENT))
    {
        return Some(ident);
    }
    // Via user_type: annotation > user_type > type_identifier
    if let Some(user_type) = first_child_of_kind(annotation_node, KIND_USER_TYPE) {
        if let Some(ident) = first_child_of_kind(user_type, KIND_TYPE_IDENT)
            .or_else(|| first_child_of_kind(user_type, KIND_SIMPLE_IDENT))
        {
            return Some(ident);
        }
    }
    // Via constructor_invocation: annotation > constructor_invocation > user_type > type_identifier
    if let Some(ctor) = first_child_of_kind(annotation_node, KIND_CONSTRUCTOR_INVOCATION) {
        if let Some(user_type) = first_child_of_kind(ctor, KIND_USER_TYPE) {
            return first_child_of_kind(user_type, KIND_TYPE_IDENT)
                .or_else(|| first_child_of_kind(user_type, KIND_SIMPLE_IDENT));
        }
    }
    None
}

/// Find the first direct child with a name identifier (simple_identifier or identifier).
fn child_ident<'a>(node: Node<'a>) -> Option<Node<'a>> {
    for i in 0..node.child_count() {
        let child = node.child(i)?;
        if child.kind() == KIND_SIMPLE_IDENT
            || child.kind() == KIND_IDENTIFIER
            || child.kind() == KIND_TYPE_IDENT
        {
            return Some(child);
        }
    }
    None
}

fn first_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == kind {
                return Some(child);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

fn has_keyword_child(node: Node<'_>, keyword: &str) -> bool {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().kind() == keyword {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    false
}

/// Check whether a Kotlin node has a modifier keyword (e.g. "suspend", "abstract").
fn has_modifier(node: Node<'_>, src: &Source<'_>, keyword: &str) -> bool {
    // Modifiers are either direct keyword children or inside a `modifiers` node.
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == KIND_MODIFIERS
                && node_text(child, src.bytes).split_whitespace().any(|w| w == keyword)
            {
                return true;
            }
            if child.kind() == keyword {
                return true;
            }
        }
    }
    false
}

/// Check whether a Java node has a modifier keyword inside a `modifiers` node.
fn has_java_modifier(node: Node<'_>, src: &Source<'_>, keyword: &str) -> bool {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == KIND_MODIFIERS
                && node_text(child, src.bytes).split_whitespace().any(|w| w == keyword)
            {
                return true;
            }
        }
    }
    false
}

/// True when this node is a direct child of a class/interface/enum body.
fn is_inside_class_body(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else { return false };
    let k = parent.kind();
    k == KIND_CLASS_BODY || k == KIND_INTERFACE_BODY || k == KIND_ENUM_CLASS_BODY || k == KIND_OBJECT_BODY
}

fn node_text<'a>(node: Node<'_>, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

fn push_token(node: Node<'_>, token_type: u32, token_modifiers_bitset: u32, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let start = node.start_position();
    let text = node_text(node, src.bytes);
    let length = text.encode_utf16().count() as u32;
    if length == 0 {
        return;
    }
    out.push(RawToken {
        line: start.row as u32,
        col: src.col_utf16(start.row, start.column),
        length,
        token_type,
        token_modifiers_bitset,
    });
}


// ─── Delta encoding ───────────────────────────────────────────────────────────

fn delta_encode(sorted: Vec<RawToken>) -> Vec<SemanticToken> {
    let mut result = Vec::with_capacity(sorted.len());
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;

    for tok in sorted {
        let delta_line = tok.line - prev_line;
        let delta_start = if delta_line == 0 {
            tok.col - prev_start
        } else {
            tok.col
        };
        result.push(SemanticToken {
            delta_line,
            delta_start,
            length: tok.length,
            token_type: tok.token_type,
            token_modifiers_bitset: tok.token_modifiers_bitset,
        });
        prev_line = tok.line;
        prev_start = tok.col;
    }
    result
}

fn visit_tree(node: Node<'_>, f: &mut impl FnMut(Node<'_>)) {
    f(node);
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            visit_tree(cursor.node(), f);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn navigation_receiver_node(node: Node<'_>) -> Option<Node<'_>> {
    (0..node.child_count())
        .filter_map(|i| node.child(i))
        .find(|child| child.is_named() && child.kind() != KIND_NAV_SUFFIX)
}

fn navigation_member_ident(node: Node<'_>) -> Option<Node<'_>> {
    let suffix = node.first_child_of_kind(KIND_NAV_SUFFIX)?;
    (0..suffix.child_count()).filter_map(|i| suffix.child(i)).find(|child| {
        child.kind() == KIND_SIMPLE_IDENT || child.kind() == KIND_TYPE_IDENT
    })
}

fn token_position(bytes: &[u8], starts: &[usize], node: Node<'_>) -> Position {
    let start = node.start_position();
    Position::new(
        start.row as u32,
        crate::inlay_hints::ts_byte_col_to_utf16(bytes, starts, start.row, start.column) as u32,
    )
}

fn is_call_callee(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent.kind() == KIND_CALL_EXPR && parent.child(0).map(|child| child.id()) == Some(node.id())
}

fn is_owner_type_symbol(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::CLASS
            | SymbolKind::INTERFACE
            | SymbolKind::ENUM
            | SymbolKind::OBJECT
            | SymbolKind::STRUCT
    )
}

fn is_type_symbol(kind: SymbolKind) -> bool {
    is_owner_type_symbol(kind)
}

fn is_member_symbol(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::METHOD
            | SymbolKind::FUNCTION
            | SymbolKind::OPERATOR
            | SymbolKind::PROPERTY
            | SymbolKind::FIELD
            | SymbolKind::CONSTANT
            | SymbolKind::VARIABLE
    )
}

fn member_token_type(kind: SymbolKind) -> Option<u32> {
    match kind {
        SymbolKind::METHOD | SymbolKind::FUNCTION | SymbolKind::OPERATOR => {
            Some(type_index(&SemanticTokenType::METHOD))
        }
        SymbolKind::PROPERTY | SymbolKind::FIELD | SymbolKind::CONSTANT | SymbolKind::VARIABLE => {
            Some(type_index(&SemanticTokenType::PROPERTY))
        }
        _ => None,
    }
}

fn range_within(inner: &Range, outer: &Range) -> bool {
    inner.start.line >= outer.start.line && inner.end.line <= outer.end.line
}

fn has_type_definition(indexer: &Indexer, name: &str) -> bool {
    indexer.definition_locations(name).into_iter().any(|loc| {
        indexer
            .files
            .get(loc.uri.as_str())
            .map(|file_data| {
                file_data
                    .symbols
                    .iter()
                    .any(|symbol| symbol.name == name && is_type_symbol(symbol.kind))
            })
            .unwrap_or(false)
    })
}

fn matches_receiver_type(extension_receiver: &str, receiver_type: &str) -> bool {
    let receiver_leaf = receiver_type.rsplit('.').next().unwrap_or(receiver_type);
    extension_receiver == receiver_type || extension_receiver == receiver_leaf
}

fn owner_member_token_type(indexer: &Indexer, receiver_type: &str, member_name: &str) -> Option<u32> {
    let receiver_leaf = receiver_type.rsplit('.').next().unwrap_or(receiver_type);
    for loc in indexer.definition_locations(receiver_leaf) {
        let Some(file_data) = indexer.files.get(loc.uri.as_str()) else {
            continue;
        };
        let owner_range = file_data
            .symbols
            .iter()
            .find(|symbol| symbol.name == receiver_leaf && is_type_symbol(symbol.kind))
            .map(|symbol| symbol.range);
        let Some(owner_range) = owner_range else {
            continue;
        };
        if let Some(symbol) = file_data
            .symbols
            .iter()
            .find(|symbol| symbol.name == member_name && is_member_symbol(symbol.kind) && range_within(&symbol.range, &owner_range))
        {
            return member_token_type(symbol.kind);
        }
    }
    None
}

fn extension_member_token_type(indexer: &Indexer, receiver_type: &str, member_name: &str) -> Option<u32> {
    for loc in indexer.definition_locations(member_name) {
        let Some(file_data) = indexer.files.get(loc.uri.as_str()) else {
            continue;
        };
        if let Some(symbol) = file_data.symbols.iter().find(|symbol| {
            symbol.name == member_name
                && is_member_symbol(symbol.kind)
                && !symbol.extension_receiver.is_empty()
                && matches_receiver_type(&symbol.extension_receiver, receiver_type)
        }) {
            return member_token_type(symbol.kind).or(Some(type_index(&SemanticTokenType::METHOD)));
        }
    }
    None
}

fn member_token_type_for_receiver(
    indexer: &Indexer,
    receiver_type: &str,
    member_name: &str,
) -> Option<u32> {
    owner_member_token_type(indexer, receiver_type, member_name)
        .or_else(|| extension_member_token_type(indexer, receiver_type, member_name))
        .or_else(|| {
            find_field_type_in_class(indexer, receiver_type, member_name)
                .map(|_| type_index(&SemanticTokenType::PROPERTY))
        })
}



fn member_return_type(indexer: &Indexer, receiver_type: &str, member_name: &str) -> Option<String> {
    find_method_return_type(indexer, receiver_type, member_name)
}

fn identifier_type(node: Node<'_>, doc: &LiveDoc, starts: &[usize], indexer: &Indexer, uri: &Url) -> Option<String> {
    let name = node.utf8_text_owned(&doc.bytes)?;
    if let Some(inferred) = indexer.infer_lambda_param_type_at(&name, uri, token_position(&doc.bytes, starts, node)) {
        return Some(inferred);
    }
    if let Some(inferred) = infer_variable_type(indexer, &name, uri) {
        return Some(inferred);
    }
    if name.starts_with(char::is_uppercase) && has_type_definition(indexer, &name) {
        return Some(name);
    }
    None
}

fn navigation_expression_type(
    node: Node<'_>,
    doc: &LiveDoc,
    starts: &[usize],
    indexer: &Indexer,
    uri: &Url,
) -> Option<String> {
    let receiver = navigation_receiver_node(node)?;
    let member = navigation_member_ident(node)?.utf8_text_owned(&doc.bytes)?;
    let receiver_type = expression_type(receiver, doc, starts, indexer, uri)?;

    if is_call_callee(node) {
        return member_return_type(indexer, &receiver_type, &member)
            .or_else(|| find_fun_return_type_by_name(indexer, &member));
    }

    find_field_type_in_class(indexer, &receiver_type, &member)
}

fn call_expression_type(node: Node<'_>, doc: &LiveDoc, starts: &[usize], indexer: &Indexer, uri: &Url) -> Option<String> {
    let (member, _) = node.call_fn_and_qualifier(&doc.bytes)?;
    if let Some(callee) = node.child(0).filter(|child| child.kind() == KIND_NAV_EXPR) {
        if let Some(receiver) = navigation_receiver_node(callee) {
            if let Some(receiver_type) = expression_type(receiver, doc, starts, indexer, uri) {
                if let Some(return_type) = member_return_type(indexer, &receiver_type, &member) {
                    return Some(return_type);
                }
            }
        }
    }
    find_fun_return_type_by_name(indexer, &member)
}

fn expression_type(node: Node<'_>, doc: &LiveDoc, starts: &[usize], indexer: &Indexer, uri: &Url) -> Option<String> {
    match node.kind() {
        KIND_SIMPLE_IDENT | KIND_TYPE_IDENT => identifier_type(node, doc, starts, indexer, uri),
        KIND_THIS_EXPR => indexer.infer_lambda_param_type_at("this", uri, token_position(&doc.bytes, starts, node)),
        KIND_NAV_EXPR => navigation_expression_type(node, doc, starts, indexer, uri),
        KIND_CALL_EXPR => call_expression_type(node, doc, starts, indexer, uri),
        _ => None,
    }
}

fn is_inside_lambda_parameters(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == KIND_LAMBDA_PARAMS {
            return true;
        }
        if parent.kind() == KIND_LAMBDA_LIT {
            return false;
        }
        current = parent.parent();
    }
    false
}

/// Resolve member accesses in navigation expressions.
/// Returns resolved tokens for member identifiers.
fn resolve_member_access(doc: &LiveDoc, src: &Source<'_>, indexer: &Indexer, uri: &Url) -> Vec<RawToken> {
    let mut tokens = Vec::new();
    visit_tree(doc.tree.root_node(), &mut |node| {
        if node.kind() != KIND_NAV_EXPR {
            return;
        }
        let Some(member_ident) = navigation_member_ident(node) else {
            return;
        };
        let Some(member_name) = member_ident.utf8_text_owned(&doc.bytes) else {
            return;
        };
        let is_call = is_call_callee(node);
        let resolved_type = navigation_receiver_node(node)
            .and_then(|receiver| expression_type(receiver, doc, &src.line_starts, indexer, uri))
            .and_then(|receiver_type| member_token_type_for_receiver(indexer, &receiver_type, &member_name));
        // For call expressions, override any PROPERTY false-positive to METHOD.
        // A called navigation suffix is always a function invocation, even if the
        // index resolved it as a property (e.g. library type not fully indexed).
        let method_type = type_index(&SemanticTokenType::METHOD);
        let property_type = type_index(&SemanticTokenType::PROPERTY);
        let token_type = if is_call {
            Some(resolved_type.map(|t| if t == property_type { method_type } else { t }).unwrap_or(method_type))
        } else {
            resolved_type
        };
        if let Some(token_type) = token_type {
            push_token(member_ident, token_type, 0, src, &mut tokens);
        }
    });
    tokens
}

/// Resolve lambda parameter identifiers (`it`, `this`, named params).
fn resolve_lambda_params(doc: &LiveDoc, src: &Source<'_>, indexer: &Indexer, uri: &Url) -> Vec<RawToken> {
    let mut tokens = Vec::new();
    let lines_arc = indexer.mem_lines_for(uri.as_str());
    let fallback_lines: Vec<String> = std::str::from_utf8(&doc.bytes)
        .unwrap_or("")
        .lines()
        .map(String::from)
        .collect();
    let lines: &[String] = lines_arc.as_deref().unwrap_or(&fallback_lines);

    visit_tree(doc.tree.root_node(), &mut |node| {
        if node.kind() == KIND_LAMBDA_LIT {
            if let Some(params) = node.first_child_of_kind(KIND_LAMBDA_PARAMS) {
                for param in params.children_of_kind(KIND_VAR_DECL) {
                    if let Some(name) = param.first_child_of_kind(KIND_SIMPLE_IDENT) {
                        let modifiers = modifier_bit(&SemanticTokenModifier::DECLARATION);
                        push_token(
                            name,
                            type_index(&SemanticTokenType::PARAMETER),
                            modifiers,
                            src,
                            &mut tokens,
                        );
                    }
                }
            }
            return;
        }

        if node.kind() == KIND_THIS_EXPR && node.enclosing_lambda_literal().is_some() {
            let pos = crate::types::CursorPos {
                line: node.start_position().row,
                utf16_col: src.col_utf16(
                    node.start_position().row,
                    node.start_position().column,
                ) as usize,
            };
            if find_this_element_type_in_lines(lines, pos, indexer, uri).is_some() {
                push_token(
                    node,
                    type_index(&SemanticTokenType::KEYWORD),
                    0,
                    src,
                    &mut tokens,
                );
            }
            return;
        }

        if node.kind() != KIND_SIMPLE_IDENT || is_inside_lambda_parameters(node) {
            return;
        }

        let Some(name) = node.utf8_text_owned(&doc.bytes) else {
            return;
        };
        let pos = crate::types::CursorPos {
            line: node.start_position().row,
            utf16_col: src.col_utf16(
                node.start_position().row,
                node.start_position().column,
            ) as usize,
        };

        if name == "it" {
            if node.enclosing_lambda_literal().is_some()
                && find_it_element_type_in_lines(lines, pos, indexer, uri).is_some()
            {
                push_token(
                    node,
                    type_index(&SemanticTokenType::PARAMETER),
                    0,
                    src,
                    &mut tokens,
                );
            }
            return;
        }

        if node.enclosing_lambda_literal().is_some()
            && indexer
                .lambda_params_at_col(uri, pos.line, pos.utf16_col)
                .iter()
                .any(|param| param == &name)
        {
            push_token(
                node,
                type_index(&SemanticTokenType::PARAMETER),
                0,
                src,
                &mut tokens,
            );
        }
    });
    tokens
}

// ─── Reference-site walker (Phase 2) ─────────────────────────────────────────

/// Walk non-declaration identifiers and resolve them against the index.
/// Emits tokens for type references, function calls, member accesses, etc.
fn walk_references(
    doc: &LiveDoc,
    src: &Source<'_>,
    language: Language,
    indexer: &Indexer,
    uri: &Url,
    raw: &mut Vec<RawToken>,
) {
    if language != Language::Kotlin {
        return;
    }
    // Tier 1: direct index lookups (type refs, top-level calls, annotations)
    let mut resolved = Vec::new();
    walk_kotlin_references(doc.tree.root_node(), src, indexer, &mut resolved);
    raw.extend(resolved);

    // Tier 2: receiver-inferred member coloring
    raw.extend(resolve_member_access(doc, src, indexer, uri));

    // Tier 3: lambda params (it/this)
    raw.extend(resolve_lambda_params(doc, src, indexer, uri));
}

fn walk_kotlin_references(node: Node<'_>, src: &Source<'_>, indexer: &Indexer, out: &mut Vec<RawToken>) {
    // Emit keyword tokens for Kotlin soft keywords
    if is_kotlin_keyword_node(node) {
        push_token(node, type_index(&SemanticTokenType::KEYWORD), 0, src, out);
    } else if let Some(token_type) = classify_kotlin_reference(node, src.bytes, indexer) {
        push_token(node, token_type, 0, src, out);
    }
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk_kotlin_references(cursor.node(), src, indexer, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Kotlin soft keywords and delegation keywords that benefit from distinct coloring.
fn is_kotlin_keyword_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "by" | "where" | "get" | "set" | "is" | "as" | "in" | "constructor"
    )
}

fn classify_kotlin_reference(node: Node<'_>, src: &[u8], indexer: &Indexer) -> Option<u32> {
    if !matches!(node.kind(), KIND_SIMPLE_IDENT | KIND_TYPE_IDENT) || is_declaration_site(node) {
        return None;
    }

    // Named arguments: `foo(name = value)` — the label is a parameter reference.
    // Matches kotlin-lsp (JetBrains) behaviour: resolves to KaValueParameterSymbol → PARAMETER.
    if is_named_argument_label(node) {
        return Some(type_index(&SemanticTokenType::PARAMETER));
    }

    if is_annotation_reference(node) {
        // Annotations are unambiguous from CST position — always emit DECORATOR
        // regardless of whether the annotation class is in our index.
        return Some(type_index(&SemanticTokenType::DECORATOR));
    }

    if let Some(token_type) = enum_entry_reference_token(node, src, indexer) {
        return Some(token_type);
    }

    if node.kind() == KIND_TYPE_IDENT && is_type_reference(node) {
        // Try to resolve for exact kind (class vs interface vs enum)
        if let Some(resolved) = resolve_symbol_kind(node_text(node, src), indexer, is_type_symbol) {
            return symbol_kind_to_token_type(resolved.kind);
        }
        // Unresolved type references are still types — emit CLASS as default
        return Some(type_index(&SemanticTokenType::CLASS));
    }

    if is_top_level_call_name(node) {
        return resolve_symbol_kind(node_text(node, src), indexer, |kind| {
            matches!(kind, SymbolKind::CLASS | SymbolKind::STRUCT | SymbolKind::FUNCTION | SymbolKind::METHOD)
        })
        .and_then(|resolved| call_symbol_kind_to_token_type(resolved.kind));
    }

    if is_navigation_receiver(node) {
        return resolve_symbol_kind(node_text(node, src), indexer, |kind| kind == SymbolKind::OBJECT)
            .map(|_| type_index(&SemanticTokenType::NAMESPACE));
    }

    None
}

fn is_declaration_site(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else { return false };
    let pk = parent.kind();
    if pk == KIND_CLASS_DECL || pk == KIND_OBJECT_DECL || pk == KIND_COMPANION_OBJ || pk == "type_alias" {
        return node.kind() == KIND_TYPE_IDENT;
    }
    if pk == KIND_FUN_DECL || pk == KIND_PARAMETER || pk == KIND_ENUM_ENTRY || pk == KIND_VAR_DECL || pk == "class_parameter" {
        return node.kind() == KIND_SIMPLE_IDENT;
    }
    if pk == KIND_TYPE_PARAM {
        return node.kind() == KIND_SIMPLE_IDENT || node.kind() == KIND_TYPE_IDENT;
    }
    false
}

fn is_annotation_reference(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else { return false };
    if parent.kind() != KIND_USER_TYPE {
        return false;
    }
    let Some(grandparent) = parent.parent() else { return false };
    // @Simple annotation: annotation > user_type > type_identifier
    if grandparent.kind() == KIND_ANNOTATION || grandparent.kind() == KIND_MULTI_ANNOTATION {
        return true;
    }
    // @Named("key") annotation: annotation > constructor_invocation > user_type > type_identifier
    if grandparent.kind() == KIND_CONSTRUCTOR_INVOCATION {
        return grandparent
            .parent()
            .is_some_and(|gp| gp.kind() == KIND_ANNOTATION || gp.kind() == KIND_MULTI_ANNOTATION);
    }
    false
}

fn is_type_reference(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        let k = parent.kind();
        k == KIND_USER_TYPE || k == KIND_FUN_DECL || k == KIND_PROP_DECL || k == "class_parameter"
    })
}

fn is_top_level_call_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else { return false };
    parent.kind() == KIND_CALL_EXPR
        && parent
            .named_child(0)
            .is_some_and(|first_child| first_child.id() == node.id())
}

fn is_navigation_receiver(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else { return false };
    parent.kind() == KIND_NAV_EXPR
        && parent
            .named_child(0)
            .is_some_and(|first_child| first_child.id() == node.id())
}

/// Named argument label: `foo(name = value)` — the `name` identifier is the
/// first child of `value_argument` and is followed by `=`.
fn is_named_argument_label(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else { return false };
    if parent.kind() != KIND_VALUE_ARG {
        return false;
    }
    // The label is the first named child and must be followed by "="
    let Some(first) = parent.named_child(0) else { return false };
    if first.id() != node.id() {
        return false;
    }
    node.next_sibling()
        .is_some_and(|sib| sib.kind() == KIND_EQ)
}

fn enum_entry_reference_token(node: Node<'_>, src: &[u8], indexer: &Indexer) -> Option<u32> {
    let parent = node.parent()?;
    let navigation = parent.parent()?;
    if parent.kind() != "navigation_suffix" || navigation.kind() != "navigation_expression" {
        return None;
    }

    let receiver = navigation.named_child(0)?;
    let receiver_kind =
        resolve_symbol_kind(node_text(receiver, src), indexer, |kind| kind == SymbolKind::ENUM)?;
    let receiver_data = indexer.files.get(receiver_kind.uri.as_str())?;
    receiver_data
        .symbols
        .iter()
        .find(|symbol| {
            symbol.kind == SymbolKind::ENUM_MEMBER
                && symbol.name == node_text(node, src)
                && range_contains(&receiver_kind.range, &symbol.selection_range.start)
        })
        .map(|_| type_index(&SemanticTokenType::ENUM_MEMBER))
}

fn resolve_symbol_kind(
    name: &str,
    indexer: &Indexer,
    matches_kind: impl Fn(SymbolKind) -> bool,
) -> Option<ResolvedReference> {
    for location in indexer.definition_locations(name) {
        let Some(data) = indexer.files.get(location.uri.as_str()) else {
            continue;
        };
        let Some(symbol) = data
            .symbols
            .iter()
            .find(|entry| entry.selection_range == location.range)
        else {
            continue;
        };
        if matches_kind(symbol.kind) {
            return Some(ResolvedReference {
                kind: symbol.kind,
                uri: location.uri.clone(),
                range: symbol.range,
            });
        }
    }
    None
}

fn call_symbol_kind_to_token_type(kind: SymbolKind) -> Option<u32> {
    match kind {
        SymbolKind::CLASS | SymbolKind::STRUCT => Some(type_index(&SemanticTokenType::CLASS)),
        _ => symbol_kind_to_token_type(kind),
    }
}

fn symbol_kind_to_token_type(kind: SymbolKind) -> Option<u32> {
    match kind {
        SymbolKind::CLASS | SymbolKind::STRUCT => Some(type_index(&SemanticTokenType::CLASS)),
        SymbolKind::INTERFACE => Some(type_index(&SemanticTokenType::INTERFACE)),
        SymbolKind::ENUM => Some(type_index(&SemanticTokenType::ENUM)),
        SymbolKind::FUNCTION => Some(type_index(&SemanticTokenType::FUNCTION)),
        SymbolKind::METHOD => Some(type_index(&SemanticTokenType::METHOD)),
        SymbolKind::PROPERTY => Some(type_index(&SemanticTokenType::PROPERTY)),
        SymbolKind::VARIABLE => Some(type_index(&SemanticTokenType::VARIABLE)),
        SymbolKind::FIELD => Some(type_index(&SemanticTokenType::PROPERTY)),
        SymbolKind::ENUM_MEMBER => Some(type_index(&SemanticTokenType::ENUM_MEMBER)),
        SymbolKind::OBJECT => Some(type_index(&SemanticTokenType::NAMESPACE)),
        _ => None,
    }
}

fn range_contains(range: &Range, position: &tower_lsp::lsp_types::Position) -> bool {
    (range.start.line, range.start.character) <= (position.line, position.character)
        && (position.line, position.character) <= (range.end.line, range.end.character)
}

struct ResolvedReference {
    kind: SymbolKind,
    uri: Url,
    range: Range,
}

// ─── Public API ──────────────────────────────────────────────────────────────

pub(crate) fn full_tokens(indexer: &Indexer, uri: &Url, doc: &LiveDoc, language: Language) -> SemanticTokens {
    SemanticTokens {
        result_id: None,
        data: collect_tokens(doc, language, None, Some(indexer), Some(uri)),
    }
}

pub(crate) fn range_tokens(indexer: &Indexer, uri: &Url, doc: &LiveDoc, language: Language, range: &Range) -> SemanticTokens {
    SemanticTokens {
        result_id: None,
        data: collect_tokens(doc, language, Some(range), Some(indexer), Some(uri)),
    }
}

/// CST-only tokens without cross-file resolution — used by unit tests that
/// don't set up a full Indexer.
#[cfg(test)]
pub(crate) fn full_tokens_cst_only(doc: &LiveDoc, language: Language) -> SemanticTokens {
    SemanticTokens {
        result_id: None,
        data: collect_tokens(doc, language, None, None, None),
    }
}

#[cfg(test)]
pub(crate) fn range_tokens_cst_only(doc: &LiveDoc, language: Language, range: &Range) -> SemanticTokens {
    SemanticTokens {
        result_id: None,
        data: collect_tokens(doc, language, Some(range), None, None),
    }
}

#[cfg(test)]
#[path = "semantic_tokens_tests.rs"]
mod tests;
