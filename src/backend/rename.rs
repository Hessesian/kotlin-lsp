use super::actions::is_non_call_keyword;
use super::helpers::resolve_references_scope;
use super::Backend;
use crate::indexer::live_tree::utf16_col_to_byte;
use crate::queries::{
    KIND_ANON_FUN, KIND_CLASS_BODY, KIND_COMPANION_OBJ, KIND_FUN_DECL, KIND_LAMBDA_LIT,
    KIND_METHOD_DECL, KIND_MULTI_VAR_DECL, KIND_NAV_EXPR, KIND_OBJECT_DECL, KIND_PROP_DECL,
    KIND_SOURCE_FILE, KIND_VAR_DECL,
};
use crate::StrExt;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

/// Return `true` when the cursor is on a local variable declaration — i.e. a
/// `property_declaration` / `variable_declaration` whose nearest scope-boundary
/// ancestor is a function/lambda body rather than a class body or source file.
///
/// This is used to decide whether to skip dotted occurrences during rename:
/// - local var `val syncWith` inside a function → `skip_dotted = true`
///   (avoid renaming `.syncWith()` method calls)
/// - class property `val myProp` → `skip_dotted = false`
///   (must rename `this.myProp` / `obj.myProp` accesses)
/// - navigation expression (`.method()`) → `skip_dotted = false`
/// - function declaration → `skip_dotted = false`
fn cst_cursor_is_local_var(indexer: &crate::indexer::Indexer, uri: &Url, pos: Position) -> bool {
    use tree_sitter::Point;

    let doc = match indexer.live_doc(uri) {
        Some(d) => d,
        None => return false,
    };
    let full_text = match std::str::from_utf8(&doc.bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let line_idx = pos.line as usize;
    let line_text = match full_text.lines().nth(line_idx) {
        Some(l) => l,
        None => return false,
    };
    let byte_col = utf16_col_to_byte(line_text, pos.character as usize);
    let point = Point {
        row: line_idx,
        column: byte_col,
    };
    let start_node = match doc
        .tree
        .root_node()
        .descendant_for_point_range(point, point)
    {
        Some(n) => n,
        None => return false,
    };

    // Track whether we've entered a property/variable declaration binding.
    let mut in_binding = false;
    let mut cur = start_node;
    loop {
        let kind = cur.kind();
        match kind {
            // Entering a property/variable binding — continue walking up.
            KIND_PROP_DECL | KIND_VAR_DECL | KIND_MULTI_VAR_DECL => {
                in_binding = true;
            }
            // Inside a binding and hit a function/lambda boundary → local variable.
            KIND_FUN_DECL | KIND_METHOD_DECL | KIND_ANON_FUN | KIND_LAMBDA_LIT if in_binding => {
                return true;
            }
            // Function/method/lambda without being in a binding → not a local var.
            KIND_FUN_DECL | KIND_METHOD_DECL | KIND_ANON_FUN | KIND_LAMBDA_LIT => return false,
            // Navigation expression → member access, not a local var.
            KIND_NAV_EXPR => return false,
            // Class body, companion object, or source file reached while in a binding
            // → class-level or top-level property, NOT a local var.
            KIND_CLASS_BODY | KIND_OBJECT_DECL | KIND_COMPANION_OBJ | KIND_SOURCE_FILE
                if in_binding =>
            {
                return false;
            }
            // Scope boundary without being in a binding → not applicable.
            KIND_CLASS_BODY | KIND_OBJECT_DECL | KIND_COMPANION_OBJ | KIND_SOURCE_FILE => {
                return false;
            }
            _ => {}
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

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
fn any_local_var_decl_in_scope(
    indexer: &crate::indexer::Indexer,
    uri: &Url,
    name: &str,
    scope: (usize, usize),
) -> bool {
    use tower_lsp::lsp_types::SymbolKind;

    let file = match indexer.files.get(uri.as_str()) {
        Some(f) => f,
        None => return false,
    };
    file.symbols
        .iter()
        .filter(|s| {
            matches!(
                s.kind,
                SymbolKind::PROPERTY | SymbolKind::VARIABLE | SymbolKind::CONSTANT
            ) && s.name == name
                && (s.selection_range.start.line as usize) >= scope.0
                && (s.selection_range.start.line as usize) <= scope.1
        })
        .any(|s| {
            let decl_pos = Position {
                line: s.selection_range.start.line,
                character: s.selection_range.start.character,
            };
            cst_cursor_is_local_var(indexer, uri, decl_pos)
        })
}

#[cfg(test)]
/// declaration or a navigation expression (member access). Returns `false`
/// for property/variable declarations, parameters, and unknown contexts.
///
/// Used as a secondary check to detect method call sites (nav expressions)
/// that are not in the file's own symbol index.
fn cst_cursor_is_method(indexer: &crate::indexer::Indexer, uri: &Url, pos: Position) -> bool {
    use tree_sitter::Point;

    let doc = match indexer.live_doc(uri) {
        Some(d) => d,
        None => return false,
    };
    let full_text = match std::str::from_utf8(&doc.bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let line_idx = pos.line as usize;
    let line_text = match full_text.lines().nth(line_idx) {
        Some(l) => l,
        None => return false,
    };
    let byte_col = utf16_col_to_byte(line_text, pos.character as usize);
    let point = Point {
        row: line_idx,
        column: byte_col,
    };
    let start_node = match doc
        .tree
        .root_node()
        .descendant_for_point_range(point, point)
    {
        Some(n) => n,
        None => return false,
    };

    // Walk up the ancestor chain looking for the first structurally significant node.
    let mut cur = start_node;
    loop {
        let kind = cur.kind();
        match kind {
            // These indicate we're inside a variable / property binding → not a method.
            KIND_PROP_DECL | KIND_VAR_DECL | KIND_MULTI_VAR_DECL => return false,
            // These indicate a method/function context → treat as method.
            KIND_FUN_DECL | KIND_METHOD_DECL | KIND_ANON_FUN => return true,
            // A navigation expression means the identifier is a qualified member access.
            KIND_NAV_EXPR => return true,
            // Stop at top-level scope boundaries without a verdict.
            KIND_SOURCE_FILE | KIND_CLASS_BODY | KIND_OBJECT_DECL | KIND_COMPANION_OBJ => {
                return false;
            }
            _ => {}
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

/// Replace all whole-word occurrences of `word` in `lines` with `replacement`.
/// Returns the full new file content as a single string (lines joined with `\n`).
pub(super) fn whole_word_replace_file(lines: &[String], word: &str, replacement: &str) -> String {
    // Use simple char-by-char replacement to avoid regex dependency.
    let wchars: Vec<char> = word.chars().collect();
    let wlen = wchars.len();
    let mut result = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("import ") || trimmed.starts_with("package ") {
            result.push_str(line);
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut j = 0usize;
        while j < chars.len() {
            // Check whole-word match at position j.
            if chars[j..].starts_with(&wchars) {
                let before_ok = j == 0 || !(chars[j - 1].is_alphanumeric() || chars[j - 1] == '_');
                let end = j + wlen;
                let after_ok =
                    end >= chars.len() || !(chars[end].is_alphanumeric() || chars[end] == '_');
                if before_ok && after_ok {
                    result.push_str(replacement);
                    j = end;
                    continue;
                }
            }
            result.push(chars[j]);
            j += 1;
        }
    }
    result
}

/// Find the line range of the innermost function/lambda scope enclosing `cursor_line`.
/// Returns `(start_line, end_line)` inclusive, or the whole file if not found.
pub(super) fn enclosing_scope(lines: &[String], cursor_line: usize) -> (usize, usize) {
    // Walk backwards to find the opening `{` of the enclosing fun/lambda.
    let mut depth = 0i32;
    let mut scope_start = 0usize;
    'outer: for i in (0..=cursor_line.min(lines.len().saturating_sub(1))).rev() {
        for ch in lines[i].chars().rev() {
            match ch {
                '}' => depth += 1,
                '{' => {
                    if depth == 0 {
                        scope_start = i;
                        break 'outer;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
    }
    // Walk forward from scope_start to find matching `}`.
    let mut depth = 0i32;
    let mut scope_end = lines.len().saturating_sub(1);
    for (i, line) in lines.iter().enumerate().skip(scope_start) {
        for ch in line.chars() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        scope_end = i;
                        // break both loops
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
pub(super) fn rename_in_scope(
    lines: &[String],
    word: &str,
    new_name: &str,
    scope: (usize, usize),
    skip_dotted: bool,
) -> Vec<TextEdit> {
    let wchars: Vec<char> = word.chars().collect();
    let wlen = wchars.len();
    if wlen == 0 {
        return vec![];
    }
    let mut edits: Vec<TextEdit> = Vec::new();

    let end = scope.1.min(lines.len().saturating_sub(1));
    for (ln, line) in lines.iter().enumerate().take(end + 1).skip(scope.0) {
        // Skip package declaration — never rename the package statement.
        let trimmed = line.trim_start();
        if trimmed.starts_with("package ") {
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut j = 0usize;
        let char_to_utf16: Vec<u32> = {
            let mut v = Vec::with_capacity(chars.len() + 1);
            let mut acc = 0u32;
            for &c in &chars {
                v.push(acc);
                acc += c.len_utf16() as u32;
            }
            v.push(acc); // sentinel
            v
        };

        while j < chars.len() {
            if chars[j..].starts_with(&wchars) {
                let before_ok = j == 0 || !(chars[j - 1].is_alphanumeric() || chars[j - 1] == '_');
                let end_idx = j + wlen;
                let after_ok = end_idx >= chars.len()
                    || !(chars[end_idx].is_alphanumeric() || chars[end_idx] == '_');
                if before_ok && after_ok {
                    // When renaming a local variable, skip member-access occurrences
                    // (those preceded by '.') to avoid conflating with same-named methods.
                    if skip_dotted && j > 0 && chars[j - 1] == '.' {
                        j = end_idx;
                        continue;
                    }
                    let start_utf16 = char_to_utf16[j];
                    let end_utf16 = char_to_utf16[end_idx];
                    edits.push(TextEdit {
                        range: Range {
                            start: Position::new(ln as u32, start_utf16),
                            end: Position::new(ln as u32, end_utf16),
                        },
                        new_text: new_name.to_owned(),
                    });
                    j = end_idx;
                    continue;
                }
            }
            j += 1;
        }
    }

    // Reverse so callers applying sequentially won't shift earlier positions.
    edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
    edits
}

struct RenameCursorSymbol {
    name: String,
    parent_class: Option<String>,
    declared_package: Option<String>,
    scope_limited_to_current_file: bool,
}

impl Backend {
    fn resolve_cursor_symbol(&self, uri: &Url, pos: Position) -> Option<RenameCursorSymbol> {
        let name = self.indexer.word_at(uri, pos)?;
        let (parent_class, declared_package, scope_limited_to_current_file) =
            if name.starts_with_uppercase() {
                let (parent_class, declared_package) =
                    resolve_references_scope(&self.indexer, uri, pos.line, &name);
                (parent_class, declared_package, false)
            } else {
                (None, None, true)
            };

        Some(RenameCursorSymbol {
            name,
            parent_class,
            declared_package,
            scope_limited_to_current_file,
        })
    }

    fn rename_local_symbol(
        &self,
        uri: &Url,
        pos: Position,
        name: &str,
        new_name: &str,
    ) -> Option<WorkspaceEdit> {
        let lines = self.indexer.lines_for(uri)?;
        let scope = enclosing_scope(&lines, pos.line as usize);
        let skip_dotted = cst_cursor_is_local_var(&self.indexer, uri, pos)
            || any_local_var_decl_in_scope(&self.indexer, uri, name, scope);
        let mut file_edits = rename_in_scope(&lines, name, new_name, scope, skip_dotted);
        if file_edits.is_empty() {
            return None;
        }
        file_edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));

        let mut changes = std::collections::HashMap::new();
        changes.insert(uri.clone(), file_edits);
        Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        })
    }

    fn definition_files_for_rename(&self, name: &str, parent_class: Option<&str>) -> Vec<String> {
        self.indexer
            .definitions
            .get(name)
            .map(|locations| {
                locations
                    .iter()
                    .filter(|location| {
                        parent_class.is_none_or(|parent| {
                            self.indexer
                                .enclosing_class_at(&location.uri, location.range.start.line)
                                .as_deref()
                                == Some(parent)
                        })
                    })
                    .filter_map(|location| location.uri.to_file_path().ok())
                    .filter_map(|path| path.to_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn collect_reference_locations(
        &self,
        uri: &Url,
        rename_target: &RenameCursorSymbol,
    ) -> Vec<Location> {
        let decl_files = self.definition_files_for_rename(
            &rename_target.name,
            rename_target.parent_class.as_deref(),
        );
        let root = self.indexer.workspace_root.read().unwrap().clone();
        let matcher = self.indexer.ignore_matcher.read().unwrap().clone();
        let uri_clone = uri.clone();
        let name = rename_target.name.clone();
        let parent_class = rename_target.parent_class.clone();
        let declared_package = rename_target.declared_package.clone();
        let mut reference_locations = tokio::task::spawn_blocking(move || {
            let request = crate::rg::RgSearchRequest::new(
                &name,
                parent_class.as_deref(),
                declared_package.as_deref(),
                root.as_deref(),
                true,
                &uri_clone,
                &decl_files,
            );
            crate::rg::rg_find_references(&request, matcher.as_deref())
        })
        .await
        .unwrap_or_default();

        let library_uris = &self.indexer.library_uris;
        if !library_uris.is_empty() {
            reference_locations.retain(|location| !library_uris.contains(location.uri.as_str()));
        }
        reference_locations
    }

    fn reference_candidate_files(
        &self,
        current_uri: &Url,
        reference_locations: &[Location],
    ) -> Vec<Url> {
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

    fn rename_lines_for_file(&self, file_uri: &Url) -> Option<Vec<String>> {
        if let Some(lines) = self.indexer.lines_for(file_uri) {
            return Some(lines.as_slice().to_vec());
        }

        let path = file_uri.to_file_path().ok()?;
        let content = std::fs::read_to_string(path).ok()?;
        Some(content.lines().map(str::to_owned).collect())
    }

    fn build_workspace_edit(
        &self,
        file_uris: &[Url],
        name: &str,
        new_name: &str,
    ) -> Option<WorkspaceEdit> {
        let mut changes = std::collections::HashMap::new();

        for file_uri in file_uris {
            let Some(lines) = self.rename_lines_for_file(file_uri) else {
                continue;
            };
            let scope = (0, lines.len().saturating_sub(1));
            let mut edits = rename_in_scope(&lines, name, new_name, scope, false);
            if edits.is_empty() {
                continue;
            }
            edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
            changes.insert(file_uri.clone(), edits);
        }

        if changes.is_empty() {
            None
        } else {
            Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            })
        }
    }

    pub(super) async fn prepare_rename_impl(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = &params.text_document.uri;
        let pos = params.position;

        let (word, range) = match self.indexer.word_and_range_at(uri, pos) {
            Some(wr) => wr,
            None => return Ok(None),
        };

        // Don't allow renaming keywords or single-char identifiers that are likely noise.
        if word.len() <= 1 || is_non_call_keyword(&word) {
            return Ok(None);
        }

        Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
            range,
            placeholder: word,
        }))
    }

    pub(super) async fn rename_impl(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let new_name = &params.new_name;

        let Some(rename_target) = self.resolve_cursor_symbol(uri, pos) else {
            return Ok(None);
        };

        if rename_target.scope_limited_to_current_file {
            return Ok(self.rename_local_symbol(uri, pos, &rename_target.name, new_name));
        }

        let reference_locations = self.collect_reference_locations(uri, &rename_target).await;
        if reference_locations.is_empty() {
            return Ok(None);
        }

        let files = self.reference_candidate_files(uri, &reference_locations);
        Ok(self.build_workspace_edit(&files, &rename_target.name, new_name))
    }
}

#[cfg(test)]
#[path = "rename_tests.rs"]
mod tests;
