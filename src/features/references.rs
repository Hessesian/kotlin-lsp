//! `find_references` feature — rg-backed reference search with scope narrowing.
//!
//! Entry point: [`find_references`]. The backend adapter calls this after resolving
//! the cursor word; the feature handles scope narrowing, rg search, library filtering,
//! and in-memory current-file hit injection.

use tower_lsp::lsp_types::{Location, Position, Range, Url};

use crate::features::traits::{DocumentAccess, ScopeQuery, SearchAccess, SymbolIndex};
use crate::rg::RgSearchRequest;
use crate::StrExt;

// ─── Public entry point ───────────────────────────────────────────────────────

pub(crate) async fn find_references(
    name: &str,
    uri: &Url,
    line: u32,
    include_decl: bool,
    index: &(impl SymbolIndex + DocumentAccess + ScopeQuery + SearchAccess + Send + Sync),
) -> Vec<Location> {
    let (parent_class, declared_pkg) = resolve_scope(index, uri, line, name);
    let decl_files = declaration_files_for(index, name, parent_class.as_deref());

    let search = ReferenceSearch {
        uri: uri.clone(),
        name: name.to_string(),
        include_decl,
        parent_class,
        declared_pkg,
        decl_files,
    };

    let mut locations = rg_locations(&search, index).await;
    locations.retain(|loc| !index.is_library_uri(&loc.uri));
    add_current_file_locations(index, uri, name, &mut locations);

    locations
}

// ─── Scope resolution ─────────────────────────────────────────────────────────

/// Determine `(parent_class, declared_pkg)` scope for a `findReferences` request.
///
/// Uppercase symbols are narrowed via import analysis or declaration-site lookup
/// so that rg can restrict to the specific class variant.
/// Lowercase symbols return `(None, None)` — bare-word search via rg.
fn resolve_scope(
    index: &(impl SymbolIndex + ScopeQuery),
    uri: &Url,
    line: u32,
    name: &str,
) -> (Option<String>, Option<String>) {
    if !name.starts_with_uppercase() {
        return (None, None);
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
    let (workspace_root, matcher) = index.rg_context();
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
        );
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
        append_line_reference_locations(uri, name, line_number, line, locations);
    }
}

fn has_reference_line(locations: &[Location], uri: &Url, line_number: u32) -> bool {
    locations
        .iter()
        .any(|loc| loc.uri == *uri && loc.range.start.line == line_number)
}

fn append_line_reference_locations(
    uri: &Url,
    name: &str,
    line_number: u32,
    line: &str,
    locations: &mut Vec<Location>,
) {
    for loc in line_reference_locations(uri, name, line_number, line) {
        if !has_reference_start(locations, &loc) {
            locations.push(loc);
        }
    }
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

fn utf16_column(text: &str) -> u32 {
    text.chars().map(|ch| ch.len_utf16() as u32).sum()
}

fn has_reference_start(locations: &[Location], candidate: &Location) -> bool {
    locations
        .iter()
        .any(|loc| loc.uri == candidate.uri && loc.range.start == candidate.range.start)
}

fn word_byte_offsets<'a>(line: &'a str, word: &'a str) -> impl Iterator<Item = usize> + 'a {
    let word_len = word.len();
    let is_id = |c: char| c.is_alphanumeric() || c == '_';
    let mut search_from = 0;
    std::iter::from_fn(move || {
        while let Some(rel) = line[search_from..].find(word) {
            let pos = search_from + rel;
            search_from = pos + word_len;
            let before_ok = pos == 0 || !is_id(line[..pos].chars().next_back()?);
            let after_ok =
                pos + word_len >= line.len() || !is_id(line[pos + word_len..].chars().next()?);
            if before_ok && after_ok {
                return Some(pos);
            }
        }
        None
    })
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
}
