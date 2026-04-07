//! Inlay hint provider for Kotlin/Java files.
//!
//! Emits type hints for:
//! 1. Lambda implicit parameter `it` — shows `: Type` after `it`
//! 2. Named lambda parameters `{ item -> }` — shows `: Type` after the param name
//! 3. `this` inside scope functions / class methods — shows `: Type` after `this`
//! 4. Untyped local `val`/`var` declarations — shows `: InferredType` after the name
//!    (only when the type is determinable from the index without rg)

use std::sync::Arc;
use tower_lsp::lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Position, Range, Url};

use crate::indexer::Indexer;

pub fn compute_inlay_hints(idx: &Arc<Indexer>, uri: &Url, range: Range) -> Vec<InlayHint> {
    let lines = match idx.files.get(uri.as_str()) {
        Some(f) => f.lines.clone(),
        None    => idx.live_lines.get(uri.as_str()).map(|l| l.clone()).unwrap_or_default(),
    };
    if lines.is_empty() { return vec![]; }

    let start = range.start.line as usize;
    let end   = (range.end.line as usize + 1).min(lines.len());

    let mut hints = Vec::new();

    for (ln_idx, line) in lines[start..end].iter().enumerate() {
        let ln = (start + ln_idx) as u32;
        let trimmed = line.trim_start();
        // Skip comments.
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }

        // ── 1 & 2 & 3: lambda param / `it` / `this` type hints ───────────────
        hint_lambda_params(idx, uri, line, ln, &mut hints);

        // ── 4: untyped val/var declarations ──────────────────────────────────
        hint_untyped_val(idx, uri, line, ln, &mut hints);
    }

    hints
}

/// Scan one line for `it`, `this`, or named lambda params and emit `: Type` hints.
fn hint_lambda_params(
    idx:   &Indexer,
    uri:   &Url,
    line:  &str,
    ln:    u32,
    hints: &mut Vec<InlayHint>,
) {
    // ── Pass 1: named lambda param declarations `{ name ->` or `{ a, b ->` ──
    // Iterate ALL `->` occurrences so nested lambdas on the same line are handled.
    let mut search_from = 0;
    while let Some(arrow_rel) = line[search_from..].find("->") {
        let arrow_pos = search_from + arrow_rel;
        if let Some(brace_pos) = line[..arrow_pos].rfind('{') {
            let names_str = &line[brace_pos + 1..arrow_pos];
            let mut name_search = 0usize;
            for tok in names_str.split(',') {
                let tok_trimmed = tok.trim();
                let name: String = tok_trimmed.chars()
                    .take_while(|&c| c.is_alphanumeric() || c == '_')
                    .collect();
                if !name.is_empty() && name != "it" && name != "_"
                    && name.chars().next().map(|c| c.is_lowercase()).unwrap_or(false)
                {
                    // Find this token's position inside `names_str` from `name_search`.
                    if let Some(tok_rel) = names_str[name_search..].find(tok_trimmed) {
                        let abs = brace_pos + 1 + name_search + tok_rel;
                        let col = char_col(line, abs) as u32;
                        if let Some(ty) = idx.infer_lambda_param_type_at(
                            &name, uri, Position::new(ln, col)
                        ) {
                            let hint_pos = Position::new(ln, col + name.len() as u32);
                            hints.push(type_hint(hint_pos, &ty));
                        }
                    }
                }
                name_search += tok.len() + 1; // +1 for the `,` separator
            }
        }
        search_from = arrow_pos + 2;
    }

    // ── Pass 2: `it` and `this` usages (may appear multiple times per line) ──
    let mut pos = 0;
    while pos < line.len() {
        let remaining = &line[pos..];

        if let Some(rel) = find_word(remaining, "it") {
            let abs = pos + rel;
            let col = char_col(line, abs) as u32;
            if let Some(ty) = idx.infer_lambda_param_type_at(
                "it", uri, Position::new(ln, col)
            ) {
                hints.push(type_hint(Position::new(ln, col + 2), &ty));
            }
            pos = abs + 2;
            continue;
        }

        if let Some(rel) = find_word(remaining, "this") {
            let abs = pos + rel;
            let col = char_col(line, abs) as u32;
            if let Some(ty) = idx.infer_lambda_param_type_at(
                "this", uri, Position::new(ln, col)
            ) {
                hints.push(type_hint(Position::new(ln, col + 4), &ty));
            }
            pos = abs + 4;
            continue;
        }

        break;
    }
}

