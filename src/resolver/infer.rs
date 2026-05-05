use tower_lsp::lsp_types::{Position, Range, SymbolKind, Url};

use crate::indexer::Indexer;
use crate::LinesExt;
use crate::StrExt;

// ─── Receiver type resolution ─────────────────────────────────────────────────

/// How the receiver expression should be resolved.
///
/// - `Variable`: a named val/var (e.g. `interactor`, `viewModel`).
///   Resolved via line-scan type annotation (`val name: Type`).
/// - `Contextual`: `it`, `this`, or a named lambda parameter.
///   Requires cursor `position` for scope analysis; falls back to
///   `infer_variable_type_raw` only if scope analysis returns nothing.
pub enum ReceiverKind<'a> {
    Variable(&'a str),
    Contextual { name: &'a str, position: Position },
}

/// A fully-normalised receiver type with multiple access forms.
///
/// All forms are derived from a single raw string (e.g. `"Outer.Inner<Param>"`):
/// - `raw`       — original with generics: `"Outer.Inner<Param>"`
/// - `qualified` — no generics, dots preserved: `"Outer.Inner"`
/// - `outer`     — first dot-segment: `"Outer"`  (used for file lookup)
/// - `leaf`      — last dot-segment: `"Inner"`   (used for fallback member lookup)
pub struct ReceiverType {
    pub raw:       String,
    pub qualified: String,
    pub outer:     String,
    pub leaf:      String,
}

impl ReceiverType {
    pub fn from_raw(raw: String) -> Self {
        // Strip generics: take chars until first `<`.
        let qualified: String = raw.chars().take_while(|&c| c != '<').collect();
        let outer = qualified.split('.').next().unwrap_or(&qualified).to_string();
        let leaf  = qualified.rsplit('.').next().unwrap_or(&qualified).to_string();
        ReceiverType { raw, qualified, outer, leaf }
    }
}

/// Infer the type of a receiver expression and normalise it into a
/// [`ReceiverType`].
///
/// Returns `None` when type inference fails (no annotation, unindexed file,
/// or lambda scope not resolvable).  Call sites then decide whether to skip
/// or fall back; this function never performs a global rg scan.
pub fn infer_receiver_type(
    idx:  &Indexer,
    kind: ReceiverKind<'_>,
    uri:  &Url,
) -> Option<ReceiverType> {
    let raw = match kind {
        ReceiverKind::Variable(name) => infer_variable_type_raw(idx, name, uri)?,
        ReceiverKind::Contextual { name, position } => {
            // Lambda / implicit-receiver path.
            if let Some(ty) = idx.infer_lambda_param_type_at(name, uri, position) {
                ty
            } else {
                // Contextual fallback: ordinary annotated var that happens to
                // appear in a lambda context (e.g. captured val with explicit type).
                infer_variable_type_raw(idx, name, uri)?
            }
        }
    };
    Some(ReceiverType::from_raw(raw))
}

/// Scan the current file's lines for a type annotation on `var_name` and return
/// the declared type name if found.  Delegates to [`infer_type_in_lines`] and
/// falls back to method return-type inference for `val x = receiver.method(...)`.
pub(crate) fn infer_variable_type(idx: &Indexer, var_name: &str, uri: &Url) -> Option<String> {
    infer_variable_type_impl(idx, var_name, uri, 4)
}

/// Like [`infer_variable_type`] but preserves generic parameters in the returned
/// type string.  e.g. `val items: List<Product>` → `"List<Product>"`.
///
/// Used by the `it`-completion path to extract the collection element type.
pub fn infer_variable_type_raw(idx: &Indexer, var_name: &str, uri: &Url) -> Option<String> {
    infer_variable_type_raw_impl(idx, var_name, uri, 4)
}

fn infer_variable_type_impl(idx: &Indexer, var_name: &str, uri: &Url, depth: u8) -> Option<String> {
    if depth == 0 { return None; }
    // Scope block: all DashMap guards are dropped before method-return inference,
    // which may call this function recursively and must not deadlock.
    let (lines, rhs_match, method_match) = {
        if let Some(ll) = idx.live_lines.get(uri.as_str()) {
            if let result @ Some(_) = ll.infer_type(var_name) {
                return result;
            }
            ((*ll).clone(), None::<String>, None::<(String, String)>)
        } else if let Some(data) = idx.files.get(uri.as_str()) {
            if let result @ Some(_) = data.lines.infer_type(var_name) {
                return result;
            }
            // CST-indexed RHS types — primary path for indexed files.
            let rhs_match = data.rhs_types.iter()
                .find(|(_, n, _)| n == var_name)
                .map(|(_, _, ty)| ty.clone());
            let method_match = data.method_call_rhs.iter()
                .find(|(_, n, _, _)| n == var_name)
                .map(|(_, _, recv, method)| (recv.clone(), method.clone()));
            let lines = data.lines.clone();
            // Drop DashMap guard before any potential recursive call.
            drop(data);
            if let Some(ty) = rhs_match {
                return Some(ty);
            }
            if let Some((recv, method)) = method_match {
                if let Some(recv_type) = infer_variable_type_impl(idx, &recv, uri, depth - 1) {
                    if let Some(ret) = find_method_return_type(idx, &recv_type, &method) {
                        return Some(ret);
                    }
                }
            }
            // Lines guard was dropped above; fall through to string-based fallback.
            return infer_method_return_type(idx, var_name, &lines, uri, depth - 1);
        } else {
            // File not indexed yet — read from disk; skip method inference.
            let path = uri.to_file_path().ok()?;
            let content = std::fs::read_to_string(&path).ok()?;
            let lines: Vec<String> = content.lines().map(String::from).collect();
            return lines.infer_type(var_name);
        }
    };
    // All DashMap guards are dropped here.  Safe to recurse.
    infer_method_return_type(idx, var_name, &lines, uri, depth - 1)
}

fn infer_variable_type_raw_impl(
    idx: &Indexer, var_name: &str, uri: &Url, depth: u8,
) -> Option<String> {
    if depth == 0 { return None; }
    let (lines, rhs_match, method_match) = {
        if let Some(ll) = idx.live_lines.get(uri.as_str()) {
            if let result @ Some(_) = ll.infer_type_raw(var_name) {
                return result;
            }
            ((*ll).clone(), None::<String>, None::<(String, String)>)
        } else if let Some(data) = idx.files.get(uri.as_str()) {
            if let result @ Some(_) = data.lines.infer_type_raw(var_name) {
                return result;
            }
            let rhs_match = data.rhs_types.iter()
                .find(|(_, n, _)| n == var_name)
                .map(|(_, _, ty)| ty.clone());
            let method_match = data.method_call_rhs.iter()
                .find(|(_, n, _, _)| n == var_name)
                .map(|(_, _, recv, method)| (recv.clone(), method.clone()));
            let lines = data.lines.clone();
            drop(data);
            if let Some(ty) = rhs_match {
                return Some(ty);
            }
            if let Some((recv, method)) = method_match {
                if let Some(recv_type) = infer_variable_type_raw_impl(idx, &recv, uri, depth - 1) {
                    if let Some(ret) = find_method_return_type(idx, &recv_type, &method) {
                        return Some(ret);
                    }
                }
            }
            return infer_method_return_type(idx, var_name, &lines, uri, depth - 1);
        } else {
            let path = uri.to_file_path().ok()?;
            let content = std::fs::read_to_string(&path).ok()?;
            let lines: Vec<String> = content.lines().map(String::from).collect();
            return lines.infer_type_raw(var_name);
        }
    };
    infer_method_return_type(idx, var_name, &lines, uri, depth - 1)
}

/// Extract the Kotlin/Android collection element type from a raw generic type string.
///
/// Handles the most common collection-like types seen in Android development:
/// - `List<Product>` → `Product`
/// - `MutableList<User>` → `User`
/// - `Flow<Event>` → `Event`
/// - `StateFlow<UiState>` → `UiState`
/// - `Set<Tag>` → `Tag`
/// - etc.
///
/// Returns `None` when the base type is not in the known collection list, or when
/// the generic parameter is a primitive/lowercase type.  In those cases the
/// caller should treat `it` as the receiver type itself (scope functions).
pub fn extract_collection_element_type(raw_type: &str) -> Option<String> {
    const COLLECTION_TYPES: &[&str] = &[
        "List", "MutableList", "ArrayList",
        "Set", "MutableSet", "HashSet", "LinkedHashSet",
        "Collection", "MutableCollection", "Iterable", "MutableIterable",
        "Sequence", "Flow", "StateFlow", "SharedFlow",
        "Channel", "SendChannel", "ReceiveChannel",
        "Array",
    ];

    let base = raw_type.ident_prefix();
    if !COLLECTION_TYPES.contains(&base.as_str()) { return None; }

    let open  = raw_type.find('<')?;
    let close = raw_type.rfind('>')?;
    if close <= open { return None; }
    let inner = &raw_type[open + 1..close];

    // Take first type argument (before the first `,` at depth 0).
    let first = first_type_arg(inner).trim().trim_matches('?');

    // Strip to the base class name only.
    let elem = first.ident_prefix();
    if elem.is_empty() || !elem.starts_with_uppercase() {
        return None;
    }
    Some(elem)
}

/// Return the first type argument in a comma-separated generic parameter list,
/// respecting nested `<>` brackets.
fn first_type_arg(s: &str) -> &str {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => return &s[..i],
            _ => {}
        }
    }
    s
}

