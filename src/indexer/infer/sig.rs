//! Pure signature-extraction helpers for the Kotlin indexer.
//!
//! All public items are `pub(crate)`.  Most functions take pure string/slice
//! inputs or an immutable `&Indexer` reference for index look-ups.  Note that
//! `find_fun_signature_full` may trigger on-demand indexing via interior
//! mutability (`index_content`) and perform disk reads when a symbol is not yet
//! in the in-memory index.

use tower_lsp::lsp_types::{SymbolKind, Url};

use crate::indexer::Indexer;

// ─── Multiline signature collector ───────────────────────────────────────────

/// Collect a human-readable function/class signature starting at `start_line`.
///
/// Rules:
/// - Track `(` / `)` depth.
/// - Once depth is back to 0, the signature ends at the current line.
/// - A line ending with `{` signals the start of the body — strip the `{`
///   and stop (we don't want the body in the hover).
/// - Lines ending with `,` inside balanced parens always continue.
/// - Cap at 15 lines to avoid runaway on pathological files.
pub(crate) fn collect_signature(lines: &[String], start_line: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut depth: i32 = 0;

    for raw_line in lines[start_line..(start_line + 15).min(lines.len())].iter() {
        let raw = raw_line.trim();

        // Count parens in this line.
        for ch in raw.chars() {
            match ch { '(' => depth += 1, ')' => depth -= 1, _ => {} }
        }

        if raw.ends_with('{') {
            // Body starts — include the line without the brace (shows inheritance).
            let trimmed = raw.trim_end_matches('{').trim_end();
            if !trimmed.is_empty() { parts.push(trimmed.to_owned()); }
            break;
        }

        parts.push(raw.to_owned());

        // Signature ends when parens are balanced and the line doesn't
        // look like a continuation (trailing comma means more params follow).
        if depth <= 0 && !raw.ends_with(',') {
            break;
        }
    }

    parts.join("\n")
}

// ─── Parameter-list extraction ────────────────────────────────────────────────

/// Collect the full parameter-list text for a function named `fn_name`.
/// Fast path only — no rg, no disk I/O, no index mutations.
/// Used by signature help (fires on every keystroke).
fn find_fun_signature(fn_name: &str, idx: &Indexer, uri: &Url) -> Option<String> {
    // 1. Import-aware resolution using only already-indexed files (no rg/disk).
    let locs = crate::resolver::resolve_symbol_no_rg(idx, fn_name, uri);
    for loc in &locs {
        let file_uri_str = loc.uri.as_str();
        if let Some(data) = idx.files.get(file_uri_str) {
            let start_line = loc.range.start.line as usize;
            if let Some(sig) = collect_params_from_line(&data.lines, start_line) {
                return Some(sig);
            }
        }
    }

    // 2. Fallback: current file → all already-indexed files (name-only scan).
    if let Some(sig) = collect_fun_params_text(fn_name, uri.as_str(), idx) {
        return Some(sig);
    }
    for entry in idx.files.iter() {
        if entry.key() == uri.as_str() { continue; }
        if let Some(sig) = collect_fun_params_text(fn_name, entry.key(), idx) {
            return Some(sig);
        }
    }
    None
}

/// Full signature lookup including rg + on-demand indexing.
/// Used by hover and lambda type inference where latency is acceptable.
pub(crate) fn find_fun_signature_full(fn_name: &str, idx: &Indexer, uri: &Url) -> Option<String> {
    if let Some(sig) = find_fun_signature(fn_name, idx, uri) {
        return Some(sig);
    }
    // Slow path: rg to locate the definition, index on-demand.
    let root = idx.workspace_root.read().unwrap().clone();
    let matcher = idx.ignore_matcher.read().unwrap().clone();
    let locs = crate::rg::rg_find_definition(fn_name, root.as_deref(), matcher.as_deref());
    for loc in &locs {
        let file_uri_str = loc.uri.as_str();
        if !idx.files.contains_key(file_uri_str) {
            if let Ok(path) = loc.uri.to_file_path() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    idx.index_content(&loc.uri, &content);
                }
            }
        }
        if let Some(sig) = collect_fun_params_text(fn_name, file_uri_str, idx) {
            return Some(sig);
        }
    }
    None
}

/// Collect everything between the outer `(…)` of a function's parameter list.
/// Scans the symbol's start line and up to 20 following lines.
/// Matches both top-level `fun` (FUNCTION) and class methods (METHOD).
pub(crate) fn collect_fun_params_text(fn_name: &str, uri_str: &str, idx: &Indexer) -> Option<String> {
    collect_all_fun_params_texts(fn_name, uri_str, idx).into_iter().next()
}

/// Like `collect_fun_params_text` but returns ALL params texts for every symbol
/// named `fn_name` in the file (a file may have multiple same-named nested classes).
pub(crate) fn collect_all_fun_params_texts(fn_name: &str, uri_str: &str, idx: &Indexer) -> Vec<String> {
    let data = match idx.files.get(uri_str) { Some(d) => d, None => return vec![] };
    let start_lines: Vec<usize> = data.symbols.iter()
        .filter(|s| s.name == fn_name
               && (s.kind == SymbolKind::FUNCTION
                   || s.kind == SymbolKind::METHOD
                   || s.kind == SymbolKind::CLASS
                   || s.kind == SymbolKind::STRUCT))  // data class → STRUCT
        .map(|s| s.range.start.line as usize)
        .collect();

    start_lines.into_iter()
        .filter_map(|start_line| collect_params_from_line(&data.lines, start_line))
        .collect()
}

