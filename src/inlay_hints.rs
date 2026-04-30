//! Inlay hint provider for Kotlin/Java files.
//!
//! Emits type hints for:
//! 1. Lambda implicit parameter `it` — shows `: Type` after `it`
//! 2. Named lambda parameters `{ item -> }` — shows `: Type` after the param name
//! 3. `this` inside scope functions / class methods — shows `: Type` after `this`
//! 4. Untyped local `val`/`var` declarations — shows `: InferredType` after the name
//!    (only when the type is determinable from the index without rg)
//!
//! Uses the live CST (tree-sitter parse tree stored in `Indexer::live_trees`) when
//! available, or re-parses on demand for files not currently open in the editor.

use std::sync::Arc;
use tower_lsp::lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Position, Range, Url};

use crate::indexer::Indexer;
use crate::indexer::NodeExt;
use crate::indexer::live_tree::{lang_for_path, parse_live};
use crate::resolver::{ReceiverKind, infer_receiver_type};

pub fn compute_inlay_hints(idx: &Arc<Indexer>, uri: &Url, range: Range) -> Vec<InlayHint> {
    // Fast path: editor has the file open → use pre-parsed live tree.
    if let Some(doc) = idx.live_doc(uri) {
        return cst_hints(idx, uri, &doc.tree, &doc.bytes, range);
    }

    // Fallback: reconstruct content from live_lines or indexed file data, then
    // re-parse. tree-sitter parses 5000 lines in ~3ms so this is not a regression.
    let lines_arc = idx.mem_lines_for(uri.as_str());
    let Some(lines) = lines_arc else { return vec![]; };
    if lines.is_empty() { return vec![]; }

    let content = lines.join("\n");
    let Some(lang) = lang_for_path(uri.path()) else { return vec![]; };
    let Some(doc)  = parse_live(&content, lang)  else { return vec![]; };
    cst_hints(idx, uri, &doc.tree, &doc.bytes, range)
}

// ─── CST walk ────────────────────────────────────────────────────────────────

/// Precompute the byte offset of each line's first byte within `bytes`.
/// Used by `ts_byte_col_to_utf16` so it doesn't rescan from the file start
/// for every node position.
fn line_starts(bytes: &[u8]) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' { starts.push(i + 1); }
    }
    starts
}

