//! Capability traits — the read-side abstraction boundary.
//!
//! Each trait groups methods by *what the Indexer can do*, not by which feature
//! uses them.  Feature functions compose the traits they need as bounds:
//!
//! ```rust,ignore
//! fn find_definition(cursor: &RawCursor, index: &(impl SymbolIndex + DocumentAccess)) { … }
//! ```
//!
//! Navigation invariant: trait method → go-to-implementation → `impl X for Indexer`.
//! Always two jumps.

use std::path::PathBuf;
use std::sync::Arc;

use tower_lsp::lsp_types::{CompletionItem, Location, Position, Range, Url};

use crate::indexer::IgnoreMatcher;
use crate::types::{FileData, SymbolEntry};

// ─── SymbolIndex ─────────────────────────────────────────────────────────────

/// Symbol lookup — find, resolve, and navigate across the indexed codebase.
#[allow(dead_code)]
pub(crate) trait SymbolIndex {
    /// Find definition locations for `name`, using `qualifier` and `from_uri`
    /// to narrow the search to imported/accessible symbols.
    fn find_definition_qualified(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
    ) -> Vec<Location>;

    /// All definition locations for `name` regardless of import context.
    fn definition_locations(&self, name: &str) -> Vec<Location>;

    /// All known direct subtypes (class/interface implementors) of `name`.
    fn subtypes_of(&self, name: &str) -> Vec<Location>;

    /// Return the `FileData` for the indexed file at `uri`, if indexed.
    fn file_data_for(&self, uri: &str) -> Option<Arc<FileData>>;

    /// All top-level symbols indexed for `uri`.
    fn file_symbols(&self, uri: &Url) -> Vec<SymbolEntry>;

    /// Iterate all indexed files, calling `f(uri_str, file_data)`.
    /// Return `false` from `f` to stop iteration early.
    fn for_each_indexed_file(&self, f: &mut dyn FnMut(&str, &Arc<FileData>) -> bool);

    /// Name of the innermost class/object enclosing `row` in `uri`, if any.
    fn enclosing_class_at(&self, uri: &Url, row: u32) -> Option<String>;
}

// ─── DocumentAccess ──────────────────────────────────────────────────────────

/// Document text and cursor-position access.
#[allow(dead_code)]
pub(crate) trait DocumentAccess {
    /// Lines from the in-memory caches only (no disk I/O).
    /// Prefers live (unsaved) buffer; falls back to indexed snapshot.
    fn mem_lines_for(&self, uri: &str) -> Option<Arc<Vec<String>>>;

    /// Lines for `uri`, including disk fallback if not live.
    fn lines_for(&self, uri: &Url) -> Option<Arc<Vec<String>>>;

    /// Extract the identifier and optional dot-qualifier at `pos`.
    fn word_and_qualifier_at(&self, uri: &Url, pos: Position) -> Option<(String, Option<String>)>;

    /// Extract just the identifier token at `pos`.
    fn word_at(&self, uri: &Url, pos: Position) -> Option<String>;

    /// Extract the identifier token and its source range at `pos`.
    fn word_and_range_at(&self, uri: &Url, pos: Position) -> Option<(String, Range)>;
}

// ─── ScopeQuery ──────────────────────────────────────────────────────────────

/// Import and package scope resolution, plus library classification.
#[allow(dead_code)]
pub(crate) trait ScopeQuery {
    /// Returns `true` if `uri` is a library/stdlib file (not workspace source).
    fn is_library_uri(&self, uri: &Url) -> bool;

    /// The declared package name for the file at `uri`.
    fn package_of(&self, uri: &Url) -> Option<String>;

    /// Scan imports in `uri` for `name`; returns `(parent_class, declared_pkg)`.
    ///
    /// E.g. `import com.example.DashboardViewModel.Effect`
    /// → `(Some("DashboardViewModel"), Some("com.example.DashboardViewModel"))`
    fn resolve_symbol_via_import(&self, uri: &Url, name: &str) -> (Option<String>, Option<String>);

    /// If `name` is declared as an inner class, return the enclosing class name.
    /// Searches `preferred_uri` first, then any definition site.
    fn declared_parent_class_of(&self, name: &str, preferred_uri: &Url) -> Option<String>;

    /// Package that `name` is declared in (searches all indexed files).
    fn declared_package_of(&self, name: &str) -> Option<String>;

    /// Returns `true` if `name` is declared in the file at `uri`.
    fn is_declared_in(&self, uri: &Url, name: &str) -> bool;
}

// ─── SearchAccess ────────────────────────────────────────────────────────────

/// Ripgrep-based fallback search context.
#[allow(dead_code)]
pub(crate) trait SearchAccess {
    /// Returns the (workspace_root, ignore_matcher) tuple used to scope `rg` calls.
    fn rg_context(&self) -> (Option<PathBuf>, Option<Arc<IgnoreMatcher>>);
}

// ─── CompletionIndex ─────────────────────────────────────────────────────────

/// Completion pipeline — already fully orchestrated inside the Indexer.
#[allow(dead_code)]
pub(crate) trait CompletionIndex {
    /// Run the full completion pipeline for `uri` at `position`.
    fn completions(
        &self,
        uri: &Url,
        position: Position,
        snippets: bool,
    ) -> (Vec<CompletionItem>, bool);

    /// `true` while a background scan/index is in progress.
    fn is_indexing_in_progress(&self) -> bool;
}

// ─── SignatureIndex ───────────────────────────────────────────────────────────

/// Function signature lookup with optional receiver type matching.
#[allow(dead_code)]
pub(crate) trait SignatureIndex {
    /// Signature text for `name`, optionally narrowed to `receiver`'s type.
    fn find_fun_signature_with_receiver(
        &self,
        uri: &Url,
        name: &str,
        receiver: Option<&str>,
    ) -> String;
}

// ─── LiveTreeAccess ──────────────────────────────────────────────────────────

/// Live-syntax access — operations that require the live tree-sitter parse tree.
///
/// Kept separate from the index-based traits because it requires live-tree state
/// that those traits do not; mixing them would force test stubs to provide CST
/// infrastructure unnecessarily.
#[allow(dead_code)]
pub(crate) trait LiveTreeAccess {
    /// Extract the call-site name, qualifier, and active parameter index
    /// at `pos` using the live parse tree for `uri`.
    ///
    /// Returns `None` when the cursor is not inside a call expression or when
    /// no live tree is available.
    fn call_info_at(
        &self,
        pos: tower_lsp::lsp_types::Position,
        uri: &Url,
    ) -> Option<crate::indexer::CallInfo>;

    /// Compute folding ranges for `uri` using the live parse tree.
    ///
    /// Returns `None` when no live tree is available for the file.
    fn folding_ranges_for(&self, uri: &Url) -> Option<Vec<tower_lsp::lsp_types::FoldingRange>>;
}
