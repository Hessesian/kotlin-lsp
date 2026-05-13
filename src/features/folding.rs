//! Folding ranges feature — uses the live tree-sitter parse tree via `LiveTreeAccess`.
//!
//! Block nodes (class body, function body, lambdas, control flow) → `Region`.
//! Consecutive `line_comment` sibling runs → `Comment`.
//! `multiline_comment` nodes spanning multiple lines → `Comment`.
//! `import_list` → `Imports`.

use tower_lsp::lsp_types::{FoldingRange, Url};

use super::traits::LiveTreeAccess;

/// Compute folding ranges for `uri` from the live parse tree.
///
/// Returns `None` when no live tree exists for the file (not yet opened/edited).
pub(crate) fn compute_folding_ranges(
    uri: &Url,
    index: &impl LiveTreeAccess,
) -> Option<Vec<FoldingRange>> {
    index.folding_ranges_for(uri)
}
