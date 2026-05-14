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
pub(crate) fn first_type_arg(s: &str) -> &str {
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

// ─── smart cast narrowing ─────────────────────────────────────────────────────

/// Maximum lines to scan backward for `when(subject)` or `if (x is T)`.
const SMART_CAST_SCAN_LINES: usize = 30;

/// Detect smart cast narrowing at a given line position.
///
/// Handles two patterns:
/// 1. `when (var) { is Type -> ... }` — cursor inside the `is Type` branch
/// 2. `if (var is Type)` / `else if (var is Type)` — cursor inside that block
///
/// Returns the narrowed type name (e.g. `"Event.OnSomethingClick"`) or `None`.
pub(crate) fn smart_cast_type_at_line(
    lines: &[String],
    var_name: &str,
    line: u32,
) -> Option<String> {
    let line_idx = line as usize;
    if line_idx >= lines.len() {
        return None;
    }

    // Strategy 1: `when (var)` block — find `is Type` on current or preceding branch line
    if let Some(ty) = when_branch_smart_cast(lines, var_name, line_idx) {
        return Some(ty);
    }

    // Strategy 2: `if (var is Type)` block
    if_is_smart_cast(lines, var_name, line_idx)
}

/// Check if cursor is inside a `when (var_name)` block and extract the `is Type` from
/// the enclosing branch.
///
/// Handles nested when: scans backward through multiple when levels, matching each
/// `is X ->` branch to its nearest enclosing `when (subject)`.
fn when_branch_smart_cast(lines: &[String], var_name: &str, line_idx: usize) -> Option<String> {
    let start = line_idx.saturating_sub(SMART_CAST_SCAN_LINES);

    // Track brace depth while scanning backward from cursor.
    // depth=0 at cursor. Opening `{` going backward means we exit a scope (depth-=1),
    // closing `}` means we enter a nested scope (depth+=1).
    let mut depth: i32 = 0;
    let mut branch_type: Option<String> = None;

    for i in (start..=line_idx).rev() {
        let trimmed = lines[i].trim();

        // Track brace depth (backward: `{` decreases, `}` increases)
        let opens = trimmed.chars().filter(|&c| c == '{').count() as i32;
        let closes = trimmed.chars().filter(|&c| c == '}').count() as i32;
        depth += closes - opens;

        // Stop at function/class boundary
        if trimmed.starts_with("fun ")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("object ")
        {
            break;
        }

        // If we haven't found our branch yet, look for `is Type ->`
        if branch_type.is_none() {
            if let Some(ty) = extract_is_type_from_when_branch(trimmed) {
                branch_type = Some(ty);
                continue;
            }
            // Stop at `else ->` or other non-`is` branch boundaries.
            if trimmed.contains(" ->") && !trimmed.starts_with("is ") {
                break;
            }
            // Stop if we hit a closing brace without finding a branch
            if depth > 0 {
                break;
            }
            continue;
        }

        // We have a branch — look for the when that owns it.
        // The when statement is one level OUT from the branch (depth should be -1 here
        // relative to where we found the branch, or the when is on a line that decreases
        // depth further).
        if is_when_subject(trimmed, var_name) {
            return branch_type;
        }
        // If we find a `when (something_else)` that doesn't match our var, this branch
        // belongs to that inner when — our var isn't narrowed by it. Reset and keep looking.
        if trimmed.contains("when") && trimmed.contains('(') {
            branch_type = None;
            // This line may also be a branch for an outer when:
            // e.g. `is Banner -> when (inner) {`
            if let Some(ty) = extract_is_type_from_when_branch(trimmed) {
                branch_type = Some(ty);
            }
        }
    }
    None
}

/// Check if cursor is inside an `if (var is Type)` or `else if (var is Type)` block.
fn if_is_smart_cast(lines: &[String], var_name: &str, line_idx: usize) -> Option<String> {
    let start = line_idx.saturating_sub(SMART_CAST_SCAN_LINES);

    // Scan backward for `if (var_name is Type` or `} else if (var_name is Type`
    // while staying at the cursor's nesting level.
    let mut brace_depth: usize = 0;
    for i in (start..line_idx).rev() {
        let trimmed = lines[i].trim();

        if brace_depth == 0 {
            if let Some(ty) = extract_if_is_type(trimmed, var_name) {
                let opens = trimmed.chars().filter(|&c| c == '{').count();
                let closes = trimmed.chars().filter(|&c| c == '}').count();
                if opens == 0 || opens != closes {
                    return Some(ty);
                }
            }
            if (trimmed.ends_with('{') || trimmed == "{") && i > start {
                if let Some(ty) = extract_if_is_type(lines[i - 1].trim(), var_name) {
                    return Some(ty);
                }
            }
        }

        for ch in trimmed.chars().rev() {
            match ch {
                '}' => brace_depth += 1,
                '{' => brace_depth = brace_depth.saturating_sub(1),
                _ => {}
            }
        }

        if trimmed.starts_with("fun ")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("object ")
        {
            break;
        }
    }
    None
}

/// Extract `Type` from a when-branch line like `is Event.OnClick ->` or `is Event.OnClick ->`
fn extract_is_type_from_when_branch(trimmed: &str) -> Option<String> {
    // Pattern: `is TypeName ->` or `is TypeName<...> ->`
    let after_is = trimmed.strip_prefix("is ")?;
    let type_end = after_is.find(" ->")?;
    let type_str = &after_is[..type_end];

    let mut end = 0usize;
    let mut generic_depth = 0usize;
    for (index, ch) in type_str.char_indices() {
        match ch {
            '<' => generic_depth += 1,
            '>' => generic_depth = generic_depth.saturating_sub(1),
            ',' if generic_depth == 0 => break,
            _ => {}
        }
        end = index + ch.len_utf8();
    }

    let type_str = type_str[..end].trim();
    if type_str.is_empty() {
        return None;
    }
    // Validate first char is uppercase
    if !type_str.chars().next()?.is_uppercase() {
        return None;
    }
    Some(type_str.to_string())
}

/// Check if line is `when (var_name)` or `when (val x = var_name)` etc.
fn is_when_subject(trimmed: &str, var_name: &str) -> bool {
    // Match: `when (var_name)` anywhere in the line — handles both
    // standalone `when (x) {` and inline `is Foo -> when (x) {`
    let pattern = "when";
    let mut search_from = 0;
    while let Some(pos) = trimmed[search_from..].find(pattern) {
        let abs_pos = search_from + pos;
        let after_when = trimmed[abs_pos + pattern.len()..].trim_start();
        if let Some(inner) = after_when.strip_prefix('(') {
            let inner = inner.trim();
            // Subject must be exactly var_name followed by `)` (possibly with whitespace)
            if let Some(rest) = inner.strip_prefix(var_name) {
                if rest.trim_start().starts_with(')') {
                    return true;
                }
            }
        }
        search_from = abs_pos + 1;
    }
    false
}

/// Extract type from `if (var_name is Type)` or `else if (var_name is Type)`
fn extract_if_is_type(trimmed: &str, var_name: &str) -> Option<String> {
    let is_pattern = format!("{var_name} is ");
    let is_identifier_char = |c: char| c.is_alphanumeric() || c == '_';
    let mut search_from = 0usize;
    let pos = loop {
        let rel = trimmed[search_from..].find(&is_pattern)?;
        let pos = search_from + rel;
        if pos == 0
            || !trimmed[..pos]
                .chars()
                .next_back()
                .is_some_and(|c| is_identifier_char(c) || c == '.')
        {
            break pos;
        }
        search_from = pos + 1;
    };

    let before = &trimmed[..pos];
    if !before.contains("if") && !before.contains("else") {
        return None;
    }

    let after = &trimmed[pos + is_pattern.len()..];
    let mut type_str = String::new();
    let mut generic_depth = 0usize;
    let mut chars = after.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '<' => {
                generic_depth += 1;
                type_str.push(ch);
            }
            '>' => {
                generic_depth = generic_depth.saturating_sub(1);
                type_str.push(ch);
            }
            ',' | ')' | '{' if generic_depth == 0 => break,
            '&' | '|' if generic_depth == 0 && chars.peek() == Some(&ch) => break,
            _ => type_str.push(ch),
        }
    }

    let type_str = type_str.trim().trim_end_matches('.');
    if type_str.is_empty() || !type_str.starts_with(|c: char| c.is_uppercase()) {
        return None;
    }
    Some(type_str.to_string())
}
