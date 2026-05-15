//! Pure signature-extraction helpers for the Kotlin indexer.
//!
//! All public items are `pub(crate)`.  Most functions take pure string/slice
//! inputs or an immutable `&Indexer` reference for index look-ups.  Note that
//! `find_fun_signature_full` may trigger on-demand indexing via interior
//! mutability (`index_content`) and perform disk reads when a symbol is not yet
//! in the in-memory index.

use tower_lsp::lsp_types::{Range, SymbolKind, Url};

use crate::indexer::Indexer;
use crate::resolver::{infer_receiver_type, ReceiverKind};
use crate::types::SourceSet;

// ─── Call-site resolution types ──────────────────────────────────────────────

/// The scope in which a function call is being resolved.
///
/// `SameFile` resolution does not filter nested classes — calling `Foo()` in the
/// file where `class Outer { data class Foo(...) }` is defined is valid.
///
/// `CrossFile` resolution skips `CLASS`/`STRUCT` symbols whose `container` is
/// non-`None` — an unqualified `Foo()` in another file cannot reach `Outer.Foo`
/// without a qualifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolutionScope {
    SameFile,
    CrossFile,
}

/// Everything needed to resolve a call expression's signature.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CallSite<'a> {
    /// The bare function / constructor name (without qualifier).
    pub name: &'a str,
    /// The receiver variable name, if any (`foo.bar()` → `Some("foo")`).
    pub qualifier: Option<&'a str>,
    /// URI of the file containing the call.
    pub caller_uri: &'a Url,
}

/// Result of resolving a call site's signature.
#[derive(Debug)]
pub(crate) enum SignatureResult {
    /// Exactly one arity envelope — safe to emit a diagnostic.
    Unique {
        params_text: String,
        param_counts: (usize, usize),
    },
    /// Multiple distinct arity envelopes found — overloaded, skip.
    Overloaded,
    /// No definition found — skip.
    NotFound,
    /// Qualified call whose receiver type could not be resolved — skip.
    UnresolvableReceiver,
}

// ─── Multiline signature collector ───────────────────────────────────────────

/// Maximum number of lines to scan when collecting a multi-line function signature.
const SIGNATURE_SCAN_LINES: usize = 15;

/// Maximum number of lines to scan when collecting a function parameter list.
const FUN_PARAMS_SCAN_LINES: usize = 20;

// ─── Import reachability ─────────────────────────────────────────────────────

/// Check whether symbols from `def_uri` are reachable from `caller_uri`.
///
/// A definition file is reachable when any of the following hold:
/// - Both files share the same package declaration (same-package access).
/// - The caller has an explicit import `package.SymbolName`.
/// - The caller has a star import `package.*` that covers the definition's package.
///
/// This is best-effort, not sound — we have no full classpath.  When
/// reachability cannot be determined (missing `FileData`, no package info) we
/// return `true` to avoid false negatives.
pub(crate) fn is_import_reachable(
    idx: &Indexer,
    caller_uri: &str,
    def_uri: &str,
    symbol_name: &str,
) -> bool {
    if caller_uri == def_uri {
        return true;
    }
    let Some(caller_data) = idx.files.get(caller_uri) else {
        return true; // fail-open
    };
    let Some(def_data) = idx.files.get(def_uri) else {
        return true; // fail-open
    };
    let def_pkg = match def_data.package.as_deref() {
        Some(p) => p,
        None => return true, // no package info → can't filter
    };
    let caller_pkg = caller_data.package.as_deref().unwrap_or("");

    // Same package — always reachable
    if caller_pkg == def_pkg {
        return true;
    }

    for import in &caller_data.imports {
        if import.covers(def_pkg, symbol_name) {
            return true;
        }
    }

    false
}

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

    for raw_line in lines[start_line..(start_line + SIGNATURE_SCAN_LINES).min(lines.len())].iter() {
        let raw = raw_line.trim();

        // Count parens in this line.
        for ch in raw.chars() {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
        }

        if raw.ends_with('{') {
            // Body starts — include the line without the brace (shows inheritance).
            let trimmed = raw.trim_end_matches('{').trim_end();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_owned());
            }
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
    let locs = idx.resolve_symbol_no_rg(fn_name, uri);
    for loc in &locs {
        let file_uri_str = loc.uri.as_str();
        if let Some(data) = idx.files.get(file_uri_str) {
            let start_line = loc.range.start.line;
            if let Some(sym) = data.symbols.iter().find(|s| {
                s.name == fn_name
                    && s.range.start.line == start_line
                    && (s.kind == SymbolKind::FUNCTION || s.kind == SymbolKind::METHOD)
            }) {
                if let Some(params) = extract_params_from_detail(&sym.detail) {
                    return Some(params);
                }
            }
        }
    }

    // 2. Fallback: current file → all already-indexed files (name-only scan).
    if let Some(sig) = collect_fun_params_text(fn_name, uri.as_str(), idx) {
        return Some(sig);
    }
    for entry in idx.files.iter() {
        if entry.key() == uri.as_str() {
            continue;
        }
        if let Some(sig) = collect_fun_params_text(fn_name, entry.key(), idx) {
            return Some(sig);
        }
    }
    None
}

