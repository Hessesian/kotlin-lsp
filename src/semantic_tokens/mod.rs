//! Semantic token classification for `textDocument/semanticTokens/full` and
//! `textDocument/semanticTokens/range`.
//!
//! Two-phase pipeline:
//! - **Phase 1** (CST): classify declarations, soft keywords, named-arg labels,
//!   and parameter use-sites purely from the tree-sitter parse tree.
//! - **Phase 2** (Index): when an `Indexer` and file URI are provided, resolve
//!   reference-site identifiers against the cross-file index for richer tokens.
//!
//! # Encoding
//! LSP semantic tokens are delta-encoded: each token stores the line *delta*
//! from the previous token and the column *delta* (from the previous token on
//! the same line, or from column 0 on a new line).  Tokens must be sorted by
//! (line, col) before encoding.

mod helpers;
mod java;
mod kotlin;
mod params;
mod resolve;

use tower_lsp::lsp_types::{
    Range, SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokens,
    SemanticTokensLegend, Url,
};

use crate::indexer::{Indexer, LiveDoc};
use crate::Language;

// ─── Legend ──────────────────────────────────────────────────────────────────

/// Ordered list of token types — index == LSP token type id.
pub(crate) const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::CLASS,          // 0
    SemanticTokenType::INTERFACE,      // 1
    SemanticTokenType::ENUM,           // 2
    SemanticTokenType::STRUCT,         // 3
    SemanticTokenType::TYPE_PARAMETER, // 4
    SemanticTokenType::FUNCTION,       // 5
    SemanticTokenType::METHOD,         // 6
    SemanticTokenType::PROPERTY,       // 7
    SemanticTokenType::PARAMETER,      // 8
    SemanticTokenType::VARIABLE,       // 9
    SemanticTokenType::ENUM_MEMBER,    // 10
    SemanticTokenType::DECORATOR,      // 11
    SemanticTokenType::NAMESPACE,      // 12
    SemanticTokenType::KEYWORD,        // 13
    SemanticTokenType::OPERATOR,       // 14
];

pub(crate) const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION,     // bit 0
    SemanticTokenModifier::READONLY,        // bit 1
    SemanticTokenModifier::STATIC,          // bit 2  (companion object members)
    SemanticTokenModifier::ABSTRACT,        // bit 3
    SemanticTokenModifier::ASYNC,           // bit 4  (suspend funs)
    SemanticTokenModifier::DEPRECATED,      // bit 5
    SemanticTokenModifier::DEFAULT_LIBRARY, // bit 6  (stdlib symbols, future use)
];

fn type_index(token_type: &SemanticTokenType) -> u32 {
    let index = TOKEN_TYPES
        .iter()
        .position(|t| t == token_type)
        .unwrap_or(0);
    debug_assert!(
        TOKEN_TYPES.get(index) == Some(token_type),
        "token type {token_type:?} not found in TOKEN_TYPES legend"
    );
    index as u32
}

fn modifier_bit(modifier: &SemanticTokenModifier) -> u32 {
    let position = TOKEN_MODIFIERS.iter().position(|m| m == modifier);
    debug_assert!(
        position.is_some(),
        "modifier {modifier:?} not found in TOKEN_MODIFIERS legend"
    );
    position.map(|i| 1u32 << i).unwrap_or(0)
}

pub(crate) fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES.to_vec(),
        token_modifiers: TOKEN_MODIFIERS.to_vec(),
    }
}

// ─── Classification ───────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct RawToken {
    pub(crate) line: u32,
    pub(crate) col: u32, // UTF-16 column
    pub(crate) length: u32,
    pub(crate) token_type: u32,
    pub(crate) token_modifiers_bitset: u32,
}

/// Bundles source bytes with precomputed line-start offsets for O(1) UTF-16
/// column conversion (avoids re-scanning the entire file per token).
struct Source<'a> {
    bytes: &'a [u8],
    line_starts: Vec<usize>,
}

impl<'a> Source<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            line_starts: crate::inlay_hints::line_starts(bytes),
            bytes,
        }
    }

    fn col_utf16(&self, row: usize, byte_col: usize) -> u32 {
        crate::inlay_hints::ts_byte_col_to_utf16(self.bytes, &self.line_starts, row, byte_col)
            as u32
    }
}

/// Per-phase token breakdown for debug output.
pub(crate) struct TokenPhases {
    pub phase1: Vec<RawToken>,
    pub phase1b: Vec<RawToken>,
    pub phase2: Vec<RawToken>,
}

impl TokenPhases {
    pub(crate) fn final_tokens(&self) -> Vec<RawToken> {
        let mut raw = self.phase1.clone();
        raw.extend_from_slice(&self.phase1b);
        raw.extend_from_slice(&self.phase2);
        raw.sort_by_key(|t| (t.line, t.col));
        raw.dedup_by_key(|t| (t.line, t.col));
        raw
    }
}

