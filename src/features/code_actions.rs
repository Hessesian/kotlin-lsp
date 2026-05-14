//! Code action feature — pure builders for LSP `textDocument/codeAction` responses.
//!
//! All functions are pure: they take text/range/uri inputs and return
//! `CodeActionOrCommand` values.  No indexer access is required.
//!
//! Entry point: [`compute_code_actions`].

use std::collections::HashMap;

use tower_lsp::lsp_types::*;

use super::text_utils::whole_word_replace_file;
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

    if has_selection && !is_import_ln {
        if let Some(a) = build_introduce_variable(line_text, uri, range) {
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

// ─── keyword guard ────────────────────────────────────────────────────────────

/// Returns `true` when `name` is a block-opening keyword that is NOT callable —
/// i.e. signature help and rename should skip it.
pub(crate) fn is_non_call_keyword(name: &str) -> bool {
    matches!(
        name,
        "fun"
            | "if"
            | "while"
            | "for"
            | "when"
            | "catch"
            | "constructor"
            | "override"
            | "else"
            | "return"
            | "throw"
            | "try"
            | "finally"
            | "object"
            | "class"
            | "interface"
            | "enum"
            | "init"
    )
}

// ─── action builders ─────────────────────────────────────────────────────────

fn build_introduce_variable(
    line_text: &str,
    uri: &Url,
    range: Range,
) -> Option<CodeActionOrCommand> {
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
    let raw_s = utf16_to_char(range.start.character as usize);
    let raw_e = utf16_to_char(range.end.character as usize);
    let (s, e) = expand_call_expr(&chars, raw_s, raw_e);
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
    if !is_kotlin || !trimmed.starts_with("import ") || trimmed.contains(" as ") {
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

/// Expand a char-index range to cover the full call expression around it.
///
/// * Expands LEFT:  eats `[a-zA-Z0-9_.]` (dotted receiver chain)
/// * Expands RIGHT: eats remaining identifier chars, then a balanced `(…)` if present
fn expand_call_expr(chars: &[char], s: usize, e: usize) -> (usize, usize) {
    let mut new_s = s;
    while new_s > 0 {
        let c = chars[new_s - 1];
        if c.is_alphanumeric() || c == '_' || c == '.' {
            new_s -= 1;
        } else {
            break;
        }
    }
    while new_s < e && chars[new_s] == '.' {
        new_s += 1;
    }

    let mut new_e = e;
    while new_e < chars.len() {
        let c = chars[new_e];
        if c.is_alphanumeric() || c == '_' {
            new_e += 1;
        } else {
            break;
        }
    }
    if new_e < chars.len() && chars[new_e] == '(' {
        let mut depth = 0usize;
        while new_e < chars.len() {
            match chars[new_e] {
                '(' => {
                    depth += 1;
                    new_e += 1;
                }
                ')' => {
                    new_e += 1;
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {
                    new_e += 1;
                }
            }
        }
    }
    (new_s, new_e)
}

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
