//! `complete` subcommand — show completion candidates at a file position.

use std::path::Path;
use std::sync::Arc;

use tower_lsp::lsp_types::{Position, Url};

use crate::indexer::Indexer;

/// Return completion labels for `file:line:col`.
/// Line and col are 1-based (human-friendly) and converted internally to 0-based.
pub(crate) fn completions_at(
    indexer: &Arc<Indexer>,
    file: &Path,
    line: u32,
    col: u32,
) -> Vec<CompletionRow> {
    let abs = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let Ok(uri) = Url::from_file_path(&abs) else {
        return Vec::new();
    };

    indexer.ensure_indexed(&uri);

    let position = Position {
        line: line.saturating_sub(1),
        character: col.saturating_sub(1),
    };

    let (items, _) = indexer.completions(&uri, position, false);
    items
        .into_iter()
        .map(|item| {
            let kind = item.kind.map(kind_name).unwrap_or_default().to_string();
            let detail = item.detail.unwrap_or_default();
            let import = item
                .additional_text_edits
                .as_ref()
                .and_then(|edits| edits.first())
                .map(|e| e.new_text.clone());
            CompletionRow {
                label: item.label,
                kind,
                detail,
                import,
            }
        })
        .collect()
}

pub(crate) struct CompletionRow {
    pub label: String,
    pub kind: String,
    pub detail: String,
    pub import: Option<String>,
}

fn kind_name(kind: tower_lsp::lsp_types::CompletionItemKind) -> &'static str {
    use tower_lsp::lsp_types::CompletionItemKind as K;
    match kind {
        K::CLASS => "class",
        K::INTERFACE => "interface",
        K::ENUM => "enum",
        K::ENUM_MEMBER => "enum_member",
        K::FUNCTION => "fun",
        K::METHOD => "method",
        K::FIELD => "field",
        K::PROPERTY => "property",
        K::VARIABLE => "var",
        K::CONSTANT => "const",
        K::STRUCT => "struct",
        K::MODULE => "namespace",
        K::VALUE => "value",
        K::TEXT => "text",
        K::UNIT => "unit",
        K::COLOR => "color",
        K::FILE => "file",
        K::REFERENCE => "reference",
        K::FOLDER => "folder",
        K::EVENT => "event",
        K::OPERATOR => "operator",
        _ => "?",
    }
}
