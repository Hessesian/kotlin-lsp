use super::rename::whole_word_replace_file;
use super::Backend;
use crate::StrExt;
use std::sync::atomic::Ordering;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

/// Returns true if `name` is a keyword that precedes a block but is NOT
/// a function call — i.e. we should NOT show signature help for it.
pub(super) fn is_non_call_keyword(name: &str) -> bool {
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

/// expression around it — e.g. `isRefreshing` → `refreshDashboardInteractor.isRefreshing()`.
///
/// - Expands LEFT:  eats `[a-zA-Z0-9_.]` (dotted receiver chain)
/// - Expands RIGHT: eats remaining identifier chars, then a balanced `(…)` if present
fn expand_call_expr(chars: &[char], s: usize, e: usize) -> (usize, usize) {
    // Expand left over [a-zA-Z0-9_.]
    let mut new_s = s;
    while new_s > 0 {
        let c = chars[new_s - 1];
        if c.is_alphanumeric() || c == '_' || c == '.' {
            new_s -= 1;
        } else {
            break;
        }
    }
    // Strip leading dots we may have swallowed.
    while new_s < e && chars[new_s] == '.' {
        new_s += 1;
    }

    // Expand right over remaining identifier chars.
    let mut new_e = e;
    while new_e < chars.len() {
        let c = chars[new_e];
        if c.is_alphanumeric() || c == '_' {
            new_e += 1;
        } else {
            break;
        }
    }
    // Eat balanced `(…)` if present.
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

/// Derive a local variable name from an expression.
///
/// `refreshDashboardInteractor.isRefreshing()` → `isRefreshing`
/// `user.getName()` → `name`  (strips "get" prefix)
/// `someValue` → `someValue`
fn derive_var_name(expr: &str) -> String {
    // Take the last `.`-separated segment, strip trailing `()` / `(…)`.
    let seg = expr.trim().rsplit('.').next().unwrap_or(expr.trim());
    let seg = if let Some(p) = seg.find('(') {
        &seg[..p]
    } else {
        seg
    };
    let seg = seg.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');

    // Strip common accessor prefixes: getXxx → xxx, isXxx → isXxx (keep),
    // hasXxx → hasXxx (keep), setXxx → skip (nothing useful).
    let result = if seg.starts_with("get") && seg.len() > "get".len() {
        let rest = &seg["get".len()..];
        // Only strip if next char is uppercase (proper camelCase).
        if rest.starts_with_uppercase() {
            let r = if let Some(first) = rest.chars().next() {
                let mut s = first.to_lowercase().collect::<String>();
                s.push_str(&rest[first.len_utf8()..]);
                s
            } else {
                rest.to_string()
            };
            r
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

impl Backend {
    pub(super) async fn completion_impl(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let pp = params.text_document_position;
        let uri = &pp.text_document.uri;
        let position = pp.position;
        let snippets = self.snippet_support.load(Ordering::Relaxed);

        let (mut items, hit_cap) = self.indexer.completions(uri, position, snippets);
        let still_indexing = self.indexer.indexing_in_progress.load(Ordering::Acquire);
        if items.is_empty() && !still_indexing {
            return Ok(None);
        }
        // Pre-select the best match so the editor highlights it without requiring
        // an extra keystroke (mirrors RA's preselect behaviour).
        if let Some(first) = items.first_mut() {
            first.preselect = Some(true);
        }
        // When hit_cap is true the list was truncated — tell the client to
        // re-request completions on every keystroke so the list stays tight
        // as the user types more characters.
        // Also mark incomplete while the workspace is still being indexed so
        // the client keeps re-querying instead of caching a partial result.
        Ok(Some(CompletionResponse::List(CompletionList {
            is_incomplete: hit_cap || still_indexing,
            items,
        })))
    }

    pub(super) async fn code_action_impl(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<Vec<CodeActionOrCommand>>> {
        let uri = &params.text_document.uri;
        let range = params.range;

        let line_text: String = {
            let ln = range.start.line as usize;
            if let Some(ll) = self.indexer.live_lines.get(uri.as_str()) {
                ll.get(ln).cloned().unwrap_or_default()
            } else if let Some(data) = self.indexer.files.get(uri.as_str()) {
                data.lines.get(ln).cloned().unwrap_or_default()
            } else {
                String::new()
            }
        };

        let trimmed = line_text.trim().to_owned();
        let is_import_ln = trimmed.starts_with("import ") || trimmed.starts_with("package ");
        let sel_start = range.start;
        let sel_end = range.end;
        let has_selection = sel_start != sel_end && sel_start.line == sel_end.line;

        let mut actions: Vec<CodeActionOrCommand> = Vec::new();

        if has_selection && !is_import_ln {
            if let Some(a) = build_introduce_variable(&line_text, uri, range) {
                actions.push(a);
            }
        }

        let all_lines: Vec<String> = {
            if let Some(ll) = self.indexer.live_lines.get(uri.as_str()) {
                ll.clone().to_vec()
            } else if let Some(data) = self.indexer.files.get(uri.as_str()) {
                data.lines.to_vec()
            } else {
                vec![]
            }
        };

        let cursor_word = line_text.word_at_utf16_col(range.start.character as usize);
        let is_kotlin = crate::Language::from_path(uri.path()).is_kotlin();

        if let Some(a) = build_import_alias_action(&trimmed, uri, range, is_kotlin) {
            actions.push(a);
        }

        if is_kotlin
            && !is_import_ln
            && !cursor_word.is_empty()
            && cursor_word.starts_with_uppercase()
        {
            if let Some(a) = build_rename_placeholder_action(&cursor_word, &all_lines, uri) {
                actions.push(a);
            }
        }

        Ok(if actions.is_empty() {
            None
        } else {
            Some(actions)
        })
    }
}

/// Builds the "Introduce local variable" code action for the selected expression.
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

    let mut changes = std::collections::HashMap::new();
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

/// Builds the "Add import alias" action for an import line (Kotlin only).
fn build_import_alias_action(
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
    // col at the end of the line (append alias there)
    let col: u32 = {
        // Reconstruct the actual import line length from trimmed — we don't have raw line_text here,
        // so we use the raw line from the range end if available, but trimmed gives us the content.
        // Since trimmed is the trim of line_text and we only need the utf16 length to append,
        // we just use trimmed length (the trailing whitespace doesn't matter for append).
        trimmed.chars().map(|c| c.len_utf16() as u32).sum()
    };
    let mut changes = std::collections::HashMap::new();
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

/// Builds the "Alias/rename in file" action for an uppercase type name (Kotlin only).
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

    // Check if there's a matching import to also alias.
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

    // Splice the import alias into the already-replaced body content so we
    // emit a single TextEdit (LSP doesn't guarantee ordering for overlapping edits).
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

    let mut changes = std::collections::HashMap::new();
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
