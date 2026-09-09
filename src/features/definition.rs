//! `goto_definition` feature — pure lookup, no LSP adapter concerns.
//!
//! Entry point: [`find_definition`] takes an enriched cursor context and
//! capability traits; returns an optional response.  All rg fallback is
//! handled here so the backend adapter is a thin `Ok(find_definition(…).await)`.

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Position, Url};

use crate::backend::cursor::CursorContext;
use crate::features::traits::{DocumentAccess, SearchAccess, SymbolIndex};
use crate::indexer::{CallShape, Indexer};
use crate::parser::parse_by_extension;
use crate::rg;
use crate::types::CursorPos;

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

/// Throttle counter for the join-panic warning below — see
/// [`crate::util::throttled_warn`]. This is the last-resort fallback on the
/// goto-definition path: a panic here silently degrades "no definition
/// found" from an rg miss to an unwound task, which looks identical to the
/// caller.
static RG_RESOLVE_JOIN_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

async fn rg_resolve(index: &impl SearchAccess, uri: &Url, name: &str) -> Vec<Location> {
    let name_clone = name.to_string();
    let file_path = uri.to_file_path().ok();
    let (root_opt, source_roots, matcher) = index.rg_scope_for_path(file_path.as_deref());
    let join_result = tokio::task::spawn_blocking(move || {
        rg::rg_find_definition(
            &name_clone,
            root_opt.as_deref(),
            &source_roots,
            matcher.as_deref(),
        )
    })
    .await;
    if let Err(ref e) = join_result {
        crate::util::throttled_warn(&RG_RESOLVE_JOIN_FAILURES, 5, || {
            crate::util::join_failure_message(
                &format!(
                    "running the rg goto-definition fallback for `{name}` from {}",
                    uri.path()
                ),
                e,
            )
        });
    }
    join_result.unwrap_or_default()
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

// ─── CST-resolved path ────────────────────────────────────────────────────────

/// Classify the identifier under `position` via the CST and, when the CST
/// gave enough information to trust the result (a declaration, or a
/// receiver-typed member reference), resolve it to its definition site(s).
///
/// Returns `None` — never an error — whenever the CST can't classify the
/// cursor position, or classification succeeds but only reaches `NameScan`
/// confidence; both cases mean "fall through to the string-first path below."
fn try_cst_resolved_definition(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    let sym = crate::indexer::classify_cursor(indexer, uri, position)?;
    match crate::indexer::resolve_identity(&sym, indexer, uri) {
        crate::indexer::NavigationSource::CstResolved(defs) if !defs.is_empty() => {
            locs_to_opt_response(defs.0)
        }
        _ => None,
    }
}

/// The call shape of the call whose callee sits under `position`, or `None`
/// when the cursor isn't precisely on a call's callee identifier (e.g. it's on
/// an argument, or the position doesn't classify at all).
///
/// Handles both a bare callee (`foo(...)`, where the identifier itself is the
/// direct callee child of the `call_expression`) and a dot-qualified callee
/// (`x.foo(...)`, where the callee is the whole `navigation_expression` —
/// `foo`'s own parent is a `nav_suffix`, not the call — via
/// `enclosing_nav_expr_if_member`, the same walk `classify_symbol_at` uses).
///
/// `pub(crate)`, not `definition.rs`-private: hover's `regular_symbol_hover`
/// reuses this directly rather than recomputing the same CST-shape lookup —
/// both features hit the identical "cursor on a call's callee" question.
pub(crate) fn call_shape_at_callee(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<CallShape> {
    let doc = indexer.live_doc_or_parse(uri)?;
    let cursor = CursorPos {
        line: position.line as usize,
        utf16_col: position.character as usize,
    };
    let node = crate::indexer::cursor_node_at(&doc, cursor)?;
    let callee_node = crate::indexer::enclosing_nav_expr_if_member(node).unwrap_or(node);
    if !crate::indexer::is_call_callee(callee_node) {
        return None;
    }
    let call_expr = callee_node.parent()?;
    Some(crate::indexer::call_shape_of(call_expr, &doc.bytes))
}

/// The base receiver type of the extension function/property whose body
/// encloses `position` — see [`crate::parser::enclosing_extension_receiver_at`].
/// `None` when `position` isn't inside an extension function/property, or the
/// file can't be parsed.
///
/// `pub(crate)`, not `definition.rs`-private: hover's `call_callee_hover`
/// reuses this directly rather than recomputing the same CST-position lookup
/// — same reason `call_shape_at_callee` is shared.
pub(crate) fn enclosing_extension_receiver_at(
    indexer: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<String> {
    let doc = indexer.live_doc_or_parse(uri)?;
    let range = tower_lsp::lsp_types::Range {
        start: position,
        end: position,
    };
    crate::parser::enclosing_extension_receiver_at(doc.tree.root_node(), &doc.bytes, range)
}

// ─── Main entry point ─────────────────────────────────────────────────────────

/// Resolve goto-definition for the given cursor context.
///
/// Handles `this`, `super`, `super.method`, contextual lambda receivers,
/// direct qualified lookups, and rg fallback — in that priority order.
pub(crate) async fn find_definition(
    ctx: &CursorContext,
    index: &Indexer,
    uri: &Url,
    position: Position,
) -> Option<GotoDefinitionResponse> {
    // CST-resolved path first: precise for declarations and receiver-typed
    // member references. Falls through to the string-first path below for
    // everything the CST can't narrow (locals, untyped receivers, keywords).
    if let Some(cst_response) = try_cst_resolved_definition(index, uri, position) {
        return Some(cst_response);
    }

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
    //
    // `ctx.contextual` also covers plain qualified calls like `triggers.collect
    // { trigger -> }` (see `CursorContext::contextual`'s doc), so this needs
    // the same self-shadow arity filter as the CST-resolved path above.
    // Filtering to empty falls through rather than returning the wrong
    // candidate — the string-qualifier fallback further down never consults
    // the extension-in-scope registry that causes the wrong match.
    if ctx.qualifier.is_some() {
        if let Some(ref rt) = ctx.contextual {
            let mut locs = index.find_definition_qualified(&ctx.word, Some(&rt.qualified), uri);
            if locs.is_empty() && rt.leaf != rt.qualified {
                locs = index.find_definition_qualified(&ctx.word, Some(&rt.leaf), uri);
            }
            if let Some(shape) = call_shape_at_callee(index, uri, position) {
                locs = crate::indexer::shape_filter_locations(index, shape, locs).resolved();
            }
            if !locs.is_empty() {
                return Some(locs_to_response(locs));
            }
        }
    }

    // General qualified or bare lookup. An unqualified call's callee gets a
    // shape-aware lookup instead of the plain name-based one — so a same-file
    // declaration whose arity can't satisfy the call doesn't shadow the real
    // (often library) target. Once the CST has confirmed this position is a
    // call's callee, an empty shape-aware result must NOT fall through to the
    // unfiltered lookup below, NOR to the plain `rg_resolve` fallback further
    // down — `resolve_callee_definition`'s own step 5 already ran the same
    // `rg` search with shape filtering; a second, unfiltered `rg` search here
    // would just re-find the same wrong-arity match by blind text match and
    // undo the whole point of computing the shape. Empty stays empty.
    if ctx.qualifier.is_none() {
        if let Some(shape) = call_shape_at_callee(index, uri, position) {
            let locs = index.find_definition_for_call(&ctx.word, uri, shape);
            if !locs.is_empty() {
                return locs_to_opt_response(locs);
            }
            // Bare-name search (imports/same-package/star/hierarchy/rg) has no
            // receiver-type awareness at all, so a call inside an extension
            // function's own body that targets a same-named member/extension
            // of that function's *own* receiver (an implicit `this.name(...)`)
            // is invisible to it — try that specifically before giving up.
            if let Some(receiver) = enclosing_extension_receiver_at(index, uri, position) {
                let locs = index
                    .find_definition_for_implicit_receiver_call(&receiver, &ctx.word, uri, shape);
                return locs_to_opt_response(locs);
            }
            return None;
        }
    }
    let mut locs = index.find_definition_qualified(&ctx.word, ctx.qualifier.as_deref(), uri);
    // `resolve_qualified` can now return a wrong-arity member alongside its
    // correct-arity extension fallback (`with_supertype_extension_fallback`)
    // — shape-filter here too, same as the branches above.
    if let Some(shape) = call_shape_at_callee(index, uri, position) {
        locs = crate::indexer::shape_filter_locations(index, shape, locs).resolved();
    }
    if !locs.is_empty() {
        return locs_to_opt_response(locs);
    }

    // Index miss → rg fallback.
    let rg_locs = rg_resolve(index, uri, &ctx.word).await;
    locs_to_opt_response(rg_locs)
}

#[cfg(test)]
#[path = "definition_tests.rs"]
mod tests;
