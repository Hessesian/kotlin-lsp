use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tower_lsp::lsp_types::{Range, SymbolKind};

/// Classification of a file's source set.
/// Determined at scan time based on file path and workspace configuration.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SourceSet {
    /// Production source code
    #[default]
    Main,
    /// Test source code (src/test/, src/androidTest/, etc.)
    Test,
    /// Library/SDK source from sourcePaths — excluded from references and rename
    Library,
}

/// File language, derived from path extension.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Language {
    Kotlin,
    Java,
    Swift,
}

impl Language {
    /// All languages, in priority order for extension matching.
    const ALL: [Language; 3] = [Language::Java, Language::Swift, Language::Kotlin];

    pub(crate) fn from_path(path: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|lang| {
                lang.parser()
                    .file_extensions()
                    .iter()
                    .any(|ext| path.ends_with(&format!(".{ext}")))
            })
            .unwrap_or(Language::Kotlin)
    }

    /// LSP language identifier; delegates to the language's provider.
    pub(crate) fn language_id(self) -> &'static str {
        self.parser().language_id()
    }

    pub(crate) fn code_fence(self) -> &'static str {
        self.language_id()
    }

    pub(crate) fn needs_semicolons(self) -> bool {
        matches!(self, Language::Java)
    }

    pub(crate) fn val_keyword(self) -> &'static str {
        match self {
            Language::Swift => "let",
            _ => "val",
        }
    }

    /// Return the stateless [`LanguageParser`] singleton for this language.
    ///
    /// This is the single authoritative dispatch point: use it instead of
    /// matching on the enum or calling `parse_by_extension` directly.
    pub(crate) fn parser(self) -> &'static dyn crate::language::LanguageParser {
        match self {
            Language::Kotlin => &crate::language::kotlin::KotlinParser,
            Language::Java => &crate::language::java::JavaParser,
            Language::Swift => &crate::language::swift::SwiftParser,
        }
    }
}

/// A position within a document used by infer functions.
///
/// `utf16_col` is a UTF-16 code unit offset, matching the LSP `Position.character` field.
/// Using a named struct (rather than a bare `(usize, usize)` pair) prevents silent
/// transposition of line and column arguments at call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorPos {
    pub line: usize,
    pub utf16_col: usize,
}

/// The caller's position context, used for visibility filtering and type-param substitution.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CallerContext<'a> {
    pub uri: Option<&'a str>,
    pub cursor_line: Option<u32>,
}

/// Kotlin/Java visibility of a declared symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) enum Visibility {
    #[default]
    Public,
    Internal,
    Protected,
    Private,
}

/// Single symbol definition entry stored in the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SymbolEntry {
    pub name: String,
    pub kind: SymbolKind,
    pub visibility: Visibility,
    /// Span of the entire declaration node.
    pub range: Range,
    /// Span of only the identifier — used for `selectionRange` in DocumentSymbol.
    pub selection_range: Range,
    /// Short signature shown in hover/symbol lists.
    /// e.g. `"fun addBiometryToPowerAuth(isAllowedForActiveOp: Boolean)"`,
    ///      `"class CreatePinViewModel"`, `"val isChecked: Boolean"`.
    /// Empty string when not computed.
    #[serde(default)]
    pub detail: String,
    /// Raw parameter text extracted from the CST at index time.
    /// Content between `(` and `)` of `function_value_parameters` / `formal_parameters`.
    /// e.g. `"x: Int, y: String = \"\""`. Empty for zero-param functions or non-callable symbols.
    #[serde(default)]
    pub params: String,
    /// `(required, total)` parameter counts derived from tree nodes at index time.
    /// A param is "required" when it has no `=` default value sibling in the CST.
    /// `(0, 0)` for non-callable symbols or zero-param functions.
    #[serde(default)]
    pub param_counts: (u8, u8),
    /// Generic type parameter names extracted from the CST at parse time.
    /// e.g. `class Foo<T, U>` → `["T", "U"]`.
    /// Empty for non-generic symbols.
    #[serde(default)]
    pub type_params: Vec<String>,
    /// For extension functions: the receiver type name (without generics).
    /// e.g. `fun MyType.foo()` → `"MyType"`, `fun <T> List<T>.bar()` → `"List"`.
    /// Empty string for non-extension symbols.
    #[serde(default)]
    pub extension_receiver: String,
    /// For extension functions: the full receiver type including generics.
    /// e.g. `fun <T> List<T>.bar()` → `"List<T>"`,
    ///      `fun <E, S> Flow<ReducedResult<E, S>>.collectState(…)` → `"Flow<ReducedResult<E, S>>"`.
    /// Empty string for non-extension symbols or when the receiver has no generics
    /// (in which case `extension_receiver` already carries the full type).
    #[serde(default)]
    pub extension_receiver_type: String,
    /// Enclosing class/object/interface name (immediate parent only).
    /// `None` for top-level declarations; `Some("ClassName")` for members.
    /// Assigned by `assign_containers()` after extraction.
    #[serde(default)]
    pub container: Option<String>,
}

