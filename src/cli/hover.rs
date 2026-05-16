//! Hover implementation for the CLI.

use std::path::Path;
use std::sync::Arc;

use tower_lsp::lsp_types::{Position, Url};

use crate::indexer::resolution::{enrich_at_line, ResolveOptions, SubstitutionContext};
use crate::indexer::Indexer;

/// Return a hover string for `file:line:col` using the pre-built index.
/// Line and col are 1-based (human-friendly) and converted internally to 0-based.
pub(crate) fn hover_at(indexer: &Arc<Indexer>, file: &Path, line: u32, col: u32) -> Option<String> {
    let abs = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let uri = Url::from_file_path(&abs).ok()?;

    // Index on-demand if this file wasn't already in cache.
    indexer.ensure_indexed(&uri);

    // Parse a live tree so that CST-based inference (lambda `it`/`this`,
    // chain resolution) works in CLI mode without a running LSP session.
    if indexer.live_doc(&uri).is_none() {
        if let Ok(content) = std::fs::read_to_string(&abs) {
            indexer.store_live_tree(&uri, &content);
        }
    }

    let resolved = enrich_at_line(
        indexer.as_ref(),
        uri.as_str(),
        line.saturating_sub(1), // 1-based → 0-based
        col.saturating_sub(1),
        SubstitutionContext::None,
        &ResolveOptions::hover(),
    );
    if let Some(resolved) = resolved {
        let mut out = resolved.signature;
        if !resolved.doc.is_empty() {
            out.push_str("\n\n");
            out.push_str(&resolved.doc);
        }
        return Some(out);
    }

    // Fallback for local bindings (e.g., function parameters) that may not
    // have a dedicated SymbolEntry at the usage line.
    let pos = Position::new(line.saturating_sub(1), col.saturating_sub(1));
    let word = indexer.word_at(&uri, pos)?;

    // Handle `it` / `this` via contextual lambda-param inference.
    if word == "it" || word == "this" {
        let kind = crate::resolver::ReceiverKind::Contextual {
            name: &word,
            position: pos,
        };
        if let Some(receiver) =
            crate::resolver::infer::infer_receiver_type(indexer.as_ref(), kind, &uri)
        {
            return Some(format!("val {word}: {}", receiver.raw));
        }
    }

    let ty = crate::resolver::infer::infer_variable_type(indexer.as_ref(), &word, &uri)?;
    Some(format!("val {word}: {ty}"))
}
