//! Phase 2: index-based reference resolution, member access, and lambda params.

use tower_lsp::lsp_types::{Position, Range, SemanticTokenModifier, SemanticTokenType, SymbolKind, Url};
use tree_sitter::Node;

use crate::indexer::{
    find_it_element_type_in_lines, find_this_element_type_in_lines, Indexer, LiveDoc, NodeExt,
};
use crate::queries::{
    KIND_CALL_EXPR, KIND_KW_AS, KIND_KW_BY, KIND_KW_CONSTRUCTOR, KIND_KW_GET, KIND_KW_IN,
    KIND_KW_IS, KIND_KW_SET, KIND_KW_WHERE, KIND_LAMBDA_LIT, KIND_LAMBDA_PARAMS, KIND_NAV_EXPR,
    KIND_SIMPLE_IDENT, KIND_THIS_EXPR, KIND_TYPE_IDENT, KIND_VAR_DECL,
};
use crate::resolver::infer::{
    find_field_type_in_class, find_fun_return_type_by_name, find_method_return_type,
    infer_variable_type,
};
use crate::Language;

use super::helpers::{
    is_annotation_reference, is_call_callee, is_declaration_site, is_inside_lambda_parameters,
    is_named_argument_label, is_navigation_receiver, is_top_level_call_name, is_type_reference,
    navigation_member_ident, navigation_receiver_node, node_text, push_token, token_position,
    visit_tree,
};
use super::{modifier_bit, type_index, RawToken, Source};

/// Walk non-declaration identifiers and resolve them against the index.
pub(super) fn walk_references(
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

fn walk_kotlin_references(
    node: Node<'_>,
    src: &Source<'_>,
    indexer: &Indexer,
    out: &mut Vec<RawToken>,
) {
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

fn is_kotlin_keyword_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        KIND_KW_BY
            | KIND_KW_WHERE
            | KIND_KW_GET
            | KIND_KW_SET
            | KIND_KW_IS
            | KIND_KW_AS
            | KIND_KW_IN
            | KIND_KW_CONSTRUCTOR
    )
}

fn classify_kotlin_reference(node: Node<'_>, src: &[u8], indexer: &Indexer) -> Option<u32> {
    if !matches!(node.kind(), KIND_SIMPLE_IDENT | KIND_TYPE_IDENT) || is_declaration_site(node) {
        return None;
    }

    if is_named_argument_label(node) {
        return Some(type_index(&SemanticTokenType::PARAMETER));
    }

    if is_annotation_reference(node) {
        return None;
    }

    if let Some(token_type) = enum_entry_reference_token(node, src, indexer) {
        return Some(token_type);
    }

    if node.kind() == KIND_TYPE_IDENT && is_type_reference(node) {
        if let Some(resolved) = resolve_symbol_kind(node_text(node, src), indexer, is_type_symbol) {
            return symbol_kind_to_token_type(resolved.kind);
        }
        return Some(type_index(&SemanticTokenType::CLASS));
    }

    if is_top_level_call_name(node) {
        return resolve_symbol_kind(node_text(node, src), indexer, |kind| {
            matches!(
                kind,
                SymbolKind::CLASS | SymbolKind::STRUCT | SymbolKind::FUNCTION | SymbolKind::METHOD
            )
        })
        .and_then(|resolved| call_symbol_kind_to_token_type(resolved.kind));
    }

    if is_navigation_receiver(node) {
        return resolve_symbol_kind(node_text(node, src), indexer, |kind| {
            kind == SymbolKind::OBJECT
        })
        .map(|_| type_index(&SemanticTokenType::NAMESPACE));
    }

    None
}

// ─── Member access resolution ────────────────────────────────────────────────

fn resolve_member_access(
    doc: &LiveDoc,
    src: &Source<'_>,
    indexer: &Indexer,
    uri: &Url,
) -> Vec<RawToken> {
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
            .and_then(|receiver_type| {
                member_token_type_for_receiver(indexer, &receiver_type, &member_name)
            });
        let method_type = type_index(&SemanticTokenType::METHOD);
        let property_type = type_index(&SemanticTokenType::PROPERTY);
        let token_type = if is_call {
            Some(
                resolved_type
                    .map(|t| if t == property_type { method_type } else { t })
                    .unwrap_or(method_type),
            )
        } else {
            resolved_type
        };
        if let Some(token_type) = token_type {
            push_token(member_ident, token_type, 0, src, &mut tokens);
        }
    });
    tokens
}