/// Scan a specific (possibly un-indexed) file for the declared type of `field_name`.
///
/// Checks the in-memory index first (lines are cached); falls back to reading
/// the file from disk when it isn't indexed yet.
pub(crate) fn infer_field_type(idx: &Indexer, file_uri: &str, field_name: &str) -> Option<String> {
    if let Some(data) = idx.files.get(file_uri) {
        return data.lines.infer_type(field_name);
    }
    let path = tower_lsp::lsp_types::Url::parse(file_uri).ok()?.to_file_path().ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let lines: Vec<String> = content.lines().map(String::from).collect();
    lines.infer_type(field_name)
}

// ─── impl Indexer wrappers ────────────────────────────────────────────────────

#[allow(dead_code)]
impl crate::indexer::Indexer {
    pub(crate) fn infer_variable_type(&self, var_name: &str, uri: &Url) -> Option<String> {
        infer_variable_type(self, var_name, uri)
    }
    pub fn infer_variable_type_raw(&self, var_name: &str, uri: &Url) -> Option<String> {
        infer_variable_type_raw(self, var_name, uri)
    }
    pub(crate) fn infer_field_type(&self, file_uri: &str, field_name: &str) -> Option<String> {
        infer_field_type(self, file_uri, field_name)
    }
}

/// Core line scanner: find `var_name:` in `lines` and return the type that follows.
///
/// Handles:
/// - Constructor parameters: `private val repo: UserRepository`
/// - Properties:             `val config: Config`
/// - Local variables:        `val result: ResultType = ...`
/// - Function parameters:    `fun foo(repo: UserRepository)`
///
/// Returns the type name without nullable marker (`?`) and generic parameters (`<…>`).
/// Only returns names starting with an uppercase letter (skips primitives / unit).
///
/// When no explicit type annotation is found, falls back to RHS assignment inference
/// (constructor calls, class literals, DI generics).
pub(crate) fn infer_type_in_lines(lines: &[String], var_name: &str) -> Option<String> {
    let pattern = format!("{var_name}:");

    for line in lines {
        if !line.contains(&pattern) { continue; }

        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }

        if let Some(pos) = line.find(&pattern) {
            // Ensure var_name is not a suffix of a longer identifier.
            let before_char = line[..pos].chars().last();
            if before_char.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false) {
                continue;
            }
            let after = &line[pos + var_name.len()..];
            let after = after.trim_start_matches(':').trim_start();
            // Allow dotted type names like `DashboardProductsReducer.Factory`
            // Stop at generic params (`<`), nullability (`?`), spaces, assignment.
            let type_name: String = after.chars()
                .take_while(|&c| c.is_alphanumeric() || c == '_' || c == '.')
                .collect();
            // Trim any trailing dots.
            let type_name = type_name.trim_end_matches('.').to_owned();
            if !type_name.is_empty()
                && type_name.starts_with_uppercase()
            {
                return Some(type_name);
            }
        }
    }

    // Secondary scan: RHS assignment inference (no explicit type annotation).
    for line in lines {
        if let Some(t) = infer_from_rhs_assignment(line, var_name) {
            return Some(t);
        }
    }

    None
}

