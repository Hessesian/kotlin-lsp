//! Per-request cursor context.
//!
//! `CursorContext::build` centralises the data-gathering that every LSP
//! feature handler (hover, goto-def, completion) used to repeat independently:
//! - extracting the word + optional dot-qualifier under the cursor
//! - resolving the contextual receiver type for `it` / `this` / named lambda params
//! - pre-resolving the lambda-param declaration location (for goto-def)
//! - fetching the live parse tree once
//!
//! Features that do NOT need an identifier under the cursor (sig-help, bare
//! completion) build their own context — this struct is not for them.

use std::sync::Arc;
use tower_lsp::lsp_types::{Location, Position, Url};

use crate::indexer::{Indexer, live_tree::LiveDoc};
use crate::resolver::{ReceiverKind, ReceiverType, infer_receiver_type};

/// Cursor context for identifier-based LSP features (hover, goto-def, completion).
///
/// Built once per request; individual fields are `None` when not applicable.
pub struct CursorContext {
    /// The identifier token under the cursor.
    pub word:      String,
    /// The dot-qualifier to the left of the cursor (e.g. `"it"`, `"viewModel"`).
    /// `None` when cursor is on a bare name with no qualifying expression.
    pub qualifier: Option<String>,
    /// Resolved contextual receiver — **only** set when `word` or `qualifier` is
    /// `it`, `this`, or a named lambda parameter.  Plain variable or type
    /// qualifiers are left for callers to resolve via `find_definition_qualified`.
    pub contextual: Option<ReceiverType>,
    /// When `contextual` is `None` and the word appears to be a named lambda
    /// parameter in scope, this holds the jump-target declaration location so
    /// goto-def can navigate to `{ name -> }` without a type.
    pub lambda_decl: Option<Location>,
    /// Live parse tree for the open file, if the editor has it open.
    pub live_doc: Option<Arc<LiveDoc>>,
}

impl CursorContext {
    /// Build a cursor context for the given URI + LSP position.
    ///
    /// Returns `None` only when there is no identifier under the cursor
    /// (e.g. cursor is in whitespace or on a non-identifier token).
    pub fn build(idx: &Indexer, uri: &Url, position: Position) -> Option<Self> {
        let (word, qualifier) = idx.word_and_qualifier_at(uri, position)?;

        let live_doc = idx.live_doc(uri);

        // Determine whether this is a contextual name (it / this / lambda param).
        let is_contextual = qualifier.as_deref().map_or(false, |q| q == "it" || q == "this")
            || (qualifier.is_none()
                && (word == "it" || word == "this"
                    || word.chars().next().map_or(false, |c| c.is_lowercase())));

        let contextual = if is_contextual {
            let name: &str = qualifier.as_deref().unwrap_or(&word);
            infer_receiver_type(idx, ReceiverKind::Contextual { name, position }, uri)
        } else {
            None
        };

        // For goto-def: if inference failed but the word is a named lambda param
        // in scope, pre-resolve the declaration location.
        let lambda_decl = if contextual.is_none() && is_contextual && qualifier.is_none() {
            let line = position.line as usize;
            let col  = position.character as usize;
            let params = idx.lambda_params_at_col(uri, line, col);
            if params.contains(&word) {
                idx.find_lambda_param_decl(uri, &word, line)
            } else {
                None
            }
        } else {
            None
        };

        Some(CursorContext { word, qualifier, contextual, lambda_decl, live_doc })
    }
}
