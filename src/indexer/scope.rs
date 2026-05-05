//! Cursor-context resolution helpers: word extraction, qualifier parsing,
//! lambda parameter inference, enclosing-class lookup.

use std::sync::Arc;
use tower_lsp::lsp_types::*;
use tree_sitter::Point;

use super::{
    Indexer,
    find_it_element_type_in_lines,
    find_this_element_type_in_lines,
    find_as_call_arg_type,
    find_named_lambda_param_type_in_lines,
    line_has_lambda_param,
    lambda_brace_pos_for_param,
    is_inside_receiver_lambda,
};
use crate::indexer::NodeExt;
use crate::queries::KIND_LAMBDA_LIT;
use crate::types::CursorPos;
use crate::StrExt;

/// Lines to scan backward when resolving variable types and lambda receivers from scope.
const SCOPE_SCAN_BACK_LINES: usize = 50;

/// Lines to scan upward when looking for a local variable declaration.
const DECL_SCAN_UP_LINES: usize = 15;

/// Lines to scan backward when looking for an enclosing call during named-argument scanning.
const ENCLOSING_CALL_SCAN_BACK: usize = 20;

impl Indexer {
    /// LSP positions are UTF-16; for ASCII-heavy Kotlin/Java identifiers the
    /// character offset is identical to the UTF-16 unit offset.
    pub fn word_at(&self, uri: &Url, position: Position) -> Option<String> {
        self.word_and_qualifier_at(uri, position).map(|(w, _)| w)
    }

    /// Like `word_at` but also returns the `Range` of the word in LSP (UTF-16) coordinates.
    pub fn word_and_range_at(&self, uri: &Url, position: Position) -> Option<(String, Range)> {
        let lines = self.lines_for(uri)?;
        let line_text = lines.get(position.line as usize)?;
        let target_utf16 = position.character as usize;
        let mut utf16_acc = 0usize;
        let mut char_idx  = 0usize;
        for ch in line_text.chars() {
            if utf16_acc >= target_utf16 { break; }
            utf16_acc += ch.len_utf16();
            char_idx  += 1;
        }
        let chars: Vec<char> = line_text.chars().collect();
        let effective = if char_idx < chars.len() && is_id_char(chars[char_idx]) {
            char_idx
        } else if char_idx > 0 && is_id_char(chars[char_idx - 1]) {
            char_idx - 1
        } else {
            return None;
        };
        let start_char = (0..=effective).rev()
            .find(|&i| !is_id_char(chars[i])).map(|i| i + 1).unwrap_or(0);
        let end_char = (effective..chars.len())
            .find(|&i| !is_id_char(chars[i])).unwrap_or(chars.len());
        if start_char >= end_char { return None; }
        let word: String = chars[start_char..end_char].iter().collect();
        if word == "_" { return None; }
        // Compute UTF-16 columns for start and end.
        let start_utf16 = chars[..start_char].iter().map(|c| c.len_utf16() as u32).sum::<u32>();
        let end_utf16   = start_utf16 + chars[start_char..end_char].iter().map(|c| c.len_utf16() as u32).sum::<u32>();
        let range = Range {
            start: Position::new(position.line, start_utf16),
            end:   Position::new(position.line, end_utf16),
        };
        Some((word, range))
    }