/// Full signature lookup including rg + on-demand indexing.
/// Used by hover and lambda type inference where latency is acceptable.
pub(crate) fn find_fun_signature_full(fn_name: &str, idx: &Indexer, uri: &Url) -> Option<String> {
    let cache_key = (fn_name.to_owned(), uri.to_string());
    if let Some(cached) = idx.sig_cache.get(&cache_key) {
        return cached.clone();
    }
    let result = find_fun_signature_full_uncached(fn_name, idx, uri);
    idx.sig_cache.insert(cache_key, result.clone());
    result
}

fn find_fun_signature_full_uncached(fn_name: &str, idx: &Indexer, uri: &Url) -> Option<String> {
    if let Some(sig) = find_fun_signature(fn_name, idx, uri) {
        return Some(sig);
    }
    // Slow path: rg to locate the definition, index on-demand.
    let open_file = uri.to_file_path().ok();
    let (root, source_roots, matcher) = idx.rg_scope_for_path(open_file.as_deref());
    let locs =
        crate::rg::rg_find_definition(fn_name, root.as_deref(), &source_roots, matcher.as_deref());
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
pub(crate) fn collect_fun_params_text(
    fn_name: &str,
    uri_str: &str,
    idx: &Indexer,
) -> Option<String> {
    collect_all_fun_params_texts(fn_name, uri_str, idx)
        .into_iter()
        .next()
}

/// Like `collect_fun_params_text` but returns ALL params texts for every symbol
/// named `fn_name` in the file (a file may have multiple same-named nested classes).
///
/// For Java files, CLASS symbols are excluded because Java class nodes never carry
/// constructor params — use the indexed CONSTRUCTOR symbols instead.
pub(crate) fn collect_all_fun_params_texts(
    fn_name: &str,
    uri_str: &str,
    idx: &Indexer,
) -> Vec<String> {
    let data = match idx.files.get(uri_str) {
        Some(d) => d,
        None => return vec![],
    };
    let is_java = uri_str.ends_with(".java");
    data.symbols
        .iter()
        .filter(|s| {
            s.name == fn_name
                && (s.kind == SymbolKind::FUNCTION
                    || s.kind == SymbolKind::METHOD
                    || s.kind == SymbolKind::CONSTRUCTOR
                    || s.kind == SymbolKind::STRUCT
                    || (s.kind == SymbolKind::CLASS && !is_java))
        })
        .filter_map(|s| {
            // Prefer pre-computed params from CST (populated at index time)
            if !s.params.is_empty() {
                return Some(s.params.clone());
            }
            // Fallback: extract from detail string, then line scan
            extract_params_from_detail(&s.detail)
                .or_else(|| collect_params_from_line(&data.lines, s.range.start.line as usize))
        })
        .collect()
}

/// Extract the parameter text from a CST-derived `detail` string.
///
/// Given `"fun foo(x: Int, y: String): Boolean"`, returns `"x: Int, y: String"`.
/// Given `"fun bar()"`, returns `None` (empty params = no required args).
/// Returns `None` if the detail is truncated (no matching `)` found).
///
/// Skips any leading annotation arguments (e.g., `@Deprecated("x") class Foo(params)`)
/// by starting the search for `(` only AFTER a declaration keyword is found.
pub(crate) fn extract_params_from_detail(detail: &str) -> Option<String> {
    if detail.is_empty() {
        return None;
    }
    // Skip annotation preamble by finding the first declaration keyword and
    // searching for `(` only after it.  This prevents `@Deprecated("x")` from
    // being mistaken for the constructor parameter list.
    const KEYWORDS: &[&str] = &["fun ", "fun<", "class ", "constructor"];
    let search_from = KEYWORDS
        .iter()
        .filter_map(|kw| detail.find(kw).map(|p| p + kw.len()))
        .min()
        .unwrap_or(0);
    let open_pos = detail[search_from..].find('(').map(|p| search_from + p)?;
    let mut depth: i32 = 0;
    let mut end_pos = None;
    for (i, ch) in detail[open_pos..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end_pos = Some(open_pos + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close_pos = end_pos?;
    let inner = &detail[open_pos + 1..close_pos];
    let trimmed = inner.trim();
    // Return Some("") for zero-param functions — distinct from None (couldn't parse).
    // This ensures 0-param overloads are visible for overload detection.
    Some(trimmed.to_string())
}

/// Walk forward from `start_line`, accumulating characters until the outermost
/// `)` closes — that ends the parameter list.
///
/// We only track `()` depth (NOT `<>`) to avoid false-triggers on `->` arrows.
/// Skips annotation lines (`@Foo(...)`) before the `fun`/`class` keyword to avoid
/// parsing annotation arguments as function parameters.
pub(crate) fn collect_params_from_line(lines: &[String], start_line: usize) -> Option<String> {
    let mut paren_depth: i32 = 0;
    let mut found_open = false;
    let mut params = String::new();
    let mut found_keyword = false;

    'outer: for ln in start_line..start_line + FUN_PARAMS_SCAN_LINES {
        let line = match lines.get(ln) {
            Some(l) => l,
            None => break,
        };
        let trimmed = line.trim();
        // Skip annotation-only lines before encountering fun/class/constructor keyword
        if !found_keyword {
            if trimmed.starts_with('@')
                && !trimmed.contains(" fun ")
                && !trimmed.contains(" fun<")
                && !trimmed.contains(" class ")
                && !trimmed.contains(" constructor")
            {
                continue;
            }
            if trimmed.starts_with("fun ")
                || trimmed.starts_with("fun<")
                || trimmed.contains(" fun ")
                || trimmed.contains(" fun<")
                || trimmed.starts_with("class ")
                || trimmed.starts_with("constructor")
                || trimmed.contains(" class ")
                || trimmed.contains(" constructor")
            {
                found_keyword = true;
            }
        }
        let chars = line.char_indices().peekable();
        for (_, ch) in chars {
            match ch {
                // If we hit '{' before finding any '(', the class has no constructor params
                '{' if !found_open && found_keyword => {
                    return Some(String::new());
                }
                '(' => {
                    paren_depth += 1;
                    if paren_depth == 1 {
                        found_open = true;
                        continue;
                    }
                    if found_open {
                        params.push(ch);
                    }
                }
                ')' => {
                    paren_depth -= 1;
                    if found_open && paren_depth == 0 {
                        break 'outer;
                    }
                    if found_open {
                        params.push(ch);
                    }
                }
                _ if found_open => params.push(ch),
                _ => {}
            }
        }
        if found_open {
            params.push('\n');
        }
    }

    if params.is_empty() {
        None
    } else {
        Some(params)
    }
}

// ─── Parameter type accessors ─────────────────────────────────────────────────

/// Split `text` at top-level commas (depth 0), skipping commas inside `()`, `<>`, `[]`, `{}`.
/// The `->` Kotlin function-type arrow is handled: `>` preceded by `-` is NOT a closing
/// generic delimiter.
/// `{}` are tracked so that lambda defaults like `= { a, b -> a }` are not split at
/// the comma inside the lambda body.
///
/// Returns the raw slices between commas; does NOT trim or filter empty parts.
pub(crate) fn split_params_at_depth_zero(text: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    let mut prev = '\0';
    for (i, ch) in text.char_indices() {
        match ch {
            '(' | '<' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '>' if prev != '-' && depth > 0 => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        prev = ch;
    }
    parts.push(&text[start..]);
    parts
}

/// Split the flattened parameter list by `,` at depth-0 (respecting `()`, `<>`).
/// Returns the type string of the parameter at position `n` (0-based).
/// Falls back to the last parameter if `n` is out of range.
///
/// NOTE: `->` in Kotlin functional types (e.g. `(Boolean) -> Flow<T>`) contains
/// `>` which would falsely decrement `<>` depth.  We skip the `>` of any `->` by
/// tracking the previous character.
pub(crate) fn nth_fun_param_type_str(params_text: &str, n: usize) -> Option<String> {
    let mut parts = split_params_at_depth_zero(params_text);
    // Drop trailing-comma empty parts (Kotlin allows `fun f(a: A, b: B,) {}`).
    parts.retain(|p| !p.trim().is_empty());
    if parts.is_empty() {
        return None;
    }

    let param = parts.get(n).unwrap_or_else(|| parts.last().unwrap()).trim();
    // Strip leading non-identifier characters (annotations, whitespace).
    let param = param.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_');
    let colon = param.find(':')?;
    Some(param[colon + 1..].trim().to_owned())
}

/// Return the type string of the last parameter in `params_text`.
pub(crate) fn last_fun_param_type_str(params_text: &str) -> Option<String> {
    let count = split_params_at_depth_zero(params_text)
        .iter()
        .filter(|p| !p.trim().is_empty())
        .count();
    nth_fun_param_type_str(params_text, count.saturating_sub(1))
}

// ─── Pure string helper ───────────────────────────────────────────────────────

/// Strip a balanced trailing `(…)` argument list from the end of `s`.
///
/// `"collection.method(arg1, arg2)"` → `"collection.method"`
/// `"collection.forEach"`           → `"collection.forEach"`  (unchanged)
pub(crate) fn strip_trailing_call_args(s: &str) -> &str {
    if !s.ends_with(')') {
        return s;
    }
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    return &s[..i];
                }
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
    idx: &Indexer,
    uri: &Url,
    name: &str,
    receiver: Option<&str>,
) -> String {
    if let Some(recv) = receiver {
        let Some(rt) = infer_receiver_type(idx, ReceiverKind::Variable(recv), uri) else {
            // Receiver present but type could not be resolved — avoid a global
            // name-only scan that may return a completely unrelated function.
            return String::new();
        };
        let locs = idx.resolve_symbol(&rt.outer, None, uri);
        for loc in &locs {
            if let Some(data) = idx.files.get(loc.uri.as_str()) {
                if let Some(sig) = collect_fun_params_text(name, loc.uri.as_str(), idx) {
                    return sig;
                }
                // Also search by line range within the type's body.
                let type_end = data
                    .symbols
                    .iter()
                    .find(|s| s.name == rt.outer)
                    .map(|s| s.range.end.line)
                    .unwrap_or(u32::MAX);
                for sym in data
                    .symbols
                    .iter()
                    .filter(|s| s.name == name && s.range.start.line <= type_end)
                {
                    if !sym.params.is_empty() {
                        return sym.params.clone();
                    }
                    if let Some(params) = extract_params_from_detail(&sym.detail) {
                        return params;
                    }
                }
            }
        }
        return String::new();
    }
    find_fun_signature_full(name, idx, uri).unwrap_or_default()
}

/// Receiver-aware params lookup: find `method_name`'s parameter text inside
/// the class `class_name`.  Uses range containment to avoid picking a method
/// from an unrelated class in the same file.
///
/// Supports dotted names like `"DepositAccountReducer.Factory"` — splits into
/// container `"DepositAccountReducer"` and type_base `"Factory"`, then filters
/// definitions to only those whose container matches.
pub(crate) fn find_method_params_in_class(
    idx: &Indexer,
    class_name: &str,
    method_name: &str,
) -> Option<String> {
    let (container, type_base) = match class_name.rsplit_once('.') {
        Some((c, b)) => (Some(c), b),
        None => (None, class_name),
    };
    let locations = idx.definitions.get(type_base)?;
    for loc in locations.iter() {
        let Some(file_data) = idx.files.get(loc.uri.as_str()) else {
            continue;
        };
        // Verify the class exists (and if qualified, check its own container).
        let has_class = file_data.symbols.iter().any(|s| {
            s.name == type_base
                && is_class_like(s.kind)
                && container.is_none_or(|c| s.container.as_deref() == Some(c))
        });
        if !has_class {
            continue;
        }

        // Find the method as a direct member of type_base.
        for sym in &file_data.symbols {
            if sym.name != method_name {
                continue;
            }
            if !matches!(
                sym.kind,
                SymbolKind::FUNCTION | SymbolKind::METHOD | SymbolKind::OPERATOR
            ) {
                continue;
            }
            if sym.container.as_deref() != Some(type_base) {
                continue;
            }
            if !sym.params.is_empty() {
                return Some(sym.params.clone());
            }
            if let Some(params) = extract_params_from_detail(&sym.detail) {
                return Some(params);
            }
        }
    }
    None
}

fn is_class_like(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::CLASS
            | SymbolKind::INTERFACE
            | SymbolKind::STRUCT
            | SymbolKind::ENUM
            | SymbolKind::OBJECT
    )
}

// ─── Unified call-site resolver ───────────────────────────────────────────────

/// Collect params text and param_counts for all callable symbols named `name`
/// in `file_data`, applying `scope` filtering rules.
///
/// `CrossFile` scope: skips `CLASS`/`STRUCT` symbols with a container (nested
/// classes) unless the caller has an explicit import for that symbol by local
/// name, and excludes files not import-reachable from the caller.
fn collect_params_from_file(
    name: &str,
    file_uri: &str,
    idx: &Indexer,
    caller_uri: &str,
    scope: ResolutionScope,
) -> Vec<(String, (u8, u8))> {
    let Some(data) = idx.files.get(file_uri) else {
        return vec![];
    };
    if scope == ResolutionScope::CrossFile && !is_import_reachable(idx, caller_uri, file_uri, name)
    {
        return vec![];
    }
    // This is only the nested-class allowlist by local name (e.g. `import
    // pkg.Outer.Config`); `is_import_reachable()` still does the full package/path
    // validation via `ImportEntry::covers()`.
    let caller_imports_by_name = if scope == ResolutionScope::CrossFile {
        idx.files
            .get(caller_uri)
            .map(|d| d.imports.iter().any(|i| !i.is_star && i.local_name == name))
            .unwrap_or(true) // fail-open
    } else {
        false
    };
    let is_java = file_uri.ends_with(".java");
    data.symbols
        .iter()
        .filter(|s| {
            s.name == name
                && !(scope == ResolutionScope::CrossFile
                    && s.container.is_some()
                    && matches!(s.kind, SymbolKind::CLASS | SymbolKind::STRUCT)
                    && !caller_imports_by_name)
                && (s.kind == SymbolKind::FUNCTION
                    || s.kind == SymbolKind::METHOD
                    || s.kind == SymbolKind::CONSTRUCTOR
                    || s.kind == SymbolKind::STRUCT
                    || (s.kind == SymbolKind::CLASS && !is_java))
        })
        .filter_map(|s| {
            let params_text = if !s.params.is_empty() {
                s.params.clone()
            } else {
                extract_params_from_detail(&s.detail).or_else(|| {
                    collect_params_from_line(&data.lines, s.range.start.line as usize)
                })?
            };
            Some((params_text, s.param_counts))
        })
        .collect()
}

/// Resolve the signature for a qualified call `qualifier.name(…)`.
///
/// Resolves the receiver type, then searches for `name` within that type's body.
/// Returns `SignatureResult::UnresolvableReceiver` when the receiver type cannot
/// be determined (prevents a fallback global scan that would match unrelated fns).
fn is_container_symbol_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::CLASS
            | SymbolKind::INTERFACE
            | SymbolKind::STRUCT
            | SymbolKind::ENUM
            | SymbolKind::OBJECT
    )
}