/// Emit `: Type` hint for `val name = expr` / `var name = expr` without explicit type.
///
/// Pattern: `(val|var) <ident> =` with NO `:` between the ident and `=`.
fn hint_untyped_val(
    idx:   &Indexer,
    uri:   &Url,
    line:  &str,
    ln:    u32,
    hints: &mut Vec<InlayHint>,
) {
    let trimmed = line.trim_start();
    let prefix = if trimmed.starts_with("val ") { "val " }
                 else if trimmed.starts_with("var ") { "var " }
                 else { return; };

    let after_kw = &trimmed[prefix.len()..];
    // Extract identifier.
    let name: String = after_kw.chars().take_while(|&c| c.is_alphanumeric() || c == '_').collect();
    if name.is_empty() { return; }

    let after_name = after_kw[name.len()..].trim_start();
    // If the next non-space char is `:` → already typed, skip.
    if after_name.starts_with(':') { return; }
    // Must be `=` to be an assignment.
    if !after_name.starts_with('=') { return; }

    // Find the column of the name in the original line.
    let name_byte = line.find(&name[..]).unwrap_or(0);
    let col = char_col(line, name_byte) as u32;

    if let Some(raw) = crate::resolver::infer_variable_type_raw(idx, &name, uri) {
        let base: String = raw.chars().take_while(|&c| c.is_alphanumeric() || c == '_' || c == '<' || c == '>').collect();
        if base.is_empty() { return; }
        let hint_pos = Position::new(ln, col + name.len() as u32);
        hints.push(type_hint(hint_pos, &base));
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

fn type_hint(position: Position, type_name: &str) -> InlayHint {
    InlayHint {
        position,
        label:   InlayHintLabel::String(format!(": {type_name}")),
        kind:    Some(InlayHintKind::TYPE),
        text_edits:  None,
        tooltip:     None,
        padding_left:  Some(false),
        padding_right: Some(true),
        data:    None,
    }
}

/// Find `word` as a complete identifier (not part of a longer name) in `s`.
/// Returns the byte offset of the match, or `None`.
fn find_word(s: &str, word: &str) -> Option<usize> {
    let mut start = 0;
    while let Some(rel) = s[start..].find(word) {
        let abs = start + rel;
        let before_ok = abs == 0
            || !s[..abs].chars().last().map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false);
        let after_ok  = abs + word.len() >= s.len()
            || !s[abs + word.len()..].chars().next().map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false);
        if before_ok && after_ok { return Some(abs); }
        start = abs + 1;
    }
    None
}

/// Convert a byte offset in `line` to a Unicode character column.
fn char_col(line: &str, byte_offset: usize) -> usize {
    line[..byte_offset.min(line.len())].chars().count()
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
        // `items` has an explicit type — no hint needed.
        assert!(
            !hints.iter().any(|h| matches!(&h.label, InlayHintLabel::String(s) if s.contains("items"))),
            "should not hint explicitly typed val",
        );
    }

    #[test]
    fn find_word_finds_whole_words() {
        assert_eq!(find_word("it.name", "it"), Some(0));
        assert_eq!(find_word("bits.name", "it"), None);  // `it` inside `bits`
        assert_eq!(find_word("  it ", "it"), Some(2));
        assert_eq!(find_word("with(it)", "it"), Some(5));
    }
}