/// Like `collect_tokens` but returns each phase's tokens separately for debug.
pub(crate) fn collect_tokens_phases(
    doc: &LiveDoc,
    language: Language,
    indexer: Option<&Indexer>,
    uri: Option<&Url>,
) -> TokenPhases {
    let src = Source::new(&doc.bytes);
    let mut phase1 = Vec::new();
    let mut phase1b = Vec::new();
    let mut phase2 = Vec::new();

    match language {
        Language::Kotlin => kotlin::walk_kotlin(doc.tree.root_node(), &src, &mut phase1),
        Language::Java => java::walk_java(doc.tree.root_node(), &src, &mut phase1),
        _ => {}
    }

    if language == Language::Kotlin {
        params::emit_kotlin_param_uses(doc.tree.root_node(), &src, &mut phase1b);
    }

    if let (Some(idx), Some(file_uri)) = (indexer, uri) {
        resolve::walk_references(doc, &src, language, idx, file_uri, &mut phase2);
    }

    TokenPhases { phase1, phase1b, phase2 }
}

/// Collect semantic tokens for `doc`, for the given `language`.
/// Returns delta-encoded `SemanticToken` values ready for the LSP response.
/// Filtered to `range` when `Some`.
pub(crate) fn collect_tokens(
    doc: &LiveDoc,
    language: Language,
    range: Option<&Range>,
    indexer: Option<&Indexer>,
    uri: Option<&Url>,
) -> Vec<SemanticToken> {
    let src = Source::new(&doc.bytes);
    let mut raw = Vec::new();

    match language {
        Language::Kotlin => kotlin::walk_kotlin(doc.tree.root_node(), &src, &mut raw),
        Language::Java => java::walk_java(doc.tree.root_node(), &src, &mut raw),
        _ => {}
    }

    if language == Language::Kotlin {
        params::emit_kotlin_param_uses(doc.tree.root_node(), &src, &mut raw);
    }

    if let (Some(idx), Some(file_uri)) = (indexer, uri) {
        resolve::walk_references(doc, &src, language, idx, file_uri, &mut raw);
    }

    raw.sort_by_key(|t| (t.line, t.col));
    raw.dedup_by_key(|t| (t.line, t.col));

    if let Some(range) = range {
        raw.retain(|t| {
            t.line >= range.start.line
                && t.line <= range.end.line
                && (t.line > range.start.line || t.col >= range.start.character)
                && (t.line < range.end.line || t.col + t.length <= range.end.character)
        });
    }

    delta_encode(raw)
}

// ─── Delta encoding ───────────────────────────────────────────────────────────

fn delta_encode(sorted: Vec<RawToken>) -> Vec<SemanticToken> {
    let mut result = Vec::with_capacity(sorted.len());
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;

    for tok in sorted {
        let delta_line = tok.line - prev_line;
        let delta_start = if delta_line == 0 {
            tok.col - prev_start
        } else {
            tok.col
        };
        result.push(SemanticToken {
            delta_line,
            delta_start,
            length: tok.length,
            token_type: tok.token_type,
            token_modifiers_bitset: tok.token_modifiers_bitset,
        });
        prev_line = tok.line;
        prev_start = tok.col;
    }
    result
}

// ─── Public API ──────────────────────────────────────────────────────────────

pub(crate) fn full_tokens(
    indexer: &Indexer,
    uri: &Url,
    doc: &LiveDoc,
    language: Language,
) -> SemanticTokens {
    SemanticTokens {
        result_id: None,
        data: collect_tokens(doc, language, None, Some(indexer), Some(uri)),
    }
}

pub(crate) fn range_tokens(
    indexer: &Indexer,
    uri: &Url,
    doc: &LiveDoc,
    language: Language,
    range: &Range,
) -> SemanticTokens {
    SemanticTokens {
        result_id: None,
        data: collect_tokens(doc, language, Some(range), Some(indexer), Some(uri)),
    }
}

/// CST-only tokens without cross-file resolution — used by unit tests that
/// don't set up a full Indexer.
#[cfg(test)]
pub(crate) fn full_tokens_cst_only(doc: &LiveDoc, language: Language) -> SemanticTokens {
    SemanticTokens {
        result_id: None,
        data: collect_tokens(doc, language, None, None, None),
    }
}

#[cfg(test)]
pub(crate) fn range_tokens_cst_only(
    doc: &LiveDoc,
    language: Language,
    range: &Range,
) -> SemanticTokens {
    SemanticTokens {
        result_id: None,
        data: collect_tokens(doc, language, Some(range), None, None),
    }
}

#[cfg(test)]
#[path = "../semantic_tokens_tests.rs"]
mod tests;
