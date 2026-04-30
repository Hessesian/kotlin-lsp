//! Lookup phase: query the index for symbol information.
//!
//! This module owns the "read path" of the indexer for symbol resolution:
//!
//! - [`Indexer::is_declared_in`]             — test if a name is declared in a file
//! - [`Indexer::find_definition`]            — resolve definition locations by name
//! - [`Indexer::find_definition_qualified`]  — resolve with optional dot-qualifier
//! - [`Indexer::hover_info`]                 — build Markdown hover snippet by name
//! - [`Indexer::hover_info_at_location`]     — build hover snippet for a specific location
//! - [`Indexer::file_symbols`]               — all symbols declared in a file
//! - [`Indexer::package_of`]                 — package declared in a file
//! - [`Indexer::declared_package_of`]        — package in which a name is declared
//! - [`Indexer::declared_parent_class_of`]   — enclosing class at declaration site
//! - [`Indexer::resolve_symbol_via_import`]  — resolve parent class / package via imports

use tower_lsp::lsp_types::*;

use crate::types::SymbolEntry;
use crate::LinesExt;
use crate::StrExt;
use crate::stdlib::hover;
use super::Indexer;
use super::doc::extract_doc_comment;

impl Indexer {
    /// Returns true if `name` has at least one definition location inside `uri`.
    pub fn is_declared_in(&self, uri: &Url, name: &str) -> bool {
        self.definitions.get(name)
            .map(|locs| locs.iter().any(|l| l.uri == *uri))
            .unwrap_or(false)
    }

    /// Resolve definition locations for `name` (with optional dot-qualifier).
    #[allow(dead_code)]
    pub fn find_definition(&self, name: &str, from_uri: &Url) -> Vec<Location> {
        self.resolve_symbol(name, None, from_uri)
    }

    pub fn find_definition_qualified(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
    ) -> Vec<Location> {
        self.resolve_symbol(name, qualifier, from_uri)
    }

    /// Build a Markdown hover snippet for a symbol name.
    pub fn hover_info(&self, name: &str) -> Option<String> {
        // Check stdlib first so well-known symbols (run, apply, map, …) get
        // proper signatures even when no project source contains them.
        if let Some(md) = hover(name) { return Some(md); }

        // Drop the dashmap ref before taking the second one.
        let loc: Location = {
            let r = self.definitions.get(name)?;
            r.first()?.clone()
        };
        self.hover_info_at_location(&loc, name)
    }

