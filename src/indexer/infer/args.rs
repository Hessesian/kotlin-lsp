//! Pure call-argument parsing helpers for the Kotlin indexer.
//!
//! All public items are `pub(crate)`.  No hidden state: functions accept pure
//! string/slice inputs or an immutable `&Indexer` reference for look-ups.

use tower_lsp::lsp_types::Url;

use crate::indexer::{Indexer, is_id_char, find_enclosing_call_name};
use crate::indexer::infer::sig::{find_fun_signature_full, nth_fun_param_type_str};

// ─── Type-directed call-argument inference ────────────────────────────────────

/// Type-directed inference for `it` or `this` used as a call argument.
///
/// When `it`/`this` appears as an argument to a function — either as a **named arg**
/// (`param = it`) or as a **positional arg** (`fn(a, it)`) — look up the expected
/// parameter type and return it as the hint.
///
/// This mimics Kotlin's implicit-receiver / lambda-param resolution by type:
/// the compiler picks the in-scope `it` or `this` whose type satisfies the
/// expected parameter type.
///
/// Examples:
///   `.send(channel = this)` → `channel: SendChannel<...>` → `SendChannel`
///   `process(it)`           → first param of `process` → e.g. `Item`
///   `fn(a, it)`             → second param of `fn` → e.g. `String`
pub(crate) fn find_as_call_arg_type(
    lines:       &[String],
    cursor_line: usize,
    cursor_col:  usize,
    idx:         &Indexer,
    uri:         &Url,
) -> Option<String> {
    let line = lines.get(cursor_line)?;
    // Slice the line up to (but not including) the cursor position.
    let before_cursor = {
        let mut byte_end = line.len();
        let mut utf16 = 0usize;
        for (bi, ch) in line.char_indices() {
            if utf16 >= cursor_col { byte_end = bi; break; }
            utf16 += ch.len_utf16();
        }
        &line[..byte_end]
    };
    let col = before_cursor.chars().count();

    // ── Named arg: `param = ` just before cursor ─────────────────────────────
    let s = before_cursor.trim_end();
    if let Some(s) = s.strip_suffix('=') {
        if !s.ends_with(|c: char| "!<>=".contains(c)) {
            let s = s.trim_end();
            let ident_start = s.rfind(|c: char| !c.is_alphanumeric() && c != '_')
                .map(|i| i + 1).unwrap_or(0);
            let named_arg = &s[ident_start..];
            if !named_arg.is_empty()
                && named_arg.chars().next().map(|c| !c.is_uppercase()).unwrap_or(false)
            {
                let preceding = s[..ident_start].trim_end().chars().last();
                if matches!(preceding, Some('(') | Some(',')) {
                    if let Some(fn_full) = find_enclosing_call_name(lines, cursor_line, col) {
                        if let Some(fn_name) = fn_full.split('.').next_back().filter(|n| !n.is_empty()) {
                            if let Some(sig) = find_fun_signature_full(fn_name, idx, uri) {
                                if let Some(param_type) = find_named_param_type_in_sig(&sig, named_arg) {
                                    let base: String = param_type.trim()
                                        .chars().take_while(|&c| is_id_char(c)).collect();
                                    if !base.is_empty() { return Some(base); }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Positional arg: `fn(a, keyword)` ────────────────────────────────────
    // Scan backward tracking paren/bracket depth; count top-level commas to
    // determine which argument position the cursor is in.
    //
    // Also track brace depth: if we encounter an unmatched `{` going backward,
    // the cursor is inside a nested lambda body — NOT directly a function arg.
    // Stop immediately so we don't mis-infer the outer function's param type.
    let mut depth: i32 = 0;
    let mut brace_depth: i32 = 0;
    let mut arg_pos: usize = 0;
    let scan_start = cursor_line.saturating_sub(20);

    for ln in (scan_start..=cursor_line).rev() {
        let chars: Vec<char> = lines[ln].chars().collect();
        let scan_to = if ln == cursor_line { col.min(chars.len()) } else { chars.len() };

        for i in (0..scan_to).rev() {
            match chars[i] {
                // Skip string interpolation `${`: treat `{` preceded by `$` as neutral.
                '{' if i > 0 && chars[i - 1] == '$' => {}
                '}' => brace_depth += 1,
                '{' => {
                    brace_depth -= 1;
                    if brace_depth < 0 {
                        // Cursor is inside a lambda body — do not match the outer call.
                        return None;
                    }
                }
                ')' | ']' => depth += 1,
                // `>` going backward = entering a generic block; guard against `->`.
                '>' if i == 0 || chars[i - 1] != '-' => depth += 1,
                '(' | '[' => {
                    depth -= 1;
                    if depth < 0 {
                        if i == 0 { return None; }
                        // Extract function name (possibly dotted) before `(`.
                        let mut end = i;
                        while end > 0 && (is_id_char(chars[end - 1]) || chars[end - 1] == '.') {
                            end -= 1;
                        }
                        if end >= i { return None; }
                        let full_name: String = chars[end..i].iter().collect();
                        let fn_name = full_name.trim_matches('.')
                            .split('.').next_back().filter(|n| !n.is_empty())?;
                        let sig = find_fun_signature_full(fn_name, idx, uri)?;
                        let param_type = nth_fun_param_type_str(&sig, arg_pos)?;
                        let base: String = param_type.trim()
                            .chars().take_while(|&c| is_id_char(c)).collect();
                        return if base.is_empty() { None } else { Some(base) };
                    }
                }
                // `<` going backward = leaving a generic block; only close if inside one.
                '<' if depth > 0 => depth -= 1,
                ',' if depth == 0 => arg_pos += 1,
                _ => {}
            }
        }
    }
    None
}

// ─── Named-arg helpers ────────────────────────────────────────────────────────

/// Detect the `IDENT =` named-arg pattern at the end of `before_brace`.
/// Returns the identifier if found (must be lowercase-first, not `!=`, `<=`, `>=`).
///
/// Also requires that the text BEFORE the identifier is only whitespace (or
/// comma + whitespace for same-line multi-arg calls), so that patterns like
/// `(isRefresh = { resultState ->` are NOT falsely matched as named args
/// (the `(` before `isRefresh` disqualifies it).
pub(crate) fn extract_named_arg_name(before_brace: &str) -> Option<&str> {
    let s = before_brace.trim_end();
    let s = s.strip_suffix('=')?;
    // Guard against `!=`, `<=`, `>=`, `==`
    if s.ends_with(|c: char| "!<>=".contains(c)) { return None; }
    let s = s.trim_end();
    // Extract trailing identifier
    let ident_start = s.rfind(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|i| i + 1)
        .unwrap_or(0);
    let ident = &s[ident_start..];
    if ident.is_empty() { return None; }
    // Named args start with a lowercase letter
    if ident.chars().next().map(|c| c.is_uppercase()).unwrap_or(true) { return None; }
    // Require the prefix to be only whitespace (optionally preceded by a comma).
    // This prevents `(isRefresh = {` from matching — the `(` before `isRefresh`
    // makes the prefix non-empty after stripping commas and whitespace.
    let prefix = s[..ident_start].trim_start().trim_start_matches(',').trim_start();
    if !prefix.is_empty() { return None; }
    Some(ident)
}

/// Find the type string of a named parameter `param_name` inside a
/// comma-separated parameter list text (output of `collect_fun_params_text`).
///
/// Handles `val`/`var` prefixes, strips them. Returns the full type string
/// (may be a functional type like `(String, Boolean) -> Unit`).
pub(crate) fn find_named_param_type_in_sig(sig: &str, param_name: &str) -> Option<String> {
    // Split by comma at depth 0, tracking `()`, `[]`, and `<>`.
    // The `>` in `->` must NOT decrement `<>` depth — skip it when prev char is `-`.
    let mut parts: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    let mut prev = '\0';
    for (i, ch) in sig.char_indices() {
        match ch {
            '(' | '[' | '<' => depth += 1,
            ')' | ']' => depth -= 1,
            '>' if prev != '-' && depth > 0 => depth -= 1,
            ',' if depth == 0 => { parts.push(&sig[start..i]); start = i + 1; }
            _ => {}
        }
        prev = ch;
    }
    if start < sig.len() { parts.push(&sig[start..]); }

    let colon_pat = format!("{param_name}:");
    for part in parts {
        let part = part.trim().trim_start_matches("val ").trim_start_matches("var ");
        // Exact param_name match (no suffix)
        let Some(col_pos) = part.find(&colon_pat) else { continue };
        let before = &part[..col_pos];
        if before.chars().last().map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false) {
            continue; // suffix match like `otherParam:`
        }
        let after = part[col_pos + colon_pat.len()..].trim();
        if !after.is_empty() { return Some(after.to_owned()); }
    }
    None
}

// ─── Lambda parameter helpers ─────────────────────────────────────────────────

/// 0-based index of `param_name` in a multi-param lambda opening `{ a, b, c ->`.
/// Returns 0 for single-param lambdas.
pub(crate) fn lambda_param_position_on_line(line: &str, param_name: &str) -> usize {
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find("->") {
        let arrow_pos = search_from + rel;
        if let Some(brace_pos) = line[..arrow_pos].rfind('{') {
            let names_str = &line[brace_pos + 1..arrow_pos];
            for (i, tok) in names_str.split(',').enumerate() {
                let tok = tok.trim();
                let n: String = tok.chars().take_while(|&c| c.is_alphanumeric() || c == '_').collect();
                if n == param_name { return i; }
            }
        }
        search_from = arrow_pos + 2;
    }
    0
}

/// Returns `true` if `after_open_brace` looks like the opening of an explicitly
/// named parameter lambda — single-param `{ name ->` or multi-param `{ a, b ->`.
///
/// Handles multi-param correctly by finding `->` via a depth-aware scan
/// (not just checking whether the text after the first word starts with `->`).
///
/// Returns `false` for:
///   - `{ it }`               — implicit single param
///   - `{ }` / `{`            — empty / block
///   - `{ setEvent(...)` }    — starts with a function call
pub(crate) fn has_named_params_not_it(after_open_brace: &str) -> bool {
    let s = after_open_brace.trim_start();
    // Find the first `->` at brace-depth 0 (ignoring `->` inside nested lambdas).
    let mut depth: i32 = 0;
    let bytes = s.as_bytes();
    let mut arrow_pos: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => { depth += 1; i += 1; }
            b'}' => { depth -= 1; i += 1; }
            b'-' if depth == 0 && i + 1 < bytes.len() && bytes[i + 1] == b'>' => {
                arrow_pos = Some(i); break;
            }
            _ => { i += 1; }
        }
    }
    let Some(ap) = arrow_pos else { return false; };
    let before_arrow = s[..ap].trim_end();
    // All tokens before `->` must be valid identifiers.
    // If any non-`it`, non-`_` identifier is present, it's a named-param lambda.
    for tok in before_arrow.split(',') {
        let tok = tok.trim();
        let name: String = tok.chars()
            .take_while(|&c| c.is_alphanumeric() || c == '_')
            .collect();
        if !name.is_empty() && name != "it" && name != "_" {
            return true;
        }
    }
    false
}

// ─── First-arg extractor ──────────────────────────────────────────────────────

/// Extract the first argument from a call expression string.
///
/// `"with(user)"` → `Some("user")`
/// `"fn()"` → `None`
pub(crate) fn extract_first_arg(call_expr: &str) -> Option<&str> {
    let paren = call_expr.find('(')?;
    let rest = &call_expr[paren + 1..];
    let mut depth: i32 = 0;
    let mut end = rest.len();
    let mut prev = '\0';
    for (i, ch) in rest.char_indices() {
        match ch {
            '(' | '<' | '[' => depth += 1,
            ')' | ']' => { if depth == 0 { end = i; break; } depth -= 1; }
            // Skip the `>` in `->` (lambda arrow) and never go negative.
            '>' if prev != '-' && depth > 0 => depth -= 1,
            ',' if depth == 0 => { end = i; break; }
            _ => {}
        }
        prev = ch;
    }
    let arg = rest[..end].trim();
    if arg.is_empty() { None } else { Some(arg) }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "args_tests.rs"]
mod tests;
