//! `find_references` feature — rg-backed reference search with scope narrowing.
//!
//! Entry point: [`find_references`]. The backend adapter calls this after resolving
//! the cursor word; the feature handles scope narrowing, rg search, library filtering,
//! and in-memory current-file hit injection.

use tower_lsp::lsp_types::{Location, Position, Range, Url};

use super::text_utils::{utf16_column, word_byte_offsets};
use crate::features::traits::{DocumentAccess, ScopeQuery, SearchAccess, SymbolIndex};
use crate::rg::RgSearchRequest;
use crate::StrExt;

// ─── Public entry point ───────────────────────────────────────────────────────

/// Finds all references to `name`, optionally scoped by `qualifier`.
///
/// When `qualifier` is `Some("ReducerA")` (cursor was on `ReducerA.Factory`),
/// the qualifier is used directly as the `parent_class` scope, bypassing the
/// fallible index-lookup that `resolve_scope` uses for unresolved nested types.
/// This prevents false positives when multiple classes define an inner class
/// with the same name (e.g. every class defines a `Factory` or `Builder`).
///
/// When `qualifier` is `None`, scope is inferred from imports and the index.
/// For lowercase methods at their declaration site that are declared inside a
/// doubly-nested class (e.g. `create` inside `Factory` inside `RegularReducer`),
/// the outer class is used for file discovery so callers that reference the outer
/// class via a variable name (`factory.create()`) are found while sibling
/// factories in the same package are excluded.
pub(crate) async fn find_references_with_qualifier(
    name: &str,
    qualifier: Option<&str>,
    uri: &Url,
    line: u32,
    include_decl: bool,
    index: &(impl SymbolIndex + DocumentAccess + ScopeQuery + SearchAccess + Send + Sync),
) -> Vec<Location> {
    let (parent_class, declared_pkg) =
        resolve_scope_with_qualifier(index, uri, line, name, qualifier);

    // For lowercase methods at their declaration site, check if they are declared
    // inside a doubly-nested class (e.g. `create` inside `Factory` inside `Reducer`).
    // `declared_pkg.is_some()` is the on_decl proxy for lowercase names: resolve_scope
    // returns (None, Some(pkg)) on_decl and (None, None) off-decl for lowercase names.
    let owner_class =
        if !name.starts_with_uppercase() && declared_pkg.is_some() && qualifier.is_none() {
            outer_class_for_decl_site(index, uri, line)
        } else {
            None
        };

    let decl_files = declaration_files_for(index, name, parent_class.as_deref());

    let search = ReferenceSearch {
        uri: uri.clone(),
        name: name.to_string(),
        include_decl,
        parent_class,
        declared_pkg,
        decl_files,
        owner_class,
    };

    let mut locations = rg_locations(&search, index).await;
    locations.retain(|loc| !index.is_library_uri(&loc.uri));
    add_current_file_locations(
        index,
        uri,
        name,
        search.parent_class.as_deref(),
        &mut locations,
    );

    locations
}

// ─── Scope resolution ─────────────────────────────────────────────────────────

/// Determine `(parent_class, declared_pkg)` scope for a `findReferences` request.
///
/// Uppercase symbols are narrowed via import analysis or declaration-site lookup
/// so that rg can restrict to the specific class variant.
/// Lowercase symbols at the **declaration site** return `(None, Some(package))` —
/// rg is scoped to same-package files.  Off-declaration-site lowercase names
/// return `(None, None)` — codebase-wide bare-word search via rg.
pub(crate) fn resolve_scope(
    index: &(impl SymbolIndex + ScopeQuery),
    uri: &Url,
    line: u32,
    name: &str,
) -> (Option<String>, Option<String>) {
    resolve_scope_with_qualifier(index, uri, line, name, None)
}

/// Like [`resolve_scope`] but accepts a dot-qualifier (the segment immediately
/// preceding `name` at the cursor, e.g. `"ReducerA"` for `ReducerA.Factory`).
///
/// An uppercase qualifier is used directly as the `parent_class`, which avoids
/// the index-lookup fallback that picks an arbitrary definition when multiple
/// classes define an inner class with the same name.
pub(crate) fn resolve_scope_with_qualifier(
    index: &(impl SymbolIndex + ScopeQuery),
    uri: &Url,
    line: u32,
    name: &str,
    qualifier: Option<&str>,
) -> (Option<String>, Option<String>) {
    // Lowercase names: only scope if we're on the declaration — restrict to the
    // declaring file's package so the bare-word search doesn't scan the whole codebase.
    if !name.starts_with_uppercase() {
        let on_decl = index.is_declared_in(uri, name)
            && index
                .definition_locations(name)
                .iter()
                .any(|l| l.uri == *uri && l.range.start.line == line);
        return if on_decl {
            (None, index.package_of(uri))
        } else {
            (None, None)
        };
    }

    // Fast path: an uppercase dot-qualifier (e.g. "ReducerA" in "ReducerA.Factory")
    // unambiguously identifies the parent class — use it directly rather than
    // guessing from the index (which is non-deterministic when multiple classes
    // share the same inner-class name).
    //
    // `word_and_qualifier_at` returns the full dot-chain (e.g. "Outer.Inner" for
    // "Outer.Inner.Factory").  We preserve the full chain as `parent_class` so
    // `has_wrong_qualifier_at_col` can match it against the full extracted chain on each
    // hit, rather than just the immediate token.
    if let Some(q) = qualifier.filter(|q| q.starts_with_uppercase()) {
        let parent_pkg = index
            .declared_package_of(q)
            .map(|p| format!("{p}.{q}"))
            .or_else(|| index.declared_package_of(name));
        return (Some(q.to_string()), parent_pkg);
    }

    let on_decl = index.is_declared_in(uri, name)
        && index
            .definition_locations(name)
            .iter()
            .any(|l| l.uri == *uri && l.range.start.line == line);
    if on_decl {
        let parent = index.enclosing_class_at(uri, line);
        let pkg = index.package_of(uri);
        return (parent, pkg);
    }
    let (parent, pkg) = index.resolve_symbol_via_import(uri, name);
    if parent.is_some() || pkg.is_some() {
        return (parent, pkg);
    }
    let parent = index.declared_parent_class_of(name, uri);
    let pkg = index.declared_package_of(name);
    (parent, pkg)
}