/// Like `infer_type_in_lines` but preserves generic parameters in the result.
///
/// `val items: List<Product>` → `"List<Product>"`
/// `val state: StateFlow<UiState>` → `"StateFlow<UiState>"`
///
/// Also handles delegate-inferred types:
/// `val foo by lazy { SomeType() }` → `"SomeType"` (single-line only)
pub(crate) fn infer_type_in_lines_raw(lines: &[String], var_name: &str) -> Option<String> {
    let pattern = format!("{var_name}:");

    for line in lines {
        if !line.contains(&pattern) { continue; }
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }
        if let Some(pos) = line.find(&pattern) {
            let before_char = line[..pos].chars().last();
            if before_char.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false) {
                continue;
            }
            let after = &line[pos + var_name.len()..];
            let after = after.trim_start_matches(':').trim_start();
            let raw = extract_type_with_generics(after);
            if !raw.is_empty() && raw.starts_with_uppercase() {
                return Some(raw);
            }
        }
    }

    // Secondary scan: `val varName by lazy { ConstructorCall() }`
    // Works only for single-line declarations without an explicit type annotation.
    let lazy_pattern = format!("{var_name} by lazy");
    for line in lines {
        if !line.contains(&lazy_pattern) { continue; }
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }
        if let Some(brace_pos) = line.find('{') {
            let after_brace = line[brace_pos + 1..].trim_start();
            // Extract the first identifier (stops at `<`, `(`, whitespace, etc.)
            let ident = after_brace.dotted_ident_prefix();
            let base = ident.split('.').last().unwrap_or(&ident);
            if !base.is_empty() && base.starts_with_uppercase() {
                return Some(base.to_owned());
            }
        }
    }

    // Tertiary scan: assignment-based type inference.
    for line in lines {
        if let Some(t) = infer_from_rhs_assignment(line, var_name) {
            return Some(t);
        }
    }

    None
}

