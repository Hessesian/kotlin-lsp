//! Pure line-scanning type inference — no index access.
//!
//! All functions here take only `&str` or `&[String]` — zero dependency on
//! `Indexer`, `FileData`, or any live index.  This makes them independently
//! testable and usable as building blocks in higher-level index-backed
//! inference (see `infer.rs`).

use tower_lsp::lsp_types::{Position, Range};

use crate::StrExt;

// ─── collection element type ──────────────────────────────────────────────────

/// Extract the element type from a known Kotlin/Java collection type.
///
/// `"List<Product>"` → `Some("Product")`
/// `"StateFlow<UiState>"` → `Some("UiState")`
///
/// Returns `None` when the base type is not in the known collection list, or when
/// the generic parameter is a primitive/lowercase type.  In those cases the
/// caller should treat `it` as the receiver type itself (scope functions).
pub(crate) fn extract_collection_element_type(raw_type: &str) -> Option<String> {
    const COLLECTION_TYPES: &[&str] = &[
        "List",
        "MutableList",
        "ArrayList",
        "Set",
        "MutableSet",
        "HashSet",
        "LinkedHashSet",
        "Collection",
        "MutableCollection",
        "Iterable",
        "MutableIterable",
        "Sequence",
        "Flow",
        "StateFlow",
        "SharedFlow",
        "Channel",
        "SendChannel",
        "ReceiveChannel",
        "Array",
    ];

    let base = raw_type.ident_prefix();
    if !COLLECTION_TYPES.contains(&base.as_str()) {
        return None;
    }

    let open = raw_type.find('<')?;
    let close = raw_type.rfind('>')?;
    if close <= open {
        return None;
    }
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
pub(super) fn first_type_arg(s: &str) -> &str {
    let mut depth = 0i32;
    let mut end = s.len();
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    &s[..end]
}

// ─── explicit type annotation inference ──────────────────────────────────────

/// Scan `lines` for an explicit type annotation of `var_name` and return the
/// base type name (generics and nullability stripped).
///
/// `"val repo: UserRepository"` → `Some("UserRepository")`
/// `"val items: List<Product>"` → `Some("List")` (base only; use raw variant
/// when generics are needed)
///
/// Returns the type name without nullable marker (`?`) and generic parameters (`<…>`).
/// Only returns names starting with an uppercase letter (skips primitives / unit).
///
/// When no explicit type annotation is found, falls back to RHS assignment inference
/// (constructor calls, class literals, DI generics).
pub(crate) fn infer_type_in_lines(lines: &[String], var_name: &str) -> Option<String> {
    let pattern = format!("{var_name}:");

    for line in lines {
        if !line.contains(&pattern) {
            continue;
        }

        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }

        if let Some(pos) = line.find(&pattern) {
            // Ensure var_name is not a suffix of a longer identifier.
            let before_char = line[..pos].chars().last();
            if before_char
                .map(|c| c.is_alphanumeric() || c == '_')
                .unwrap_or(false)
            {
                continue;
            }
            let after = &line[pos + var_name.len()..];
            let after = after.trim_start_matches(':').trim_start();
            // Allow dotted type names like `DashboardProductsReducer.Factory`
            // Stop at generic params (`<`), nullability (`?`), spaces, assignment.
            let type_name: String = after
                .chars()
                .take_while(|&c| c.is_alphanumeric() || c == '_' || c == '.')
                .collect();
            // Trim any trailing dots.
            let type_name = type_name.trim_end_matches('.').to_owned();
            if !type_name.is_empty() && type_name.starts_with_uppercase() {
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
        if !line.contains(&pattern) {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }
        if let Some(pos) = line.find(&pattern) {
            let before_char = line[..pos].chars().last();
            if before_char
                .map(|c| c.is_alphanumeric() || c == '_')
                .unwrap_or(false)
            {
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
        if !line.contains(&lazy_pattern) {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }
        if let Some(brace_pos) = line.find('{') {
            let after_brace = line[brace_pos + 1..].trim_start();
            // Extract the first identifier (stops at `<`, `(`, whitespace, etc.)
            let ident = after_brace.dotted_ident_prefix();
            let base = ident.split('.').next_back().unwrap_or(&ident);
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

// ─── RHS assignment inference ─────────────────────────────────────────────────

/// Extract the right-hand side expression of `var_name = <expr>` from `line`.
///
/// Handles flexible whitespace (`var_name=expr` and `var_name = expr`), whole-word
/// boundaries, and rejects type-annotation positions (`: var_name`).
/// Returns a slice into `line` starting at the first non-space character of the RHS.
pub(super) fn find_rhs_str<'a>(line: &'a str, var_name: &str) -> Option<&'a str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
        return None;
    }
    let pos = line.find(var_name)?;
    // Whole-word check: character before var_name must not be alphanumeric or `_`.
    if pos > 0 {
        let b = line.as_bytes()[pos - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            return None;
        }
    }
    // Whole-word check: character after var_name must not be alphanumeric or `_`.
    let end = pos + var_name.len();
    let c_after = line.as_bytes().get(end).copied().unwrap_or(b' ');
    if c_after.is_ascii_alphanumeric() || c_after == b'_' {
        return None;
    }
    // Reject type-annotation position: last non-space token before name is `:`, `,`, or `<`.
    let last_tok = line[..pos].trim_end().chars().last().unwrap_or(' ');
    if last_tok == ':' || last_tok == ',' || last_tok == '<' {
        return None;
    }
    // Find `=` after the name, skipping whitespace.
    let after = &line[end..];
    let trimmed_after = after.trim_start();
    if !trimmed_after.starts_with('=') {
        return None;
    }
    // Reject `==` and `=>`.
    let next = trimmed_after.as_bytes().get(1).copied().unwrap_or(b' ');
    if next == b'=' || next == b'>' {
        return None;
    }
    Some(trimmed_after[1..].trim_start())
}

/// Attempt to infer the type of `var_name` from the right-hand side of an assignment.
/// Infer a Kotlin type from a single RHS assignment line without an explicit type annotation.
///
/// Called as a secondary/tertiary scan when no explicit type annotation (`var_name:`)
/// is found.  Handles the most common Android/Kotlin patterns:
///
/// 1. Constructor call:  `val x = SomeType(args)` → `"SomeType"`
/// 2. DI generic:        `val x = inject<SomeType>()` → `"SomeType"`
/// 3. Class literal arg: `val x = recv.create(SomeType::class.java)` → `"SomeType"`
///    Only matches when `::class` is *inside* argument parens (not `val k = T::class`).
///
/// Returns `None` when none of the patterns match.
pub(super) fn infer_from_rhs_assignment(line: &str, var_name: &str) -> Option<String> {
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
        let base = dotted.split('.').next_back().unwrap_or(&dotted);
        if base.starts_with_uppercase() {
            let after_ident = rhs[dotted.len()..].trim_start();
            if after_ident.starts_with('(') || after_ident.starts_with('{') {
                return Some(base.to_owned());
            }
        }
    }

    // Pattern 3: class literal argument — `recv.method(TypeName::class` where
    // `::class` appears after `(` in the RHS.  This is the Retrofit pattern:
    //   val api = retrofit.create(DashboardApi::class.java)
    // Deliberately narrow: only matches when the `::class` is inside parens, so
    // `val key = SomeType::class` (bare class ref, key is KClass<T>) is NOT matched.
    if let Some(paren_pos) = rhs.find('(') {
        let inside = &rhs[paren_pos + 1..];
        if let Some(class_pos) = inside.find("::class") {
            let before_class = inside[..class_pos].trim_end();
            let type_name = before_class
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next_back()
                .unwrap_or("");
            if !type_name.is_empty() && type_name.starts_with_uppercase() {
                return Some(type_name.to_owned());
            }
        }
    }

    None
}

// ─── Method return-type detail parsing ───────────────────────────────────────

/// Returns `true` when the first function call in `rhs` (opening paren at
/// `paren_pos`) is followed by a dot at depth 0, indicating a method chain.
///
/// `"getFoo(args).bar()"` → `true`   (chained — don't infer from `getFoo` alone)
/// `"getFoo(args)"` → `false`        (standalone — safe to use `getFoo`'s return type)
pub(super) fn has_dot_after_first_call(rhs: &str, paren_pos: usize) -> bool {
    let mut depth2 = 0i32;
    let mut past_close = false;
    for c in rhs[paren_pos..].chars() {
        match c {
            '(' | '[' | '{' => depth2 += 1,
            ')' | ']' | '}' => {
                depth2 -= 1;
                if depth2 == 0 {
                    past_close = true;
                }
            }
            '.' if past_close => return true,
            c if past_close && !c.is_whitespace() => return false,
            _ => {}
        }
    }
    false
}

/// Parse the return type from a `SymbolEntry.detail` signature string.
///
/// `"fun getDetail(req: Req): Response<Data>"` → `"Response<Data>"`
/// `"fun doSomething()"` → `None`
pub(super) fn extract_return_type_from_detail(detail: &str) -> Option<String> {
    let close_paren = detail.rfind(')')?;
    let after = detail[close_paren + 1..].trim_start();
    if !after.starts_with(':') {
        return None;
    }
    let type_part = after[1..].trim_start();
    let type_name = extract_type_with_generics(type_part);
    if !type_name.is_empty() && type_name.starts_with_uppercase() {
        Some(type_name)
    } else {
        None
    }
}

/// Extract a type name (with generics) from the start of a string.
///
/// `"List<Product> = emptyList()"` → `"List<Product>"`
/// `"StateFlow<UiState>"` → `"StateFlow<UiState>"`
/// `"User?"` → `"User"`  (nullable stripped at the outer `?`)
pub(crate) fn extract_type_with_generics(s: &str) -> String {
    let mut result = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '<' => {
                depth += 1;
                result.push(c);
            }
            '>' => {
                if depth > 0 {
                    depth -= 1;
                    result.push(c);
                    if depth == 0 {
                        break;
                    }
                } else {
                    break;
                }
            }
            // Stop at these outside of generic brackets.
            '?' | ' ' | '=' | ',' | ')' | '\n' if depth == 0 => break,
            _ => result.push(c),
        }
    }
    result
}

// ─── Declaration range lookup ─────────────────────────────────────────────────

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
    let lambda_brace = format!("{{ {name} ->"); // with brace prefix

    for (line_num, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }

        // ── typed parameter / val / var ─────────────────────────────────────
        if line.contains(&typed_pattern) {
            if let Some(pos) = line.find(&typed_pattern) {
                let before = line[..pos].chars().last();
                if !before
                    .map(|c| c.is_alphanumeric() || c == '_')
                    .unwrap_or(false)
                    && !line[..pos].trim_end().ends_with('"')
                {
                    let col = line[..pos].encode_utf16().count() as u32;
                    let len = name.encode_utf16().count() as u32;
                    return Some(Range {
                        start: Position {
                            line: line_num as u32,
                            character: col,
                        },
                        end: Position {
                            line: line_num as u32,
                            character: col + len,
                        },
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
                || trimmed.starts_with(&format!("{name},")) // multi-param `a, b ->`
                || (trimmed.contains(&lambda_arrow)
                    && line[..line.find(&lambda_arrow).unwrap_or(0)]
                        .chars()
                        .all(|c| {
                            c.is_whitespace()
                                || c == '{'
                                || c == '('
                                || c == ','
                                || c.is_alphanumeric()
                                || c == '_'
                        }));
            if is_lambda {
                if let Some(pos) = line.find(name) {
                    // Make sure we matched the right token (word boundary check)
                    let before = pos
                        .checked_sub(1)
                        .and_then(|i| line.as_bytes().get(i))
                        .copied();
                    let after = line.as_bytes().get(pos + name.len()).copied();
                    let boundary = before
                        .map(|b| !b.is_ascii_alphanumeric() && b != b'_')
                        .unwrap_or(true)
                        && after
                            .map(|b| !b.is_ascii_alphanumeric() && b != b'_')
                            .unwrap_or(true);
                    if boundary {
                        let col = line[..pos].encode_utf16().count() as u32;
                        let len = name.encode_utf16().count() as u32;
                        return Some(Range {
                            start: Position {
                                line: line_num as u32,
                                character: col,
                            },
                            end: Position {
                                line: line_num as u32,
                                character: col + len,
                            },
                        });
                    }
                }
            }
        }
    }
    None
}
