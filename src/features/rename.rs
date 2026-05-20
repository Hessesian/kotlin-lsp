//! `rename` feature — shared rename logic behind thin backend adapters.
//!
//! Entry points: [`prepare_rename_impl`] and [`rename_impl`]. The backend
//! adapter only unwraps LSP params; this module handles scope resolution,
//! local-only renames, rg-backed workspace discovery, and edit construction.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::Arc;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

use crate::features::references::resolve_scope;
use crate::features::text_utils::is_keyword_for_file;
#[cfg(test)]
use crate::indexer::live_tree::utf16_col_to_byte;
use crate::indexer::{cst_cursor_is_local_var, Indexer};
#[cfg(test)]
use crate::queries::{
    KIND_ANON_FUN, KIND_CLASS_BODY, KIND_COMPANION_OBJ, KIND_FUN_DECL, KIND_METHOD_DECL,
    KIND_MULTI_VAR_DECL, KIND_NAV_EXPR, KIND_OBJECT_DECL, KIND_PROP_DECL, KIND_SOURCE_FILE,
    KIND_VAR_DECL,
};
use crate::StrExt;

/// Return `true` when the file index (within `scope`) contains a `val`/`var`
/// declaration of `name` that is itself a local variable.
///
/// Complements `cst_cursor_is_local_var` for the *reference* case: when the
/// cursor is on a reference the CST parent chain never contains a
/// `property_declaration`, so `cst_cursor_is_local_var(cursor_pos)` returns
/// false. This function recovers the declaration from the index and applies the
/// same CST check on its `selection_range`, so both declaration and reference
/// sites produce the correct `skip_dotted` result.
///
/// Class-level properties are naturally excluded: they are declared outside the
/// function's `scope` window and therefore never matched by the line filter.
pub(crate) fn any_local_var_decl_in_scope(
    indexer: &Indexer,
    uri: &Url,
    name: &str,
    scope: (usize, usize),
) -> bool {
    use tower_lsp::lsp_types::SymbolKind;

    let Some(data) = indexer.file_data_for(uri.as_str()) else {
        return false;
    };
    data.symbols
        .iter()
        .filter(|symbol| {
            matches!(
                symbol.kind,
                SymbolKind::PROPERTY | SymbolKind::VARIABLE | SymbolKind::CONSTANT
            ) && symbol.name == name
                && (symbol.selection_range.start.line as usize) >= scope.0
                && (symbol.selection_range.start.line as usize) <= scope.1
        })
        .any(|symbol| {
            let declaration_position = Position {
                line: symbol.selection_range.start.line,
                character: symbol.selection_range.start.character,
            };
            cst_cursor_is_local_var(indexer, uri, declaration_position)
        })
}

#[cfg(test)]
/// declaration or a navigation expression (member access). Returns `false`
/// for property/variable declarations, parameters, and unknown contexts.
///
/// Used as a secondary check to detect method call sites (nav expressions)
/// that are not in the file's own symbol index.
pub(crate) fn cst_cursor_is_method(indexer: &Indexer, uri: &Url, pos: Position) -> bool {
    use tree_sitter::Point;

    let doc = match indexer.live_doc(uri) {
        Some(doc) => doc,
        None => return false,
    };
    let full_text = match std::str::from_utf8(&doc.bytes) {
        Ok(text) => text,
        Err(_) => return false,
    };
    let line_index = pos.line as usize;
    let line_text = match full_text.lines().nth(line_index) {
        Some(line) => line,
        None => return false,
    };
    let byte_column = utf16_col_to_byte(line_text, pos.character as usize);
    let point = Point {
        row: line_index,
        column: byte_column,
    };
    let start_node = match doc
        .tree
        .root_node()
        .descendant_for_point_range(point, point)
    {
        Some(node) => node,
        None => return false,
    };

    let mut current_node = start_node;
    loop {
        match current_node.kind() {
            KIND_PROP_DECL | KIND_VAR_DECL | KIND_MULTI_VAR_DECL => return false,
            KIND_FUN_DECL | KIND_METHOD_DECL | KIND_ANON_FUN | KIND_NAV_EXPR => return true,
            KIND_SOURCE_FILE | KIND_CLASS_BODY | KIND_OBJECT_DECL | KIND_COMPANION_OBJ => {
                return false;
            }
            _ => {}
        }
        match current_node.parent() {
            Some(parent_node) => current_node = parent_node,
            None => return false,
        }
    }
}

