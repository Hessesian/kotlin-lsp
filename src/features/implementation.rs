//! `goto_implementation` feature — find all implementors/subtypes of a symbol.
//!
//! Entry point: [`find_implementation`] dispatches to two paths:
//!
//! - **Type path** (`IService`, `BaseClass`): BFS the subtype graph to find all
//!   implementing/extending classes, with an rg cold-start fallback.
//! - **Method path** (`fun load()` inside an interface): resolve the declaring
//!   class, BFS its subtypes, then locate `override fun load` within each
//!   implementor's FileData.  Falls back to rg when the index is cold.

use std::collections::HashSet;
use std::sync::Arc;

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, SymbolKind, Url};

use crate::features::definition::locs_to_opt_response;
use crate::features::traits::{DocumentAccess, SearchAccess, SymbolIndex};
use crate::rg;
use crate::types::FileData;

/// Find all implementations/subtypes of the symbol under the cursor at `uri`.
///
/// - If `word` names a method/function declared inside a class or interface,
///   returns the `override fun` locations across all implementors.
/// - Otherwise, returns the class/struct locations that implement the named type.
pub(crate) async fn find_implementation(
    word: &str,
    index: &(impl SymbolIndex + DocumentAccess + SearchAccess),
    uri: &Url,
    line: u32,
) -> Option<GotoDefinitionResponse> {
    if let Some(declaring_class) = declaring_class_of_method(index, uri, word, line) {
        find_method_implementations(word, &declaring_class, index, uri).await
    } else {
        find_type_implementations(word, index, uri).await
    }
}

// ─── Type-level implementation ────────────────────────────────────────────────

/// BFS over the subtype graph to find all classes that implement/extend `type_name`.
/// rg fallback when the index is cold (no subtypes indexed yet).
async fn find_type_implementations(
    type_name: &str,
    index: &(impl SymbolIndex + DocumentAccess + SearchAccess),
    uri: &Url,
) -> Option<GotoDefinitionResponse> {
    let mut locs: Vec<Location> = index.subtypes_of(type_name);

    if locs.is_empty() {
        let file_path = uri.to_file_path().ok();
        let (root_opt, source_roots, matcher) = index.rg_scope_for_path(file_path.as_deref());
        let word_clone = type_name.to_string();
        let rg_impls = tokio::task::spawn_blocking(move || {
            rg::rg_find_implementors(
                &word_clone,
                root_opt.as_deref(),
                &source_roots,
                matcher.as_deref(),
            )
        })
        .await
        .unwrap_or_default();
        if !rg_impls.is_empty() {
            return locs_to_opt_response(rg_impls);
        }
    }

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
    let mut visited: HashSet<String> = HashSet::from([type_name.to_string()]);
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

// ─── Method-level implementation ──────────────────────────────────────────────

/// If `word` is a function/method declared inside a class or interface at `uri`,
/// returns the name of the declaring class.  Returns `None` for top-level
/// functions, constructors, and non-function symbols.
fn declaring_class_of_method(
    index: &impl SymbolIndex,
    uri: &Url,
    word: &str,
    line: u32,
) -> Option<String> {
    index
        .file_symbols(uri)
        .into_iter()
        .find(|s| {
            s.name == word
                && matches!(s.kind, SymbolKind::FUNCTION | SymbolKind::METHOD)
                && s.container.is_some()
                && s.selection_range.start.line == line
        })
        .and_then(|s| s.container)
}

/// Collect `override fun method_name` (Kotlin) or same-named method (Java)
/// locations within `data` for a confirmed subtype.
///
/// For Kotlin, `detail` must contain `"override"` to exclude same-named
/// non-overriding overloads. For Java, `@Override` lives on the preceding line
/// and is not in the indexed detail, so all same-named methods in a confirmed
/// subtype are included.
fn method_overrides_in(method_name: &str, data: &Arc<FileData>, uri: &Url) -> Vec<Location> {
    let lang = crate::Language::from_path(uri.path());
    data.symbols
        .iter()
        .filter(|s| {
            s.name == method_name
                && matches!(s.kind, SymbolKind::FUNCTION | SymbolKind::METHOD)
                && lang.detail_is_override(&s.detail)
        })
        .map(|s| Location {
            uri: uri.clone(),
            range: s.selection_range,
        })
        .collect()
}

/// BFS the subtype graph of `declaring_class`, collecting `override fun method_name`
/// locations from each implementor's FileData.
/// rg fallback when the index is cold.
async fn find_method_implementations(
    method_name: &str,
    declaring_class: &str,
    index: &(impl SymbolIndex + DocumentAccess + SearchAccess),
    uri: &Url,
) -> Option<GotoDefinitionResponse> {
    let mut override_locs: Vec<Location> = Vec::new();
    let mut queue: Vec<String> = vec![declaring_class.to_string()];
    let mut visited: HashSet<String> = HashSet::from([declaring_class.to_string()]);

    while let Some(type_name) = queue.pop() {
        for class_loc in index.subtypes_of(&type_name) {
            let Some(data) = index.file_data_for(class_loc.uri.as_str()) else {
                continue;
            };

            // Enqueue this subtype's name for transitive BFS.
            if let Some(class_sym) = data
                .symbols
                .iter()
                .find(|s| s.selection_range == class_loc.range)
            {
                let class_name = class_sym.name.clone();
                if !visited.contains(&class_name) {
                    visited.insert(class_name.clone());
                    queue.push(class_name);
                }
            }

            // Collect all override sites for the method in this file.
            for loc in method_overrides_in(method_name, &data, &class_loc.uri) {
                if !override_locs
                    .iter()
                    .any(|l| l.uri == loc.uri && l.range == loc.range)
                {
                    override_locs.push(loc);
                }
            }
        }
    }

    // rg fallback when index is cold (implementors not yet indexed).
    if override_locs.is_empty() {
        let file_path = uri.to_file_path().ok();
        let (root_opt, source_roots, matcher) = index.rg_scope_for_path(file_path.as_deref());
        let method = method_name.to_string();
        let class = declaring_class.to_string();
        let rg_locs = tokio::task::spawn_blocking(move || {
            rg::rg_find_method_overrides(
                &method,
                &class,
                root_opt.as_deref(),
                &source_roots,
                matcher.as_deref(),
            )
        })
        .await
        .unwrap_or_default();
        return locs_to_opt_response(rg_locs);
    }

    locs_to_opt_response(override_locs)
}

#[cfg(test)]
#[path = "implementation_tests.rs"]
mod tests;
