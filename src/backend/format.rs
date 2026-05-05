//! Hover Markdown formatting helpers.
//!
//! These functions turn a [`ResolvedSymbol`] into the final Markdown string
//! returned by the `textDocument/hover` handler.  They are presentation-only
//! and contain no resolution logic.

use crate::indexer::lookup::{lang_str, symbol_kw_for_lang};
use crate::indexer::resolution::ResolvedSymbol;
use tower_lsp::lsp_types::SymbolKind;

/// Format a standard symbol hover: optional KDoc block + fenced code block.
///
/// ```text
/// /** KDoc comment */
///
/// ---
///
/// ```kotlin
/// fun foo(x: Int): String
/// ```
/// ```
pub(super) fn format_symbol_hover(info: &ResolvedSymbol, uri_path: &str) -> String {
    let lang = lang_str(uri_path);
    let sig = info.signature.as_str();

    let code_block = if sig.is_empty() {
        // Signature unavailable — fall back to keyword + known symbol name.
        format!(
            "```{lang}\n{} {}\n```",
            symbol_kw_for_lang(info.kind, lang),
            info.name
        )
    } else {
        format!("```{lang}\n{sig}\n```")
    };

    if info.doc.is_empty() {
        code_block
    } else {
        format!("{}\n\n---\n\n{code_block}", info.doc)
    }
}

/// Format a contextual hover for an `it` / named lambda parameter:
///
/// ```text
/// ```kotlin
/// val it: AccountType
/// ```
///
/// ---
///
/// <optional type-symbol hover>
/// ```
///
/// `type_sig_md` — the synthesized declaration line, e.g. `"val it: AccountType"`.
/// `type_detail` — optional hover markdown for the resolved type symbol itself.
pub(super) fn format_contextual_hover(
    type_sig_md: &str,
    uri_path: &str,
    type_detail: Option<&str>,
) -> String {
    let lang = lang_str(uri_path);
    let sig_block = format!("```{lang}\n{type_sig_md}\n```");
    match type_detail {
        Some(td) if !td.is_empty() => format!("{sig_block}\n\n---\n\n{td}"),
        _ => sig_block,
    }
}

/// Return the language keyword for a symbol kind (Swift-aware).
#[allow(dead_code)]
pub(super) fn kw_for_kind(kind: SymbolKind, uri_path: &str) -> &'static str {
    symbol_kw_for_lang(kind, lang_str(uri_path))
}