fn range_contains(range: &Range, candidate: &Range) -> bool {
    range.start.line <= candidate.start.line && candidate.end.line <= range.end.line
}

fn resolve_qualified(call: &CallSite<'_>, qualifier: &str, idx: &Indexer) -> SignatureResult {
    let Some(rt) = infer_receiver_type(idx, ReceiverKind::Variable(qualifier), call.caller_uri)
    else {
        return SignatureResult::UnresolvableReceiver;
    };
    let locs = idx.resolve_symbol(&rt.outer, None, call.caller_uri);
    for loc in &locs {
        let Some(data) = idx.files.get(loc.uri.as_str()) else {
            continue;
        };
        let type_sym = data
            .symbols
            .iter()
            .find(|s| s.name == rt.outer && is_container_symbol_kind(s.kind));
        let members: Vec<_> = data
            .symbols
            .iter()
            .filter(|s| {
                if s.name != call.name {
                    return false;
                }
                if s.container.as_deref() == Some(&rt.outer) {
                    return true;
                }
                if call.name == rt.outer && is_container_symbol_kind(s.kind) {
                    return true;
                }
                s.container.is_none()
                    && type_sym.is_some_and(|type_sym| range_contains(&type_sym.range, &s.range))
            })
            .collect();
        if members.is_empty() {
            continue;
        }
        // Collect all distinct param_counts to detect overloads.
        let mut seen: Vec<(u8, u8)> = vec![];
        for sym in &members {
            if !seen.contains(&sym.param_counts) {
                seen.push(sym.param_counts);
            }
        }
        if seen.len() > 1 {
            return SignatureResult::Overloaded;
        }
        let sym = members[0];
        let params_text = if !sym.params.is_empty() {
            sym.params.clone()
        } else if let Some(p) = extract_params_from_detail(&sym.detail) {
            p
        } else {
            continue;
        };
        return SignatureResult::Unique {
            param_counts: (sym.param_counts.0 as usize, sym.param_counts.1 as usize),
            params_text,
        };
    }
    SignatureResult::NotFound
}

