//! Phase 1 CST-only classification for Kotlin declarations.

use tower_lsp::lsp_types::{SemanticTokenModifier, SemanticTokenType};
use tree_sitter::Node;

use crate::queries::{
    KIND_ANNOTATION, KIND_CLASS_DECL, KIND_CLASS_PARAM, KIND_COMPANION_OBJ, KIND_ENUM_ENTRY,
    KIND_FUN_DECL, KIND_KW_AS, KIND_KW_AS_SAFE, KIND_KW_BY, KIND_KW_IN, KIND_KW_IN_NOT,
    KIND_KW_IS, KIND_KW_IS_NOT, KIND_MULTI_ANNOTATION, KIND_MULTI_VAR_DECL, KIND_OBJECT_DECL,
    KIND_PARAMETER, KIND_PROP_DECL, KIND_SIMPLE_IDENT, KIND_TYPE_IDENT, KIND_TYPE_PARAM,
    KIND_VALUE_ARG, KIND_VAR_DECL,
};

use super::helpers::{
    child_ident, find_annotation_ident, first_child_of_kind, has_deprecated_annotation,
    has_keyword_child, has_modifier, is_in_companion_body, is_inside_class_body, node_text,
    push_token, value_arg_label,
};
use super::{modifier_bit, type_index, RawToken, Source};

pub(super) fn walk_kotlin(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
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
        k if k == KIND_CLASS_PARAM => kotlin_class_param_token(node, src, out),
        KIND_PARAMETER => {
            let mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
            if let Some(name) = child_ident(node) {
                push_token(
                    name,
                    type_index(&SemanticTokenType::PARAMETER),
                    mods,
                    src,
                    out,
                );
            }
        }
        KIND_ENUM_ENTRY => {
            let mods = modifier_bit(&SemanticTokenModifier::DECLARATION)
                | modifier_bit(&SemanticTokenModifier::READONLY);
            if let Some(name) = child_ident(node) {
                push_token(
                    name,
                    type_index(&SemanticTokenType::ENUM_MEMBER),
                    mods,
                    src,
                    out,
                );
            }
        }
        KIND_ANNOTATION | KIND_MULTI_ANNOTATION => {
            if find_annotation_ident(node).is_some() {
                push_token(node, type_index(&SemanticTokenType::DECORATOR), 0, src, out);
            }
        }
        k if k == KIND_VALUE_ARG => {
            if let Some(label) = value_arg_label(node) {
                push_token(
                    label,
                    type_index(&SemanticTokenType::PARAMETER),
                    0,
                    src,
                    out,
                );
            }
        }
        k if k == KIND_KW_IS
            || k == KIND_KW_IS_NOT
            || k == KIND_KW_AS
            || k == KIND_KW_AS_SAFE
            || k == KIND_KW_IN
            || k == KIND_KW_IN_NOT
            || k == KIND_KW_BY =>
        {
            push_token(node, type_index(&SemanticTokenType::KEYWORD), 0, src, out);
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
    if has_deprecated_annotation(node, src.bytes) {
        mods |= modifier_bit(&SemanticTokenModifier::DEPRECATED);
    }
    if let Some(name) = child_ident(node) {
        push_token(name, token_type, mods, src, out);
    }
}

fn kotlin_object_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let mut mods = modifier_bit(&SemanticTokenModifier::DECLARATION);
    if has_deprecated_annotation(node, src.bytes) {
        mods |= modifier_bit(&SemanticTokenModifier::DEPRECATED);
    }
    if let Some(name) = child_ident(node) {
        push_token(
            name,
            type_index(&SemanticTokenType::NAMESPACE),
            mods,
            src,
            out,
        );
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
    if has_deprecated_annotation(node, src.bytes) {
        mods |= modifier_bit(&SemanticTokenModifier::DEPRECATED);
    }
    if is_in_companion_body(node) {
        mods |= modifier_bit(&SemanticTokenModifier::STATIC);
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
    if has_deprecated_annotation(node, src.bytes) {
        mods |= modifier_bit(&SemanticTokenModifier::DEPRECATED);
    }
    if is_in_companion_body(node) {
        mods |= modifier_bit(&SemanticTokenModifier::STATIC);
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
        push_token(
            ident,
            type_index(&SemanticTokenType::TYPE_PARAMETER),
            mods,
            src,
            out,
        );
    }
}

fn kotlin_class_param_token(node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let has_val = (0..node.child_count()).any(|index| {
        node.child(index)
            .is_some_and(|child| node_text(child, src.bytes) == "val")
    });
    let has_var = (0..node.child_count()).any(|index| {
        node.child(index)
            .is_some_and(|child| node_text(child, src.bytes) == "var")
    });
    let Some(name) = child_ident(node) else {
        return;
    };
    let (token_type, mut mods) = if has_val {
        (
            type_index(&SemanticTokenType::PROPERTY),
            modifier_bit(&SemanticTokenModifier::DECLARATION)
                | modifier_bit(&SemanticTokenModifier::READONLY),
        )
    } else if has_var {
        (
            type_index(&SemanticTokenType::PROPERTY),
            modifier_bit(&SemanticTokenModifier::DECLARATION),
        )
    } else {
        (
            type_index(&SemanticTokenType::PARAMETER),
            modifier_bit(&SemanticTokenModifier::DECLARATION),
        )
    };
    if is_in_companion_body(node) {
        mods |= modifier_bit(&SemanticTokenModifier::STATIC);
    }
    if has_deprecated_annotation(node, src.bytes) {
        mods |= modifier_bit(&SemanticTokenModifier::DEPRECATED);
    }
    push_token(name, token_type, mods, src, out);
}