/// Find the line range of the innermost function/lambda scope enclosing `cursor_line`.
/// Returns `(start_line, end_line)` inclusive, or the whole file if not found.
pub(crate) fn enclosing_scope(lines: &[String], cursor_line: usize) -> (usize, usize) {
    let mut depth = 0i32;
    let mut scope_start = 0usize;
    'outer: for line_index in (0..=cursor_line.min(lines.len().saturating_sub(1))).rev() {
        for character in lines[line_index].chars().rev() {
            match character {
                '}' => depth += 1,
                '{' => {
                    if depth == 0 {
                        scope_start = line_index;
                        break 'outer;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
    }

    let mut depth = 0i32;
    let mut scope_end = lines.len().saturating_sub(1);
    for (line_index, line) in lines.iter().enumerate().skip(scope_start) {
        for character in line.chars() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        scope_end = line_index;
                        return (scope_start, scope_end);
                    }
                }
                _ => {}
            }
        }
    }
    (scope_start, scope_end)
}

/// Return TextEdits replacing all whole-word occurrences of `word` with `new_name`
/// within `lines[scope.0..=scope.1]`, in reverse order (safe for sequential apply).
///
/// When `skip_dotted` is `true`, occurrences immediately preceded by `.` are
/// skipped. This avoids renaming same-named method calls when the user is
/// renaming a local variable (e.g. `val syncWith` vs `.syncWith()`).
pub(crate) fn rename_in_scope(
    lines: &[String],
    word: &str,
    new_name: &str,
    scope: (usize, usize),
    skip_dotted: bool,
) -> Vec<TextEdit> {
    let word_characters: Vec<char> = word.chars().collect();
    let word_length = word_characters.len();
    if word_length == 0 {
        return vec![];
    }
    let mut edits: Vec<TextEdit> = Vec::new();

    let end_line = scope.1.min(lines.len().saturating_sub(1));
    for (line_number, line) in lines.iter().enumerate().take(end_line + 1).skip(scope.0) {
        if line.trim_start().starts_with("package ") {
            continue;
        }
        let line_characters: Vec<char> = line.chars().collect();
        let character_to_utf16: Vec<u32> = {
            let mut utf16_offsets = Vec::with_capacity(line_characters.len() + 1);
            let mut utf16_offset = 0u32;
            for &character in &line_characters {
                utf16_offsets.push(utf16_offset);
                utf16_offset += character.len_utf16() as u32;
            }
            utf16_offsets.push(utf16_offset);
            utf16_offsets
        };

        let mut character_index = 0usize;
        while character_index < line_characters.len() {
            if line_characters[character_index..].starts_with(&word_characters) {
                let word_start_ok = character_index == 0
                    || !(line_characters[character_index - 1].is_alphanumeric()
                        || line_characters[character_index - 1] == '_');
                let word_end_index = character_index + word_length;
                let word_end_ok = word_end_index >= line_characters.len()
                    || !(line_characters[word_end_index].is_alphanumeric()
                        || line_characters[word_end_index] == '_');
                if word_start_ok && word_end_ok {
                    if skip_dotted
                        && character_index > 0
                        && line_characters[character_index - 1] == '.'
                    {
                        character_index = word_end_index;
                        continue;
                    }
                    edits.push(TextEdit {
                        range: Range {
                            start: Position::new(
                                line_number as u32,
                                character_to_utf16[character_index],
                            ),
                            end: Position::new(
                                line_number as u32,
                                character_to_utf16[word_end_index],
                            ),
                        },
                        new_text: new_name.to_owned(),
                    });
                    character_index = word_end_index;
                    continue;
                }
            }
            character_index += 1;
        }
    }

    edits.sort_by_key(|edit| Reverse(edit.range.start));
    edits
}

struct RenameCursorSymbol {
    name: String,
    parent_class: Option<String>,
    declared_package: Option<String>,
    scope_limited_to_current_file: bool,
}

fn resolve_cursor_symbol(
    indexer: &Indexer,
    uri: &Url,
    pos: Position,
) -> Option<RenameCursorSymbol> {
    let name = indexer.word_at(uri, pos)?;
    let file_path = uri.path();
    if is_keyword_for_file(&name, file_path) {
        return None;
    }
    let (parent_class, declared_package, scope_limited_to_current_file) =
        if name.starts_with_uppercase() {
            let (parent_class, declared_package) = resolve_scope(indexer, uri, pos.line, &name);
            (parent_class, declared_package, false)
        } else if cst_cursor_is_local_var(indexer, uri, pos) {
            // Local variable inside a function/lambda — scope rename to the enclosing block.
            (None, None, true)
        } else {
            // Class-level property or top-level declaration — rename across files.
            (None, None, false)
        };

    Some(RenameCursorSymbol {
        name,
        parent_class,
        declared_package,
        scope_limited_to_current_file,
    })
}