    /// Build hover markdown for `name` at a specific resolved `Location`.
    /// Used by the hover handler so it shows the same symbol as go-to-definition.
    pub fn hover_info_at_location(&self, loc: &Location, name: &str) -> Option<String> {
        // On-demand index: the file may have been found by rg but not yet indexed.
        if !self.files.contains_key(loc.uri.as_str()) {
            if let Ok(path) = loc.uri.to_file_path() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    self.index_content(&loc.uri, &content);
                }
            }
        }
        let data = self.files.get(loc.uri.as_str())?;
        // Prefer exact match by resolved location range; fall back to name match
        // for symbols found via rg where the range may not align exactly.
        let sym = data.symbols.iter().find(|s| s.selection_range == loc.range)
            .or_else(|| data.symbols.iter().find(|s| s.name == name))?;

        let start_line = sym.selection_range.start.line as usize;
        let sig = data.lines.collect_signature(start_line);

        let lang = if loc.uri.path().ends_with(".kt") { "kotlin" }
                   else if loc.uri.path().ends_with(".swift") { "swift" }
                   else { "java" };

        let code_block = if sig.is_empty() {
            format!("```{}\n{} {}\n```", lang, symbol_kw_for_lang(sym.kind, lang), name)
        } else {
            format!("```{}\n{}\n```", lang, sig)
        };

        // Prepend KDoc / Javadoc comment if one immediately precedes the declaration.
        if let Some(doc) = extract_doc_comment(&data.lines, start_line) {
            Some(format!("{doc}\n\n---\n\n{code_block}"))
        } else {
            Some(code_block)
        }
    }

    /// All symbols declared in the given file (for `documentSymbol`).
    pub fn file_symbols(&self, uri: &Url) -> Vec<SymbolEntry> {
        self.files
            .get(uri.as_str())
            .map(|d| d.symbols.clone())
            .unwrap_or_default()
    }

    /// Return the package declared in the given file, if any.
    pub fn package_of(&self, uri: &Url) -> Option<String> {
        self.files.get(uri.as_str())?.package.clone()
    }

    /// Return the package in which `name` is declared, by looking up its
    /// definition locations and reading the `package` field of those files.
    pub fn declared_package_of(&self, name: &str) -> Option<String> {
        let locs = self.definitions.get(name)?;
        for loc in locs.iter() {
            if let Some(f) = self.files.get(loc.uri.as_str()) {
                if let Some(pkg) = &f.package {
                    return Some(pkg.clone());
                }
            }
        }
        None
    }

    /// If `name` is declared as an inner/nested class, return the name of its
    /// enclosing class at the declaration site in `preferred_uri` (if found there),
    /// otherwise the first definition site.
    pub fn declared_parent_class_of(&self, name: &str, preferred_uri: &Url) -> Option<String> {
        let locs = self.definitions.get(name)?;
        // Try declaration in the preferred (current) file first.
        for loc in locs.iter() {
            if loc.uri == *preferred_uri {
                return self.enclosing_class_at(&loc.uri, loc.range.start.line);
            }
        }
        // Fall back to first definition in any file.
        for loc in locs.iter() {
            if let Some(parent) = self.enclosing_class_at(&loc.uri, loc.range.start.line) {
                return Some(parent);
            }
        }
        None
    }

    /// Scan imports in `uri` for `name` and return (parent_class, declared_pkg)
    /// as resolved from the import statement.  E.g.:
    ///   `import com.example.DashboardViewModel.Effect`
    ///   → parent_class = Some("DashboardViewModel"), pkg = Some("com.example.DashboardViewModel")
    pub fn resolve_symbol_via_import(
        &self,
        uri: &Url,
        name: &str,
    ) -> (Option<String>, Option<String>) {
        let file = match self.files.get(uri.as_str()) {
            Some(f) => f,
            None    => return (None, None),
        };
        for line in file.lines.iter() {
            let t = line.trim();
            if !t.starts_with("import ") { continue; }
            // Handle `import a.b.c.Name` and `import a.b.c.Name as Alias`
            let import_path = t["import ".len()..].split_whitespace().next().unwrap_or("");
            let segments: Vec<&str> = import_path.split('.').collect();
            // Last segment should match `name` (or be `*`).
            let last = *segments.last().unwrap_or(&"");
            if last != name && last != "*" { continue; }

            // Found a matching import. The declared package is everything up to (not incl.) `name`.
            // The parent class is the segment immediately before `name` if it starts uppercase.
            if last == name && segments.len() >= 2 {
                let pkg = segments[..segments.len() - 1].join(".");
                let parent = segments.get(segments.len() - 2)
                    .filter(|s| s.starts_with_uppercase())
                    .map(|s| s.to_string());
                return (parent, Some(pkg));
            }
        }
        (None, None)
    }
}

fn symbol_kw(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::CLASS          => "class",
        SymbolKind::INTERFACE      => "interface",
        SymbolKind::FUNCTION       => "fun",
        SymbolKind::METHOD         => "fun",
        SymbolKind::VARIABLE       => "var",
        SymbolKind::CONSTANT       => "val",
        SymbolKind::OBJECT         => "object",
        SymbolKind::TYPE_PARAMETER => "typealias",
        SymbolKind::ENUM           => "enum class",
        SymbolKind::FIELD          => "field",
        _                          => "symbol",
    }
}

fn symbol_kw_for_lang(kind: SymbolKind, lang: &str) -> &'static str {
    let kw = symbol_kw(kind);
    // Swift uses `func`, not `fun`.
    if lang == "swift" && kw == "fun" { "func" } else { kw }
}

#[cfg(test)]
#[path = "lookup_tests.rs"]
mod tests;