// ─── Lambda parameter resolution ─────────────────────────────────────────────

fn resolve_lambda_params(
    doc: &LiveDoc,
    src: &Source<'_>,
    indexer: &Indexer,
    uri: &Url,
) -> Vec<RawToken> {
    let mut tokens = Vec::new();
    let lines_arc = indexer.mem_lines_for(uri.as_str());
    let fallback;
    let lines: &[String] = match lines_arc.as_deref() {
        Some(l) => l,
        None => {
            fallback = std::str::from_utf8(&doc.bytes)
                .unwrap_or("")
                .lines()
                .map(String::from)
                .collect::<Vec<_>>();
            &fallback
        }
    };

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
                utf16_col: src.col_utf16(node.start_position().row, node.start_position().column)
                    as usize,
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
            utf16_col: src.col_utf16(node.start_position().row, node.start_position().column)
                as usize,
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

// ─── Type inference helpers ──────────────────────────────────────────────────

fn identifier_type(
    node: Node<'_>,
    doc: &LiveDoc,
    starts: &[usize],
    indexer: &Indexer,
    uri: &Url,
) -> Option<String> {
    let name = node.utf8_text_owned(&doc.bytes)?;
    if let Some(inferred) =
        indexer.infer_lambda_param_type_at(&name, uri, token_position(&doc.bytes, starts, node))
    {
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

fn call_expression_type(
    node: Node<'_>,
    doc: &LiveDoc,
    starts: &[usize],
    indexer: &Indexer,
    uri: &Url,
) -> Option<String> {
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

fn expression_type(
    node: Node<'_>,
    doc: &LiveDoc,
    starts: &[usize],
    indexer: &Indexer,
    uri: &Url,
) -> Option<String> {
    match node.kind() {
        KIND_SIMPLE_IDENT | KIND_TYPE_IDENT => identifier_type(node, doc, starts, indexer, uri),
        KIND_THIS_EXPR => indexer.infer_lambda_param_type_at(
            "this",
            uri,
            token_position(&doc.bytes, starts, node),
        ),
        KIND_NAV_EXPR => navigation_expression_type(node, doc, starts, indexer, uri),
        KIND_CALL_EXPR => call_expression_type(node, doc, starts, indexer, uri),
        _ => None,
    }
}

// ─── Symbol resolution helpers ───────────────────────────────────────────────

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
    let start_ok =
        (inner.start.line, inner.start.character) >= (outer.start.line, outer.start.character);
    let end_ok = (inner.end.line, inner.end.character) <= (outer.end.line, outer.end.character);
    start_ok && end_ok
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

fn owner_member_token_type(
    indexer: &Indexer,
    receiver_type: &str,
    member_name: &str,
) -> Option<u32> {
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
        if let Some(symbol) = file_data.symbols.iter().find(|symbol| {
            symbol.name == member_name
                && is_member_symbol(symbol.kind)
                && range_within(&symbol.range, &owner_range)
        }) {
            return member_token_type(symbol.kind);
        }
    }
    None
}

fn extension_member_token_type(
    indexer: &Indexer,
    receiver_type: &str,
    member_name: &str,
) -> Option<u32> {
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

fn enum_entry_reference_token(node: Node<'_>, src: &[u8], indexer: &Indexer) -> Option<u32> {
    let parent = node.parent()?;
    let navigation = parent.parent()?;
    if parent.kind() != crate::queries::KIND_NAV_SUFFIX || navigation.kind() != KIND_NAV_EXPR {
        return None;
    }

    let receiver = navigation.named_child(0)?;
    let receiver_kind = resolve_symbol_kind(node_text(receiver, src), indexer, |kind| {
        kind == SymbolKind::ENUM
    })?;
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

fn range_contains(range: &Range, position: &Position) -> bool {
    (range.start.line, range.start.character) <= (position.line, position.character)
        && (position.line, position.character) < (range.end.line, range.end.character)
}

struct ResolvedReference {
    kind: SymbolKind,
    uri: Url,
    range: Range,
}
