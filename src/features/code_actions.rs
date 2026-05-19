//! Code action feature — pure builders for LSP `textDocument/codeAction` responses.
//!
//! All functions are pure: they take text/range/uri inputs and return
//! `CodeActionOrCommand` values.  No indexer access is required.
//!
//! Entry point: [`compute_code_actions`].

use std::collections::HashMap;

use tower_lsp::lsp_types::*;

use super::text_utils::whole_word_replace_file;
use crate::indexer::live_tree::{lang_for_path, parse_live, utf16_col_to_byte};
use crate::queries::{KIND_CALL_EXPR, KIND_LAMBDA_LIT, KIND_SOURCE_FILE};
use crate::StrExt;

// ─── entry point ──────────────────────────────────────────────────────────────

/// Compute all applicable code actions for the cursor position / selection.
///
/// * `line_text`  — text of the line at `range.start.line`
/// * `all_lines`  — all lines of the file (for whole-file edits)
/// * `is_kotlin`  — whether the file is a Kotlin source file
pub(crate) fn compute_code_actions(
    line_text: &str,
    all_lines: &[String],
    uri: &Url,
    range: Range,
    is_kotlin: bool,
) -> Vec<CodeActionOrCommand> {
    let trimmed = line_text.trim();
    let is_import_ln = trimmed.starts_with("import ") || trimmed.starts_with("package ");
    let has_selection = range.start != range.end && range.start.line == range.end.line;

    let mut actions: Vec<CodeActionOrCommand> = Vec::new();

    if has_selection && !is_import_ln && is_kotlin {
        if let Some(a) = build_introduce_variable(line_text, all_lines, uri, range) {
            actions.push(a);
        }
    }

    if let Some(a) = build_import_alias_action(line_text, trimmed, uri, range, is_kotlin) {
        actions.push(a);
    }

    let cursor_word = line_text.word_at_utf16_col(range.start.character as usize);
    if is_kotlin && !is_import_ln && !cursor_word.is_empty() && cursor_word.starts_with_uppercase()
    {
        if let Some(a) = build_rename_placeholder_action(&cursor_word, all_lines, uri) {
            actions.push(a);
        }
    }

    actions
}

// ─── package declaration action ───────────────────────────────────────────────

/// Known source-set root markers, longest-specific first so the best match
/// wins when multiple markers appear in the same path.
const SOURCE_SET_ROOTS: &[&str] = &[
    "src/commonMain/kotlin/",
    "src/androidMain/kotlin/",
    "src/iosMain/kotlin/",
    "src/jvmMain/kotlin/",
    "src/jsMain/kotlin/",
    "src/nativeMain/kotlin/",
    "src/commonTest/kotlin/",
    "src/androidTest/kotlin/",
    "src/iosTest/kotlin/",
    "src/jvmTest/kotlin/",
    "src/main/kotlin/",
    "src/main/java/",
    "src/test/kotlin/",
    "src/test/java/",
    "src/androidTest/java/",
];

/// Derive the Kotlin/Java package string from an absolute file path.
///
/// Searches for the rightmost (most specific) source-set root marker using
/// path-segment boundary matching (`/{marker}`), then converts the sub-path
/// between the marker and the filename into a dot-separated package identifier.
///
/// Returns `None` when:
/// - no recognised marker is found in the path
/// - the file lives directly at the source root (no sub-directory → top-level package)
/// - any path segment is not a valid Java/Kotlin identifier (e.g. starts with a digit)
pub(crate) fn resolve_package_from_path(path: &str) -> Option<String> {
    // For each marker, find the rightmost occurrence after a path separator so
    // that `…/src/main/kotlin/com/example/src/main/kotlin/Foo.kt` resolves the
    // inner occurrence and gives `com.example.src.main.kotlin` — the intended one.
    let (_, after_root) = SOURCE_SET_ROOTS
        .iter()
        .filter_map(|root| {
            let needle = format!("/{root}");
            // rfind gives the rightmost (deepest) match
            path.rfind(&needle)
                .map(|pos| (*root, &path[pos + needle.len()..]))
                // also allow the path to start directly with the marker
                .or_else(|| path.strip_prefix(root).map(|rest| (*root, rest)))
        })
        // prefer the longest matching root (most specific)
        .max_by_key(|(root, _)| root.len())?;

    let pkg_path = after_root.rsplit_once('/')?.0;
    if pkg_path.is_empty() {
        return None; // file directly at source root — no package
    }

    let pkg = pkg_path.replace('/', ".");
    if pkg.split('.').all(is_valid_identifier) {
        Some(pkg)
    } else {
        None
    }
}

