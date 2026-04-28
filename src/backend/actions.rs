use std::sync::atomic::Ordering;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use super::Backend;
use super::rename::whole_word_replace_file;

/// Returns true if `name` is a keyword that precedes a block but is NOT
/// a function call — i.e. we should NOT show signature help for it.
pub(super) fn is_non_call_keyword(name: &str) -> bool {
    matches!(name,
        "fun" | "if" | "while" | "for" | "when" | "catch" | "constructor"
        | "override" | "else" | "return" | "throw" | "try" | "finally"
        | "object" | "class" | "interface" | "enum" | "init"
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
        if c.is_alphanumeric() || c == '_' || c == '.' { new_s -= 1; } else { break; }
    }
    // Strip leading dots we may have swallowed.
    while new_s < e && chars[new_s] == '.' { new_s += 1; }

    // Expand right over remaining identifier chars.
    let mut new_e = e;
    while new_e < chars.len() {
        let c = chars[new_e];
        if c.is_alphanumeric() || c == '_' { new_e += 1; } else { break; }
    }
    // Eat balanced `(…)` if present.
    if new_e < chars.len() && chars[new_e] == '(' {
        let mut depth = 0usize;
        while new_e < chars.len() {
            match chars[new_e] {
                '(' => { depth += 1; new_e += 1; }
                ')' => { new_e += 1; depth -= 1; if depth == 0 { break; } }
                _   => { new_e += 1; }
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
    let seg = if let Some(p) = seg.find('(') { &seg[..p] } else { seg };
    let seg = seg.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');

    // Strip common accessor prefixes: getXxx → xxx, isXxx → isXxx (keep),
    // hasXxx → hasXxx (keep), setXxx → skip (nothing useful).
    let result = if seg.starts_with("get") && seg.len() > 3 {
        let rest = &seg[3..];
        // Only strip if next char is uppercase (proper camelCase).
        if rest.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
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

    if result.is_empty() { "value".to_string() } else { result }
}

impl Backend {
    pub(super) async fn completion_impl(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let pp       = params.text_document_position;
        let uri      = &pp.text_document.uri;
        let position = pp.position;
        let snippets = self.snippet_support.load(Ordering::Relaxed);

        let (items, hit_cap) = self.indexer.completions(uri, position, snippets);
        let still_indexing = self.indexer.indexing_in_progress.load(Ordering::Acquire);
        if items.is_empty() && !still_indexing {
            return Ok(None);
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
        let uri      = &params.text_document.uri;
        let range    = params.range;

        // Read the current line from live_lines (most up-to-date).
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

        let mut actions: Vec<CodeActionOrCommand> = Vec::new();

        let trimmed = line_text.trim();
        let is_import_line = trimmed.starts_with("import ") || trimmed.starts_with("package ");

        // ── 1. Introduce local variable ──────────────────────────────────────
        // Available when there is a non-empty selection on a single line,
        // but NOT on import/package lines.
        let sel_start = range.start;
        let sel_end   = range.end;
        let has_selection = sel_start != sel_end && sel_start.line == sel_end.line;

        if has_selection && !is_import_line {
            let chars: Vec<char> = line_text.chars().collect();
            let raw_s = (sel_start.character as usize).min(chars.len());
            let raw_e = (sel_end.character as usize).min(chars.len());

            // Expand the selection to capture the full dotted-call expression.
            // Helix often sends only the word under the cursor (e.g. `isRefreshing`)
            // even when the user wants the whole `receiver.isRefreshing()`.
            let (s, e) = expand_call_expr(&chars, raw_s, raw_e);
            let expr: String = chars[s..e].iter().collect();

            if !expr.trim().is_empty() {
                let var_name = derive_var_name(&expr);
                let indent: String = line_text.chars().take_while(|c| c.is_whitespace()).collect();

                // Single edit: replace entire line with two lines:
                //   1) val <name> = <expr>
                //   2) original line with <expr> substituted by <name>
                let prefix: String = chars[..s].iter().collect();
                let suffix: String = chars[e..].iter().collect();
                let replaced_line = format!("{prefix}{var_name}{suffix}");
                let line_utf16_len: u32 = line_text.chars().map(|c| c.len_utf16() as u32).sum();
                let new_text = format!("{indent}val {var_name} = {expr}\n{replaced_line}");

                let mut changes = std::collections::HashMap::new();
                changes.insert(uri.clone(), vec![TextEdit {
                    range: Range {
                        start: Position { line: sel_start.line, character: 0 },
                        end:   Position { line: sel_start.line, character: line_utf16_len },
                    },
                    new_text,
                }]);

                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Introduce local variable `{var_name}`"),
                    kind:  Some(CodeActionKind::REFACTOR_EXTRACT),
                    edit:  Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                    ..Default::default()
                }));
            }
        }

        // ── 2. Add import alias / rename in file ────────────────────────────

        // Read all lines once.
        let all_lines: Vec<String> = {
            if let Some(ll) = self.indexer.live_lines.get(uri.as_str()) {
                ll.clone().to_vec()
            } else if let Some(data) = self.indexer.files.get(uri.as_str()) {
                data.lines.to_vec()
            } else {
                vec![]
            }
        };

        // Word under cursor.
        let cursor_word: String = {
            let chars: Vec<char> = line_text.chars().collect();
            let col = (range.start.character as usize).min(chars.len());
            let mut ws = col;
            while ws > 0 && (chars[ws-1].is_alphanumeric() || chars[ws-1] == '_') { ws -= 1; }
            let mut we = col;
            while we < chars.len() && (chars[we].is_alphanumeric() || chars[we] == '_') { we += 1; }
            chars[ws..we].iter().collect()
        };

        // Case A: cursor on import line — append ` as <last_segment>`.
        // Only relevant for Kotlin/KTS files (Java/Swift don't use this syntax).
        let is_kotlin = uri.path().ends_with(".kt") || uri.path().ends_with(".kts");
        if is_kotlin && trimmed.starts_with("import ") && !trimmed.contains(" as ") {
            let path  = trimmed.trim_start_matches("import ").trim().trim_end_matches(".*");
            let alias = path.rsplit('.').next().unwrap_or(path);
            if !alias.is_empty() {
                let ln  = range.start.line;
                let col = line_text.chars().count() as u32;
                let mut changes = std::collections::HashMap::new();
                changes.insert(uri.clone(), vec![TextEdit {
                    range: Range {
                        start: Position { line: ln, character: col },
                        end:   Position { line: ln, character: col },
                    },
                    new_text: format!(" as {alias}"),
                }]);
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Add import alias `as {alias}`"),
                    kind:  Some(CodeActionKind::QUICKFIX),
                    edit:  Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                    ..Default::default()
                }));
            }
        }

        // Case B: cursor on a type name in code — offer two actions:
        //   B1. Add ` as <word>` to matching import (import line only, safe).
        //   B2. Replace all whole-word occurrences in this file with `_<word>`
        //       as a placeholder (single whole-file TextEdit, no crash risk).
        //       User then does  %s_Word<ret>cNewName<esc>  in Helix.
        // Only for Kotlin/KTS files — Java/Swift use different rename flows.
        if is_kotlin && !is_import_line && !cursor_word.is_empty()
            && cursor_word.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
        {
            // Combined: add `as _Word` to matching import + rename Word→_Word in body (single action).
            if !all_lines.is_empty() {
                let placeholder = format!("_{cursor_word}");
                // Rename in non-import lines only (whole-file TextEdit).
                let new_content = whole_word_replace_file(&all_lines, &cursor_word, &placeholder);
                let last_line   = (all_lines.len() - 1) as u32;
                let last_col    = all_lines.last().map(|l| l.chars().count() as u32).unwrap_or(0);

                // Check if there's a matching import to also alias.
                let import_edit = all_lines.iter().enumerate()
                    .find(|(_, l)| {
                        let t = l.trim();
                        t.starts_with("import ") && !t.contains(" as ")
                        && t.rsplit(['.', ' ']).next().map(|s| s == cursor_word).unwrap_or(false)
                    })
                    .map(|(import_ln, import_line_text)| {
                        let col = import_line_text.chars().count() as u32;
                        TextEdit {
                            range: Range {
                                start: Position { line: import_ln as u32, character: col },
                                end:   Position { line: import_ln as u32, character: col },
                            },
                            new_text: format!(" as {placeholder}"),
                        }
                    });

                // Body rename replaces the whole file (skipping import lines).
                let mut body_edit = TextEdit {
                    range: Range {
                        start: Position { line: 0,        character: 0 },
                        end:   Position { line: last_line, character: last_col },
                    },
                    new_text: new_content,
                };

                // If we also have an import alias edit, we must embed it inside the body
                // content (since both edits touch the same file and LSP applies them
                // sequentially — easiest is to patch the already-replaced body content
                // at the import line position directly).
                if let Some(ie) = import_edit {
                    // Splice the alias into the body content at the right line.
                    let mut body_lines: Vec<String> = body_edit.new_text.split('\n').map(|s| s.to_owned()).collect();
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
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title,
                    kind:  Some(CodeActionKind::REFACTOR),
                    edit:  Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                    ..Default::default()
                }));
            }
        }

        Ok(if actions.is_empty() { None } else { Some(actions) })
    }
}