/// Extract the right-hand side expression of `var_name = <expr>` from `line`.
///
/// Handles flexible whitespace (`var_name=expr` and `var_name = expr`), whole-word
/// boundaries, and rejects type-annotation positions (`: var_name`).
/// Returns a slice into `line` starting at the first non-space character of the RHS.
fn find_rhs_str<'a>(line: &'a str, var_name: &str) -> Option<&'a str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
        return None;
    }
    let pos = line.find(var_name)?;
    // Whole-word check: character before var_name must not be alphanumeric or `_`.
    if pos > 0 {
        let b = line.as_bytes()[pos - 1];
        if b.is_ascii_alphanumeric() || b == b'_' { return None; }
    }
    // Whole-word check: character after var_name must not be alphanumeric or `_`.
    let end = pos + var_name.len();
    let c_after = line.as_bytes().get(end).copied().unwrap_or(b' ');
    if c_after.is_ascii_alphanumeric() || c_after == b'_' { return None; }
    // Reject type-annotation position: last non-space token before name is `:`, `,`, or `<`.
    let last_tok = line[..pos].trim_end().chars().last().unwrap_or(' ');
    if last_tok == ':' || last_tok == ',' || last_tok == '<' { return None; }
    // Find `=` after the name, skipping whitespace.
    let after = &line[end..];
    let trimmed_after = after.trim_start();
    if !trimmed_after.starts_with('=') { return None; }
    // Reject `==` and `=>`.
    let next = trimmed_after.as_bytes().get(1).copied().unwrap_or(b' ');
    if next == b'=' || next == b'>' { return None; }
    Some(trimmed_after[1..].trim_start())
}

/// Attempt to infer the type of `var_name` from the right-hand side of an assignment.
///
/// Called as a secondary/tertiary scan when no explicit type annotation (`var_name:`)
/// is found.  Handles the most common Android/Kotlin patterns:
///
/// 1. Constructor call:  `val x = SomeType(args)` → `"SomeType"`
/// 2. DI generic:        `val x = inject<SomeType>()` → `"SomeType"`
///
/// Returns `None` when none of the patterns match.
fn infer_from_rhs_assignment(line: &str, var_name: &str) -> Option<String> {
    let rhs = find_rhs_str(line, var_name)?;

    // Pattern 2: DI generic — `inject<SomeType>()`, `get<SomeType>()`, etc.
    const DI_PREFIXES: &[&str] = &["inject<", "get<", "viewModel<", "activityViewModel<"];
    for prefix in DI_PREFIXES {
        if let Some(start) = rhs.find(prefix) {
            let after = &rhs[start + prefix.len()..];
            let type_name = after.ident_prefix();
            if !type_name.is_empty() && type_name.starts_with_uppercase() {
                return Some(type_name);
            }
        }
    }

    // Pattern 1: constructor call — RHS starts with UppercaseIdent followed by `(` or `{`.
    let dotted = rhs.dotted_ident_prefix();
    if !dotted.is_empty() {
        let base = dotted.split('.').last().unwrap_or(&dotted);
        if base.starts_with_uppercase() {
            let after_ident = rhs[dotted.len()..].trim_start();
            if after_ident.starts_with('(') || after_ident.starts_with('{') {
                return Some(base.to_owned());
            }
        }
    }

    None
}

