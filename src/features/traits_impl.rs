//! `impl` blocks wiring each capability trait to `Indexer`.
//!
//! Two-jump navigation: any trait call → go-to-implementation → lands here.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tower_lsp::lsp_types::{CompletionItem, Location, Position, Range, Url};

use crate::indexer::{find_fun_signature_with_receiver, IgnoreMatcher, Indexer};
use crate::types::{FileData, SymbolEntry};

use super::traits::{
    CompletionIndex, DocumentAccess, LiveTreeAccess, ScopeQuery, SearchAccess, SignatureIndex,
    SymbolIndex,
};

// ─── SymbolIndex ─────────────────────────────────────────────────────────────

impl SymbolIndex for Indexer {
    fn find_definition_qualified(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
    ) -> Vec<Location> {
        self.find_definition_qualified(name, qualifier, from_uri)
    }

    fn definition_locations(&self, name: &str) -> Vec<Location> {
        self.definition_locations(name)
    }

    fn subtypes_of(&self, name: &str) -> Vec<Location> {
        self.subtypes_of(name)
    }

    fn file_data_for(&self, uri: &str) -> Option<Arc<FileData>> {
        self.files.get(uri).map(|f| f.clone())
    }

    fn file_symbols(&self, uri: &Url) -> Vec<SymbolEntry> {
        self.file_symbols(uri)
    }

    fn for_each_indexed_file(&self, f: &mut dyn FnMut(&str, &Arc<FileData>) -> bool) {
        self.for_each_indexed_file(f);
    }

    fn enclosing_class_at(&self, uri: &Url, row: u32) -> Option<String> {
        self.enclosing_class_at(uri, row)
    }
}

// ─── DocumentAccess ──────────────────────────────────────────────────────────

impl DocumentAccess for Indexer {
    fn mem_lines_for(&self, uri: &str) -> Option<Arc<Vec<String>>> {
        self.mem_lines_for(uri)
    }

    fn lines_for(&self, uri: &Url) -> Option<Arc<Vec<String>>> {
        self.lines_for(uri)
    }

    fn word_and_qualifier_at(&self, uri: &Url, pos: Position) -> Option<(String, Option<String>)> {
        self.word_and_qualifier_at(uri, pos)
    }

    fn word_at(&self, uri: &Url, pos: Position) -> Option<String> {
        self.word_at(uri, pos)
    }

    fn word_and_range_at(&self, uri: &Url, pos: Position) -> Option<(String, Range)> {
        self.word_and_range_at(uri, pos)
    }
}

// ─── ScopeQuery ──────────────────────────────────────────────────────────────

impl ScopeQuery for Indexer {
    fn is_library_uri(&self, uri: &Url) -> bool {
        self.is_library_uri(uri)
    }

    fn package_of(&self, uri: &Url) -> Option<String> {
        self.package_of(uri)
    }

    fn resolve_symbol_via_import(&self, uri: &Url, name: &str) -> (Option<String>, Option<String>) {
        self.resolve_symbol_via_import(uri, name)
    }

    fn declared_parent_class_of(&self, name: &str, preferred_uri: &Url) -> Option<String> {
        self.declared_parent_class_of(name, preferred_uri)
    }

    fn declared_package_of(&self, name: &str) -> Option<String> {
        self.declared_package_of(name)
    }

    fn is_declared_in(&self, uri: &Url, name: &str) -> bool {
        self.is_declared_in(uri, name)
    }
}

// ─── SearchAccess ────────────────────────────────────────────────────────────

impl SearchAccess for Indexer {
    fn rg_context(&self) -> (Option<PathBuf>, Option<Arc<IgnoreMatcher>>) {
        let root = self.workspace_root.get();
        let ignore = self.ignore_matcher.read().ok().and_then(|g| g.clone());
        (root, ignore)
    }

    fn rg_scope_for_path(
        &self,
        open_file: Option<&std::path::Path>,
    ) -> (Option<PathBuf>, Vec<String>, Option<Arc<IgnoreMatcher>>) {
        Indexer::rg_scope_for_path(self, open_file)
    }
}

// ─── CompletionIndex ─────────────────────────────────────────────────────────

impl CompletionIndex for Indexer {
    fn completions(
        &self,
        uri: &Url,
        position: Position,
        snippets: bool,
    ) -> (Vec<CompletionItem>, bool) {
        self.completions(uri, position, snippets)
    }

    fn is_indexing_in_progress(&self) -> bool {
        self.indexing_in_progress.load(Ordering::Acquire)
    }
}

// ─── SignatureIndex ───────────────────────────────────────────────────────────

impl SignatureIndex for Indexer {
    fn find_fun_signature_with_receiver(
        &self,
        uri: &Url,
        name: &str,
        receiver: Option<&str>,
    ) -> Option<String> {
        find_fun_signature_with_receiver(self, uri, name, receiver)
    }
}

// ─── LiveTreeAccess ──────────────────────────────────────────────────────────

impl LiveTreeAccess for Indexer {
    fn call_info_at(
        &self,
        pos: tower_lsp::lsp_types::Position,
        uri: &Url,
    ) -> Option<crate::indexer::CallInfo> {
        crate::indexer::cst_call_info(pos, self, uri)
    }

    fn folding_ranges_for(&self, uri: &Url) -> Option<Vec<tower_lsp::lsp_types::FoldingRange>> {
        crate::indexer::cst_folding_ranges(self, uri)
    }
}
