use tower_lsp::lsp_types::{Position, Range, Url};

use crate::indexer::Indexer;
use crate::LinesExt;

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
/// the declared type name if found.  Delegates to [`infer_type_in_lines`].
pub(crate) fn infer_variable_type(idx: &Indexer, var_name: &str, uri: &Url) -> Option<String> {
    // Prefer live_lines: updated synchronously on every keystroke, so they
    // reflect unsaved edits that the indexed snapshot may not yet contain.
    if let Some(ll) = idx.live_lines.get(uri.as_str()) {
        if let result @ Some(_) = ll.infer_type(var_name) {
            return result;
        }
    }
    // Fall back to indexed snapshot.  Use declared_names as a fast reject
    // only when live_lines are unavailable or returned nothing, since the
    // index may lag behind in-flight edits.
    if let Some(data) = idx.files.get(uri.as_str()) {
        if !data.declared_names.iter().any(|n| n == var_name) {
            return None;
        }
        return data.lines.infer_type(var_name);
    }
    // File not indexed yet — read from disk.
    let path = uri.to_file_path().ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let lines: Vec<String> = content.lines().map(String::from).collect();
    lines.infer_type(var_name)
}

/// Like [`infer_variable_type`] but preserves generic parameters in the returned
/// type string.  e.g. `val items: List<Product>` → `"List<Product>"`.
///
/// Used by the `it`-completion path to extract the collection element type.
pub fn infer_variable_type_raw(idx: &Indexer, var_name: &str, uri: &Url) -> Option<String> {
    if let Some(ll) = idx.live_lines.get(uri.as_str()) {
        if let result @ Some(_) = ll.infer_type_raw(var_name) {
            return result;
        }
    }
    if let Some(data) = idx.files.get(uri.as_str()) {
        return data.lines.infer_type_raw(var_name);
    }
    let path = uri.to_file_path().ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let lines: Vec<String> = content.lines().map(String::from).collect();
    lines.infer_type_raw(var_name)
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

    let base: String = raw_type.chars().take_while(|&c| c.is_alphanumeric() || c == '_').collect();
    if !COLLECTION_TYPES.contains(&base.as_str()) { return None; }

    let open  = raw_type.find('<')?;
    let close = raw_type.rfind('>')?;
    if close <= open { return None; }
    let inner = &raw_type[open + 1..close];

    // Take first type argument (before the first `,` at depth 0).
    let first = first_type_arg(inner).trim().trim_matches('?');

    // Strip to the base class name only.
    let elem: String = first.chars().take_while(|&c| c.is_alphanumeric() || c == '_').collect();
    if elem.is_empty() || !crate::indexer::starts_with_uppercase(&elem) {
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
                && crate::indexer::starts_with_uppercase(&type_name)
            {
                return Some(type_name);
            }
        }
    }
    None
}

/// Like `infer_type_in_lines` but preserves generic parameters in the result.
///
/// `val items: List<Product>` → `"List<Product>"`
/// `val state: StateFlow<UiState>` → `"StateFlow<UiState>"`
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
            if !raw.is_empty() && crate::indexer::starts_with_uppercase(&raw) {
                return Some(raw);
            }
        }
    }
    None
}

/// Capture a type expression including balanced generic parameters.
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
