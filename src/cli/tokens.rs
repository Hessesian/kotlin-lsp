//! `tokens` and `tree` debug subcommands.
//!
//! `tokens <file>` decodes the semantic token stream for a file and prints
//! one line per token so you can see exactly what type/modifiers are emitted
//! and at which source position.
//!
//! `tree <file>` dumps the tree-sitter CST to stdout.

use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use tower_lsp::lsp_types::Url;

use crate::indexer::live_tree::{lang_for_path, parse_live};
use crate::indexer::Indexer;
use crate::semantic_tokens::{collect_tokens, TOKEN_MODIFIERS, TOKEN_TYPES};
use crate::Language;

// ── Token row ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(crate) struct TokenRow {
    pub line: u32,
    pub col: u32,
    pub len: u32,
    pub token_type: String,
    pub modifiers: Vec<String>,
    pub text: String,
}

// ── Token dump ───────────────────────────────────────────────────────────────

/// Collect and decode semantic tokens for `file`.
///
/// If `cst_only` is true, the index is not consulted — only the tree-sitter
/// CST classification is used.  If `indexer` is `None`, `cst_only` is implied.
pub(crate) fn token_rows(
    file: &Path,
    indexer: Option<&Arc<Indexer>>,
    cst_only: bool,
) -> Result<Vec<TokenRow>, String> {
    let abs = file
        .canonicalize()
        .unwrap_or_else(|_| file.to_path_buf());
    let path_str = abs.to_string_lossy();

    let content = std::fs::read_to_string(&abs)
        .map_err(|e| format!("cannot read {}: {e}", abs.display()))?;

    let ts_lang = lang_for_path(&path_str)
        .ok_or_else(|| format!("unsupported file type: {}", abs.display()))?;

    let language = Language::from_path(&path_str);

    let doc = parse_live(&content, ts_lang)
        .ok_or_else(|| format!("tree-sitter parse failed for {}", abs.display()))?;

    let uri = Url::from_file_path(&abs)
        .map_err(|_| format!("cannot convert path to URI: {}", abs.display()))?;

    let (idx_ref, uri_ref) = if cst_only || indexer.is_none() {
        (None, None)
    } else {
        let idx = indexer.unwrap();
        idx.ensure_indexed(&uri);
        (Some(idx.as_ref()), Some(&uri))
    };

    let tokens = collect_tokens(&doc, language, None, idx_ref, uri_ref);

    // Decode the delta-encoded token stream back to absolute positions.
    let lines: Vec<&str> = content.lines().collect();
    let mut rows = Vec::with_capacity(tokens.len());
    let mut abs_line = 0u32;
    let mut abs_col = 0u32;

    for tok in &tokens {
        if tok.delta_line > 0 {
            abs_line += tok.delta_line;
            abs_col = tok.delta_start;
        } else {
            abs_col += tok.delta_start;
        }

        let type_name = TOKEN_TYPES
            .get(tok.token_type as usize)
            .map(|t| format!("{t:?}"))
            .unwrap_or_else(|| format!("#{}", tok.token_type));

        let modifiers = decode_modifiers(tok.token_modifiers_bitset);

        let text = lines
            .get(abs_line as usize)
            .map(|line| {
                let bytes = line.as_bytes();
                // col is UTF-16; for ASCII-dominant Kotlin source, byte == UTF-16 col
                let start = abs_col as usize;
                let end = (abs_col + tok.length) as usize;
                if end <= bytes.len() {
                    &line[start..end]
                } else {
                    ""
                }
            })
            .unwrap_or("")
            .to_owned();

        rows.push(TokenRow {
            line: abs_line,
            col: abs_col,
            len: tok.length,
            token_type: type_name,
            modifiers,
            text,
        });
    }

    Ok(rows)
}

fn decode_modifiers(bits: u32) -> Vec<String> {
    TOKEN_MODIFIERS
        .iter()
        .enumerate()
        .filter(|(i, _)| bits & (1 << i) != 0)
        .map(|(_, m)| format!("{m:?}"))
        .collect()
}

pub(crate) fn print_token_rows(rows: &[TokenRow], json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(rows).unwrap_or_default()
        );
        return;
    }
    for row in rows {
        let mods = if row.modifiers.is_empty() {
            String::new()
        } else {
            format!("+{}", row.modifiers.join(","))
        };
        println!(
            "{}:{}+{}  {}{:30}  {:?}",
            row.line,
            row.col,
            row.len,
            row.token_type,
            mods,
            row.text,
        );
    }
}

// ── Tree dump ────────────────────────────────────────────────────────────────

/// Parse `file` and print the tree-sitter CST to stdout.
pub(crate) fn dump_tree(file: &Path) -> Result<(), String> {
    let abs = file
        .canonicalize()
        .unwrap_or_else(|_| file.to_path_buf());
    let path_str = abs.to_string_lossy();

    let content = std::fs::read_to_string(&abs)
        .map_err(|e| format!("cannot read {}: {e}", abs.display()))?;

    let ts_lang = lang_for_path(&path_str)
        .ok_or_else(|| format!("unsupported file type: {}", abs.display()))?;

    let doc = parse_live(&content, ts_lang)
        .ok_or_else(|| format!("tree-sitter parse failed for {}", abs.display()))?;

    let source = content.as_bytes();
    print_node(doc.tree.root_node(), source, 0);
    Ok(())
}

fn print_node(node: tree_sitter::Node<'_>, source: &[u8], depth: usize) {
    let indent = "  ".repeat(depth);
    let start = node.start_position();
    let end = node.end_position();

    if node.is_named() {
        let text_preview = if node.child_count() == 0 {
            // Leaf: show the text
            let s = &source[node.start_byte()..node.end_byte()];
            let s = std::str::from_utf8(s).unwrap_or("?");
            // Truncate long leaves
            let s = if s.len() > 40 { &s[..40] } else { s };
            format!(" {:?}", s)
        } else {
            String::new()
        };
        println!(
            "{}{} [{}:{}-{}:{}]{}",
            indent,
            node.kind(),
            start.row,
            start.column,
            end.row,
            end.column,
            text_preview,
        );
    } else {
        // Anonymous node (keyword/punctuation)
        let s = &source[node.start_byte()..node.end_byte()];
        let s = std::str::from_utf8(s).unwrap_or("?");
        println!(
            "{}\"{}\" [{}:{}-{}:{}]",
            indent, s, start.row, start.column, end.row, end.column,
        );
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        print_node(child, source, depth + 1);
    }
}
