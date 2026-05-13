//! Hover feature — rich Markdown hover computed from the index and live cursor context.
//!
//! Uses `WorkspaceRead` as the capability bound rather than the new capability traits because
//! the underlying resolution pipeline (`resolve_symbol_info`, `enrich_at_location`,
//! `build_subst_map`) depends on `IndexRead`, and `WorkspaceRead: IndexRead`.
//! Migrating these to the new traits is tracked as part of F5 cleanup.

use tower_lsp::lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Url};

use crate::backend::cursor::CursorContext;
use crate::backend::format::{format_contextual_hover, format_symbol_hover};
use crate::indexer::apply_type_subst;
use crate::indexer::resolution::{
    build_subst_map, enrich_at_location, resolve_symbol_info, ResolveOptions, SubstitutionContext,
    WorkspaceRead,
};
use crate::resolver::ReceiverType;

/// Compute a hover response for the cursor at `position` in `uri`.
///
/// Returns `None` when no useful hover information is available (unknown symbol,
/// cursor on a keyword, etc.).
pub(crate) fn compute_hover<W: WorkspaceRead>(
    workspace: &W,
    ctx: &CursorContext,
    uri: &Url,
    position: Position,
) -> Option<Hover> {
    if let Some(hover) = contextual_lambda_hover(workspace, ctx, uri, position) {
        return Some(hover);
    }
    if ctx.qualifier.is_none() && ctx.lambda_decl.is_some() {
        return None;
    }
    if let Some(hover) = contextual_receiver_hover(workspace, ctx, uri, position) {
        return Some(hover);
    }
    regular_symbol_hover(workspace, ctx, uri, position)
}

fn contextual_lambda_hover<W: WorkspaceRead>(
    workspace: &W,
    ctx: &CursorContext,
    uri: &Url,
    position: Position,
) -> Option<Hover> {
    if ctx.qualifier.is_some() {
        return None;
    }
    let receiver_type = ctx.contextual.as_ref()?;
    let type_name = contextual_hover_type_name(workspace, receiver_type, uri, position.line);
    let leaf = type_name.rsplit('.').next().unwrap_or(type_name.as_str());
    let signature = format!("{} {}: {type_name}", hover_binding_keyword(uri), ctx.word);
    let detail = resolve_hover_markdown(workspace, leaf, None, uri, position.line)
        .or_else(|| crate::stdlib::hover(leaf));
    Some(make_markdown_hover(format_contextual_hover(
        &signature,
        uri.path(),
        detail.as_deref(),
    )))
}

fn contextual_hover_type_name<W: WorkspaceRead>(
    workspace: &W,
    receiver_type: &ReceiverType,
    uri: &Url,
    line: u32,
) -> String {
    let subst = build_subst_map(workspace, uri.as_str(), line);
    if subst.is_empty() {
        return receiver_type.raw.clone();
    }
    apply_type_subst(&receiver_type.raw, &subst)
}

fn contextual_receiver_hover<W: WorkspaceRead>(
    workspace: &W,
    ctx: &CursorContext,
    uri: &Url,
    position: Position,
) -> Option<Hover> {
    let receiver_type = ctx.contextual.as_ref()?;
    ctx.qualifier.as_ref()?;
    let location = resolve_with_receiver_fallback(workspace, &ctx.word, receiver_type, uri)
        .into_iter()
        .next()?;
    let info = enrich_at_location(
        workspace,
        &location,
        &ctx.word,
        hover_substitution_context(uri, position.line),
        &ResolveOptions::hover(),
    )?;
    Some(make_markdown_hover(format_symbol_hover(&info, uri.path())))
}

fn regular_symbol_hover<W: WorkspaceRead>(
    workspace: &W,
    ctx: &CursorContext,
    uri: &Url,
    position: Position,
) -> Option<Hover> {
    let markdown = resolve_hover_markdown(
        workspace,
        &ctx.word,
        ctx.qualifier.as_deref(),
        uri,
        position.line,
    )
    .or_else(|| crate::stdlib::hover(&ctx.word))?;
    Some(make_markdown_hover(markdown))
}

fn resolve_hover_markdown<W: WorkspaceRead>(
    workspace: &W,
    word: &str,
    qualifier: Option<&str>,
    uri: &Url,
    line: u32,
) -> Option<String> {
    resolve_symbol_info(
        workspace,
        word,
        qualifier,
        uri,
        hover_substitution_context(uri, line),
        &ResolveOptions::hover(),
    )
    .map(|info| format_symbol_hover(&info, uri.path()))
}

/// Resolve a symbol name with receiver-type fallback.
///
/// Tries the fully-qualified receiver name first; on miss, falls back to the
/// leaf type name (e.g. `DashboardViewModel` instead of `com.example.DashboardViewModel`).
pub(crate) fn resolve_with_receiver_fallback<W: WorkspaceRead>(
    workspace: &W,
    word: &str,
    rt: &ReceiverType,
    uri: &Url,
) -> Vec<tower_lsp::lsp_types::Location> {
    let locs = workspace.find_definition_qualified(word, Some(&rt.qualified), uri);
    if locs.is_empty() && rt.leaf != rt.qualified {
        workspace.find_definition_qualified(word, Some(&rt.leaf), uri)
    } else {
        locs
    }
}

fn hover_binding_keyword(uri: &Url) -> &'static str {
    crate::Language::from_path(uri.path()).val_keyword()
}

fn hover_substitution_context(uri: &Url, line: u32) -> SubstitutionContext<'_> {
    SubstitutionContext::CrossFile {
        calling_uri: uri.as_str(),
        cursor_line: Some(line),
    }
}

fn make_markdown_hover(markdown: String) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: None,
    }
}
