use std::sync::Arc;
use serde::{Deserialize, Serialize};
use tower_lsp::lsp_types::{Range, SymbolKind};

/// Source language inferred from a file's path extension.
///
/// Used to centralise all `path.ends_with(".kt")` / `".swift"` / `".java"` dispatch.
/// Obtain via [`Language::from_path`]; the default is `Kotlin` (most common case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Language {
    #[default]
    Kotlin,
    Java,
    Swift,
}

impl Language {
    /// Infer the language from a file path or URI string.
    ///
    /// Explicit matches: `.java` → Java, `.swift` → Swift.
    /// Everything else (`.kt`, `.kts`, unknown extensions) → Kotlin, since this
    /// server is Kotlin-primary and `.kts` files use the same language features.
    pub fn from_path(path: &str) -> Self {
        if path.ends_with(".swift")                        { Language::Swift }
        else if path.ends_with(".java")                    { Language::Java  }
        else                                               { Language::Kotlin }
    }

    pub fn is_kotlin(self)  -> bool { matches!(self, Language::Kotlin) }
    pub fn is_java(self)    -> bool { matches!(self, Language::Java)   }
    pub fn is_swift(self)   -> bool { matches!(self, Language::Swift)  }
}

/// A position within a document used by infer functions.
///
/// `utf16_col` is a UTF-16 code unit offset, matching the LSP `Position.character` field.
/// Using a named struct (rather than a bare `(usize, usize)` pair) prevents silent
/// transposition of line and column arguments at call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPos {
    pub line:      usize,
    pub utf16_col: usize,
}

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
    /// Short signature shown in hover/symbol lists.
    /// e.g. `"fun addBiometryToPowerAuth(isAllowedForActiveOp: Boolean)"`,
    ///      `"class CreatePinViewModel"`, `"val isChecked: Boolean"`.
    /// Empty string when not computed.
    #[serde(default)]
    pub detail:           String,
    /// Generic type parameter names extracted from the CST at parse time.
    /// e.g. `class Foo<T, U>` → `["T", "U"]`.
    /// Empty for non-generic symbols.
    #[serde(default)]
    pub type_params:          Vec<String>,
    /// For extension functions: the receiver type name (without generics).
    /// e.g. `fun MyType.foo()` → `"MyType"`, `fun <T> List<T>.bar()` → `"List"`.
    /// Empty string for non-extension symbols.
    #[serde(default)]
    pub extension_receiver:   String,
}

impl SymbolEntry {
    /// Return the line number where the symbol's identifier starts.
    ///
    /// This is a convenience accessor for `.selection_range.start.line` (the identifier line),
    /// distinguishing it from `.range.start.line` (the full declaration start, which may differ on
    /// multiline declarations). Reduces coupling and avoids repeated deep field access.
    pub fn start_line(&self) -> u32 {
        self.selection_range.start.line
    }
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

/// A structural syntax error detected by tree-sitter.
///
/// These are zero-false-positive issues: missing brackets, unclosed strings,
/// garbled syntax from a bad edit.  They are NOT serialized to the disk cache
/// (cheap to recompute on every parse).
#[derive(Debug, Clone)]
pub struct SyntaxError {
    pub range:   Range,
    pub message: String,
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
    /// Supertype relationships extracted from the CST at parse time.
    /// Each entry is `(class_name_line, supertype_name, type_args)` where:
    /// - `class_name_line` matches `SymbolEntry::selection_range.start.line` for the declaring class
    /// - `supertype_name` is the base name without type arguments (e.g. `"FlowReducer"`)
    /// - `type_args` are the concrete type arguments (e.g. `["Event", "Effect", "State"]`)
    #[serde(default)]
    pub supers: Vec<(u32, String, Vec<String>)>,
    /// RHS-inferred types for unannotated properties, extracted from the CST at parse time.
    /// Each entry is `(declaration_line, var_name, inferred_type)`.
    /// Used as the primary type inference path for indexed files, avoiding fragile string
    /// scanning for patterns like `inject<T>()`, `by lazy { T() }`, and `T(args)`.
    #[serde(default)]
    pub rhs_types: Vec<(u32, String, String)>,
    /// Method-call RHS patterns for unannotated properties: `val x = receiver.method(args)`.
    /// Each entry is `(declaration_line, var_name, receiver_name, method_name)`.
    /// Used by method-return-type inference for indexed files.
    #[serde(default)]
    pub method_call_rhs: Vec<(u32, String, String, String)>,
    /// Structural syntax errors from tree-sitter (ERROR / MISSING nodes).
    /// Transient — not serialized to disk cache.
    #[serde(skip)]
    pub syntax_errors: Vec<SyntaxError>,
}

impl FileData {
    /// Find the name of the innermost class/interface/object/enum that contains
    /// `line` in this file's symbol list. Returns `None` if the symbol is
    /// top-level (not inside any class).
    pub fn containing_class_at(&self, line: u32) -> Option<String> {
        const CLASS_KINDS: &[SymbolKind] = &[
            SymbolKind::CLASS, SymbolKind::INTERFACE, SymbolKind::STRUCT,
            SymbolKind::ENUM,  SymbolKind::OBJECT,
        ];
        self.symbols.iter()
            .filter(|s| CLASS_KINDS.contains(&s.kind))
            .filter(|s| s.range.start.line <= line && line <= s.range.end.line)
            .min_by_key(|s| s.range.end.line.saturating_sub(s.range.start.line))
            .map(|s| s.name.clone())
    }

