//! Diagnostic: detect call sites with mismatched argument counts.
//!
//! Walks the live CST for all `call_expression` nodes, resolves the callee's
//! signature, and emits a warning when the number of provided arguments does
//! not match the required parameters.
//!
//! Skipped cases (too ambiguous without full type resolution):
//! - Calls with named arguments
//! - Calls with trailing lambdas
//! - Overloaded functions (multiple signatures found)
//! - Signatures containing `vararg`

use tower_lsp::lsp_types::*;

use crate::indexer::{
    collect_all_fun_params_texts, find_fun_signature_with_receiver, split_params_at_depth_zero,
    Indexer, NodeExt,
};
use crate::queries::{KIND_CALL_EXPR, KIND_CALL_SUFFIX, KIND_LAMBDA_LIT, KIND_VALUE_ARG};

/// Scan a file for call-argument count mismatches and return diagnostics.
pub(crate) fn call_arg_diagnostics(indexer: &Indexer, uri: &Url) -> Vec<Diagnostic> {
    let doc = match indexer.live_doc(uri) {
        Some(d) => d,
        None => return Vec::new(),
    };
    let bytes = &doc.bytes;
    let root = doc.tree.root_node();
    let mut diagnostics = Vec::new();
    collect_call_nodes(root, bytes, indexer, uri, &mut diagnostics);
    diagnostics
}

fn collect_call_nodes(
    node: tree_sitter::Node,
    bytes: &[u8],
    indexer: &Indexer,
    uri: &Url,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if node.kind() == KIND_CALL_EXPR {
        if let Some(diag) = check_call_args(&node, bytes, indexer, uri) {
            diagnostics.push(diag);
        }
    }

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_call_nodes(child, bytes, indexer, uri, diagnostics);
        }
    }
}

fn check_call_args(
    call_node: &tree_sitter::Node,
    bytes: &[u8],
    indexer: &Indexer,
    uri: &Url,
) -> Option<Diagnostic> {
    // Skip calls with trailing lambdas — too complex without SAM resolution
    if has_trailing_lambda(call_node) {
        return None;
    }

    let (fn_name, qualifier) = call_node.call_fn_and_qualifier(bytes)?;

    // Skip calls with named arguments — positional counting is invalid
    let value_arguments = call_node.find_value_arguments();
    let provided_count = count_provided_args(value_arguments.as_ref(), bytes);

    if has_named_args(value_arguments.as_ref(), bytes) {
        return None;
    }

    // Resolve signature(s)
    let signatures = resolve_signatures(indexer, uri, &fn_name, qualifier.as_deref());

    // Skip if no signatures found or overloaded (ambiguous)
    if signatures.is_empty() || signatures.len() > 1 {
        return None;
    }

    let params_text = &signatures[0];

    // Skip vararg functions
    if params_text.contains("vararg ") || params_text.contains("vararg\t") {
        return None;
    }

    let (required, total) = count_params(params_text);

    if provided_count < required {
        let range = diagnostic_range(call_node, value_arguments.as_ref());
        let message = if required == total {
            format!("expected {required} argument(s), found {provided_count}")
        } else {
            format!("expected at least {required} argument(s), found {provided_count}")
        };
        return Some(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("kotlin-lsp".into()),
            message,
            ..Default::default()
        });
    }

    if provided_count > total {
        let range = diagnostic_range(call_node, value_arguments.as_ref());
        let message = format!("expected at most {total} argument(s), found {provided_count}");
        return Some(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("kotlin-lsp".into()),
            message,
            ..Default::default()
        });
    }

    None
}

/// Check if the call has a trailing lambda (lambda as last argument outside parens).
/// CST patterns:
/// - `foo { }` → call_suffix → annotated_lambda → lambda_literal
/// - `foo(a) { }` → call_suffix → lambda_literal (or annotated_lambda → lambda_literal)
fn has_trailing_lambda(call_node: &tree_sitter::Node) -> bool {
    for i in 0..call_node.child_count() {
        let Some(child) = call_node.child(i) else {
            continue;
        };
        if child.kind() == KIND_LAMBDA_LIT {
            return true;
        }
        if child.kind() == KIND_CALL_SUFFIX && contains_lambda(&child) {
            return true;
        }
    }
    false
}

/// Recursively check if a node contains a lambda_literal (up to 3 levels deep).
fn contains_lambda(node: &tree_sitter::Node) -> bool {
    for i in 0..node.child_count() {
        let Some(child) = node.child(i) else {
            continue;
        };
        if child.kind() == KIND_LAMBDA_LIT {
            return true;
        }
        // Check annotated_lambda → lambda_literal
        for j in 0..child.child_count() {
            if let Some(gc) = child.child(j) {
                if gc.kind() == KIND_LAMBDA_LIT {
                    return true;
                }
            }
        }
    }
    false
}

/// Count `value_argument` children inside a `value_arguments` node.
fn count_provided_args(value_arguments: Option<&tree_sitter::Node>, bytes: &[u8]) -> usize {
    let Some(va) = value_arguments else {
        return 0;
    };
    let _ = bytes; // reserved for future use
    let mut count = 0;
    for i in 0..va.child_count() {
        if let Some(child) = va.child(i) {
            if child.kind() == KIND_VALUE_ARG {
                count += 1;
            }
        }
    }
    count
}

