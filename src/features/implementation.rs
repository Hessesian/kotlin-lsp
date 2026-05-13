//! `goto_implementation` feature — find all implementors/subtypes of a symbol.
//!
//! Entry point: [`find_implementation`] does an index-first BFS over the
//! subtype graph then falls back to rg when the index is cold.

use std::collections::HashSet;

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Url};

use crate::features::definition::locs_to_opt_response;
use crate::features::traits::{DocumentAccess, SearchAccess, SymbolIndex};
use crate::rg;

/// Find all implementations/subtypes of the symbol under the cursor at `uri`.
///
/// Index-first: BFS over the subtype graph (direct + transitive, depth-limited).
/// rg fallback: when the index is cold (no direct subtypes found).
pub(crate) async fn find_implementation(
    word: &str,
    index: &(impl SymbolIndex + DocumentAccess + SearchAccess),
    uri: &Url,
) -> Option<GotoDefinitionResponse> {
    // Direct subtypes from the index.
    let mut locs: Vec<Location> = index.subtypes_of(word);

    // Cold-start: rg heuristic to find implementors before index is warm.
    if locs.is_empty() {
        let file_path = uri.to_file_path().ok();
        let (workspace_root, matcher) = index.rg_context();
        let root_opt = rg::effective_rg_root(workspace_root.as_deref(), file_path.as_deref());
        let word_clone = word.to_string();
        let rg_impls = tokio::task::spawn_blocking(move || {
            rg::rg_find_implementors(&word_clone, root_opt.as_deref(), matcher.as_deref())
        })
        .await
        .unwrap_or_default();
        if !rg_impls.is_empty() {
            return locs_to_opt_response(rg_impls);
        }
    }

    // BFS over transitive subtypes (depth-limited by visited set).
    let mut queue: Vec<String> = locs
        .iter()
        .filter_map(|loc| {
            let data = index.file_data_for(loc.uri.as_str())?;
            data.symbols
                .iter()
                .find(|s| s.selection_range == loc.range)
                .map(|s| s.name.clone())
        })
        .collect();
    let mut visited: HashSet<String> = HashSet::from([word.to_string()]);
    while let Some(name) = queue.pop() {
        if visited.contains(&name) {
            continue;
        }
        visited.insert(name.clone());
        for loc in index.subtypes_of(&name) {
            if locs
                .iter()
                .any(|l| l.uri == loc.uri && l.range == loc.range)
            {
                continue;
            }
            if let Some(data) = index.file_data_for(loc.uri.as_str()) {
                if let Some(sym) = data.symbols.iter().find(|s| s.selection_range == loc.range) {
                    queue.push(sym.name.clone());
                }
            }
            locs.push(loc);
        }
    }

    locs_to_opt_response(locs)
}
