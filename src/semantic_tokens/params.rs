//! Phase 1b: parameter use-site coloring (CST-only, no index required).

use tower_lsp::lsp_types::SemanticTokenType;
use tree_sitter::Node;

use crate::queries::{
    KIND_BLOCK, KIND_CATCH_BLOCK, KIND_CONTROL_STRUCTURE_BODY, KIND_FOR_STMT, KIND_FUN_BODY,
    KIND_FUN_DECL, KIND_FUN_VALUE_PARAMS, KIND_INTERP_IDENT, KIND_LAMBDA_LIT, KIND_METHOD_DECL,
    KIND_MULTI_VAR_DECL, KIND_PARAMETER, KIND_PROP_DECL, KIND_SIMPLE_IDENT, KIND_STATEMENTS,
    KIND_VAR_DECL,
};

use super::helpers::{
    child_ident, first_child_of_kind, is_declaration_site, push_token, visit_tree,
};
use super::{type_index, RawToken, Source};

/// Emit PARAMETER tokens for every use of a function parameter within its body.
pub(super) fn emit_kotlin_param_uses(root: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    visit_tree(root, &mut |node| {
        if node.kind() == KIND_FUN_DECL || node.kind() == KIND_METHOD_DECL {
            emit_param_uses_for_function(node, src, out);
        }
    });
}

fn collect_fun_param_names(fn_node: Node<'_>, bytes: &[u8]) -> Vec<String> {
    let Some(params_node) = first_child_of_kind(fn_node, KIND_FUN_VALUE_PARAMS) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut cursor = params_node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.kind() == KIND_PARAMETER {
                if let Some(ident) = child_ident(child) {
                    if let Ok(name) = ident.utf8_text(bytes) {
                        names.push(name.to_owned());
                    }
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    names
}

fn emit_param_uses_for_function(fn_node: Node<'_>, src: &Source<'_>, out: &mut Vec<RawToken>) {
    let params = collect_fun_param_names(fn_node, src.bytes);
    if params.is_empty() {
        return;
    }
    let Some(body) = first_child_of_kind(fn_node, KIND_FUN_BODY) else {
        return;
    };
    emit_param_refs_in_scope(body, &params, &[], src, out);
}

fn emit_param_refs_in_scope(
    node: Node<'_>,
    params: &[String],
    shadowed: &[String],
    src: &Source<'_>,
    out: &mut Vec<RawToken>,
) {
    if node.kind() == KIND_FOR_STMT {
        let body = first_child_of_kind(node, KIND_CONTROL_STRUCTURE_BODY);
        let shadow_name = local_binding_name(node, params, src.bytes);
        let mut shadowed_in_body = shadowed.to_vec();
        if let Some(name) = shadow_name.as_ref() {
            if !shadowed_in_body.iter().any(|shadow| shadow == name) {
                shadowed_in_body.push(name.clone());
            }
        }

        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                let child_shadowed = if body.is_some_and(|body_node| body_node.id() == child.id()) {
                    shadowed_in_body.as_slice()
                } else {
                    shadowed
                };
                emit_param_refs_in_scope(child, params, child_shadowed, src, out);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        return;
    }

    if matches!(
        node.kind(),
        KIND_FUN_BODY
            | KIND_LAMBDA_LIT
            | KIND_CONTROL_STRUCTURE_BODY
            | KIND_CATCH_BLOCK
            | KIND_BLOCK
            | KIND_STATEMENTS
    ) {
        let mut local_shadowed = shadowed.to_vec();
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                let new_shadow = local_binding_name(child, params, src.bytes);
                emit_param_refs_in_scope(child, params, &local_shadowed, src, out);
                if let Some(name) = new_shadow {
                    if !local_shadowed.iter().any(|shadow| shadow == &name) {
                        local_shadowed.push(name);
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        return;
    }

    if matches!(node.kind(), KIND_SIMPLE_IDENT | KIND_INTERP_IDENT)
        && !is_declaration_site(node)
        && node.utf8_text(src.bytes).is_ok_and(|name| {
            !shadowed.iter().any(|s| s == name) && params.iter().any(|p| p == name)
        })
    {
        push_token(node, type_index(&SemanticTokenType::PARAMETER), 0, src, out);
        return;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            emit_param_refs_in_scope(cursor.node(), params, shadowed, src, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn local_binding_name(node: Node<'_>, params: &[String], bytes: &[u8]) -> Option<String> {
    match node.kind() {
        KIND_PROP_DECL | KIND_FOR_STMT => {
            if let Some(multi) = first_child_of_kind(node, KIND_MULTI_VAR_DECL) {
                let mut cursor = multi.walk();
                for child in multi.children(&mut cursor) {
                    if child.kind() == KIND_VAR_DECL {
                        if let Some(ident) = child_ident(child) {
                            if let Ok(name) = ident.utf8_text(bytes) {
                                if params.contains(&name.to_owned()) {
                                    return Some(name.to_owned());
                                }
                            }
                        }
                    }
                }
                return None;
            }
            let variable_declaration = first_child_of_kind(node, KIND_VAR_DECL)?;
            let ident = child_ident(variable_declaration)?;
            let name = ident.utf8_text(bytes).ok()?.to_owned();
            params.contains(&name).then_some(name)
        }
        _ => None,
    }
}