fn rename_local_symbol(
    indexer: &Indexer,
    uri: &Url,
    pos: Position,
    name: &str,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    let lines = indexer.lines_for(uri)?;
    let scope = enclosing_scope(&lines, pos.line as usize);
    let skip_dotted = cst_cursor_is_local_var(indexer, uri, pos)
        || any_local_var_decl_in_scope(indexer, uri, name, scope);
    let mut file_edits = rename_in_scope(&lines, name, new_name, scope, skip_dotted);
    if file_edits.is_empty() {
        return None;
    }
    file_edits.sort_by_key(|edit| Reverse(edit.range.start));

    let mut changes = HashMap::new();
    changes.insert(uri.clone(), file_edits);
    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

fn definition_files_for_rename(
    indexer: &Indexer,
    name: &str,
    parent_class: Option<&str>,
) -> Vec<String> {
    indexer
        .definition_locations(name)
        .into_iter()
        .filter(|location| {
            parent_class.is_none_or(|parent| {
                indexer
                    .enclosing_class_at(&location.uri, location.range.start.line)
                    .as_deref()
                    == Some(parent)
            })
        })
        .filter_map(|location| location.uri.to_file_path().ok())
        .filter_map(|path| path.to_str().map(str::to_owned))
        .collect()
}

async fn collect_reference_locations(
    indexer: &Arc<Indexer>,
    uri: &Url,
    rename_target: &RenameCursorSymbol,
) -> Vec<Location> {
    let declaration_files = definition_files_for_rename(
        indexer,
        &rename_target.name,
        rename_target.parent_class.as_deref(),
    );
    let file_path = uri.to_file_path().ok();
    let (workspace_root, source_paths, matcher) = indexer.rg_scope_for_path(file_path.as_deref());
    let uri = uri.clone();
    let name = rename_target.name.clone();
    let parent_class = rename_target.parent_class.clone();
    let declared_package = rename_target.declared_package.clone();
    let mut reference_locations = tokio::task::spawn_blocking(move || {
        let request = crate::rg::RgSearchRequest::new(
            &name,
            parent_class.as_deref(),
            declared_package.as_deref(),
            workspace_root.as_deref(),
            true,
            &uri,
            &declaration_files,
        )
        .with_source_paths(&source_paths);
        crate::rg::rg_find_references(&request, matcher.as_deref())
    })
    .await
    .unwrap_or_default();

    reference_locations.retain(|location| !indexer.is_library_uri(&location.uri));
    reference_locations
}

fn reference_candidate_files(current_uri: &Url, reference_locations: &[Location]) -> Vec<Url> {
    let mut files = vec![current_uri.clone()];
    for location in reference_locations {
        if !files.contains(&location.uri) {
            files.push(location.uri.clone());
        }
    }
    log::debug!(
        "[rename] rg found {} locs across {} files",
        reference_locations.len(),
        files.len()
    );
    files
}

fn rename_lines_for_file(indexer: &Indexer, file_uri: &Url) -> Option<Vec<String>> {
    if let Some(lines) = indexer.lines_for(file_uri) {
        return Some(lines.as_slice().to_vec());
    }

    let path = file_uri.to_file_path().ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    Some(content.lines().map(str::to_owned).collect())
}

fn build_workspace_edit(
    indexer: &Indexer,
    file_uris: &[Url],
    name: &str,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    let mut changes = HashMap::new();

    for file_uri in file_uris {
        let Some(lines) = rename_lines_for_file(indexer, file_uri) else {
            continue;
        };
        let scope = (0, lines.len().saturating_sub(1));
        let mut edits = rename_in_scope(&lines, name, new_name, scope, false);
        if edits.is_empty() {
            continue;
        }
        edits.sort_by_key(|edit| Reverse(edit.range.start));
        changes.insert(file_uri.clone(), edits);
    }

    (!changes.is_empty()).then_some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

pub(crate) async fn prepare_rename_impl(
    indexer: &Indexer,
    uri: &Url,
    pos: Position,
) -> Result<Option<PrepareRenameResponse>> {
    let (word, range) = match indexer.word_and_range_at(uri, pos) {
        Some(word_and_range) => word_and_range,
        None => return Ok(None),
    };

    if word.len() <= 1 || is_keyword_for_file(&word, uri.path()) {
        return Ok(None);
    }

    Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
        range,
        placeholder: word,
    }))
}

pub(crate) async fn rename_impl(
    indexer: &Arc<Indexer>,
    uri: &Url,
    pos: Position,
    new_name: &str,
) -> Result<Option<WorkspaceEdit>> {
    let Some(rename_target) = resolve_cursor_symbol(indexer, uri, pos) else {
        return Ok(None);
    };

    if rename_target.scope_limited_to_current_file {
        return Ok(rename_local_symbol(
            indexer,
            uri,
            pos,
            &rename_target.name,
            new_name,
        ));
    }

    let reference_locations = collect_reference_locations(indexer, uri, &rename_target).await;
    if reference_locations.is_empty() {
        return Ok(None);
    }

    let files = reference_candidate_files(uri, &reference_locations);
    Ok(build_workspace_edit(
        indexer,
        &files,
        &rename_target.name,
        new_name,
    ))
}