impl SymbolEntry {
    /// Return the line number where the symbol's identifier starts.
    ///
    /// This is a convenience accessor for `.selection_range.start.line` (the identifier line),
    /// distinguishing it from `.range.start.line` (the full declaration start, which may differ on
    /// multiline declarations). Reduces coupling and avoids repeated deep field access.
    pub(crate) fn selection_start(&self) -> u32 {
        self.selection_range.start.line
    }
}

/// One import statement parsed from a Kotlin file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ImportEntry {
    /// Fully-qualified path without the trailing `.*`.
    /// e.g. `"com.example.Foo"` or `"com.example"` for star imports.
    pub full_path: String,
    /// The name usable locally: last segment, alias, or `"*"` for star.
    pub local_name: String,
    /// True for `import com.example.*`.
    pub is_star: bool,
}

impl ImportEntry {
    /// Does this import make `symbol_name` accessible when defined in `def_pkg`?
    ///
    /// Handles:
    /// - Star import: `import com.example.*` covers any symbol in package `com.example`
    /// - Direct import: `import com.example.Foo` covers `Foo` from `com.example`
    /// - Nested class import: `import com.example.Outer.Config` covers `Config` from `com.example`
    ///   (the nested container `Outer` is an intermediate segment)
    pub(crate) fn covers(&self, def_pkg: &str, symbol_name: &str) -> bool {
        if self.is_star {
            return self.full_path == def_pkg;
        }
        if self.local_name != symbol_name {
            return false;
        }
        if self.full_path == format!("{def_pkg}.{symbol_name}") {
            return true;
        }
        if let Some(rest) = self.full_path.strip_prefix(def_pkg) {
            if let Some(rest) = rest.strip_prefix('.') {
                return rest == symbol_name || rest.ends_with(&format!(".{symbol_name}"));
            }
        }
        false
    }
}

/// A structural syntax error detected by tree-sitter.
///
/// These are zero-false-positive issues: missing brackets, unclosed strings,
/// garbled syntax from a bad edit.  They are NOT serialized to the disk cache
/// (cheap to recompute on every parse).
#[derive(Debug, Clone)]
pub(crate) struct SyntaxError {
    pub range: Range,
    pub message: String,
}

/// All data we keep in memory for one source file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct FileData {
    pub symbols: Vec<SymbolEntry>,
    pub imports: Vec<ImportEntry>,
    /// Package declaration, e.g. `"com.example.app"`.
    pub package: Option<String>,
    /// Raw source lines — kept for `word_at()` lookups without hitting disk.
    /// Wrapped in Arc so that `clone()` is a cheap atomic refcount bump,
    /// not a full Vec<String> copy (which allocates one heap block per line).
    pub lines: Arc<Vec<String>>,
    /// Source set classification for this file.
    #[serde(default)]
    pub source_set: SourceSet,
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
    /// Field-access RHS patterns for unannotated properties: `val x = receiver.field`.
    /// Each entry is `(declaration_line, var_name, receiver_name, field_name)`.
    /// Used by field-type inference for indexed files (e.g. constructor params
    /// that expose a field as a class property).
    #[serde(default)]
    pub field_access_rhs: Vec<(u32, String, String, String)>,
    /// Explicit type annotations for properties, extracted from the CST at parse time.
    /// Each entry is `(declaration_line, var_name, declared_type)` where `declared_type`
    /// preserves generics and nullability: `val x: List<Foo>?` → `"List<Foo>?"`.
    /// Covers both `user_type` and `nullable_type` annotation nodes.
    /// Takes priority over line-scan inference for indexed files.
    #[serde(default)]
    pub type_annotations: Vec<(u32, String, String)>,
    /// Structural syntax errors from tree-sitter (ERROR / MISSING nodes).
    /// Transient — not serialized to disk cache.
    #[serde(skip)]
    pub syntax_errors: Vec<SyntaxError>,
}

impl FileData {
    /// Find the name of the innermost class/interface/object/enum that contains
    /// `line` in this file's symbol list. Returns `None` if the symbol is
    /// top-level (not inside any class).
    pub(crate) fn containing_class_at(&self, line: u32) -> Option<String> {
        const CLASS_KINDS: &[SymbolKind] = &[
            SymbolKind::CLASS,
            SymbolKind::INTERFACE,
            SymbolKind::STRUCT,
            SymbolKind::ENUM,
            SymbolKind::OBJECT,
        ];
        self.symbols
            .iter()
            .filter(|s| CLASS_KINDS.contains(&s.kind))
            .filter(|s| s.range.start.line <= line && line <= s.range.end.line)
            .min_by_key(|s| s.range.end.line.saturating_sub(s.range.start.line))
            .map(|s| s.name.clone())
    }
}

/// Result of parsing a single file. Pure data, no side effects.
/// This is what index_content will return instead of mutating DashMaps.
#[derive(Debug, Clone)]
pub(crate) struct FileIndexResult {
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
pub(crate) struct IndexStats {
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
pub(crate) struct WorkspaceIndexResult {
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

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
