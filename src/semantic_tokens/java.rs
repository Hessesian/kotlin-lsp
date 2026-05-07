//! Phase 1 CST-only classification for Java declarations.

use tower_lsp::lsp_types::{SemanticTokenModifier, SemanticTokenType};
use tree_sitter::Node;

use crate::queries::{
    KIND_ANNOTATION, KIND_ANNOTATION_TYPE_DECL, KIND_CLASS_DECL, KIND_ENUM_CONSTANT,
    KIND_ENUM_JAVA_DECL, KIND_FIELD_DECL, KIND_FORMAL_PARAM, KIND_IDENTIFIER,
    KIND_INTERFACE_DECL, KIND_MARKER_ANNOTATION, KIND_METHOD_DECL, KIND_MODIFIERS,
    KIND_MOD_ABSTRACT, KIND_MOD_FINAL, KIND_MOD_STATIC, KIND_RECORD_DECL, KIND_SPREAD_PARAM,
    KIND_TYPE_IDENT, KIND_TYPE_PARAM, KIND_VAR_DECLARATOR,
};

use super::helpers::{child_ident, first_child_of_kind, push_token};
use super::{modifier_bit, type_index, RawToken, Source};

pub(super) fn walk_java(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
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
    match node.kind() {
        k if k == KIND_CLASS_DECL || k == KIND_RECORD_DECL => {
            push_java_class_like_token(node, src, out)
        }
        k if k == KIND_INTERFACE_DECL || k == KIND_ANNOTATION_TYPE_DECL => {
            push_java_interface_like_token(node, k, src, out)
        }
        KIND_ENUM_JAVA_DECL => push_java_enum_token(node, src, out),
        KIND_METHOD_DECL => push_java_method_token(node, src, out),
        KIND_FIELD_DECL => push_java_field_tokens(node, src, out),
        KIND_FORMAL_PARAM | KIND_SPREAD_PARAM => push_java_parameter_token(node, src, out),
        KIND_ENUM_CONSTANT => push_java_enum_member_token(node, src, out),
        KIND_TYPE_PARAM => push_java_type_parameter_token(node, src, out),
        KIND_MARKER_ANNOTATION | KIND_ANNOTATION => push_java_annotation_token(node, src, out),
        _ => {}
    }
}

fn push_java_class_like_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let mut mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
    if has_java_modifier(node, KIND_MOD_ABSTRACT) {
        mods |= modifier_bit(&SemanticTokenModifier::ABSTRACT);
    }
    if has_java_deprecated(node, src.bytes) {
        mods |= modifier_bit(&SemanticTokenModifier::DEPRECATED);
    }
    if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
        push_token(name, type_index(&SemanticTokenType::CLASS), mods, src, out);
    }
}

fn push_java_interface_like_token(
    node: Node<'_>,
    kind: &str,
    src: &Source<'_>,
    out: &mut Vec<RawToken>,
) {
    let token_type = if kind == KIND_ANNOTATION_TYPE_DECL {
        type_index(&SemanticTokenType::DECORATOR)
    } else {
        type_index(&SemanticTokenType::INTERFACE)
    };
    let mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
    if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
        push_token(name, token_type, mods, src, out);
    }
}

fn push_java_enum_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
    if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
        push_token(name, type_index(&SemanticTokenType::ENUM), mods, src, out);
    }
}

fn push_java_method_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let mut mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
    if has_java_modifier(node, KIND_MOD_ABSTRACT) {
        mods |= modifier_bit(&SemanticTokenModifier::ABSTRACT);
    }
    if has_java_deprecated(node, src.bytes) {
        mods |= modifier_bit(&SemanticTokenModifier::DEPRECATED);
    }
    if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
        push_token(name, type_index(&SemanticTokenType::METHOD), mods, src, out);
    }
}

fn push_java_field_tokens(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let mut mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
    if has_java_modifier(node, KIND_MOD_FINAL) {
        mods |= modifier_bit(&SemanticTokenModifier::READONLY);
    }
    if has_java_modifier(node, KIND_MOD_STATIC) {
        mods |= modifier_bit(&SemanticTokenModifier::STATIC);
    }
    if has_java_deprecated(node, src.bytes) {
        mods |= modifier_bit(&SemanticTokenModifier::DEPRECATED);
    }
    for index in 0..node.child_count() {
        let Some(child) = node.child(index) else {
            continue;
        };
        if child.kind() != KIND_VAR_DECLARATOR {
            continue;
        }
        if let Some(name) = first_child_of_kind(child, KIND_IDENTIFIER) {
            push_token(
                name,
                type_index(&SemanticTokenType::PROPERTY),
                mods,
                src,
                out,
            );
        }
    }
}

fn push_java_parameter_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
    if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
        push_token(
            name,
            type_index(&SemanticTokenType::PARAMETER),
            mods,
            src,
            out,
        );
    }
}

fn push_java_enum_member_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let mods = modifier_bit(&SemanticTokenModifier::DECLARATION)
        | modifier_bit(&SemanticTokenModifier::READONLY);
    if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
        push_token(
            name,
            type_index(&SemanticTokenType::ENUM_MEMBER),
            mods,
            src,
            out,
        );
    }
}

fn push_java_type_parameter_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
    if let Some(name) = first_child_of_kind(node, KIND_TYPE_IDENT)
        .or_else(|| first_child_of_kind(node, KIND_IDENTIFIER))
    {
        push_token(
            name,
            type_index(&SemanticTokenType::TYPE_PARAMETER),
            mods,
            src,
            out,
        );
    }
}

fn push_java_annotation_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    if let Some(name) = first_child_of_kind(node, KIND_IDENTIFIER) {
        push_token(name, type_index(&SemanticTokenType::DECORATOR), 0, src, out);
    }
}

// ─── Java-only modifier helpers ─────────────────────────────────────────────

fn has_java_modifier(node: Node<'_>, keyword: &str) -> bool {
    let Some(mods) = first_child_of_kind(node, KIND_MODIFIERS) else {
        return false;
    };
    let mut cursor = mods.walk();
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

fn has_java_deprecated(node: Node<'_>, src: &[u8]) -> bool {
    let Some(modifiers) = first_child_of_kind(node, KIND_MODIFIERS) else {
        return false;
    };
    let mut cursor = modifiers.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if (child.kind() == KIND_MARKER_ANNOTATION || child.kind() == KIND_ANNOTATION)
                && child_ident(child)
                    .and_then(|name| name.utf8_text(src).ok())
                    .is_some_and(|text| text == "Deprecated")
            {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    false
}
