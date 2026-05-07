//! Shared tree-sitter helpers used by Kotlin, Java, and resolution modules.

use tree_sitter::Node;

use crate::queries::{
    KIND_ANNOTATION, KIND_CLASS_BODY, KIND_CLASS_DECL, KIND_CLASS_PARAM, KIND_COMPANION_OBJ,
    KIND_CONSTRUCTOR_INVOCATION, KIND_ENUM_CLASS_BODY, KIND_ENUM_ENTRY, KIND_FUN_DECL,
    KIND_IDENTIFIER, KIND_INTERFACE_BODY, KIND_INTERFACE_DECL, KIND_MODIFIERS,
    KIND_MULTI_ANNOTATION, KIND_OBJECT_BODY, KIND_OBJECT_DECL, KIND_PARAMETER, KIND_PREFIX_EXPR,
    KIND_SIMPLE_IDENT, KIND_TYPE_ALIAS, KIND_TYPE_IDENT, KIND_TYPE_PARAM, KIND_USER_TYPE,
    KIND_VAR_DECL,
};

use super::{RawToken, Source};

pub(super) fn find_annotation_ident(annotation_node: Node<'_>) -> Option<Node<'_>> {
    // Direct: annotation > type_identifier or simple_identifier
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
pub(super) fn child_ident<'a>(node: Node<'a>) -> Option<Node<'a>> {
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

pub(super) fn first_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
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

pub(super) fn has_keyword_child(node: Node<'_>, keyword: &str) -> bool {
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
pub(super) fn has_modifier(node: Node<'_>, src: &Source<'_>, keyword: &str) -> bool {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == KIND_MODIFIERS
                && node_text(child, src.bytes)
                    .split_whitespace()
                    .any(|w| w == keyword)
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

fn is_deprecated_annotation(node: Node<'_>, src: &[u8]) -> bool {
    (node.kind() == KIND_ANNOTATION || node.kind() == KIND_MULTI_ANNOTATION)
        && find_annotation_ident(node)
            .and_then(|ident| ident.utf8_text(src).ok())
            .is_some_and(|text| text == "Deprecated")
}

fn contains_deprecated_annotation(node: Node<'_>, src: &[u8]) -> bool {
    if is_deprecated_annotation(node, src) {
        return true;
    }
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if contains_deprecated_annotation(cursor.node(), src) {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    false
}

/// Returns true if the node has a `@Deprecated` annotation.
///
/// Checks the node's own `modifiers` child first, then scans only
/// immediately-preceding annotation/modifier siblings (stops at the first
/// non-annotation sibling to avoid false positives from earlier declarations).
pub(super) fn has_deprecated_annotation(node: Node<'_>, src: &[u8]) -> bool {
    if first_child_of_kind(node, KIND_MODIFIERS)
        .is_some_and(|modifiers| contains_deprecated_annotation(modifiers, src))
    {
        return true;
    }
    let Some(mut sibling) = node.prev_sibling() else {
        return false;
    };
    loop {
        let kind = sibling.kind();
        if kind == KIND_ANNOTATION
            || kind == KIND_MULTI_ANNOTATION
            || kind == KIND_MODIFIERS
            || kind == KIND_PREFIX_EXPR
        {
            if contains_deprecated_annotation(sibling, src) {
                return true;
            }
        } else {
            break;
        }
        let Some(prev) = sibling.prev_sibling() else {
            break;
        };
        sibling = prev;
    }
    false
}

pub(super) fn is_in_companion_body(node: Node<'_>) -> bool {
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if parent.kind() == KIND_COMPANION_OBJ {
            return true;
        }
        if parent.kind() == KIND_CLASS_DECL
            || parent.kind() == KIND_INTERFACE_DECL
            || parent.kind() == KIND_OBJECT_DECL
        {
            return false;
        }
        ancestor = parent.parent();
    }
    false
}

/// True when this node is a direct child of a class/interface/enum body.
pub(super) fn is_inside_class_body(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let kind = parent.kind();
    kind == KIND_CLASS_BODY
        || kind == KIND_INTERFACE_BODY
        || kind == KIND_ENUM_CLASS_BODY
        || kind == KIND_OBJECT_BODY
}

pub(super) fn node_text<'a>(node: Node<'_>, src: &'a [u8]) -> &'a str {
    node.utf8_text(src).unwrap_or("")
}

pub(super) fn push_token(
    node: Node<'_>,
    token_type: u32,
    token_modifiers_bitset: u32,
    src: &Source<'_>,
    out: &mut Vec<RawToken>,
) {
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

pub(super) fn is_declaration_site(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let pk = parent.kind();
    if pk == KIND_CLASS_DECL
        || pk == KIND_OBJECT_DECL
        || pk == KIND_COMPANION_OBJ
        || pk == KIND_TYPE_ALIAS
    {
        return node.kind() == KIND_TYPE_IDENT;
    }
    if pk == KIND_FUN_DECL
        || pk == KIND_PARAMETER
        || pk == KIND_ENUM_ENTRY
        || pk == KIND_VAR_DECL
        || pk == KIND_CLASS_PARAM
    {
        return node.kind() == KIND_SIMPLE_IDENT;
    }
    if pk == KIND_TYPE_PARAM {
        return node.kind() == KIND_SIMPLE_IDENT || node.kind() == KIND_TYPE_IDENT;
    }
    false
}

pub(super) fn visit_tree(node: Node<'_>, f: &mut impl FnMut(Node<'_>)) {
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

pub(super) fn value_arg_label(node: Node<'_>) -> Option<Node<'_>> {
    let first = node.named_child(0)?;
    if first.kind() != KIND_SIMPLE_IDENT {
        return None;
    }
    first
        .next_sibling()
        .is_some_and(|s| s.kind() == crate::queries::KIND_EQ)
        .then_some(first)
}

pub(super) fn is_named_argument_label(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent.kind() == crate::queries::KIND_VALUE_ARG
        && value_arg_label(parent).is_some_and(|l| l.id() == node.id())
}

// ─── Token type helpers ─────────────────────────────────────────────────────

pub(super) fn is_inside_lambda_parameters(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == crate::queries::KIND_LAMBDA_PARAMS {
            return true;
        }
        if parent.kind() == crate::queries::KIND_LAMBDA_LIT {
            return false;
        }
        current = parent.parent();
    }
    false
}

pub(super) fn is_annotation_reference(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != KIND_USER_TYPE {
        return false;
    }
    let Some(grandparent) = parent.parent() else {
        return false;
    };
    if grandparent.kind() == KIND_ANNOTATION || grandparent.kind() == KIND_MULTI_ANNOTATION {
        return true;
    }
    if grandparent.kind() == KIND_CONSTRUCTOR_INVOCATION {
        return grandparent
            .parent()
            .is_some_and(|gp| gp.kind() == KIND_ANNOTATION || gp.kind() == KIND_MULTI_ANNOTATION);
    }
    false
}

pub(super) fn is_type_reference(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        let kind = parent.kind();
        kind == KIND_USER_TYPE
            || kind == crate::queries::KIND_FUN_DECL
            || kind == crate::queries::KIND_PROP_DECL
            || kind == crate::queries::KIND_CLASS_PARAM
    })
}

pub(super) fn is_top_level_call_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent.kind() == crate::queries::KIND_CALL_EXPR
        && parent
            .named_child(0)
            .is_some_and(|first_child| first_child.id() == node.id())
}

pub(super) fn is_navigation_receiver(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent.kind() == crate::queries::KIND_NAV_EXPR
        && parent
            .named_child(0)
            .is_some_and(|first_child| first_child.id() == node.id())
}

pub(super) fn navigation_receiver_node(node: Node<'_>) -> Option<Node<'_>> {
    (0..node.child_count())
        .filter_map(|i| node.child(i))
        .find(|child| child.is_named() && child.kind() != crate::queries::KIND_NAV_SUFFIX)
}

pub(super) fn navigation_member_ident(node: Node<'_>) -> Option<Node<'_>> {
    use crate::indexer::NodeExt;
    let suffix = node.first_child_of_kind(crate::queries::KIND_NAV_SUFFIX)?;
    (0..suffix.child_count())
        .filter_map(|i| suffix.child(i))
        .find(|child| child.kind() == KIND_SIMPLE_IDENT || child.kind() == KIND_TYPE_IDENT)
}

pub(super) fn token_position(
    bytes: &[u8],
    starts: &[usize],
    node: Node<'_>,
) -> tower_lsp::lsp_types::Position {
    let start = node.start_position();
    tower_lsp::lsp_types::Position::new(
        start.row as u32,
        crate::inlay_hints::ts_byte_col_to_utf16(bytes, starts, start.row, start.column) as u32,
    )
}

pub(super) fn is_call_callee(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    parent.kind() == crate::queries::KIND_CALL_EXPR
        && parent.child(0).map(|child| child.id()) == Some(node.id())
}