/// Preorder-walk the tree and emit inlay hints for nodes within `range`.
fn cst_hints(
    idx:   &Arc<Indexer>,
    uri:   &Url,
    tree:  &tree_sitter::Tree,
    bytes: &[u8],
    range: Range,
) -> Vec<InlayHint> {
    let starts = line_starts(bytes);
    let mut hints  = Vec::new();
    let mut cursor = tree.walk();

    'walk: loop {
        let node = cursor.node();
        let ns = node.start_position().row as u32;
        let ne = node.end_position().row as u32;

        // Node starts after the requested range → done.
        if ns > range.end.line { break; }

        // Entire subtree precedes the requested range → skip it.
        if ne < range.start.line {
            loop {
                if cursor.goto_next_sibling() { continue 'walk; }
                if !cursor.goto_parent()      { break 'walk; }
            }
        }

        match node.kind() {
            "lambda_literal" => {
                hint_lambda(idx, uri, &node, bytes, &starts, range, &mut hints);
            }
            "simple_identifier" => {
                if node.utf8_text(bytes) == Ok("it") {
                    let pos = ts_pos_to_lsp(node.start_position(), &starts, bytes);
                    if in_range(pos.line, range) {
                        let kind = ReceiverKind::Contextual { name: "it", position: pos };
                        if let Some(rt) = infer_receiver_type(idx, kind, uri) {
                            hints.push(type_hint(ts_pos_to_lsp(node.end_position(), &starts, bytes), &rt.raw));
                        }
                    }
                }
            }
            "this_expression" => {
                let pos = ts_pos_to_lsp(node.start_position(), &starts, bytes);
                if in_range(pos.line, range) {
                    let kind = ReceiverKind::Contextual { name: "this", position: pos };
                    if let Some(rt) = infer_receiver_type(idx, kind, uri) {
                        hints.push(type_hint(ts_pos_to_lsp(node.end_position(), &starts, bytes), &rt.raw));
                    }
                }
            }
            "property_declaration" => {
                hint_property(idx, uri, &node, bytes, &starts, range, &mut hints);
            }
            _ => {}
        }

        // Descend to first child, or advance to next sibling / ancestor sibling.
        if cursor.goto_first_child() { continue; }
        loop {
            if cursor.goto_next_sibling() { break; }
            if !cursor.goto_parent() { break 'walk; }
        }
    }

    hints
}

/// Emit `: Type` hints for named parameters in a `lambda_literal`.
///
/// Structure confirmed from tree-sitter-kotlin probe:
/// `lambda_literal { lambda_parameters { variable_declaration { simple_identifier } } -> statements }`
fn hint_lambda(
    idx:    &Arc<Indexer>,
    uri:    &Url,
    node:   &tree_sitter::Node<'_>,
    bytes:  &[u8],
    starts: &[usize],
    range:  Range,
    hints:  &mut Vec<InlayHint>,
) {
    let mut nc = node.walk();
    for child in node.children(&mut nc) {
        if child.kind() != "lambda_parameters" { continue; }

        let mut pc = child.walk();
        for param in child.children(&mut pc) {
            if param.kind() != "variable_declaration" { continue; }

            // Skip params that already carry a type annotation (`: Type` child).
            let mut vc = param.walk();
            let mut has_type  = false;
            let mut name_node = None;
            for pchild in param.children(&mut vc) {
                match pchild.kind() {
                    "simple_identifier" if name_node.is_none() => { name_node = Some(pchild); }
                    ":"                                        => { has_type = true; break; }
                    _ => {}
                }
            }
            if has_type { continue; }

            let Some(name_n) = name_node                    else { continue };
            let Ok(name)     = name_n.utf8_text(bytes)      else { continue };
            let name = name.trim();
            if name.is_empty() || name == "_" { continue; }
            if !name.chars().next().map(|c| c.is_lowercase()).unwrap_or(false) { continue; }

            let start_pos = ts_pos_to_lsp(name_n.start_position(), starts, bytes);
            let end_pos   = ts_pos_to_lsp(name_n.end_position(),   starts, bytes);
            if !in_range(start_pos.line, range) { continue; }

            if let Some(rt) = infer_receiver_type(idx, ReceiverKind::Contextual { name, position: start_pos }, uri) {
                hints.push(type_hint(end_pos, &rt.raw));
            }
        }
        break; // only one lambda_parameters block per literal
    }
}

/// Emit `: Type` hint for `val name = expr` / `var name = expr` without explicit type.
fn hint_property(
    idx:    &Arc<Indexer>,
    uri:    &Url,
    node:   &tree_sitter::Node<'_>,
    bytes:  &[u8],
    starts: &[usize],
    range:  Range,
    hints:  &mut Vec<InlayHint>,
) {
    // Find the variable_declaration child.
    let mut nc  = node.walk();
    let mut var_decl = None;
    for child in node.children(&mut nc) {
        if child.kind() == "variable_declaration" { var_decl = Some(child); break; }
    }
    let Some(vd) = var_decl else { return };

    // Check for existing type annotation.
    let mut vc        = vd.walk();
    let mut has_type  = false;
    let mut name_node = None;
    for child in vd.children(&mut vc) {
        match child.kind() {
            "simple_identifier" if name_node.is_none() => { name_node = Some(child); }
            ":"                                        => { has_type = true; break; }
            _ => {}
        }
    }
    if has_type { return; }

    // Must have `=` (skip abstract / delegate declarations without an initializer).
    let mut nc2      = node.walk();
    let mut init_node = None;
    let mut past_eq   = false;
    for child in node.children(&mut nc2) {
        if child.kind() == "=" { past_eq = true; continue; }
        if past_eq { init_node = Some(child); break; }
    }
    let Some(init) = init_node else { return };

    let Some(name_n) = name_node               else { return };
    let Ok(name)     = name_n.utf8_text(bytes) else { return };
    let name = name.trim();
    if name.is_empty() { return; }

    let end_pos = ts_pos_to_lsp(name_n.end_position(), starts, bytes);
    if !in_range(end_pos.line, range) { return; }

    // Derive the type name from the initializer expression.
    if let Some(ty) = infer_type_from_init(init, bytes) {
        hints.push(type_hint(end_pos, &ty));
        return;
    }

    // Fallback: text-based inference (handles `val x: Type` pattern aliases etc.)
    if let Some(rt) = infer_receiver_type(idx, ReceiverKind::Variable(name), uri) {
        let base: String = rt.raw.chars()
            .take_while(|&c| c.is_alphanumeric() || c == '_' || c == '<' || c == '>')
            .collect();
        if !base.is_empty() {
            hints.push(type_hint(end_pos, &base));
        }
    }
}

/// Infer a display type name from the CST initializer node.
///
/// Returns `Some(name)` when the initializer is a constructor or factory call
/// whose callee starts with an uppercase letter — indicating the type name is
/// the same as the callee (`val user = User(…)` → `"User"`).
fn infer_type_from_init(init: tree_sitter::Node<'_>, bytes: &[u8]) -> Option<String> {
    // call_expression: callee(...) or callee<T>(...)
    if init.kind() == "call_expression" {
        let name = init.call_fn_name(bytes)?;
        if name.starts_with(|c: char| c.is_uppercase()) {
            return Some(name);
        }
    }
    None
}

// ─── helpers ─────────────────────────────────────────────────────────────────

fn type_hint(position: Position, type_name: &str) -> InlayHint {
    InlayHint {
        position,
        label:        InlayHintLabel::String(format!(": {type_name}")),
        kind:         Some(InlayHintKind::TYPE),
        text_edits:   None,
        tooltip:      None,
        padding_left: Some(false),
        padding_right: Some(true),
        data:         None,
    }
}

#[inline]
fn in_range(line: u32, range: Range) -> bool {
    line >= range.start.line && line <= range.end.line
}

/// Convert a tree-sitter `Point` (row, byte-column) to an LSP `Position`
/// (0-based line, UTF-16 code-unit column).
fn ts_pos_to_lsp(pos: tree_sitter::Point, starts: &[usize], bytes: &[u8]) -> Position {
    Position::new(pos.row as u32, ts_byte_col_to_utf16(bytes, starts, pos.row, pos.column) as u32)
}

/// Count the UTF-16 code units from the start of `row` up to `byte_col`.
///
/// `starts` must have been produced by `line_starts(bytes)` — it is used to
/// jump directly to the line without rescanning the whole file (O(1) lookup
/// instead of O(file_size)).
fn ts_byte_col_to_utf16(bytes: &[u8], starts: &[usize], row: usize, byte_col: usize) -> usize {
    let line_start = starts.get(row).copied().unwrap_or_else(|| {
        bytes.split(|&b| b == b'\n').take(row).map(|l| l.len() + 1).sum()
    });
    let end = (line_start + byte_col).min(bytes.len());
    std::str::from_utf8(&bytes[line_start..end])
        .map(|s| s.chars().map(|c| c.len_utf16()).sum())
        .unwrap_or(byte_col)
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn uri(path: &str) -> Url { Url::parse(&format!("file:///test{path}")).unwrap() }

    fn indexed(path: &str, src: &str) -> (Url, Arc<Indexer>) {
        let u   = uri(path);
        let idx = Arc::new(Indexer::new());
        idx.index_content(&u, src);
        (u, idx)
    }

    fn hints_for(src: &str) -> Vec<InlayHint> {
        let (u, idx) = indexed("/t.kt", src);
        let lines = src.lines().count() as u32;
        compute_inlay_hints(&idx, &u, Range {
            start: Position::new(0, 0),
            end:   Position::new(lines, 0),
        })
    }

    #[test]
    fn it_type_hint() {
        let src = "val items: List<Product> = emptyList()\nitems.forEach { it.name }";
        let hints = hints_for(src);
        assert!(
            hints.iter().any(|h| matches!(&h.label, InlayHintLabel::String(s) if s == ": Product")),
            "expected ': Product' hint for it, got: {hints:?}",
        );
    }

    #[test]
    fn named_param_type_hint() {
        let src = "val items: List<Order> = emptyList()\nitems.forEach { order ->\n    order.id\n}";
        let hints = hints_for(src);
        assert!(
            hints.iter().any(|h| matches!(&h.label, InlayHintLabel::String(s) if s == ": Order")),
            "expected ': Order' hint for named param, got: {hints:?}",
        );
    }

    #[test]
    fn no_hint_for_typed_val() {
        let src = "val items: List<Product> = emptyList()";
        let hints = hints_for(src);
        assert!(
            !hints.iter().any(|h| matches!(&h.label, InlayHintLabel::String(s) if s.contains("items"))),
            "should not hint explicitly typed val",
        );
    }

    #[test]
    fn hints_inject_constructor_lambdas() {
        let src = r#"package test

class ProductsUseCases
class MviViewModel

class DashboardProductsViewModel @javax.inject.Inject constructor(
  private val productsUseCases: ProductsUseCases,
) : MviViewModel() {

  private val items: List<String> = emptyList()

  fun loadData() {
    items.forEach { it.length }
    items.map { item ->
      item.uppercase()
    }
  }
}
"#;
        let hints = hints_for(src);
        eprintln!("inject_constructor hints: {hints:?}");
        assert!(
            hints.iter().any(|h| matches!(&h.label, InlayHintLabel::String(s) if s == ": String")),
            "expected ': String' hint for it/item in @Inject constructor class, got: {hints:?}",
        );
    }

    #[test]
    fn hints_survive_syntax_error() {
        let src = "val items: List<Product> = emptyList()\nitems.forEach { it.name\n";
        let hints = hints_for(src);
        assert!(
            hints.iter().any(|h| matches!(&h.label, InlayHintLabel::String(s) if s == ": Product")),
            "hints should still work despite syntax error, got: {hints:?}",
        );
    }

    #[test]
    fn hints_nested_named_arg_lambda() {
        let src = r#"package test

class SheetReloadActions(
    val buildingSavings: (String) -> Unit,
    val loan: (String, Boolean) -> Unit,
)

class Vm {
    private val reducer by lazy {
        SheetReloadActions(
            buildingSavings = { println(it) },
            loan = { loanId, isWustenrot -> println(loanId) },
        )
    }
}
"#;
        let hints = hints_for(src);
        eprintln!("nested_named_arg hints: {hints:?}");
        let has_string = hints.iter().any(|h| {
            matches!(&h.label, InlayHintLabel::String(s) if s == ": String")
        });
        eprintln!("has_string={has_string}");
        assert!(has_string,
            "expected ': String' hint for it/loanId in nested named-arg lambda, got: {hints:?}");
    }

    #[test]
    fn hints_nested_named_arg_cross_file() {
        let idx = Arc::new(Indexer::new());
        let u1 = uri("/DashboardProductsReducer.kt");
        idx.index_content(&u1, r#"package test

class DashboardProductsReducer {
    data class SheetReloadActions(
        val buildingSavings: (String) -> Unit,
        val cards: (CardProduct) -> Unit,
        val loan: (String, Boolean) -> Unit,
    )
}

class CardProduct
"#);
        let u2 = uri("/Vm.kt");
        let vm_src = r#"package test

import test.DashboardProductsReducer

class Vm {
    private val reducer by lazy {
        DashboardProductsReducer.SheetReloadActions(
            buildingSavings = { println(it) },
            cards = { println(it) },
            loan = { loanId, isWustenrot -> println(loanId) },
        )
    }
}
"#;
        idx.index_content(&u2, vm_src);
        let lines = vm_src.lines().count() as u32;
        let hints = compute_inlay_hints(&idx, &u2, Range {
            start: Position::new(0, 0),
            end: Position::new(lines, 0),
        });
        eprintln!("cross_file hints: {hints:?}");
        let has_string = hints.iter().any(|h| {
            matches!(&h.label, InlayHintLabel::String(s) if s == ": String")
        });
        let has_card = hints.iter().any(|h| {
            matches!(&h.label, InlayHintLabel::String(s) if s == ": CardProduct")
        });
        eprintln!("has_string={has_string} has_card={has_card}");
        assert!(has_string,
            "expected ': String' hint for it in cross-file named-arg lambda, got: {hints:?}");
        assert!(has_card,
            "expected ': CardProduct' hint for it in cards lambda, got: {hints:?}");
    }

    #[test]
    fn ts_byte_col_utf16_ascii() {
        // For ASCII content the UTF-16 column equals the byte column.
        let bytes = b"fun main() {}\n";
        let starts = line_starts(bytes);
        assert_eq!(ts_byte_col_to_utf16(bytes, &starts, 0, 4), 4); // "fun " = 4 bytes = 4 UTF-16 units
    }

    #[test]
    fn ts_byte_col_utf16_multibyte() {
        // "café" — 'é' is U+00E9 (2 UTF-8 bytes, 1 UTF-16 unit).
        let line = "café foo";
        let bytes = line.as_bytes();
        let starts = line_starts(bytes);
        // byte offset 6 is after "café " (c=1,a=1,f=1,é=2,space=1 → 6 bytes)
        // char cols: c=0,a=1,f=1(wait: c-a-f-é = 4 chars, then space = 5 chars total for "café ")
        // UTF-16: same as char count for BMP chars = 5
        let byte_col = "café ".len(); // 6 bytes
        let utf16 = ts_byte_col_to_utf16(bytes, &starts, 0, byte_col);
        assert_eq!(utf16, 5, "expected 5 UTF-16 units for 'café '");
    }

    #[test]
    fn untyped_val_constructor_call_gets_hint() {
        // `val user = User("alice")` — no explicit type annotation.
        // hint_property should emit `: User` from the CST initializer.
        let src = r#"package test
class User(val name: String)
fun make() {
    val user = User("alice")
}
"#;
        let hints = hints_for(src);
        assert!(
            hints.iter().any(|h| matches!(&h.label, InlayHintLabel::String(s) if s == ": User")),
            "expected ': User' hint for untyped val with constructor call, got: {hints:?}",
        );
    }

    #[test]
    fn it_inside_nested_lambda_not_suspend() {
        // Regression: `it` inside `setState { it }` where `setState` has a
        // `suspend` function type parameter was incorrectly showing `: suspend`.
        // `find_as_call_arg_type` must bail out when the backward scan crosses
        // an unmatched `{`, meaning `it` is inside a nested lambda body.
        let src = r#"package test

class State
class Effect

class Vm {
    private val items: List<State> = emptyList()

    fun load() {
        items.forEach { item ->
            setState { item }
        }
    }

    fun setState(reducer: suspend State.() -> State) {}
}
"#;
        let hints = hints_for(src);
        let bad = hints.iter().any(|h| matches!(&h.label, InlayHintLabel::String(s) if s == ": suspend"));
        assert!(!bad, "must not emit ': suspend' hint for it inside nested lambda, got: {hints:?}");
    }
}
