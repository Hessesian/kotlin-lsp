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
    ///
    /// **Note:** Production code uses `resolution::resolve_symbol_info` +
    /// `backend::format::format_symbol_hover` instead. This method is retained
    /// for lookup unit tests (Phase 10 will migrate tests and delete it).
    pub fn hover_info(&self, name: &str, calling_uri: Option<&str>) -> Option<String> {
        // Check stdlib first so well-known symbols (run, apply, map, …) get
        // proper signatures even when no project source contains them.
        if let Some(md) = hover(name) { return Some(md); }

        // Drop the dashmap ref before taking the second one.
        let loc: Location = {
            let r = self.definitions.get(name)?;
            r.first()?.clone()
        };
        self.hover_info_at_location(&loc, name, calling_uri, None)
    }

    /// Build hover markdown for `name` at a specific resolved `Location`.
    ///
    /// **Note:** Production code uses `resolution::enrich_at_location` +
    /// `backend::format::format_symbol_hover` instead. This method is retained
    /// for lookup unit tests (Phase 10 will migrate tests and delete it).
    ///
    /// `calling_uri` — the file where the cursor is (used to substitute generic type
    /// parameters with concrete types when the symbol is from a base class).
    /// `cursor_line` — the line of the cursor in `calling_uri`; used to disambiguate
    /// when multiple classes in the same file extend the same generic base.
    pub fn hover_info_at_location(&self, loc: &Location, name: &str, calling_uri: Option<&str>, cursor_line: Option<u32>) -> Option<String> {
        // On-demand index: the file may have been found by rg but not yet indexed.
        if !self.files.contains_key(loc.uri.as_str()) {
            if let Ok(path) = loc.uri.to_file_path() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    self.index_content(&loc.uri, &content);
                }
            }
        }
        // Extract needed fields and drop the DashMap guard before any inference
        // call (which may reacquire the same shard and deadlock otherwise).
        let (sym_kind, sym_sel_range, sym_detail, lines) = {
            let data = self.files.get(loc.uri.as_str())?;
            let sym = data.symbols.iter().find(|s| s.selection_range == loc.range)
                .or_else(|| data.symbols.iter().find(|s| s.name == name))?;
            (sym.kind, sym.selection_range, sym.detail.clone(), data.lines.clone())
        };

        let start_line = sym_sel_range.start.line as usize;

        // For property/variable symbols use the pre-indexed detail (a single declaration
        // line) rather than collect_signature, which would keep reading until it finds a
        // closing ')' and accidentally include sibling constructor parameters.
        let raw_sig = if matches!(sym_kind, SymbolKind::PROPERTY | SymbolKind::VARIABLE)
            && !sym_detail.is_empty()
        {
            sym_detail
        } else {
            lines.collect_signature(start_line)
        };

        // For unannotated val/var, try to show an inferred type instead of the
        // raw RHS expression (e.g. `val response: AccountDetailResponseBody`
        // instead of `val response = service.getDetail(...)`).
        let sig = if matches!(sym_kind, SymbolKind::PROPERTY | SymbolKind::VARIABLE)
            && !raw_sig.contains(&format!("{name}:"))
        {
            if let Some(inferred) = self.infer_variable_type(name, &loc.uri) {
                // Keep modifiers from the original signature (e.g. `private`, `override`).
                // Find the name in the raw_sig and replace everything from `name` onward
                // with `name: InferredType`, discarding the `= rhs` initializer.
                let needle = format!(" {name}");
                let name_pos = raw_sig.find(&needle)
                    .map(|p| p + 1)
                    .or_else(|| if raw_sig.starts_with(name) { Some(0) } else { None });
                if let Some(pos) = name_pos {
                    format!("{}: {inferred}", &raw_sig[..pos + name.len()])
                } else {
                    let kw = if sym_kind == SymbolKind::VARIABLE { "var" } else { "val" };
                    format!("{kw} {name}: {inferred}")
                }
            } else {
                raw_sig
            }
        } else {
            raw_sig
        };

        // Apply generic type parameter substitution when the cursor is in a different
        // file (subtype) than where the symbol is defined.
        let sig = if let Some(cu) = calling_uri {
            let subst = build_type_param_subst(self, loc.uri.as_str(), sym_sel_range.start.line, cu, cursor_line);
            if subst.is_empty() { sig } else { apply_subst(&sig, &subst) }
        } else {
            sig
        };

        let lang = lang_str(loc.uri.path());

        let code_block = if sig.is_empty() {
            format!("```{}\n{} {}\n```", lang, symbol_kw_for_lang(sym_kind, lang), name)
        } else {
            format!("```{}\n{}\n```", lang, sig)
        };

        // Prepend KDoc / Javadoc comment if one immediately precedes the declaration.
        if let Some(doc) = extract_doc_comment(&lines, start_line) {
            Some(format!("{doc}\n\n---\n\n{code_block}"))
        } else {
            Some(code_block)
        }
    }

    /// Returns the pre-computed `detail` string (declaration signature) for the
    /// symbol declared at the given line+character in `uri_str`. Used by
    /// `completionItem/resolve` to populate `CompletionItem.detail`.
    ///
    /// `calling_uri` — the file where the cursor is; used for generic type substitution.
    pub fn symbol_detail_at(&self, uri_str: &str, line: u32, col: u32, calling_uri: Option<&str>) -> Option<String> {
        let data = self.files.get(uri_str)?;
        let sym = data.symbols.iter()
            .find(|s| s.selection_range.start.line == line
                   && s.selection_range.start.character == col)
            .or_else(|| data.symbols.iter().find(|s| s.selection_range.start.line == line))?;
        let lang = lang_str(uri_str);
        let raw = if sym.detail.is_empty() {
            format!("{} {}", symbol_kw_for_lang(sym.kind, lang), sym.name)
        } else {
            sym.detail.clone()
        };
        if let Some(cu) = calling_uri {
            let subst = build_type_param_subst(self, uri_str, sym.selection_range.start.line, cu, None);
            if !subst.is_empty() { return Some(apply_subst(&raw, &subst)); }
        }
        Some(raw)
    }

    /// Build Markdown documentation for a completion item identified by its
    /// source file URI and declaration line+character.
    ///
    /// Called by `completionItem/resolve` to lazily populate `documentation`
    /// without bloating the initial completion response.
    ///
    /// Returns `(doc_markdown, detail)` where `doc_markdown` is the KDoc/Javadoc
    /// comment only (no code block — the signature is already shown in `detail`)
    /// and `detail` is the short signature string for `CompletionItem.detail`.
    ///
    /// `calling_uri` — the file where the cursor is; used for generic type substitution.
    pub fn completion_docs_for(&self, uri_str: &str, line: u32, col: u32, calling_uri: Option<&str>) -> Option<(String, String)> {
        let data = self.files.get(uri_str)?;
        let start_line = line as usize;

        let sym = data.symbols.iter()
            .find(|s| s.selection_range.start.line == line
                   && s.selection_range.start.character == col)
            .or_else(|| data.symbols.iter().find(|s| s.selection_range.start.line == line))?;

        let lang = lang_str(uri_str);

        // detail: prefer the pre-computed SymbolEntry.detail; fall back to
        // a minimal keyword + name string so the field is never empty.
        let raw_detail = if sym.detail.is_empty() {
            format!("{} {}", symbol_kw_for_lang(sym.kind, lang), sym.name)
        } else {
            sym.detail.clone()
        };

        // Apply generic type parameter substitution when requested.
        let detail = if let Some(cu) = calling_uri {
            let subst = build_type_param_subst(self, uri_str, sym.selection_range.start.line, cu, None);
            if subst.is_empty() { raw_detail } else { apply_subst(&raw_detail, &subst) }
        } else {
            raw_detail
        };

        // documentation: KDoc/Javadoc only — the signature is already in detail.
        let doc_md = extract_doc_comment(&data.lines, start_line)?;

        Some((doc_md, detail))
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

    /// Apply generic type parameter substitution to `sig` for a symbol at `sym_uri:sym_line`
    /// when viewed from `calling_uri`.  Returns the substituted string, or the original if
    /// no substitution is applicable.
    pub(crate) fn type_subst_sig(&self, sym_uri: &str, sym_line: u32, calling_uri: &str, sig: &str) -> String {
        let subst = build_type_param_subst(self, sym_uri, sym_line, calling_uri, None);
        if subst.is_empty() { sig.to_owned() } else { apply_subst(sig, &subst) }
    }

}

