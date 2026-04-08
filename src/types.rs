use std::sync::Arc;
use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{Range, SymbolKind};

/// Kotlin/Java visibility of a declared symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Visibility {
    #[default]
    Public,
    Internal,
    Protected,
    Private,
}

/// Single symbol definition entry stored in the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolEntry {
    pub name:             String,
    pub kind:             SymbolKind,
    pub visibility:       Visibility,
    /// Span of the entire declaration node.
    pub range:            Range,
    /// Span of only the identifier — used for `selectionRange` in DocumentSymbol.
    pub selection_range:  Range,
}

/// One import statement parsed from a Kotlin file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportEntry {
    /// Fully-qualified path without the trailing `.*`.
    /// e.g. `"com.example.Foo"` or `"com.example"` for star imports.
    pub full_path:  String,
    /// The name usable locally: last segment, alias, or `"*"` for star.
    pub local_name: String,
    /// True for `import com.example.*`.
    pub is_star:    bool,
}

/// All data we keep in memory for one source file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileData {
    pub symbols: Vec<SymbolEntry>,
    pub imports: Vec<ImportEntry>,
    /// Package declaration, e.g. `"com.example.app"`.
    pub package: Option<String>,
    /// Raw source lines — kept for `word_at()` lookups without hitting disk.
    /// Wrapped in Arc so that `clone()` is a cheap atomic refcount bump,
    /// not a full Vec<String> copy (which allocates one heap block per line).
    pub lines:   Arc<Vec<String>>,
    /// Lower-cased identifiers found before `:` on non-comment lines.
    /// Populated once at parse time; used by completion without re-scanning.
    pub declared_names: Vec<String>,
}