/// Check if any argument uses named-argument syntax (`label = expr`).
fn has_named_args(value_arguments: Option<&tree_sitter::Node>, bytes: &[u8]) -> bool {
    let Some(va) = value_arguments else {
        return false;
    };
    for i in 0..va.child_count() {
        if let Some(child) = va.child(i) {
            if child.kind() == KIND_VALUE_ARG && child.named_arg_label(bytes).is_some() {
                return true;
            }
        }
    }
    false
}

/// Resolve all known signatures for a function name.
/// Returns multiple entries if overloaded.
fn resolve_signatures(
    indexer: &Indexer,
    uri: &Url,
    fn_name: &str,
    qualifier: Option<&str>,
) -> Vec<String> {
    // First try receiver-aware lookup (returns a single best match)
    let receiver_sig = find_fun_signature_with_receiver(indexer, uri, fn_name, qualifier);
    let receiver_sig = if receiver_sig.is_empty() {
        None
    } else {
        Some(receiver_sig)
    };

    // Also collect all same-name signatures across indexed files for overload detection
    let mut all_sigs: Vec<String> = Vec::new();

    // Check definitions map for the function name
    if let Some(locs) = indexer.definitions.get(fn_name) {
        for loc in locs.iter() {
            if is_test_file(loc.uri.as_str()) {
                continue;
            }
            let sigs = collect_all_fun_params_texts(fn_name, loc.uri.as_str(), indexer);
            all_sigs.extend(sigs);
        }
    }

    // Also check current file
    let current_sigs = collect_all_fun_params_texts(fn_name, uri.as_str(), indexer);
    for sig in &current_sigs {
        if !all_sigs.contains(sig) {
            all_sigs.push(sig.clone());
        }
    }

    // Deduplicate by arity envelope (required..=total)
    let arities: Vec<(usize, usize)> = all_sigs.iter().map(|s| count_params(s)).collect();
    let unique_arities: std::collections::HashSet<(usize, usize)> = arities.into_iter().collect();

    // If multiple distinct arities exist → overloaded
    if unique_arities.len() > 1 {
        return all_sigs; // caller sees len > 1 → skip
    }

    // Return the receiver-aware signature if available, else first from all_sigs
    if let Some(sig) = receiver_sig {
        vec![sig]
    } else if let Some(sig) = all_sigs.into_iter().next() {
        vec![sig]
    } else {
        Vec::new()
    }
}

/// Heuristic: a file path likely belongs to a test source set.
fn is_test_file(uri_str: &str) -> bool {
    // Common Android/Gradle test source sets
    uri_str.contains("/src/test/")
        || uri_str.contains("/src/androidTest/")
        || uri_str.contains("/src/commonTest/")
        || uri_str.contains("/src/iosTest/")
}

/// Parse a parameter list and return `(required_count, total_count)`.
/// Parameters with default values (containing `=` at depth 0) are optional.
fn count_params(params_text: &str) -> (usize, usize) {
    let raw = params_text.trim_matches(|c| c == '(' || c == ')');
    let parts = split_params_at_depth_zero(raw);
    let params: Vec<&str> = parts
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let total = params.len();
    let required = params.iter().filter(|p| !has_default_value(p)).count();
    (required, total)
}

/// Check if a single parameter text has a default value (`=` at depth 0).
/// Example: `c: Boolean = true` → true, `f: (Int) -> String` → false
fn has_default_value(param: &str) -> bool {
    let mut depth: i32 = 0;
    let mut prev = '\0';
    // Find the first `:` at depth 0 — everything after is the type + optional default
    let mut past_colon = false;
    for ch in param.chars() {
        if !past_colon {
            if ch == ':' && depth == 0 {
                past_colon = true;
            }
            match ch {
                '(' | '<' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                '>' if prev != '-' && depth > 0 => depth -= 1,
                _ => {}
            }
            prev = ch;
            continue;
        }
        // After the type colon, look for `=` at depth 0
        match ch {
            '(' | '<' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '>' if prev != '-' && depth > 0 => depth -= 1,
            '=' if depth == 0 => return true,
            _ => {}
        }
        prev = ch;
    }
    false
}

/// Build the diagnostic range — prefer the `value_arguments` span, fall back to
/// the callee name span within the call expression.
fn diagnostic_range(
    call_node: &tree_sitter::Node,
    value_arguments: Option<&tree_sitter::Node>,
) -> Range {
    if let Some(va) = value_arguments {
        let start = va.start_position();
        let end = va.end_position();
        return Range::new(
            Position::new(start.row as u32, start.column as u32),
            Position::new(end.row as u32, end.column as u32),
        );
    }
    // Fallback: call expression start
    let start = call_node.start_position();
    let end = call_node.end_position();
    Range::new(
        Position::new(start.row as u32, start.column as u32),
        Position::new(end.row as u32, end.column as u32),
    )
}

#[cfg(test)]
#[path = "call_arg_diagnostics_tests.rs"]
mod tests;