fn declaration_files_for(
    index: &(impl SymbolIndex + ScopeQuery),
    name: &str,
    parent_class: Option<&str>,
) -> Vec<String> {
    index
        .definition_locations(name)
        .into_iter()
        .filter(|loc| reference_matches_parent_class(index, loc, parent_class))
        .filter_map(|loc| loc.uri.to_file_path().ok())
        .filter_map(|path| path.to_str().map(|s| s.to_owned()))
        .collect()
}

/// For a lowercase method at its declaration site, returns the outer-outer class
/// if the method is inside a doubly-nested class
/// (e.g. `create` inside `Factory` inside `RegularReducer` → `"RegularReducer"`).
///
/// Returns `None` if the method has only one level of nesting or none.
/// Uses `enclosing_class_at` (line-specific CST walk) for the direct parent, then
/// `declared_parent_class_of` (preferred-URI-first index lookup) for the outer parent.
fn outer_class_for_decl_site(
    index: &(impl SymbolIndex + ScopeQuery),
    uri: &Url,
    line: u32,
) -> Option<String> {
    let direct_parent = index.enclosing_class_at(uri, line)?;
    index.declared_parent_class_of(&direct_parent, uri)
}

fn reference_matches_parent_class(
    index: &impl SymbolIndex,
    location: &Location,
    parent_class: Option<&str>,
) -> bool {
    let Some(parent_class) = parent_class else {
        return true;
    };
    index
        .enclosing_class_at(&location.uri, location.range.start.line)
        .as_deref()
        == Some(parent_class)
}

// ─── rg search ────────────────────────────────────────────────────────────────

async fn rg_locations(
    search: &ReferenceSearch,
    index: &(impl SearchAccess + Send + Sync),
) -> Vec<Location> {
    let file_path = search.uri.to_file_path().ok();
    let (workspace_root, source_roots, matcher) = index.rg_scope_for_path(file_path.as_deref());
    let request = search.clone();
    tokio::task::spawn_blocking(move || {
        let rg_req = RgSearchRequest::new(
            &request.name,
            request.parent_class.as_deref(),
            request.declared_pkg.as_deref(),
            workspace_root.as_deref(),
            request.include_decl,
            &request.uri,
            &request.decl_files,
        )
        .with_source_paths(&source_roots);
        let rg_req = match request.owner_class.as_deref() {
            Some(owner) => rg_req.with_owner_class(owner),
            None => rg_req,
        };
        crate::rg::rg_find_references(&rg_req, matcher.as_deref())
    })
    .await
    .unwrap_or_default()
}

// ─── In-memory current-file injection ─────────────────────────────────────────

fn add_current_file_locations(
    index: &impl DocumentAccess,
    uri: &Url,
    name: &str,
    parent_class: Option<&str>,
    locations: &mut Vec<Location>,
) {
    let Some(lines) = index.mem_lines_for(uri.as_str()) else {
        return;
    };
    for (line_idx, line) in lines.iter().enumerate() {
        let line_number = line_idx as u32;
        if has_reference_line(locations, uri, line_number) {
            continue;
        }
        // Check qualifier per-occurrence so that a line containing both a valid
        // and an invalid qualified reference (e.g. `ReducerA.Factory, ReducerC.Factory`)
        // keeps the valid hit instead of dropping the whole line.
        for loc in line_reference_locations(uri, name, line_number, line) {
            if has_reference_start(locations, &loc) {
                continue;
            }
            if let Some(parent) = parent_class {
                if crate::rg::has_wrong_qualifier_at_col(
                    line,
                    name,
                    parent,
                    loc.range.start.character,
                ) {
                    continue;
                }
            }
            locations.push(loc);
        }
    }
}

fn has_reference_line(locations: &[Location], uri: &Url, line_number: u32) -> bool {
    locations
        .iter()
        .any(|loc| loc.uri == *uri && loc.range.start.line == line_number)
}

fn line_reference_locations(uri: &Url, name: &str, line_number: u32, line: &str) -> Vec<Location> {
    word_byte_offsets(line, name)
        .map(|offset| reference_location(uri, name, line_number, line, offset))
        .collect()
}

fn reference_location(
    uri: &Url,
    name: &str,
    line_number: u32,
    line: &str,
    offset: usize,
) -> Location {
    let start = utf16_column(&line[..offset]);
    let end = start + utf16_column(name);
    Location {
        uri: uri.clone(),
        range: Range::new(
            Position::new(line_number, start),
            Position::new(line_number, end),
        ),
    }
}

fn has_reference_start(locations: &[Location], candidate: &Location) -> bool {
    locations
        .iter()
        .any(|loc| loc.uri == candidate.uri && loc.range.start == candidate.range.start)
}

// ─── Internal transfer type ────────────────────────────────────────────────────

#[derive(Clone)]
struct ReferenceSearch {
    uri: Url,
    name: String,
    include_decl: bool,
    parent_class: Option<String>,
    declared_pkg: Option<String>,
    decl_files: Vec<String>,
    /// Outer-outer class for owner-scoped file discovery; see [`outer_class_for_decl_site`].
    owner_class: Option<String>,
}

#[cfg(test)]
#[path = "references_tests.rs"]
mod tests;