    /// Find a symbol at a specific position (line, utf16_col).
    ///
    /// Attempts exact position match first (col-aware), then falls back
    /// to line-only match for imprecise callers. Returns None if no symbol
    /// is found at that location.
    pub fn symbol_at(&self, line: u32, col: u32) -> Option<&SymbolEntry> {
        // Exact position match first
        self.symbols.iter()
            .find(|s| s.selection_range.start.line == line && s.selection_range.start.character == col)
            .or_else(|| {
                // Fallback: line-only match
                self.symbols.iter()
                    .find(|s| s.selection_range.start.line == line)
            })
    }
}


/// Result of parsing a single file. Pure data, no side effects.
/// This is what index_content will return instead of mutating DashMaps.
#[derive(Debug, Clone)]
pub struct FileIndexResult {
    /// File URI that was parsed.
    pub uri: tower_lsp::lsp_types::Url,
    /// Parsed file data (symbols, imports, package, lines).
    pub data: FileData,
    /// Supertype relationships discovered in this file.
    /// Format: (supertype_name, implementing_class_location)
    pub supertypes: Vec<(String, tower_lsp::lsp_types::Location)>,
    /// Content hash for cache invalidation.
    pub content_hash: u64,
    /// Parse error if tree-sitter failed.
    #[allow(dead_code)]
    pub error: Option<String>,
}

/// Statistics about an indexing run.
#[derive(Debug, Clone, Default)]
pub struct IndexStats {
    /// Total files discovered.
    #[allow(dead_code)]
    pub files_discovered: usize,
    /// Files loaded from cache (mtime unchanged).
    pub cache_hits: usize,
    /// Files actually parsed by tree-sitter.
    pub files_parsed: usize,
    /// Total symbols extracted.
    pub symbols_extracted: usize,
    /// Total packages found.
    #[allow(dead_code)]
    pub packages_found: usize,
    /// Parse errors encountered.
    #[allow(dead_code)]
    pub errors: usize,
}

/// Result of indexing an entire workspace. Pure data, no side effects.
/// This is what index_workspace will return instead of mutating state.
#[derive(Debug, Clone)]
pub struct WorkspaceIndexResult {
    /// All successfully parsed files.
    pub files: Vec<FileIndexResult>,
    /// Statistics about the indexing run.
    pub stats: IndexStats,
    /// Workspace root that was indexed.
    #[allow(dead_code)]
    pub workspace_root: std::path::PathBuf,
    /// True if the run was aborted mid-way (e.g. root generation changed).
    /// Callers must NOT call apply_workspace_result when this is true — doing
    /// so would reset_index_state() and apply only the partial result set.
    pub aborted: bool,
    /// True when the workspace was fully scanned (not truncated by MAX_INDEX_FILES).
    /// Written into the on-disk cache so warm-manifest mode is only used when the
    /// cache is a complete snapshot of the workspace.
    pub complete_scan: bool,
}