    /// Returns a clone of the live (possibly unsaved) lines for a URI.
    pub fn lines_for(&self, uri: &Url) -> Option<Arc<Vec<String>>> {
        // Prefer live (unsaved) lines, fall back to indexed file.
        if let Some(live) = self.live_lines.get(uri.as_str()) {
            return Some(live.clone());
        }
        if let Some(f) = self.files.get(uri.as_str()) {
            return Some(f.lines.clone());
        }
        // File not indexed yet (cold start / indexing in progress) — read from disk
        // so that word_at / word_and_qualifier_at work and rg fallbacks can fire.
        if let Ok(path) = uri.to_file_path() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
                return Some(Arc::new(lines));
            }
        }
        None
    }

    /// Like `word_at` but also returns the single dot-qualifier immediately
    /// preceding the word, if any.
    ///
    /// `AccountPickerMapper.Content`  cursor on `Content`
    ///   → `Some(("Content", Some("AccountPickerMapper")))`
    ///
    /// `List<StaticDocument>` cursor on `StaticDocument`
    ///   → `Some(("StaticDocument", None))`
    pub fn word_and_qualifier_at(
        &self,
        uri: &Url,
        position: Position,
    ) -> Option<(String, Option<String>)> {
        let lines = self.lines_for(uri)?;
        let line = lines.get(position.line as usize)?;

        // UTF-16 → char index
        let target_utf16 = position.character as usize;
        let mut utf16_acc = 0usize;
        let mut char_idx  = 0usize;
        for ch in line.chars() {
            if utf16_acc >= target_utf16 { break; }
            utf16_acc += ch.len_utf16();
            char_idx  += 1;
        }

        let chars: Vec<char> = line.chars().collect();
        let effective = if char_idx < chars.len() && is_id_char(chars[char_idx]) {
            char_idx
        } else if char_idx > 0 && is_id_char(chars[char_idx - 1]) {
            char_idx - 1
        } else {
            return None;
        };

        let start = (0..=effective)
            .rev()
            .find(|&i| !is_id_char(chars[i]))
            .map(|i| i + 1)
            .unwrap_or(0);

        let end = (effective..chars.len())
            .find(|&i| !is_id_char(chars[i]))
            .unwrap_or(chars.len());

        if start >= end { return None; }
        let word: String = chars[start..end].iter().collect();
        if word == "_" { return None; }

        // Scan back over the full dot-chain preceding the word.
        // `A.B.C.D` cursor on `D` → qualifier `"A.B.C"`, not just `"C"`.
        // `resolve_qualified` then uses the ROOT segment ("A") to locate the file
        // and searches that file for the word ("D"), handling arbitrary nesting depth.
        let qualifier = if start >= 2 && chars[start - 1] == '.' {
            let mut scan = start - 1; // pointing at the final dot
            while scan > 0 && (is_id_char(chars[scan - 1]) || chars[scan - 1] == '.') {
                scan -= 1;
            }
            let q: String = chars[scan..start - 1].iter().collect();
            let q = q.trim_start_matches('.').to_string();
            if !q.is_empty() && q != "_" { Some(q) } else { None }
        } else {
            // No dot-qualifier. Check if this looks like a named argument: `word = value`
            // (but NOT `word ==`). If so, scan backward for the enclosing call's name
            // and use that as the qualifier so we search the constructor/function's params.
            let after: String = chars[end..].iter().collect();
            let after_trimmed = after.trim_start();
            let is_named_arg = after_trimmed.starts_with('=')
                && !after_trimmed.starts_with("==");
            if is_named_arg {
                find_enclosing_call_name(&lines, position.line as usize, start)
                    .and_then(|callee| callee_to_qualifier(&callee))
            } else {
                None
            }
        };

        Some((word, qualifier))
    }

    /// If `name` at `position` is `it` or a named lambda parameter, return the
    /// inferred element/receiver type name (e.g. `"Product"`, `"User"`).
    ///
    /// Used by hover and go-to-definition to provide useful info for lambda params.
    /// Handles both same-line and multi-line lambda declarations by scanning
    /// backward through file lines (not just the text before the cursor).
    pub fn infer_lambda_param_type_at(
        &self,
        name:     &str,
        uri:      &Url,
        position: Position,
    ) -> Option<String> {
        let line_no = position.line as usize;

        // Prefer live_lines (current editor content, updated synchronously on
        // did_change) over files.lines (refreshed after debounced reindex).
        // Type resolution still uses the index (definitions, files) by name —
        // that data remains valid even before reindex completes.
        let lines: Arc<Vec<String>> = self.mem_lines_for(uri.as_str())?;

        if name == "it" || name == "this" {
            let pos = CursorPos { line: line_no, utf16_col: position.character as usize };
            let lambda_type = if name == "this" {
                find_this_element_type_in_lines(&lines, pos, self, uri)
            } else {
                find_it_element_type_in_lines(&lines, pos, self, uri)
            };
            if lambda_type.is_some() { return lambda_type; }
            // Type-directed fallback: if `it`/`this` is a call argument (named or
            // positional), look up the expected parameter type from the function signature.
            // Mimics Kotlin's type-directed implicit-receiver / lambda-param resolution.
            if let Some(ty) = find_as_call_arg_type(&lines, pos, self, uri) {
                return Some(ty);
            }
            // Fallback for `this` in a regular class method body (not a lambda):
            // scan backward for the enclosing class/object declaration.
            // Guard: if cursor is inside a receiver-lambda (apply/run/with) that just
            // failed to resolve its receiver type, `this` refers to that lambda's
            // receiver — not the enclosing class.  Returning enclosing_class_at here
            // would silently return the wrong type.
            if name == "this" {
                let pos2 = CursorPos { line: line_no, utf16_col: position.character as usize };
                if is_inside_receiver_lambda(&lines, pos2, self, uri) {
                    return None;
                }
                return self.enclosing_class_at(uri, position.line);
            }
            None
        } else {
            // For named params: scan backward for `{ name ->` pattern.
            // Also check the CURRENT line (needed when cursor is ON the param
            // at its declaration line, before `->` — before_cursor wouldn't
            // contain the arrow).
            find_named_lambda_param_type_in_lines(&lines, name, line_no, self, uri)
        }
    }

    /// Lambda parameter names that are **in scope** at `(cursor_line, cursor_col)`.
    ///
    /// Uses the same brace-depth backward-scan algorithm as
    /// `find_it_element_type_in_lines`: `}` increments depth, `{` decrements;
    /// when depth < 0 we've found an *enclosing* `{` lambda.  Sibling/inner lambdas
    /// whose closing `}` appears before their `{` in the backward scan self-balance
    /// and never trigger depth < 0, so they are correctly excluded.
    ///
    /// Example — cursor inside `{ resultState -> … }`:
    ///   `reloadableProduct(…, { isRefresh -> … }) { resultState -> │ }`
    ///   → returns `["resultState"]`,  NOT `["isRefresh", "resultState"]`
    pub fn lambda_params_at(&self, uri: &Url, cursor_line: usize) -> Vec<String> {
        self.lambda_params_at_col(uri, cursor_line, usize::MAX)
    }

    /// Like `lambda_params_at` but also respects `cursor_col` when scanning the
    /// cursor line.  Passing `usize::MAX` is equivalent to `lambda_params_at`.
    ///
    /// The column limit prevents the closing `}` of an inline lambda from being
    /// seen when the cursor is inside that lambda on the same line:
    ///   `loan = { loanId, isWustenrot -> setEvent(...) },`
    ///                                                  ^ cursor here
    /// Without the limit, the scan hits `}` first (depth→1), then `{` resets to 0
    /// (not <0), so the lambda params are never collected.
    pub fn lambda_params_at_col(&self, uri: &Url, cursor_line: usize, cursor_col: usize) -> Vec<String> {
        // ── CST fast path ────────────────────────────────────────────────────
        if let Some(doc) = self.live_doc(uri) {
            let line_text = self.live_lines.get(uri.as_str())
                .and_then(|ll| ll.get(cursor_line).cloned())
                .unwrap_or_default();
            let byte_col   = crate::indexer::live_tree::utf16_col_to_byte(&line_text, cursor_col);
            let point      = Point { row: cursor_line, column: byte_col };
            if let Some(node) = doc.tree.root_node().descendant_for_point_range(point, point) {
                let mut params: Vec<String> = Vec::new();
                let mut cur = node;
                loop {
                    if cur.kind() == KIND_LAMBDA_LIT {
                        let new_names = cur.collect_lambda_param_names(&doc.bytes, &params);
                        params.extend(new_names);
                    }
                    let Some(p) = cur.parent() else { break; };
                    cur = p;
                }
                return params;
            }
        }

        // ── Text fallback ────────────────────────────────────────────────────
        let lines = self.live_lines.get(uri.as_str())
            .map(|ll| ll.clone())
            .or_else(|| self.files.get(uri.as_str()).map(|f| f.lines.clone()))
            .unwrap_or_default();

        let mut params: Vec<String> = Vec::new();
        let mut depth: i32 = 0;
        let scan_start = cursor_line.saturating_sub(SCOPE_SCAN_BACK_LINES);

        for ln in (scan_start..=cursor_line).rev() {
            let line = match lines.get(ln) { Some(l) => l, None => continue };
            let scan_line: &str = if ln == cursor_line && cursor_col < line.len() {
                let mut utf16 = 0usize;
                let mut byte_end = line.len();
                for (bi, ch) in line.char_indices() {
                    if utf16 >= cursor_col { byte_end = bi; break; }
                    utf16 += ch.len_utf16();
                }
                &line[..byte_end]
            } else {
                line
            };
            for (bi, ch) in scan_line.char_indices().rev() {
                match ch {
                    '}' => depth += 1,
                    '{' => {
                        depth -= 1;
                        if depth < 0 {
                            if line[..bi].ends_with('$') { depth = 0; continue; }
                            let after = line[bi + 1..].trim_start();
                            if let Some(arrow_pos) = after.find("->") {
                                let names_str = &after[..arrow_pos];
                                for tok in names_str.split(',') {
                                    let name = tok.trim().ident_prefix();
                                    if !name.is_empty() && name != "it" && name != "_"
                                        && name.starts_with_lowercase()
                                        && !params.contains(&name) { params.push(name.clone()); }
                                }
                            }
                            depth = 0;
                        }
                    }
                    _ => {}
                }
            }
        }
        params
    }

    /// Find the `{ name ->` declaration line for a lambda parameter in scope at
    /// `cursor_line`.  Returns a `Location` pointing to the opening `{` of the
    /// enclosing lambda (the parameter's declaration site).
    pub fn find_lambda_param_decl(&self, uri: &Url, param_name: &str, cursor_line: usize) -> Option<Location> {
        let lines = self.live_lines.get(uri.as_str())
            .map(|ll| ll.clone())
            .or_else(|| self.files.get(uri.as_str()).map(|f| f.lines.clone()))?;

        let scan_start = cursor_line.saturating_sub(SCOPE_SCAN_BACK_LINES);
        for ln in (scan_start..=cursor_line).rev() {
            let line = match lines.get(ln) { Some(l) => l, None => continue };
            if !line_has_lambda_param(line, param_name) { continue; }
            if let Some(brace_pos) = lambda_brace_pos_for_param(line, param_name) {
                let char_col = line[..brace_pos].chars().count() as u32;
                return Some(Location {
                    uri: uri.clone(),
                    range: tower_lsp::lsp_types::Range {
                        start: tower_lsp::lsp_types::Position { line: ln as u32, character: char_col },
                        end:   tower_lsp::lsp_types::Position { line: ln as u32, character: char_col + 1 },
                    },
                });
            }
        }
        None
    }

    /// Find the name of the innermost enclosing class/interface/object
    /// that contains `row` in the given file.
    ///
    /// Used by `references` to scope a short symbol name (e.g. `Loading`) to
    /// its parent sealed class so we can filter out unrelated `Loading` classes
    /// in other sealed hierarchies.
    pub fn enclosing_class_at(&self, uri: &Url, row: u32) -> Option<String> {
        let row = row as usize;

        // ── CST fast path ────────────────────────────────────────────────────
        if let Some(doc) = self.live_doc(uri) {
            // Use the first non-whitespace byte on the row as the probe column.
            let probe_col = self.live_lines.get(uri.as_str())
                .and_then(|ll| ll.get(row).cloned())
                .map(|l| l.len() - l.trim_start().len())
                .unwrap_or(0);
            let point = Point { row, column: probe_col };
            if let Some(node) = doc.tree.root_node().descendant_for_point_range(point, point) {
                let mut cur = node;
                loop {
                    match cur.kind() {
                        "class_declaration" | "interface_declaration"
                        | "object_declaration" | "companion_object" => {
                            // Preserve existing semantics: exclude the node if its
                            // declaration starts on the query row (cursor is on the
                            // class's own declaration line).
                            if cur.start_position().row < row {
                                if let Some(name) = cur.extract_type_name(&doc.bytes) {
                                    return Some(name);
                                }
                            }
                        }
                        _ => {}
                    }
                    match cur.parent() {
                        Some(p) => cur = p,
                        None    => break,
                    }
                }
            }
        }

        // ── Text fallback ────────────────────────────────────────────────────
        let file = self.files.get(uri.as_str())?;
        let mut depth = 0i32;
        let end = row.min(file.lines.len().saturating_sub(1));
        for i in (0..=end).rev() {
            let line = match file.lines.get(i) { Some(l) => l, None => continue };
            for ch in line.chars().rev() {
                match ch { '}' => depth += 1, '{' => depth -= 1, _ => {} }
            }
            if depth < 0 && i < row {
                let t = line.trim();
                if let Some(name) = extract_class_decl_name(t) { return Some(name); }
                let scan_up = i.saturating_sub(DECL_SCAN_UP_LINES);
                for j in (scan_up..i).rev() {
                    if let Some(prev) = file.lines.get(j) {
                        if let Some(name) = extract_class_decl_name(prev.trim()) { return Some(name); }
                        let pt = prev.trim();
                        if pt.starts_with('}') || pt.ends_with('}') { break; }
                    }
                }
                depth = 0;
            }
        }
        None
    }
}