// ─── Method return-type inference ─────────────────────────────────────────────

/// Scan `lines` for `var_name = receiver.method(...)` and return the inferred
/// return type of `method`.
///
/// Only handles one level of chaining: `simpleIdent.method(args)`.
/// Skips `this`, `super`, and dotted/chained receivers.
fn infer_method_return_type(
    idx: &Indexer, var_name: &str, lines: &[String], uri: &Url, depth: u8,
) -> Option<String> {
    for line in lines {
        let rhs = match find_rhs_str(line, var_name) {
            Some(r) => r,
            None => continue,
        };

        // Match `receiver.method(` where receiver is a simple identifier.
        let paren_pos = match rhs.find('(') {
            Some(p) => p,
            None => continue,
        };
        let before_paren = &rhs[..paren_pos];
        let dot_pos = match before_paren.rfind('.') {
            Some(d) => d,
            None => continue,
        };
        let receiver = before_paren[..dot_pos].trim();
        let method   = before_paren[dot_pos + 1..].trim();

        if receiver.is_empty() || method.is_empty() { continue; }
        // Skip `this`/`super` and multi-segment receivers.
        if receiver == "this" || receiver == "super" || receiver.contains('.') { continue; }
        if !method.starts_with_lowercase() { continue; }

        // Recursively infer the receiver type (DashMap guards already dropped).
        if let Some(receiver_type) = infer_variable_type_impl(idx, receiver, uri, depth) {
            if let Some(ret) = find_method_return_type(idx, &receiver_type, method) {
                return Some(ret);
            }
        }
    }
    None
}

/// Look up `method_name` in the symbol index for `type_name` and return its
/// return type, extracted from `SymbolEntry.detail`.
fn find_method_return_type(idx: &Indexer, type_name: &str, method_name: &str) -> Option<String> {
    // `definitions` is keyed by simple (unqualified) name; strip any qualifying prefix.
    let type_base = type_name.split('.').last().unwrap_or(type_name);
    let locations = idx.definitions.get(type_base)?;
    for loc in locations.iter() {
        if let Some(file_data) = idx.files.get(loc.uri.as_str()) {
            // Find the class entry for type_base so we can do range containment
            // filtering — avoids picking a same-named method from an unrelated class
            // in the same file.
            let class_range = file_data.symbols.iter()
                .find(|s| s.name == type_base)
                .map(|s| s.range);

            for sym in &file_data.symbols {
                if sym.name != method_name { continue; }
                if !matches!(sym.kind, SymbolKind::FUNCTION | SymbolKind::METHOD | SymbolKind::OPERATOR) {
                    continue;
                }
                // When we know the class range, skip methods outside it.
                if let Some(cr) = class_range {
                    if sym.range.start.line < cr.start.line || sym.range.end.line > cr.end.line {
                        continue;
                    }
                }
                // Try detail first; fall back to source lines when detail is truncated.
                if let Some(ret) = extract_return_type_from_detail(&sym.detail) {
                    return Some(ret);
                }
                // detail may be truncated (120 char limit) — try the source lines.
                let start_line = sym.selection_range.start.line as usize;
                let full_sig = file_data.lines.collect_signature(start_line);
                if let Some(ret) = extract_return_type_from_detail(&full_sig) {
                    return Some(ret);
                }
            }
        }
    }
    None
}

