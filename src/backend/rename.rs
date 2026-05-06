use super::actions::is_non_call_keyword;
use super::helpers::resolve_references_scope;
use super::Backend;
use crate::indexer::live_tree::utf16_col_to_byte;
use crate::queries::{
    KIND_ANON_FUN, KIND_CLASS_BODY, KIND_COMPANION_OBJ, KIND_FUN_DECL, KIND_METHOD_DECL,
    KIND_MULTI_VAR_DECL, KIND_NAV_EXPR, KIND_OBJECT_DECL, KIND_PROP_DECL, KIND_SOURCE_FILE,
    KIND_VAR_DECL,
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
            // Inside a binding and hit a function boundary → local variable.
            KIND_FUN_DECL | KIND_METHOD_DECL | KIND_ANON_FUN if in_binding => return true,
            // Function/method declaration without being in a binding → not a local var.
            KIND_FUN_DECL | KIND_METHOD_DECL | KIND_ANON_FUN => return false,
            // Navigation expression → member access, not a local var.
            KIND_NAV_EXPR => return false,
            // Class body, companion object, or source file reached while in a binding
            // → class-level or top-level property, NOT a local var.
            KIND_CLASS_BODY | KIND_OBJECT_DECL | KIND_COMPANION_OBJ | KIND_SOURCE_FILE
                if in_binding =>
            {
                return false
            }
            // Scope boundary without being in a binding → not applicable.
            KIND_CLASS_BODY | KIND_OBJECT_DECL | KIND_COMPANION_OBJ | KIND_SOURCE_FILE => {
                return false
            }
            _ => {}
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

/// Return `true` when the CST node at `pos` belongs to a function/method
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
                return false
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

impl Backend {
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

        let name = match self.indexer.word_at(uri, pos) {
            Some(w) => w,
            None => return Ok(None),
        };

        // ── Resolve scoping (delegates to the shared helper used by findReferences) ─
        let (parent_class, declared_pkg) = if name.starts_with_uppercase() {
            resolve_references_scope(&self.indexer, uri, pos.line, &name)
        } else {
            // Lowercase symbol — limit to enclosing scope in current file only.
            let lines = match self.indexer.lines_for(uri) {
                Some(l) => l,
                None => return Ok(None),
            };
            let scope = enclosing_scope(&lines, pos.line as usize);

            // `skip_dotted = true` only for local variables — a `val`/`var` whose
            // nearest scope ancestor is a function body (not a class body or source file).
            // Class properties, top-level vals, and method call sites all need dotted
            // accesses included so that `this.myProp` / `obj.myProp` are also renamed.
            let skip_dotted = cst_cursor_is_local_var(&self.indexer, uri, pos);

            let mut file_edits = rename_in_scope(&lines, &name, new_name, scope, skip_dotted);
            if file_edits.is_empty() {
                return Ok(None);
            }
            file_edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
            let mut changes = std::collections::HashMap::new();
            changes.insert(uri.clone(), file_edits);
            return Ok(Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }));
        };

        let decl_files: Vec<String> = self
            .indexer
            .definitions
            .get(&name)
            .map(|locs| {
                locs.iter()
                    .filter(|l| {
                        if let Some(ref parent) = parent_class {
                            self.indexer
                                .enclosing_class_at(&l.uri, l.range.start.line)
                                .as_deref()
                                == Some(parent.as_str())
                        } else {
                            true
                        }
                    })
                    .filter_map(|l| l.uri.to_file_path().ok())
                    .filter_map(|p| p.to_str().map(|s| s.to_owned()))
                    .collect()
            })
            .unwrap_or_default();

        // ── Find all reference locations (off-thread, same as references handler) ──
        let root = self.indexer.workspace_root.read().unwrap().clone();
        let matcher = self.indexer.ignore_matcher.read().unwrap().clone();
        let uri_clone = uri.clone();
        let name2 = name.clone();
        let parent2 = parent_class.clone();
        let decl2 = declared_pkg.clone();
        // include_declaration=true so we also rename the declaration site
        let ref_locs = tokio::task::spawn_blocking(move || {
            crate::rg::rg_find_references(
                &name2,
                parent2.as_deref(),
                decl2.as_deref(),
                root.as_deref(),
                true,
                &uri_clone,
                &decl_files,
                matcher.as_deref(),
            )
        })
        .await
        .unwrap_or_default();

        // Filter out library-source locations — library files are read-only (not user code).
        let mut ref_locs = ref_locs;
        let lib = &self.indexer.library_uris;
        if !lib.is_empty() {
            ref_locs.retain(|loc| !lib.contains(loc.uri.as_str()));
        }

        if ref_locs.is_empty() {
            return Ok(None);
        }

        // ── Collect unique files that have references ───────────────────────
        // Always include current file (may have unsaved content rg can't see).
        let mut files: Vec<Url> = vec![uri.clone()];
        for loc in &ref_locs {
            if !files.contains(&loc.uri) {
                files.push(loc.uri.clone());
            }
        }
        eprintln!(
            "[rename] rg found {} locs across {} files",
            ref_locs.len(),
            files.len()
        );

        // ── Build TextEdits per file using rename_in_scope ──────────────────
        // We do NOT use rg location columns directly because Pass A uses a
        // qualified pattern (ParentClass.Name) so the match column points to
        // ParentClass, not Name. Instead we use rg_find_references only to
        // identify which files need editing, then do precise word replacement.
        let mut changes: std::collections::HashMap<Url, Vec<TextEdit>> =
            std::collections::HashMap::new();

        for file_uri in &files {
            // Prefer in-memory lines (open buffer with unsaved edits), then fall
            // back to reading from disk so we can rename closed files too.
            let mem_lines = self.indexer.lines_for(file_uri);
            let disk_lines: Vec<String>;
            let lines: &[String] = match mem_lines {
                Some(ref arc) => arc.as_slice(),
                None => {
                    let path = match file_uri.to_file_path() {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    match std::fs::read_to_string(&path) {
                        Ok(content) => {
                            disk_lines = content.lines().map(|l| l.to_owned()).collect();
                            &disk_lines
                        }
                        Err(_) => continue,
                    }
                }
            };
            let lines = lines.to_vec();

            let scope = (0, lines.len().saturating_sub(1));
            let edits = rename_in_scope(&lines, &name, new_name, scope, false);

            if !edits.is_empty() {
                let mut edits = edits;
                edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
                changes.insert(file_uri.clone(), edits);
            }
        }

        if changes.is_empty() {
            return Ok(None);
        }
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{cst_cursor_is_local_var, cst_cursor_is_method};
    use crate::indexer::Indexer;
    use tower_lsp::lsp_types::{Position, Url};

    fn make_indexer_with(src: &str) -> (Indexer, Url) {
        let idx = Indexer::new();
        let uri = Url::parse("file:///tmp/test.kt").unwrap();
        idx.store_live_tree(&uri, src);
        (idx, uri)
    }

    fn make_indexed(src: &str) -> (Indexer, Url) {
        let idx = Indexer::new();
        let uri = Url::parse("file:///tmp/test.kt").unwrap();
        idx.index_content(&uri, src);
        idx.store_live_tree(&uri, src);
        (idx, uri)
    }

    #[test]
    fn cst_var_declaration_is_not_method() {
        let src = "val syncWith = repo.syncWith(Arg())\n";
        let (idx, uri) = make_indexer_with(src);
        // Cursor on `syncWith` at col 4 (the variable declaration)
        let pos = Position {
            line: 0,
            character: 4,
        };
        assert!(
            !cst_cursor_is_method(&idx, &uri, pos),
            "val decl should not be method"
        );
    }

    #[test]
    fn cst_nav_expr_member_access_is_method() {
        let src = "val syncWith = repo.syncWith(Arg())\n";
        let (idx, uri) = make_indexer_with(src);
        // Cursor on `syncWith` in `.syncWith(` — after 'repo.' → col 20
        let pos = Position {
            line: 0,
            character: 20,
        };
        assert!(
            cst_cursor_is_method(&idx, &uri, pos),
            "navigation_expression should be method"
        );
    }

    #[test]
    fn cst_function_declaration_is_method() {
        let src = "fun syncWith(arg: Arg): Result {\n    return Result()\n}\n";
        let (idx, uri) = make_indexer_with(src);
        // Cursor on `syncWith` at col 4
        let pos = Position {
            line: 0,
            character: 4,
        };
        assert!(
            cst_cursor_is_method(&idx, &uri, pos),
            "fun declaration should be method"
        );
    }

    #[test]
    fn cst_var_reference_not_method() {
        let src = "fun go() {\n    val x = 1\n    x.toString()\n}\n";
        let (idx, uri) = make_indexer_with(src);
        // Cursor on `x` at line 1, col 8 (in `val x = 1`)
        let pos = Position {
            line: 1,
            character: 8,
        };
        assert!(
            !cst_cursor_is_method(&idx, &uri, pos),
            "var decl should not be method"
        );
    }

    /// Local variable inside a function body → is local var (skip_dotted = true).
    #[test]
    fn local_var_inside_function_is_local() {
        let src = "fun go() {\n    val localFoo = 1\n}\n";
        let (idx, uri) = make_indexed(src);
        // Cursor on `localFoo` at line 1, col 8
        let pos = Position {
            line: 1,
            character: 8,
        };
        assert!(
            cst_cursor_is_local_var(&idx, &uri, pos),
            "val inside fun should be local var"
        );
    }

    /// Class property → not a local var, dotted accesses should be renamed.
    #[test]
    fn class_property_is_not_local_var() {
        let src = "class Foo {\n    val myProp: String = \"\"\n}\n";
        let (idx, uri) = make_indexed(src);
        // Cursor on `myProp` at line 1, col 8
        let pos = Position {
            line: 1,
            character: 8,
        };
        assert!(
            !cst_cursor_is_local_var(&idx, &uri, pos),
            "class property should not be local var"
        );
    }
}
