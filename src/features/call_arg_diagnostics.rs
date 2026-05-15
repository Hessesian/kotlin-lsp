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
    collect_all_fun_params_texts, find_fun_signature_with_receiver, live_tree::LiveDoc,
    split_params_at_depth_zero, Indexer, NodeExt,
};
use crate::queries::{KIND_CALL_EXPR, KIND_CALL_SUFFIX, KIND_LAMBDA_LIT, KIND_VALUE_ARG};
use crate::util::is_test_file;

/// Scan a file for call-argument count mismatches and return diagnostics.
///
/// The caller provides a `LiveDoc` parsed from the *same text* that was just
/// indexed, guaranteeing the CST and the indexed signature data are consistent.
pub(crate) fn call_arg_diagnostics(indexer: &Indexer, uri: &Url, doc: &LiveDoc) -> Vec<Diagnostic> {
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

    // Skip unqualified calls inside scope-function lambdas (apply/run/with/let/also).
    // These have an implicit `this` receiver that we cannot resolve, so global lookup
    // would match the wrong overload.
    if qualifier.is_none() && is_inside_scope_function_lambda(call_node, bytes) {
        return None;
    }

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

    // Skip vararg functions (Kotlin `vararg` or Java `...`)
    if params_text.contains("vararg ")
        || params_text.contains("vararg\t")
        || params_text.contains("...")
    {
        return None;
    }

    // Prefer CST-derived param_counts from index; fall back to text-based counting
    let (required, total) = resolve_param_counts(indexer, uri, &fn_name, qualifier.as_deref())
        .unwrap_or_else(|| count_params(params_text));

    if provided_count < required {
        let range = diagnostic_range(call_node, value_arguments.as_ref());
        let message = if required == total {
            format!("{fn_name}: expected {required} argument(s), found {provided_count}")
        } else {
            format!("{fn_name}: expected at least {required} argument(s), found {provided_count}")
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
        let message =
            format!("{fn_name}: expected at most {total} argument(s), found {provided_count}");
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

/// Check if the call is inside a lambda that belongs to a scope function
/// (apply, run, with, let, also). Unqualified calls inside these lambdas
/// have an implicit `this` receiver we cannot resolve.
fn is_inside_scope_function_lambda(call_node: &tree_sitter::Node, bytes: &[u8]) -> bool {
    const SCOPE_FUNCTIONS: &[&str] = &[
        "apply",
        "run",
        "with",
        "let",
        "also",
        "takeIf",
        "takeUnless",
    ];

    let mut node = *call_node;
    for _ in 0..15 {
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.kind() == KIND_LAMBDA_LIT {
            // Walk up from the lambda to find the enclosing call_expression
            if let Some(call_ancestor) = find_enclosing_call(&parent) {
                if let Some((name, _)) = call_ancestor.call_fn_and_qualifier(bytes) {
                    if SCOPE_FUNCTIONS.contains(&name.as_str()) {
                        return true;
                    }
                }
            }
            return false;
        }
        node = parent;
    }
    false
}

fn find_enclosing_call<'a>(node: &tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    let mut current = node.parent()?;
    for _ in 0..5 {
        if current.kind() == KIND_CALL_EXPR {
            return Some(current);
        }
        current = current.parent()?;
    }
    None
}

/// Check if the call has a trailing lambda (lambda as last argument outside parens).
/// CST patterns:
/// - `foo { }` → call_suffix → annotated_lambda → lambda_literal
/// - `foo(a) { }` → outer call_expression wraps inner call_expression + call_suffix(lambda)
/// - Incomplete `foo(a) {` → tree-sitter error recovery may place `{` as a sibling
///
/// Tree-sitter splits `foo(a) { }` into nested call_expressions:
///   call_expression (outer)
///     call_expression (inner) → foo(a)
///     call_suffix → annotated_lambda → lambda_literal
/// We check both the node itself AND its parent for the lambda suffix.
fn has_trailing_lambda(call_node: &tree_sitter::Node) -> bool {
    if check_lambda_in_children(call_node) {
        return true;
    }

    // Nested call_expression: the lambda lives on the parent call_expression
    if let Some(parent) = call_node.parent() {
        if parent.kind() == KIND_CALL_EXPR && check_lambda_in_children(&parent) {
            return true;
        }
    }

    // Incomplete code: tree-sitter may place the `{` as a next sibling
    // outside the call_expression (e.g. `withContext(x) {` with no closing `}`).
    if let Some(next) = call_node.next_sibling() {
        let kind = next.kind();
        if kind == "{" || kind == KIND_LAMBDA_LIT {
            return true;
        }
        if kind == KIND_CALL_SUFFIX && contains_lambda(&next) {
            return true;
        }
        // ERROR node starting with `{` — likely an incomplete lambda
        if kind == "ERROR" {
            if let Some(first_child) = next.child(0) {
                if first_child.kind() == "{" {
                    return true;
                }
            }
        }
    }
    false
}

fn check_lambda_in_children(node: &tree_sitter::Node) -> bool {
    for i in 0..node.child_count() {
        let Some(child) = node.child(i) else {
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

/// Resolve `(required, total)` param counts directly from `SymbolEntry::param_counts`.
/// Returns `None` if no matching symbol found or if the symbol was never indexed
/// with param_counts (non-callable symbols like interfaces/fields).
fn resolve_param_counts(
    indexer: &Indexer,
    uri: &Url,
    fn_name: &str,
    qualifier: Option<&str>,
) -> Option<(usize, usize)> {
    // If qualified, resolve receiver type first
    if let Some(recv) = qualifier {
        use crate::resolver::infer::{infer_receiver_type, ReceiverKind};
        let rt = infer_receiver_type(indexer, ReceiverKind::Variable(recv), uri)?;
        let locs = indexer.resolve_symbol(&rt.outer, None, uri);
        for loc in &locs {
            if let Some(data) = indexer.files.get(loc.uri.as_str()) {
                if let Some(sym) = data
                    .symbols
                    .iter()
                    .find(|s| s.name == fn_name && is_callable_kind(s.kind))
                {
                    return Some((sym.param_counts.0 as usize, sym.param_counts.1 as usize));
                }
            }
        }
        return None;
    }
    // Unqualified: check definitions and current file
    let mut found: Option<(u8, u8)> = None;

    // For unqualified calls, prefer same-file definitions to avoid false
    // "overloaded" results from hundreds of same-name functions across the workspace.
    if qualifier.is_none() {
        if let Some(data) = indexer.files.get(uri.as_str()) {
            for sym in data
                .symbols
                .iter()
                .filter(|s| s.name == fn_name && is_callable_kind(s.kind))
            {
                if let Some(prev) = found {
                    if prev != sym.param_counts {
                        return None;
                    }
                }
                found = Some(sym.param_counts);
            }
        }
        if found.is_some() {
            return found.map(|(r, t)| (r as usize, t as usize));
        }
    }

    if let Some(locs) = indexer.definitions.get(fn_name) {
        for loc in locs.iter() {
            if is_test_file(loc.uri.as_str()) {
                continue;
            }
            if let Some(data) = indexer.files.get(loc.uri.as_str()) {
                for sym in data
                    .symbols
                    .iter()
                    .filter(|s| s.name == fn_name && is_callable_kind(s.kind))
                {
                    if let Some(prev) = found {
                        if prev != sym.param_counts {
                            return None; // overloaded — let caller skip
                        }
                    }
                    found = Some(sym.param_counts);
                }
            }
        }
    }
    // Also current file
    if let Some(data) = indexer.files.get(uri.as_str()) {
        for sym in data
            .symbols
            .iter()
            .filter(|s| s.name == fn_name && is_callable_kind(s.kind))
        {
            if let Some(prev) = found {
                if prev != sym.param_counts {
                    return None;
                }
            }
            found = Some(sym.param_counts);
        }
    }
    found.map(|(r, t)| (r as usize, t as usize))
}

fn is_callable_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::FUNCTION | SymbolKind::METHOD | SymbolKind::CONSTRUCTOR
    )
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

    // If a qualifier (receiver) is present but type could not be resolved,
    // don't fall through to a global name scan — it would match unrelated
    // functions from other classes (e.g. `cancel`, `show`).
    if qualifier.is_some() && receiver_sig.is_none() {
        return Vec::new();
    }

    // Also collect all same-name signatures across indexed files for overload detection
    let mut all_sigs: Vec<String> = Vec::new();

    // For unqualified calls (no receiver), check the current file first.
    // If the function is defined here, use only those definitions — global
    // lookup across the whole workspace would match hundreds of unrelated
    // same-name overrides (e.g. 945 `loadData` implementations) causing
    // a false "overloaded — skip" result.
    let current_sigs = collect_all_fun_params_texts(fn_name, uri.as_str(), indexer);
    if qualifier.is_none() && !current_sigs.is_empty() {
        // For unqualified calls, don't use receiver_sig — it comes from a global
        // name scan that may find only one overload, masking same-file overloads.
        // Check arity diversity first; if ambiguous, return all so caller skips.
        let arities: Vec<(usize, usize)> = current_sigs.iter().map(|s| count_params(s)).collect();
        let unique: std::collections::HashSet<_> = arities.into_iter().collect();
        return if unique.len() > 1 {
            current_sigs // caller sees len > 1 → skip
        } else {
            current_sigs.into_iter().take(1).collect()
        };
    }

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

    // Also include current file (may already be in definitions, but ensure coverage)
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
