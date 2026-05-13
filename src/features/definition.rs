//! `goto_definition` feature — pure lookup, no LSP adapter concerns.
//!
//! Entry point: [`find_definition`] takes an enriched cursor context and
//! capability traits; returns an optional response.  All rg fallback is
//! handled here so the backend adapter is a thin `Ok(find_definition(…).await)`.

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Url};

use crate::backend::cursor::CursorContext;
use crate::features::traits::{DocumentAccess, SearchAccess, SymbolIndex};
use crate::parser::parse_by_extension;
use crate::rg;

// ─── Response helpers ─────────────────────────────────────────────────────────

pub(crate) fn locs_to_response(locs: Vec<Location>) -> GotoDefinitionResponse {
    match locs.len() {
        1 => {
            GotoDefinitionResponse::Scalar(locs.into_iter().next().expect("len == 1 by match arm"))
        }
        _ => GotoDefinitionResponse::Array(locs),
    }
}

pub(crate) fn locs_to_opt_response(locs: Vec<Location>) -> Option<GotoDefinitionResponse> {
    match locs.len() {
        0 => None,
        1 => locs.into_iter().next().map(GotoDefinitionResponse::Scalar),
        _ => Some(GotoDefinitionResponse::Array(locs)),
    }
}

// ─── rg fallback ─────────────────────────────────────────────────────────────

async fn rg_resolve(index: &impl SearchAccess, uri: &Url, name: &str) -> Vec<Location> {
    let name_clone = name.to_string();
    let file_path = uri.to_file_path().ok();
    let (workspace_root, matcher) = index.rg_context();
    let root_opt = rg::effective_rg_root(workspace_root.as_deref(), file_path.as_deref());
    tokio::task::spawn_blocking(move || {
        rg::rg_find_definition(&name_clone, root_opt.as_deref(), matcher.as_deref())
    })
    .await
    .unwrap_or_default()
}

// ─── Super helpers ────────────────────────────────────────────────────────────

/// Collect the parent class names for the class enclosing `row` in `uri`.
pub(crate) fn super_names_at(
    index: &(impl SymbolIndex + DocumentAccess),
    uri: &Url,
    row: u32,
) -> Vec<String> {
    let Some(class_name) = index.enclosing_class_at(uri, row) else {
        return vec![];
    };
    let locs = index.definition_locations(&class_name);
    for loc in &locs {
        if let Some(file) = index.file_data_for(loc.uri.as_str()) {
            let names: Vec<String> = file
                .supers
                .iter()
                .filter(|(l, _, _)| *l == loc.range.start.line)
                .map(|(_, n, _)| n.clone())
                .collect();
            if !names.is_empty() {
                return names;
            }
        }
    }
    // Fallback: parse live_lines for the open file to catch unsaved edits.
    if let Some(lines) = index.mem_lines_for(uri.as_str()) {
        let content = lines.join("\n");
        let names: Vec<String> = parse_by_extension(uri.path(), &content)
            .supers
            .into_iter()
            .map(|(_, n, _)| n)
            .collect();
        if !names.is_empty() {
            return names;
        }
    }
    vec![]
}

pub(crate) async fn goto_super_class(
    index: &(impl SymbolIndex + DocumentAccess + SearchAccess),
    uri: &Url,
    row: u32,
) -> Option<GotoDefinitionResponse> {
    for super_name in &super_names_at(index, uri, row) {
        let locs = index.find_definition_qualified(super_name, None, uri);
        if !locs.is_empty() {
            return Some(locs_to_response(locs));
        }
        let rg_locs = rg_resolve(index, uri, super_name).await;
        if !rg_locs.is_empty() {
            return Some(locs_to_response(rg_locs));
        }
    }
    None
}

pub(crate) async fn goto_super_method(
    index: &(impl SymbolIndex + DocumentAccess + SearchAccess),
    uri: &Url,
    row: u32,
    method: &str,
) -> Option<GotoDefinitionResponse> {
    let locs = index.find_definition_qualified(method, Some("super"), uri);
    if !locs.is_empty() {
        return Some(locs_to_response(locs));
    }
    // Method not found in indexed hierarchy (e.g. Android SDK parent) — fall back
    // to navigating to the parent class itself.
    goto_super_class(index, uri, row).await
}

// ─── Main entry point ─────────────────────────────────────────────────────────

/// Resolve goto-definition for the given cursor context.
///
/// Handles `this`, `super`, `super.method`, contextual lambda receivers,
/// direct qualified lookups, and rg fallback — in that priority order.
pub(crate) async fn find_definition(
    ctx: &CursorContext,
    index: &(impl SymbolIndex + DocumentAccess + SearchAccess),
    uri: &Url,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    // `this` → enclosing class definition.
    if ctx.qualifier.is_none() && ctx.word == "this" {
        if let Some(class_name) = index.enclosing_class_at(uri, position.line) {
            let locs = index.find_definition_qualified(&class_name, None, uri);
            if !locs.is_empty() {
                return Some(locs_to_response(locs));
            }
        }
        return None;
    }

    // `super` → first supertype of the enclosing class.
    if ctx.qualifier.is_none() && ctx.word == "super" {
        return goto_super_class(index, uri, position.line).await;
    }

    // `super.method(...)` → resolve method in the parent class.
    if ctx.qualifier.as_deref() == Some("super") {
        return goto_super_method(index, uri, position.line, &ctx.word).await;
    }

    // `it` / named lambda param → element/receiver type class.
    if ctx.qualifier.is_none() {
        if let Some(ref rt) = ctx.contextual {
            let locs = index.find_definition_qualified(rt.leaf.as_str(), None, uri);
            if !locs.is_empty() {
                return Some(locs_to_response(locs));
            }
        }
        // Lambda param with failed type inference → jump to `{ name -> }`.
        if let Some(loc) = ctx.lambda_decl.as_ref() {
            return Some(GotoDefinitionResponse::Scalar(loc.clone()));
        }
    }

    // `this.field` / `it.field` — already-resolved contextual receiver.
    if ctx.qualifier.is_some() {
        if let Some(ref rt) = ctx.contextual {
            let locs = index.find_definition_qualified(&ctx.word, Some(&rt.qualified), uri);
            let locs = if locs.is_empty() && rt.leaf != rt.qualified {
                index.find_definition_qualified(&ctx.word, Some(&rt.leaf), uri)
            } else {
                locs
            };
            if !locs.is_empty() {
                return Some(locs_to_response(locs));
            }
        }
    }

    // General qualified or bare lookup.
    let locs = index.find_definition_qualified(&ctx.word, ctx.qualifier.as_deref(), uri);
    if !locs.is_empty() {
        return locs_to_opt_response(locs);
    }

    // Index miss → rg fallback.
    let rg_locs = rg_resolve(index, uri, &ctx.word).await;
    locs_to_opt_response(rg_locs)
}