/// Walk forward from `start_line`, accumulating characters until the outermost
/// `)` closes — that ends the parameter list.
///
/// We only track `()` depth (NOT `<>`) to avoid false-triggers on `->` arrows.
pub(crate) fn collect_params_from_line(lines: &[String], start_line: usize) -> Option<String> {
    let mut paren_depth: i32 = 0;
    let mut found_open = false;
    let mut params = String::new();

    'outer: for ln in start_line..start_line + 20 {
        let line = match lines.get(ln) { Some(l) => l, None => break };
        let chars = line.char_indices().peekable();
        for (_, ch) in chars {
            match ch {
                '(' => {
                    paren_depth += 1;
                    if paren_depth == 1 { found_open = true; continue; }
                    if found_open { params.push(ch); }
                }
                ')' => {
                    paren_depth -= 1;
                    if found_open && paren_depth == 0 { break 'outer; }
                    if found_open { params.push(ch); }
                }
                _ if found_open => params.push(ch),
                _ => {}
            }
        }
        if found_open { params.push('\n'); }
    }

    if params.is_empty() { None } else { Some(params) }
}

// ─── Parameter type accessors ─────────────────────────────────────────────────

/// Split the flattened parameter list by `,` at depth-0 (respecting `()`, `<>`).
/// Returns the type string of the parameter at position `n` (0-based).
/// Falls back to the last parameter if `n` is out of range.
///
/// NOTE: `->` in Kotlin functional types (e.g. `(Boolean) -> Flow<T>`) contains
/// `>` which would falsely decrement `<>` depth.  We skip the `>` of any `->` by
/// tracking the previous character.
pub(crate) fn nth_fun_param_type_str(params_text: &str, n: usize) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    let mut prev = '\0';
    for (i, ch) in params_text.char_indices() {
        match ch {
            '(' | '<' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            // Skip `>` of `->` and guard against going negative on bare `>` operators.
            '>' if prev != '-' && depth > 0 => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&params_text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        prev = ch;
    }
    parts.push(&params_text[start..]);
    // Drop trailing-comma empty parts (Kotlin allows `fun f(a: A, b: B,) {}`).
    parts.retain(|p| !p.trim().is_empty());
    if parts.is_empty() { return None; }

    let param = parts.get(n).unwrap_or_else(|| parts.last().unwrap()).trim();
    // Strip leading non-identifier characters (annotations, whitespace).
    let param = param.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_');
    let colon = param.find(':')?;
    Some(param[colon + 1..].trim().to_owned())
}

/// Return the type string of the last parameter in `params_text`.
pub(crate) fn last_fun_param_type_str(params_text: &str) -> Option<String> {
    // Count top-level parameters (same `->` skip logic as nth_fun_param_type_str).
    let count = {
        let mut n = 1usize;
        let mut depth: i32 = 0;
        let mut prev = '\0';
        for ch in params_text.chars() {
            match ch {
                '(' | '<' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                '>' if prev != '-' && depth > 0 => depth -= 1,
                ',' if depth == 0 => n += 1,
                _ => {}
            }
            prev = ch;
        }
        n
    };
    nth_fun_param_type_str(params_text, count.saturating_sub(1))
}

// ─── Pure string helper ───────────────────────────────────────────────────────

/// Strip a balanced trailing `(…)` argument list from the end of `s`.
///
/// `"collection.method(arg1, arg2)"` → `"collection.method"`
/// `"collection.forEach"`           → `"collection.forEach"`  (unchanged)
pub(crate) fn strip_trailing_call_args(s: &str) -> &str {
    if !s.ends_with(')') { return s; }
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 { return &s[..i]; }
            }
            _ => {}
        }
    }
    s
}

// ─── Receiver-aware signature lookup ─────────────────────────────────────────

/// Signature lookup with optional dot-receiver context.
///
/// When `receiver` is given (e.g. `"oneYearOlderInteractor"`), resolves its
/// type, finds that type's file, and looks up `name` there specifically.
/// Falls back to the plain name-based search if receiver resolution fails.
///
/// This is the free-function form of the former `Indexer::find_fun_signature_with_receiver`
/// method.  `backend.rs` calls this directly.
pub(crate) fn find_fun_signature_with_receiver(
    idx:      &Indexer,
    uri:      &Url,
    name:     &str,
    receiver: Option<&str>,
) -> String {
    if let Some(recv) = receiver {
        // Infer the receiver's type and resolve to its file.
        if let Some(type_name) = crate::resolver::infer_variable_type_raw(idx, recv, uri) {
            let outer = type_name.split('.').next().unwrap_or(&type_name);
            let locs = crate::resolver::resolve_symbol(idx, outer, None, uri);
            for loc in &locs {
                if let Some(data) = idx.files.get(loc.uri.as_str()) {
                    if let Some(sig) = collect_fun_params_text(name, loc.uri.as_str(), idx) {
                        return sig;
                    }
                    // Also search by line range within the type's body.
                    let type_end = data.symbols.iter()
                        .find(|s| s.name == outer)
                        .map(|s| s.range.end.line)
                        .unwrap_or(u32::MAX);
                    for sym in data.symbols.iter()
                        .filter(|s| s.name == name && s.range.start.line <= type_end)
                    {
                        if let Some(sig) = collect_params_from_line(
                            &data.lines,
                            sym.range.start.line as usize,
                        ) {
                            return sig;
                        }
                    }
                }
            }
        }
    }
    find_fun_signature_full(name, idx, uri).unwrap_or_default()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "sig_tests.rs"]
mod tests;