/// A valid Java/Kotlin identifier segment must start with a letter or `_` and
/// contain only letters, digits, or `_`.  Hyphens are **not** silently rewritten —
/// if a directory uses them the package would be wrong, so we return `None`.
fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_alphabetic() || first == '_') && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// Find the line number where `package <pkg>` should be inserted.
///
/// In Kotlin, `@file:` annotations **must** precede the package declaration.
/// This function scans the leading block (blank lines, comments, `@file:` lines)
/// and returns the index of the line just after the last `@file:` annotation, or
/// `0` when none are present.
fn find_package_insert_line(all_lines: &[String]) -> u32 {
    let mut last_file_anno: Option<usize> = None;
    for (i, line) in all_lines.iter().enumerate() {
        let t = line.trim();
        if t.starts_with("@file:") {
            last_file_anno = Some(i);
        } else if !t.is_empty()
            && !t.starts_with("//")
            && !t.starts_with("/*")
            && !t.starts_with('*')
        {
            break; // reached real code — stop scanning
        }
    }
    last_file_anno.map(|i| (i + 1) as u32).unwrap_or(0)
}

/// Return a `HINT` diagnostic when a Kotlin/Java file is missing a package
/// declaration that can be derived from its path.
///
/// The range covers the line where the package should be inserted so editors
/// surface the hint—and the associated code action lightbulb—when the cursor
/// is near the top of the file.
pub(crate) fn missing_package_diagnostic(all_lines: &[String], uri: &Url) -> Option<Diagnostic> {
    let lang = crate::Language::from_path(uri.path());
    if !matches!(lang, crate::Language::Kotlin | crate::Language::Java) {
        return None;
    }
    if all_lines.iter().any(|l| l.trim().starts_with("package ")) {
        return None;
    }
    let pkg = resolve_package_from_path(uri.path())?;
    let insert_line = find_package_insert_line(all_lines);
    let line_len = all_lines
        .get(insert_line as usize)
        .map(|l| l.chars().map(|c| c.len_utf16() as u32).sum::<u32>())
        .unwrap_or(0);
    // Ensure the range is at least as wide as the word "package" so the hint
    // is visible even on empty lines.
    let end_col = line_len.max("package".len() as u32);
    Some(Diagnostic {
        range: Range::new(
            Position::new(insert_line, 0),
            Position::new(insert_line, end_col),
        ),
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("kotlin-lsp".into()),
        message: format!("Missing package declaration (`{pkg}`)"),
        ..Default::default()
    })
}

/// Build an "Add missing package declaration" code action.
///
/// Fires when the file has no `package` declaration and the path can be resolved
/// to a valid package identifier via [`resolve_package_from_path`].
pub(crate) fn build_add_package_action(
    all_lines: &[String],
    uri: &Url,
) -> Option<CodeActionOrCommand> {
    // Skip if file already declares a package
    if all_lines.iter().any(|l| l.trim().starts_with("package ")) {
        return None;
    }

    let pkg = resolve_package_from_path(uri.path())?;
    let insert_line = find_package_insert_line(all_lines);

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: Range {
                start: Position {
                    line: insert_line,
                    character: 0,
                },
                end: Position {
                    line: insert_line,
                    character: 0,
                },
            },
            new_text: format!("package {pkg}\n\n"),
        }],
    );
    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Add missing package declaration `{pkg}`"),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    }))
}

// ─── action builders ─────────────────────────────────────────────────────────

