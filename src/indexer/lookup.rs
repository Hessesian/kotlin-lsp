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
    pub fn hover_info(&self, name: &str, calling_uri: Option<&str>) -> Option<String> {
        // Check stdlib first so well-known symbols (run, apply, map, …) get
        // proper signatures even when no project source contains them.
        if let Some(md) = hover(name) { return Some(md); }

        // Drop the dashmap ref before taking the second one.
        let loc: Location = {
            let r = self.definitions.get(name)?;
            r.first()?.clone()
        };
        self.hover_info_at_location(&loc, name, calling_uri)
    }

    /// Build hover markdown for `name` at a specific resolved `Location`.
    /// Used by the hover handler so it shows the same symbol as go-to-definition.
    ///
    /// `calling_uri` — the file where the cursor is (used to substitute generic type
    /// parameters with concrete types when the symbol is from a base class).
    pub fn hover_info_at_location(&self, loc: &Location, name: &str, calling_uri: Option<&str>) -> Option<String> {
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
        let raw_sig = data.lines.collect_signature(start_line);

        // Apply generic type parameter substitution when the cursor is in a different
        // file (subtype) than where the symbol is defined.
        let sig = if let Some(cu) = calling_uri {
            let subst = build_type_param_subst(self, loc.uri.as_str(), sym.selection_range.start.line, cu);
            if subst.is_empty() { raw_sig } else { apply_subst(&raw_sig, &subst) }
        } else {
            raw_sig
        };

        let lang = lang_str(loc.uri.path());

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
            let subst = build_type_param_subst(self, uri_str, sym.selection_range.start.line, cu);
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
            let subst = build_type_param_subst(self, uri_str, sym.selection_range.start.line, cu);
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
        let subst = build_type_param_subst(self, sym_uri, sym_line, calling_uri);
        if subst.is_empty() { sig.to_owned() } else { apply_subst(sig, &subst) }
    }

    /// Build a combined type-parameter substitution map for the enclosing class at
    /// `cursor_line` in `uri`.  Gathers type params from ALL supertypes of that class
    /// and maps them to concrete type arguments.
    ///
    /// Used by inlay hints to convert inferred types like `EffectType` → `Effect`
    /// when the cursor is inside a class that specialises a generic base.
    pub(crate) fn type_subst_for_enclosing_class(&self, uri: &str, cursor_line: u32) -> std::collections::HashMap<String, String> {
        build_enclosing_class_subst(self, uri, cursor_line)
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

fn lang_str(path: &str) -> &'static str {
    if path.ends_with(".kt") || path.ends_with(".kts") { "kotlin" }
    else if path.ends_with(".swift")                   { "swift" }
    else                                               { "java" }
}

// ─── Generic type parameter substitution ─────────────────────────────────────

/// Find the name of the innermost class/interface/object that contains `sym_line`
/// in the given file's symbol list. Returns `None` if the symbol is top-level.
fn find_containing_class_name(data: &crate::types::FileData, sym_line: u32) -> Option<String> {
    use tower_lsp::lsp_types::SymbolKind;
    const CLASS_KINDS: &[SymbolKind] = &[
        SymbolKind::CLASS, SymbolKind::INTERFACE, SymbolKind::STRUCT,
        SymbolKind::ENUM, SymbolKind::OBJECT,
    ];
    data.symbols.iter()
        .filter(|s| CLASS_KINDS.contains(&s.kind))
        .filter(|s| s.range.start.line <= sym_line && sym_line <= s.range.end.line)
        // innermost = smallest range span
        .min_by_key(|s| s.range.end.line.saturating_sub(s.range.start.line))
        .map(|s| s.name.clone())
}

/// Parse type parameter names from a class/interface declaration line or detail string.
///
/// e.g. `"interface FlowReducer<EventType, out EffectType, StateType>"` → `["EventType", "EffectType", "StateType"]`
///
/// Handles variance annotations (`in`, `out`) and type constraints (`T : Bound`).
fn parse_type_params(decl: &str) -> Vec<String> {
    super::parse_type_params_from_decl(decl)
}

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
) -> std::collections::HashMap<String, String> {
    if sym_uri == calling_uri {
        return Default::default();
    }

    let sym_data = match idx.files.get(sym_uri) {
        Some(d) => d,
        None => {
            return Default::default();
        }
    };

    let container_name = match find_containing_class_name(&sym_data, sym_line) {
        Some(n) => n,
        None => {
            return Default::default();
        }
    };

    let container_sym = match sym_data.symbols.iter().find(|s| s.name == container_name) {
        Some(s) => s,
        None => {
            return Default::default();
        }
    };
    let decl_text = if !container_sym.detail.is_empty() {
        container_sym.detail.clone()
    } else {
        let line_idx = container_sym.selection_range.start.line as usize;
        sym_data.lines.get(line_idx).cloned().unwrap_or_default()
    };
    let mut type_params = parse_type_params(&decl_text);
    // If detail is truncated (e.g. annotation only), scan source lines for type params
    if type_params.is_empty() {
        let start_line = container_sym.selection_range.start.line as usize;
        for offset in 0..5 {
            if let Some(line) = sym_data.lines.get(start_line + offset) {
                let params = parse_type_params(line);
                if !params.is_empty() {
                    type_params = params;
                    break;
                }
            }
        }
    }
    if type_params.is_empty() { return Default::default(); }

    let calling_data = match idx.files.get(calling_uri) {
        Some(d) => d,
        None => {
            return Default::default();
        }
    };
    let type_args = calling_data.supers.iter()
        .find(|(_, base, _)| base == &container_name)
        .map(|(_, _, args)| args.clone())
        .unwrap_or_default();

    if type_args.is_empty() { return Default::default(); }

    let result: std::collections::HashMap<String, String> = type_params.iter().zip(type_args.iter())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    result
}