/// Extract the return type from a function `detail` string.
///
/// `"fun getDetail(req: Req): Response<Data>"` → `"Response<Data>"`
/// `"fun doSomething()"` → `None`
fn extract_return_type_from_detail(detail: &str) -> Option<String> {
    let close_paren = detail.rfind(')')?;
    let after = detail[close_paren + 1..].trim_start();
    if !after.starts_with(':') { return None; }
    let type_part = after[1..].trim_start();
    let type_name = extract_type_with_generics(type_part);
    if !type_name.is_empty() && type_name.starts_with_uppercase() {
        Some(type_name)
    } else {
        None
    }
}
///
/// `"List<Product> = emptyList()"` → `"List<Product>"`
/// `"StateFlow<UiState>"` → `"StateFlow<UiState>"`
/// `"User?"` → `"User"`  (nullable stripped at the outer `?`)
fn extract_type_with_generics(s: &str) -> String {
    let mut result = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '<' => { depth += 1; result.push(c); }
            '>' => {
                if depth > 0 {
                    depth -= 1;
                    result.push(c);
                    if depth == 0 { break; }
                } else { break; }
            }
            // Stop at these outside of generic brackets.
            '?' | ' ' | '=' | ',' | ')' | '\n' if depth == 0 => break,
            _ => result.push(c),
        }
    }
    result
}

/// Return the `Range` of the declaration `name:` on the first matching line,
/// or `None` if not found.
///
/// Used to locate function parameters and other declarations that are not in
/// the tree-sitter symbol index (e.g. `fun foo(account: AccountModel)`).
pub(crate) fn find_declaration_range_in_lines(lines: &[String], name: &str) -> Option<Range> {
    // Pattern 1: `name: Type` — typed parameter, val/var declaration, constructor param
    let typed_pattern = format!("{name}:");

    // Pattern 2: `{ name ->` or `name ->` — untyped lambda / trailing-lambda parameter
    let lambda_arrow = format!("{name} ->");
    let lambda_brace = format!("{{ {name} ->");  // with brace prefix

    for (line_num, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }

        // ── typed parameter / val / var ─────────────────────────────────────
        if line.contains(&typed_pattern) {
            if let Some(pos) = line.find(&typed_pattern) {
                let before = line[..pos].chars().last();
                if !before.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false)
                    && !line[..pos].trim_end().ends_with('"')
                {
                    let col = pos as u32;
                    return Some(Range {
                        start: Position { line: line_num as u32, character: col },
                        end:   Position { line: line_num as u32, character: col + name.len() as u32 },
                    });
                }
            }
        }

        // ── untyped lambda parameter: `{ name ->` or leading `name ->` ─────
        if line.contains(&lambda_arrow) {
            // Must be `{ name ->` (with brace) or the name at the start of the
            // lambda params after trimming whitespace/opening brace.
            let is_lambda = line.contains(&lambda_brace)
                || trimmed.starts_with(&lambda_arrow)
                || trimmed.starts_with(&format!("{name},"))  // multi-param `a, b ->`
                || (trimmed.contains(&lambda_arrow)
                    && line[..line.find(&lambda_arrow).unwrap_or(0)]
                        .chars()
                        .all(|c| c.is_whitespace() || c == '{' || c == '(' || c == ',' || c.is_alphanumeric() || c == '_'));
            if is_lambda {
                if let Some(pos) = line.find(name) {
                    // Make sure we matched the right token (word boundary check)
                    let before = pos.checked_sub(1).and_then(|i| line.as_bytes().get(i)).copied();
                    let after  = line.as_bytes().get(pos + name.len()).copied();
                    let boundary = before.map(|b| !b.is_ascii_alphanumeric() && b != b'_').unwrap_or(true)
                        && after.map(|b| !b.is_ascii_alphanumeric() && b != b'_').unwrap_or(true);
                    if boundary {
                        let col = pos as u32;
                        return Some(Range {
                            start: Position { line: line_num as u32, character: col },
                            end:   Position { line: line_num as u32, character: col + name.len() as u32 },
                        });
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod infer_tests {
    use super::extract_return_type_from_detail;

    #[test]
    fn return_type_simple() {
        assert_eq!(
            extract_return_type_from_detail("fun getDetail(req: Req): AccountDetail"),
            Some("AccountDetail".into()),
        );
    }

    #[test]
    fn return_type_generic() {
        assert_eq!(
            extract_return_type_from_detail("fun getAccountDetail(body: Body): Response<AccountDetail>"),
            Some("Response<AccountDetail>".into()),
        );
    }

    #[test]
    fn return_type_unit_returns_none() {
        assert_eq!(extract_return_type_from_detail("fun doSomething(x: Int)"), None);
    }

    #[test]
    fn return_type_primitive_returns_none() {
        assert_eq!(extract_return_type_from_detail("fun count(): int"), None);
    }

    #[test]
    fn return_type_nullable_stripped() {
        assert_eq!(
            extract_return_type_from_detail("fun find(): User?"),
            Some("User".into()),
        );
    }
}
