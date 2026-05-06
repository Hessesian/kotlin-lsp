//! Pure call-argument parsing helpers for the Kotlin indexer.
//!
//! All public items are `pub(crate)`.  No hidden state: functions accept pure
//! string/slice inputs or an immutable `&Indexer` reference for look-ups.

use tower_lsp::lsp_types::Url;

use crate::indexer::infer::sig::{
    find_fun_signature_full, nth_fun_param_type_str, split_params_at_depth_zero,
};
use crate::indexer::NodeExt;
use crate::indexer::{find_enclosing_call_name, is_id_char, Indexer};
use crate::queries::KIND_CALL_EXPR;
use crate::types::CursorPos;
use crate::StrExt;

// ─── Type-directed call-argument inference ────────────────────────────────────

/// Lines to scan backward when searching for the enclosing function call in inlay-hint inference.
const ARG_SCAN_BACK_LINES: usize = 20;

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
    lines: &[String],
    pos: CursorPos,
    idx: &Indexer,
    uri: &Url,
) -> Option<String> {
    let (before_cursor, cursor_column) = line_prefix_before_cursor(lines, pos)?;

    if let Some(result) = cst_call_arg_type(pos, idx, uri) {
        return Some(result);
    }

    if let Some(result) =
        find_named_call_arg_type(lines, pos.line, cursor_column, idx, uri, before_cursor)
    {
        return Some(result);
    }

    find_positional_call_arg_type(lines, pos.line, cursor_column, idx, uri)
}

struct CallContext {
    function_name: String,
    argument_position: usize,
}

fn line_prefix_before_cursor(lines: &[String], pos: CursorPos) -> Option<(&str, usize)> {
    let line = lines.get(pos.line)?;
    let byte_end = crate::indexer::live_tree::utf16_col_to_byte(line, pos.utf16_col);
    let before_cursor = &line[..byte_end];
    Some((before_cursor, before_cursor.chars().count()))
}

fn find_named_call_arg_type(
    lines: &[String],
    cursor_line: usize,
    cursor_column: usize,
    idx: &Indexer,
    uri: &Url,
    before_cursor: &str,
) -> Option<String> {
    let before_assignment = before_cursor.trim_end().strip_suffix('=')?;
    if before_assignment.ends_with(|ch: char| "!<>=".contains(ch)) {
        return None;
    }

    let before_assignment = before_assignment.trim_end();
    let identifier_start = before_assignment
        .rfind(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .map_or(0, |index| index + 1);
    let named_argument = &before_assignment[identifier_start..];
    if named_argument.is_empty() || named_argument.chars().next().is_none_or(char::is_uppercase) {
        return None;
    }

    let preceding_char = before_assignment[..identifier_start]
        .trim_end()
        .chars()
        .last();
    if !matches!(preceding_char, Some('(') | Some(',')) {
        return None;
    }

    let full_name = find_enclosing_call_name(lines, cursor_line, cursor_column)?;
    let function_name = full_name
        .split('.')
        .next_back()
        .filter(|name| !name.is_empty())?;
    let signature = find_fun_signature_full(function_name, idx, uri)?;
    let parameter_type = find_named_param_type_in_sig(&signature, named_argument)?;
    normalize_parameter_base_type(&parameter_type)
}

fn find_positional_call_arg_type(
    lines: &[String],
    cursor_line: usize,
    cursor_column: usize,
    idx: &Indexer,
    uri: &Url,
) -> Option<String> {
    let call_context = find_call_context(lines, cursor_line, cursor_column)?;
    let signature = find_fun_signature_full(&call_context.function_name, idx, uri)?;
    let parameter_type = nth_fun_param_type_str(&signature, call_context.argument_position)?;
    normalize_parameter_base_type(&parameter_type)
}

fn find_call_context(
    lines: &[String],
    cursor_line: usize,
    cursor_column: usize,
) -> Option<CallContext> {
    let mut depth: i32 = 0;
    let mut brace_depth: i32 = 0;
    let mut argument_position: usize = 0;
    let scan_start = cursor_line.saturating_sub(ARG_SCAN_BACK_LINES);

    for line_index in (scan_start..=cursor_line).rev() {
        let chars: Vec<char> = lines[line_index].chars().collect();
        let scan_to = if line_index == cursor_line {
            cursor_column.min(chars.len())
        } else {
            chars.len()
        };

        for column_index in (0..scan_to).rev() {
            match chars[column_index] {
                '{' if column_index > 0 && chars[column_index - 1] == '$' => {}
                '}' => brace_depth += 1,
                '{' => {
                    brace_depth -= 1;
                    if brace_depth < 0 {
                        return None;
                    }
                }
                ')' | ']' => depth += 1,
                '>' if column_index == 0 || chars[column_index - 1] != '-' => depth += 1,
                '(' | '[' => {
                    depth -= 1;
                    if depth < 0 {
                        let function_name = extract_called_function_name(&chars, column_index)?;
                        return Some(CallContext {
                            function_name,
                            argument_position,
                        });
                    }
                }
                '<' if depth > 0 => depth -= 1,
                ',' if depth == 0 => argument_position += 1,
                _ => {}
            }
        }
    }

    None
}

fn extract_called_function_name(chars: &[char], opening_paren_index: usize) -> Option<String> {
    if opening_paren_index == 0 {
        return None;
    }

    let mut identifier_start = opening_paren_index;
    while identifier_start > 0
        && (is_id_char(chars[identifier_start - 1]) || chars[identifier_start - 1] == '.')
    {
        identifier_start -= 1;
    }
    if identifier_start >= opening_paren_index {
        return None;
    }

    let full_name: String = chars[identifier_start..opening_paren_index]
        .iter()
        .collect();
    full_name
        .trim_matches('.')
        .split('.')
        .next_back()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn normalize_parameter_base_type(parameter_type: &str) -> Option<String> {
    let base_type = parameter_type.trim().ident_prefix();
    if base_type.is_empty() {
        None
    } else {
        Some(base_type.to_owned())
    }
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
    if s.ends_with(|c: char| "!<>=".contains(c)) {
        return None;
    }
    let s = s.trim_end();
    // Extract trailing identifier
    let ident_start = s
        .rfind(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|i| i + 1)
        .unwrap_or(0);
    let ident = &s[ident_start..];
    if ident.is_empty() {
        return None;
    }
    // Named args start with a lowercase letter
    if ident.starts_with_uppercase() {
        return None;
    }
    // Require the prefix to be only whitespace (optionally preceded by a comma).
    // This prevents `(isRefresh = {` from matching — the `(` before `isRefresh`
    // makes the prefix non-empty after stripping commas and whitespace.
    let prefix = s[..ident_start]
        .trim_start()
        .trim_start_matches(',')
        .trim_start();
    if !prefix.is_empty() {
        return None;
    }
    Some(ident)
}

/// Find the type string of a named parameter `param_name` inside a
/// comma-separated parameter list text (output of `collect_fun_params_text`).
///
/// Handles `val`/`var` prefixes, strips them. Returns the full type string
/// (may be a functional type like `(String, Boolean) -> Unit`).
pub(crate) fn find_named_param_type_in_sig(sig: &str, param_name: &str) -> Option<String> {
    let parts = split_params_at_depth_zero(sig);

    let colon_pat = format!("{param_name}:");
    for part in parts {
        let part = part
            .trim()
            .trim_start_matches("val ")
            .trim_start_matches("var ");
        // Exact param_name match (no suffix)
        let Some(col_pos) = part.find(&colon_pat) else {
            continue;
        };
        let before = &part[..col_pos];
        if before
            .chars()
            .last()
            .map(|c| c.is_alphanumeric() || c == '_')
            .unwrap_or(false)
        {
            continue; // suffix match like `otherParam:`
        }
        let after = part[col_pos + colon_pat.len()..].trim();
        if !after.is_empty() {
            return Some(after.to_owned());
        }
    }
    None
}

// ─── Lambda parameter helpers ─────────────────────────────────────────────────

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
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                i += 1;
            }
            b'-' if depth == 0 && i + 1 < bytes.len() && bytes[i + 1] == b'>' => {
                arrow_pos = Some(i);
                break;
            }
            _ => {
                i += 1;
            }
        }
    }
    let Some(ap) = arrow_pos else {
        return false;
    };
    let before_arrow = s[..ap].trim_end();
    // All tokens before `->` must be valid identifiers.
    // If any non-`it`, non-`_` identifier is present, it's a named-param lambda.
    for tok in before_arrow.split(',') {
        let tok = tok.trim();
        let name: String = tok
            .chars()
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
            ')' | ']' => {
                if depth == 0 {
                    end = i;
                    break;
                }
                depth -= 1;
            }
            // Skip the `>` in `->` (lambda arrow) and never go negative.
            '>' if prev != '-' && depth > 0 => depth -= 1,
            ',' if depth == 0 => {
                end = i;
                break;
            }
            _ => {}
        }
        prev = ch;
    }
    let arg = rest[..end].trim();
    if arg.is_empty() {
        None
    } else {
        Some(arg)
    }
}