/// Build a substitution map from ALL supertypes of the enclosing class at `cursor_line`.
///
/// For each super with type args (e.g., `FlowReducer<Event, Effect, State>`), looks up
/// the super's type params (`EventType`, `EffectType`, `StateType`) and builds the mapping.
/// This allows inlay hints to show concrete types instead of generic param names.
fn build_enclosing_class_subst(
    idx:         &Indexer,
    uri:         &str,
    cursor_line: u32,
) -> std::collections::HashMap<String, String> {
    let data = match idx.files.get(uri) { Some(d) => d, None => {
        return Default::default();
    }};

    // Find the enclosing class
    let class_name = match find_containing_class_name(&data, cursor_line) {
        Some(n) => n,
        None => {
            return Default::default();
        }
    };

    // Get the class symbol's line to find its supers
    let class_sym = match data.symbols.iter().find(|s| s.name == class_name) {
        Some(s) => s,
        None => return Default::default(),
    };
    let class_line = class_sym.selection_range.start.line;

    // Gather all supers with type args for this class
    let mut result = std::collections::HashMap::new();

    for (line, base_name, type_args) in data.supers.iter() {
        if *line != class_line || type_args.is_empty() { continue; }

        // Look up the super class's type parameters from its symbol detail
        let super_sym = idx.definitions.get(base_name.as_str())
            .and_then(|locs| locs.first().cloned());
        if super_sym.is_none() {
            continue;
        }
        let Some(super_loc) = super_sym else { continue };
        let Some(super_data) = idx.files.get(super_loc.uri.as_str()) else {
            continue;
        };
        let Some(sym) = super_data.symbols.iter().find(|s| s.name == *base_name) else {
            continue;
        };

        let decl_text = if !sym.detail.is_empty() {
            &sym.detail
        } else {
            continue;
        };

        // Try detail first; if it doesn't have type params (e.g. truncated at annotation),
        // scan subsequent source lines for the actual class/interface declaration.
        let mut type_params = parse_type_params(decl_text);
        if type_params.is_empty() {
            let start_line = sym.selection_range.start.line as usize;
            // Look at up to 5 lines starting from the symbol's line
            for offset in 0..5 {
                if let Some(line) = super_data.lines.get(start_line + offset) {
                    let params = parse_type_params(line);
                    if !params.is_empty() {
                        type_params = params;
                        break;
                    }
                }
            }
        }
        // Zip params → args
        for (param, arg) in type_params.iter().zip(type_args.iter()) {
            result.insert(param.clone(), arg.clone());
        }
    }

    // Also collect substitutions from member property types.
    // If the enclosing class has a property like `val reducer: DashboardProductsReducer`
    // and that type extends `FlowReducer<Event, Effect, State>`, include FlowReducer's
    // type param → concrete arg mappings.
    for sym in data.symbols.iter() {
        if sym.selection_range.start.line <= class_line { continue; }
        if !matches!(sym.kind, SymbolKind::FIELD | SymbolKind::PROPERTY) { continue; }
        // Extract type name from detail (e.g., "private val foo: SomeType by lazy")
        let type_name = extract_property_type_name(&sym.detail);
        if type_name.is_empty() { continue; }
        // Look up the property type's class definition
        let Some(locs) = idx.definitions.get(type_name) else { continue };
        let Some(loc) = locs.first() else { continue };
        let Some(prop_type_data) = idx.files.get(loc.uri.as_str()) else { continue };
        // Find this type's supers and resolve their type params
        let prop_sym = match prop_type_data.symbols.iter().find(|s| s.name == type_name) {
            Some(s) => s,
            None => continue,
        };
        let prop_class_line = prop_sym.selection_range.start.line;
        for (line, base_name, type_args) in prop_type_data.supers.iter() {
            if *line != prop_class_line || type_args.is_empty() { continue; }
            // Already have these params mapped? Skip.
            let Some(super_locs) = idx.definitions.get(base_name.as_str()) else { continue };
            let Some(super_loc) = super_locs.first() else { continue };
            let Some(super_file) = idx.files.get(super_loc.uri.as_str()) else { continue };
            let Some(super_sym) = super_file.symbols.iter().find(|s| s.name == *base_name) else { continue };
            let mut type_params = parse_type_params(&super_sym.detail);
            if type_params.is_empty() {
                let start = super_sym.selection_range.start.line as usize;
                for offset in 0..5 {
                    if let Some(line) = super_file.lines.get(start + offset) {
                        let params = parse_type_params(line);
                        if !params.is_empty() { type_params = params; break; }
                    }
                }
            }
            for (param, arg) in type_params.iter().zip(type_args.iter()) {
                result.entry(param.clone()).or_insert_with(|| arg.clone());
            }
        }
    }

    result
}

/// Extract the simple type name from a property detail string.
/// E.g. "private val foo: DashboardProductsReducer by lazy" → "DashboardProductsReducer"
/// E.g. "val x: List<String>" → "List"
fn extract_property_type_name(detail: &str) -> &str {
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