/// Resolve the signature for an unqualified call `name(…)`.
///
/// Priority:
/// 1. Current file (same-file definitions are exact — no import filtering needed).
/// 2. Definitions map, cross-file with import-aware filtering.
///
/// If multiple distinct arity envelopes are found, returns `Overloaded`.
fn resolve_unqualified(call: &CallSite<'_>, idx: &Indexer) -> SignatureResult {
    // Same-file first: if defined here, use only those — avoids workspace-wide
    // overload explosion (e.g. 945 `loadData` implementations).
    let same_file = collect_params_from_file(
        call.name,
        call.caller_uri.as_str(),
        idx,
        call.caller_uri.as_str(),
        ResolutionScope::SameFile,
    );
    if !same_file.is_empty() {
        return build_result(same_file);
    }

    let caller_source_set = idx
        .files
        .get(call.caller_uri.as_str())
        .map(|file| file.source_set)
        .unwrap_or_default();

    // Cross-file: definitions map with import + nested-class filtering.
    let mut all: Vec<(String, (u8, u8))> = Vec::new();
    if let Some(locs) = idx.definitions.get(call.name) {
        for loc in locs.iter() {
            let definition_source_set = idx
                .files
                .get(loc.uri.as_str())
                .map(|file| file.source_set)
                .unwrap_or_default();
            if definition_source_set == SourceSet::Test && caller_source_set != SourceSet::Test {
                continue;
            }
            let sigs = collect_params_from_file(
                call.name,
                loc.uri.as_str(),
                idx,
                call.caller_uri.as_str(),
                ResolutionScope::CrossFile,
            );
            all.extend(sigs);
        }
    }
    build_result(all)
}