pub(crate) fn symbol_kw(kind: SymbolKind) -> &'static str {
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

pub(crate) fn symbol_kw_for_lang(kind: SymbolKind, lang: &str) -> &'static str {
    let kw = symbol_kw(kind);
    // Swift uses `func`, not `fun`.
    if lang == "swift" && kw == "fun" { "func" } else { kw }
}

pub(crate) fn lang_str(path: &str) -> &'static str {
    match crate::Language::from_path(path) {
        crate::Language::Kotlin => "kotlin",
        crate::Language::Swift  => "swift",
        crate::Language::Java   => "java",
    }
}

// ─── Generic type parameter substitution ─────────────────────────────────────

/// Build a type-parameter → concrete-type substitution map for a symbol declared
/// inside a generic class/interface when viewed from a specialised subtype.
///
/// For example: `FlowReducer<EventType, out EffectType, StateType>` specialised by
/// `DashboardProductsReducer : FlowReducer<Event, Effect, State>` gives
/// `{"EventType" → "Event", "EffectType" → "Effect", "StateType" → "State"}`.
///
/// Returns an empty map when substitution is not applicable (same file, no generics,
/// or the calling file doesn't implement the container class).
fn build_type_param_subst(
    idx:         &Indexer,
    sym_uri:     &str,
    sym_line:    u32,
    calling_uri: &str,
    cursor_line: Option<u32>,
) -> std::collections::HashMap<String, String> {
    if sym_uri == calling_uri {
        return Default::default();
    }

    let sym_data = match idx.files.get(sym_uri) {
        Some(d) => d,
        None => return Default::default(),
    };

    let container_name = match sym_data.containing_class_at(sym_line) {
        Some(n) => n,
        None    => return Default::default(),
    };

    let container_sym = match sym_data.symbols.iter().find(|s| s.name == container_name) {
        Some(s) => s,
        None    => return Default::default(),
    };

    let type_params = &container_sym.type_params;
    if type_params.is_empty() { return Default::default(); }

    let calling_data = match idx.files.get(calling_uri) {
        Some(d) => d,
        None    => return Default::default(),
    };

    // When multiple classes in the same file extend the same base, use cursor_line
    // to identify the specific calling class and scope the supers lookup.
    let calling_class_line = cursor_line
        .and_then(|line| calling_data.containing_class_at(line))
        .and_then(|name| calling_data.symbols.iter().find(|s| s.name == name))
        .map(|s| s.selection_range.start.line);

    let type_args = calling_data.supers.iter()
        .find(|(line, base, _)| {
            base == &container_name
                && calling_class_line.is_none_or(|class_line| *line == class_line)
        })
        .map(|(_, _, args)| args.clone())
        .unwrap_or_default();

    if type_args.is_empty() { return Default::default(); }

    type_params.iter().zip(type_args.iter())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Extract the simple type name from a property detail string.
/// E.g. "private val foo: DashboardProductsReducer by lazy" → "DashboardProductsReducer"
/// E.g. "val x: List<String>" → "List"
pub(crate) fn extract_property_type_name(detail: &str) -> &str {
    // Find ": Type" pattern
    let colon_pos = match detail.find(':') {
        Some(p) => p,
        None => return "",
    };
    let after_colon = detail[colon_pos + 1..].trim_start();
    // Take the first identifier (uppercase start = type name)
    let end = after_colon.find(|c: char| !c.is_alphanumeric() && c != '_').unwrap_or(after_colon.len());
    let name = &after_colon[..end];
    if name.is_empty() || !name.chars().next().unwrap_or(' ').is_uppercase() {
        return "";
    }
    name
}

/// Apply a type-parameter substitution map to a type string (public for inlay_hints).
pub(crate) fn apply_type_subst(sig: &str, subst: &std::collections::HashMap<String, String>) -> String {
    apply_subst(sig, subst)
}

/// Apply a type-parameter substitution map to a signature string.
///
/// Only replaces whole-word occurrences (character boundaries), so `EventType` is
/// not partially replaced when looking up `Event`.
fn apply_subst(sig: &str, subst: &std::collections::HashMap<String, String>) -> String {
    if subst.is_empty() { return sig.to_owned(); }
    let mut result = String::with_capacity(sig.len() + 16);
    let chars: Vec<char> = sig.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_alphabetic() || ch == '_' {
            // Collect the full identifier starting at i.
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let ident: String = chars[start..i].iter().collect();
            if let Some(replacement) = subst.get(&ident) {
                result.push_str(replacement);
            } else {
                result.push_str(&ident);
            }
        } else {
            result.push(ch);
            i += 1;
        }
    }
    result
}

#[cfg(test)]
#[path = "lookup_tests.rs"]
mod tests;