fn build_introduce_variable(
    line_text: &str,
    all_lines: &[String],
    uri: &Url,
    range: Range,
) -> Option<CodeActionOrCommand> {
    let expanded = expand_selection_to_call(all_lines, range, uri.path());
    let expanded = if expanded.start.line == expanded.end.line {
        expanded
    } else {
        range
    };
    let chars: Vec<char> = line_text.chars().collect();
    let utf16_to_char = |utf16: usize| {
        let mut cu = 0usize;
        for (i, c) in chars.iter().enumerate() {
            if cu >= utf16 {
                return i;
            }
            cu += c.len_utf16();
        }
        chars.len()
    };
    let s = utf16_to_char(expanded.start.character as usize);
    let e = utf16_to_char(expanded.end.character as usize);
    let expr: String = chars[s..e].iter().collect();
    if expr.trim().is_empty() {
        return None;
    }

    let var_name = derive_var_name(&expr);
    let indent: String = line_text
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    let prefix: String = chars[..s].iter().collect();
    let suffix: String = chars[e..].iter().collect();
    let replaced_line = format!("{prefix}{var_name}{suffix}");
    let line_utf16_len: u32 = line_text.chars().map(|c| c.len_utf16() as u32).sum();
    let new_text = format!("{indent}val {var_name} = {expr}\n{replaced_line}");

    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: Range {
                start: Position {
                    line: range.start.line,
                    character: 0,
                },
                end: Position {
                    line: range.start.line,
                    character: line_utf16_len,
                },
            },
            new_text,
        }],
    );
    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Introduce local variable `{var_name}`"),
        kind: Some(CodeActionKind::REFACTOR_EXTRACT),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    }))
}

fn build_import_alias_action(
    line_text: &str,
    trimmed: &str,
    uri: &Url,
    range: Range,
    is_kotlin: bool,
) -> Option<CodeActionOrCommand> {
    if !is_kotlin
        || !trimmed.starts_with("import ")
        || trimmed.contains(" as ")
        || trimmed.contains(".*")
    {
        return None;
    }
    let path = trimmed
        .trim_start_matches("import ")
        .trim()
        .trim_end_matches(".*");
    let alias = path.rsplit('.').next().unwrap_or(path);
    if alias.is_empty() {
        return None;
    }

    let ln = range.start.line;
    let col: u32 = line_text.chars().map(|c| c.len_utf16() as u32).sum();
    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: Range {
                start: Position {
                    line: ln,
                    character: col,
                },
                end: Position {
                    line: ln,
                    character: col,
                },
            },
            new_text: format!(" as {alias}"),
        }],
    );
    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Add import alias `as {alias}`"),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    }))
}

fn build_rename_placeholder_action(
    cursor_word: &str,
    all_lines: &[String],
    uri: &Url,
) -> Option<CodeActionOrCommand> {
    if all_lines.is_empty() {
        return None;
    }
    let placeholder = format!("_{cursor_word}");
    let new_content = whole_word_replace_file(all_lines, cursor_word, &placeholder);
    let last_line = (all_lines.len() - 1) as u32;
    let last_col = all_lines
        .last()
        .map(|l| l.chars().map(|c| c.len_utf16() as u32).sum::<u32>())
        .unwrap_or(0);

    let import_edit = all_lines
        .iter()
        .enumerate()
        .find(|(_, l)| {
            let t = l.trim();
            t.starts_with("import ")
                && !t.contains(" as ")
                && t.rsplit(['.', ' '])
                    .next()
                    .map(|s| s == cursor_word)
                    .unwrap_or(false)
        })
        .map(|(import_ln, import_line_text)| {
            let col = import_line_text
                .chars()
                .map(|c| c.len_utf16() as u32)
                .sum::<u32>();
            TextEdit {
                range: Range {
                    start: Position {
                        line: import_ln as u32,
                        character: col,
                    },
                    end: Position {
                        line: import_ln as u32,
                        character: col,
                    },
                },
                new_text: format!(" as {placeholder}"),
            }
        });

    let mut body_edit = TextEdit {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: last_line,
                character: last_col,
            },
        },
        new_text: new_content,
    };

    if let Some(ie) = import_edit {
        let mut body_lines: Vec<String> = body_edit
            .new_text
            .split('\n')
            .map(|s| s.to_owned())
            .collect();
        let iln = ie.range.start.line as usize;
        if iln < body_lines.len() {
            body_lines[iln].push_str(&ie.new_text);
        }
        body_edit.new_text = body_lines.join("\n");
    }

    let title = if body_edit.new_text.contains(&placeholder) {
        format!("Alias `{cursor_word}` as `{placeholder}` in file (then :%s/{placeholder}/NewName)")
    } else {
        format!("Rename `{cursor_word}` → `{placeholder}` in file")
    };

    let mut changes = HashMap::new();
    changes.insert(uri.clone(), vec![body_edit]);
    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title,
        kind: Some(CodeActionKind::REFACTOR),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    }))
}