// ─── CST helpers for call-argument type inference ────────────────────────────

/// CST fast path: walk from cursor up to `value_argument`, then to
/// `call_expression`, and look up the expected parameter type.
///
/// Returns `None` when:
/// - no live tree is available (falls through to text scan)
/// - cursor is inside a lambda literal rather than a direct call argument
/// - the enclosing function is not indexed
fn cst_call_arg_type(pos: CursorPos, idx: &Indexer, uri: &Url) -> Option<String> {
    use crate::indexer::live_tree::utf16_col_to_byte;
    use tree_sitter::Point;

    let doc = idx.live_doc(uri)?;
    let bytes = &doc.bytes;

    // Get the line text to convert UTF-16 col → byte offset.
    let line_text = std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.lines().nth(pos.line))
        .unwrap_or("");
    let byte_col = utf16_col_to_byte(line_text, pos.utf16_col);
    let point = Point {
        row: pos.line,
        column: byte_col,
    };
    let start_node = doc
        .tree
        .root_node()
        .descendant_for_point_range(point, point)?;

    // Walk up: look for value_argument; bail out if we hit lambda_literal first.
    let mut cur = start_node;
    let value_arg = loop {
        match cur.kind() {
            "value_argument" => break Some(cur),
            "lambda_literal" => break None,
            _ => match cur.parent() {
                Some(p) => cur = p,
                None => break None,
            },
        }
    }?;

    // Walk up from value_argument to call_expression.
    let mut node = value_arg;
    let call_expr = loop {
        match node.parent() {
            Some(p) if p.kind() == KIND_CALL_EXPR => break Some(p),
            Some(p) => node = p,
            None => break None,
        }
    }?;

    let fn_name = call_expr.call_fn_name(bytes)?;
    let sig = find_fun_signature_full(&fn_name, idx, uri)?;

    // Named arg: value_argument has [simple_identifier "="] prefix in grammar.
    let param_type = if let Some(arg_name) = value_arg.named_arg_label(bytes) {
        find_named_param_type_in_sig(&sig, &arg_name)
    } else {
        nth_fun_param_type_str(&sig, value_arg.value_arg_position())
    }?;

    let base = param_type.trim().ident_prefix();
    if base.is_empty() {
        None
    } else {
        Some(base)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "args_tests.rs"]
mod tests;