/// Build a `SignatureResult` from a list of `(params_text, param_counts)` pairs.
///
/// Deduplicates by arity envelope. If there are multiple distinct envelopes,
/// the function is considered overloaded and the caller should skip the diagnostic.
fn build_result(entries: Vec<(String, (u8, u8))>) -> SignatureResult {
    if entries.is_empty() {
        return SignatureResult::NotFound;
    }
    // Deduplicate by arity envelope.
    let mut seen: std::collections::HashSet<(u8, u8)> = std::collections::HashSet::new();
    let mut deduped: Vec<(String, (u8, u8))> = Vec::new();
    for (text, counts) in entries {
        if seen.insert(counts) {
            deduped.push((text, counts));
        }
    }
    if deduped.len() > 1 {
        return SignatureResult::Overloaded;
    }
    let (params_text, (required, total)) = deduped.into_iter().next().unwrap();
    SignatureResult::Unique {
        params_text,
        param_counts: (required as usize, total as usize),
    }
}

/// Resolve the call site's signature using a unified, single-entry-point pipeline.
///
/// Resolution strategy:
/// - **Qualified** (`qualifier.name(…)`): resolve receiver type, find method within it.
///   Returns `UnresolvableReceiver` when the receiver type is unknown to prevent
///   a fallback global-name scan from matching unrelated functions.
/// - **Unqualified** (`name(…)`): check the current file first, then cross-file
///   definitions filtered by import reachability and nested-class exclusion.
///
/// This function does NOT trigger on-demand indexing (`rg` / disk reads).
/// Use `find_fun_signature_full` for hover/completion where latency is acceptable.
pub(crate) fn resolve_call_signature(call: &CallSite<'_>, idx: &Indexer) -> SignatureResult {
    if let Some(qualifier) = call.qualifier {
        resolve_qualified(call, qualifier, idx)
    } else {
        resolve_unqualified(call, idx)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "sig_tests.rs"]
mod tests;