// ─── text helpers ─────────────────────────────────────────────────────────────

/// Expand `range` (a single-line selection) to cover the enclosing
/// `call_expression` node in the CST.
///
/// Parses the file with tree-sitter, finds the leaf node at the start of the
/// selection, walks ancestors until `call_expression` is found, then converts
/// its byte range to a UTF-16 LSP `Range`.
///
/// Falls back to `range` unchanged when no live tree is available or when the
/// cursor is not inside a call expression.
fn expand_selection_to_call(all_lines: &[String], range: Range, uri_path: &str) -> Range {
    expand_selection_to_call_inner(all_lines, range, uri_path).unwrap_or(range)
}

fn expand_selection_to_call_inner(
    all_lines: &[String],
    range: Range,
    uri_path: &str,
) -> Option<Range> {
    let lang = lang_for_path(uri_path)?;
    let content = all_lines.join("\n");
    let doc = parse_live(&content, lang)?;

    let cursor_line = range.start.line as usize;
    let line_text = all_lines.get(cursor_line)?;
    let start_byte_col = utf16_col_to_byte(line_text, range.start.character as usize);

    let point = tree_sitter::Point {
        row: cursor_line,
        column: start_byte_col,
    };
    let leaf = doc
        .tree
        .root_node()
        .descendant_for_point_range(point, point)?;

    let call_node = call_expr_ancestor(leaf)?;

    let start = call_node.start_position();
    let end = call_node.end_position();
    let start_line_text = all_lines.get(start.row)?;
    let end_line_text = all_lines.get(end.row)?;

    let start_utf16 = byte_col_to_utf16(start_line_text, start.column);
    let end_utf16 = byte_col_to_utf16(end_line_text, end.column);

    Some(Range {
        start: Position {
            line: start.row as u32,
            character: start_utf16,
        },
        end: Position {
            line: end.row as u32,
            character: end_utf16,
        },
    })
}

/// Walk from `node` up the ancestor chain to find the nearest `call_expression`.
/// Stops at lambda literals and source-file boundaries (never returns those).
fn call_expr_ancestor(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut cur = node;
    loop {
        if cur.kind() == KIND_CALL_EXPR {
            return Some(cur);
        }
        if cur.kind() == KIND_LAMBDA_LIT || cur.kind() == KIND_SOURCE_FILE {
            return None;
        }
        cur = cur.parent()?;
    }
}

/// Convert a tree-sitter byte-offset column to a UTF-16 column index.
fn byte_col_to_utf16(line_text: &str, byte_col: usize) -> u32 {
    line_text[..byte_col.min(line_text.len())]
        .chars()
        .map(|c| c.len_utf16() as u32)
        .sum()
}

#[cfg(test)]
#[path = "code_actions_tests.rs"]
mod tests;

/// Derive a short local variable name from an expression.
///
/// `refreshDashboardInteractor.isRefreshing()` → `isRefreshing`
/// `user.getName()` → `name`  (strips "get" prefix)
/// `someValue` → `someValue`
fn derive_var_name(expr: &str) -> String {
    let seg = expr.trim().rsplit('.').next().unwrap_or(expr.trim());
    let seg = seg.find('(').map(|p| &seg[..p]).unwrap_or(seg);
    let seg = seg.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');

    let result = if seg.starts_with("get") && seg.len() > "get".len() {
        let rest = &seg["get".len()..];
        if rest.starts_with_uppercase() {
            if let Some(first) = rest.chars().next() {
                let mut s = first.to_lowercase().collect::<String>();
                s.push_str(&rest[first.len_utf8()..]);
                s
            } else {
                rest.to_string()
            }
        } else {
            seg.to_string()
        }
    } else {
        seg.to_string()
    };

    if result.is_empty() {
        "value".to_string()
    } else {
        result
    }
}