/// Thin wrapper around [`NodeExt::collect_lambda_param_names`] for `super::` access
/// in the companion test module.
#[cfg(test)]
fn collect_lambda_param_names(
    lambda_node: tree_sitter::Node<'_>,
    bytes:       &[u8],
    existing:    &[String],
) -> Vec<String> {
    lambda_node.collect_lambda_param_names(bytes, existing)
}

/// If `line` is a class/interface/object/sealed declaration, return the type name.
pub(super) fn extract_class_decl_name(line: &str) -> Option<String> {
    // Strip common modifiers: Kotlin + Java + Swift
    let mut rest = line;
    let modifiers = [
        "abstract ", "sealed ", "data ", "open ", "inner ", "private ",
        "protected ", "public ", "internal ", "inline ", "value ", "enum ",
        "companion ", "override ", "final ",
        // Swift-specific
        "fileprivate ", "@objc ", "static ", "final ",
    ];
    loop {
        let before = rest;
        for m in &modifiers { rest = rest.strip_prefix(m).unwrap_or(rest).trim_start(); }
        // Skip @Annotations (Kotlin) and @attributes (Swift)
        if rest.starts_with('@') {
            if let Some(after) = rest.find(' ') { rest = rest[after..].trim_start(); }
        }
        if rest == before { break; }
    }
    // Now rest should start with a type keyword
    let rest = rest.strip_prefix("class ")
        .or_else(|| rest.strip_prefix("interface "))
        .or_else(|| rest.strip_prefix("object "))
        .or_else(|| rest.strip_prefix("struct "))
        .or_else(|| rest.strip_prefix("protocol "))
        .or_else(|| rest.strip_prefix("extension "))?;
    // Extract the identifier
    let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    if name.is_empty() || !name.starts_with_uppercase() {
        return None;
    }
    Some(name)
}

pub(crate) fn is_id_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Return the trailing contiguous identifier slice in `s` — the longest
/// suffix whose characters all satisfy `is_id_char`.  Returns `""` if none.
///
/// Example: `last_ident_in("foo.barBaz")` → `"barBaz"`
pub(crate) fn last_ident_in(s: &str) -> &str {
    let ident_bytes: usize = s.chars().rev()
        .take_while(|&c| is_id_char(c))
        .map(|c| c.len_utf8())
        .sum();
    &s[s.len() - ident_bytes..]
}

/// Scan backward from `(line_no, col)` — where `col` is the START of the cursor
/// word — to find the name of the enclosing function/constructor call.
///
/// Used to resolve named arguments: `User(name = "Alice")` with cursor on `name`
/// → scan back past the `(` → return `"User"`.
///
/// Returns the FULL dotted callee name (e.g. `"BottomSheetState.empty"`, `"User"`).
/// The caller converts this to a qualifier via `callee_to_qualifier`.
///
/// Scans at most 20 lines backward to avoid runaway on deeply nested expressions.
/// Tracks `()` and `[]` depth; lambda `{}` bodies are transparent (their inner
/// `()` still balance) so we don't need special-case brace handling.
pub(crate) fn find_enclosing_call_name(lines: &[String], line_no: usize, col: usize) -> Option<String> {
    let mut depth: i32 = 0;
    let scan_range_start = line_no.saturating_sub(ENCLOSING_CALL_SCAN_BACK);

    for ln in (scan_range_start..=line_no).rev() {
        let line_chars: Vec<char> = lines[ln].chars().collect();
        let scan_to = if ln == line_no { col } else { line_chars.len() };

        for i in (0..scan_to).rev() {
            match line_chars[i] {
                ')' | ']' => depth += 1,
                '(' | '[' => {
                    depth -= 1;
                    if depth < 0 {
                        // This `(` opened the call we're inside.
                        if i == 0 { return None; }
                        // Extract the identifier (possibly dotted) just before `(`.
                        let mut end = i;
                        while end > 0 && (is_id_char(line_chars[end - 1]) || line_chars[end - 1] == '.') {
                            end -= 1;
                        }
                        if end >= i { return None; }
                        let name: String = line_chars[end..i].iter().collect();
                        let name = name.trim_matches('.').to_string();
                        return if name.is_empty() { None } else { Some(name) };
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Convert a raw callee name (from `find_enclosing_call_name`) to the qualifier
/// to use when resolving a named argument parameter.
///
/// Rules:
/// - Last segment uppercase → constructor call, qualifier = last segment.
///   `"User"` → `"User"`, `"com.example.User"` → `"User"`
/// - Last segment lowercase (method call) → look for the rightmost uppercase
///   segment in the receiver chain as the owner type.
///   `"BottomSheetState.empty"` → `"BottomSheetState"`
///   `"SomeClass.companion.build"` → `"SomeClass"` (last uppercase before method)
/// - Pure lowercase, no uppercase anywhere → `None` (can't resolve statically).
fn callee_to_qualifier(full_callee: &str) -> Option<String> {
    let segments: Vec<&str> = full_callee.split('.').collect();
    let last = *segments.last()?;

    // Constructor call: last segment is a type name (uppercase first char).
    if last.starts_with_uppercase() {
        return Some(last.to_string());
    }

    // Method call: find rightmost uppercase segment in the receiver chain.
    // `BottomSheetState.empty` → segments[..-1] = ["BottomSheetState"] → "BottomSheetState"
    // `viewModel.state.copy`   → no uppercase in receiver → None
    let receiver = &segments[..segments.len() - 1];
    receiver.iter().rev()
        .find(|s| s.starts_with_uppercase())
        .map(|s| s.to_string())
}

#[cfg(test)]
#[path = "scope_tests.rs"]
mod scope_tests;
