// Unified resolution service for symbol lookup, substitution, and extraction.
// Phase 2: Core `resolve_symbol_info` pipeline implementation.

use std::collections::HashMap;
use std::sync::Arc;
use tower_lsp::lsp_types::{SymbolKind, Url};

use crate::indexer::doc::extract_doc_comment;
use crate::indexer::Location;
use crate::types::{CallerContext, FileData, SymbolEntry};
use crate::LinesExt;

/// Domain-level resolution result. Small, owned data suitable for LSP adapters.
pub(crate) struct ResolvedSymbol {
    /// Symbol definition location; only accessed in tests and future callers.
    #[allow(dead_code)]
    pub location: Location,
    /// The original symbol name (from the index), independent of signature parsing.
    pub name: String,
    pub kind: SymbolKind,
    /// Pre-substitution signature; kept for test assertions.
    #[allow(dead_code)]
    pub raw_signature: String,
    pub signature: String,
    /// Substitution map used to build `signature`; kept for test assertions.
    #[allow(dead_code)]
    pub subst: HashMap<String, String>,
    pub doc: String,
}

/// Options controlling resolution behaviour and allowed fallbacks.
pub(crate) struct ResolveOptions {
    pub allow_rg: bool,
    pub include_doc: bool,
    pub apply_subst: bool,
    /// When true, prefer `SymbolEntry.detail` (cached, short) over the full
    /// `collect_signature` source read.  Use for completion detail strings.
    pub prefer_cached_detail: bool,
}

impl ResolveOptions {
    pub(crate) fn hover() -> Self {
        Self {
            allow_rg: true,
            include_doc: true,
            apply_subst: true,
            prefer_cached_detail: false,
        }
    }
    pub(crate) fn completion() -> Self {
        Self {
            allow_rg: false,
            include_doc: true,
            apply_subst: true,
            prefer_cached_detail: true,
        }
    }
    #[allow(dead_code)]
    pub(crate) fn goto_def() -> Self {
        Self {
            allow_rg: true,
            include_doc: false,
            apply_subst: false,
            prefer_cached_detail: false,
        }
    }
}

/// Substitution context used by the pipeline.
pub(crate) enum SubstitutionContext<'a> {
    None,
    /// Cross-file substitution: the symbol is from another file and we need to
    /// substitute generic type params with the concrete args used by the caller.
    ///
    /// `cursor_line`: the cursor's line in `calling_uri`, used to identify which
    /// class is calling (when a file has multiple classes extending the same base).
    /// `None` = unknown / don't disambiguate → picks the first matching class.
    CrossFile {
        calling_uri: &'a str,
        cursor_line: Option<u32>,
    },
    #[allow(dead_code)]
    Precomputed(&'a HashMap<String, String>),
}

/// Test seam trait: read-only view into index state. Keep this lightweight for tests.
pub(crate) trait IndexRead {
    fn get_definitions(&self, name: &str) -> Option<Vec<Location>>;
    fn get_file_data(&self, uri: &str) -> Option<Arc<FileData>>;

    /// Resolve definition locations for `name` with qualifier and import context.
    /// Default implementation uses the global definitions map (no import awareness).
    /// Production `Indexer` overrides this with the full resolver.
    fn resolve_locations(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
        allow_rg: bool,
    ) -> Vec<Location> {
        let _ = (qualifier, from_uri, allow_rg);
        self.get_definitions(name).unwrap_or_default()
    }

    /// Infer the concrete type of an unannotated variable/property.
    ///
    /// Used to augment hover for `val foo = someCall()` with `val foo: ReturnType`
    /// when no explicit `: Type` annotation is present.
    /// Default impl returns `None`; production Indexer delegates to full type inference.
    fn infer_variable_type_for(&self, _name: &str, _uri: &Url) -> Option<String> {
        None
    }

    /// Trigger on-demand indexing for a file if needed (production hook).
    /// Default impl does nothing; test stubs don't need on-demand indexing.
    /// Callers that need on-demand indexing must call `ensure_indexed_on_demand()`
    /// before `get_file_data()` (as `build_type_param_subst_impl` does).
    fn ensure_indexed_on_demand(&self, _uri: &str) {}
}

// ─── Pipeline Entry Point (thin coordinator) ───────────────────────────────

/// Core resolution pipeline: locate → load → enrich → substitute → extract.
/// Thin coordinator that delegates to pure functions and trait methods.
pub(crate) fn resolve_symbol_info<I: IndexRead>(
    index: &I,
    name: &str,
    qualifier: Option<&str>,
    from_uri: &Url,
    subst_ctx: SubstitutionContext<'_>,
    options: &ResolveOptions,
) -> Option<ResolvedSymbol> {
    let location = locate_symbol(index, name, qualifier, from_uri, options.allow_rg)?;
    let data = index.get_file_data(location.uri.as_str())?;
    enrich_symbol(index, &data, &location, name, subst_ctx, options)
}

/// Enrich a pre-resolved location without a locate step.
///
/// Used when the caller already holds a `Location` (e.g., from
/// `resolve_with_receiver_fallback`) and only needs enrichment.
pub(crate) fn enrich_at_location<I: IndexRead>(
    index: &I,
    location: &Location,
    name: &str,
    subst_ctx: SubstitutionContext<'_>,
    options: &ResolveOptions,
) -> Option<ResolvedSymbol> {
    let data = index.get_file_data(location.uri.as_str())?;
    enrich_symbol(index, &data, location, name, subst_ctx, options)
}

/// Find the symbol at `(line, col)` in `uri_str`, then enrich it.
///
/// Used by `completion_resolve` which stores line/col in the completion item
/// data rather than a full `Location`.  If no symbol is at the exact position,
/// falls back to first symbol whose selection range starts on `line`.
pub(crate) fn enrich_at_line<I: IndexRead>(
    index: &I,
    uri_str: &str,
    line: u32,
    col: u32,
    subst_ctx: SubstitutionContext<'_>,
    options: &ResolveOptions,
) -> Option<ResolvedSymbol> {
    let data = index.get_file_data(uri_str)?;
    let sym = data
        .symbols
        .iter()
        .find(|s| {
            s.selection_start() == line
                && s.selection_range.start.character <= col
                && col < s.selection_range.end.character
        })
        .or_else(|| data.symbols.iter().find(|s| s.selection_start() == line))?;

    let uri = Url::parse(uri_str).ok()?;
    let location = Location {
        uri,
        range: sym.selection_range,
    };
    enrich_symbol(index, &data, &location, &sym.name, subst_ctx, options)
}

/// Build substitution map for enclosing class at cursor position.
pub(crate) fn build_subst_map<I: IndexRead>(
    index: &I,
    uri: &str,
    cursor_line: u32,
) -> HashMap<String, String> {
    build_enclosing_class_subst_impl(index, uri, cursor_line)
}

/// Apply cross-file type-parameter substitution to a signature string.
///
/// Equivalent to the old `Indexer::type_subst_sig` but works over `IndexRead`
/// so it can be used from `resolver/complete.rs` without depending on `Indexer`.
pub(crate) fn cross_file_type_subst<I: IndexRead>(
    index: &I,
    sym_uri: &str,
    sym_line: u32,
    calling_uri: &str,
    caller_cursor_line: Option<u32>,
    sig: &str,
) -> String {
    let subst = build_type_param_subst_impl(
        index,
        sym_uri,
        sym_line,
        CallerContext {
            uri: Some(calling_uri),
            cursor_line: caller_cursor_line,
        },
    );
    if subst.is_empty() {
        sig.to_owned()
    } else {
        super::apply_type_subst(sig, &subst)
    }
}

/// Extract the simple type name from a property detail string.
/// E.g. `"private val foo: DashboardProductsReducer by lazy"` → `"DashboardProductsReducer"`
/// E.g. `"val x: List<String>"` → `"List"`
pub(crate) fn extract_property_type_name(detail: &str) -> &str {
    let colon_pos = match detail.find(':') {
        Some(p) => p,
        None => return "",
    };
    let after_colon = detail[colon_pos + 1..].trim_start();
    let end = after_colon
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(after_colon.len());
    let name = &after_colon[..end];
    if name.is_empty() || !name.chars().next().unwrap_or(' ').is_uppercase() {
        return "";
    }
    name
}

// ─── Pure Data Transformation Functions ──────────────────────────────────

/// Extract canonical signature respecting caller intent.
///
/// - `prefer_cached_detail = true` (completion): use `detail`-first — it's
///   pre-computed, concise (~120 chars), and safe for single-line UI slots.
/// - `prefer_cached_detail = false` (hover): prefer `collect_signature` for
///   the full untruncated declaration; fall back to `detail`.
///
/// For properties/variables `collect_signature` reads forward until `{`/`=`
/// and can accidentally collect sibling constructor params, so `detail` is
/// always used for those kinds regardless of the `prefer_cached_detail` flag.
fn extract_canonical_signature(sym: &SymbolEntry, data: &FileData, prefer_cached: bool) -> String {
    if (prefer_cached || matches!(sym.kind, SymbolKind::PROPERTY | SymbolKind::VARIABLE))
        && !sym.detail.is_empty()
    {
        return sym.detail.clone();
    }
    let full = data.lines.collect_signature(sym.selection_start() as usize);
    if !full.is_empty() {
        full
    } else {
        sym.detail.clone()
    }
}

/// Apply type-parameter substitution to a signature string.
fn apply_subst(sig: &str, subst: &HashMap<String, String>) -> String {
    super::apply_type_subst(sig, subst)
}

// ─── Glue Functions (coordinate I/O + data transformation) ──────────────────

/// Locate first definition of a symbol using import-aware resolution.
fn locate_symbol<I: IndexRead>(
    index: &I,
    name: &str,
    qualifier: Option<&str>,
    from_uri: &Url,
    allow_rg: bool,
) -> Option<Location> {
    index
        .resolve_locations(name, qualifier, from_uri, allow_rg)
        .into_iter()
        .next()
}

/// Find SymbolEntry in FileData by range or name.
fn find_symbol_entry<'a>(
    data: &'a FileData,
    location: &Location,
    name: &str,
) -> Option<&'a SymbolEntry> {
    data.symbols
        .iter()
        .find(|s| s.selection_range == location.range)
        .or_else(|| data.symbols.iter().find(|s| s.name == name))
}

/// Enrich symbol with signature, substitution, and docs.
fn enrich_symbol<I: IndexRead>(
    index: &I,
    data: &FileData,
    location: &Location,
    name: &str,
    subst_ctx: SubstitutionContext<'_>,
    options: &ResolveOptions,
) -> Option<ResolvedSymbol> {
    let sym = find_symbol_entry(data, location, name)?;

    let raw_signature = extract_canonical_signature(sym, data, options.prefer_cached_detail);

    // For unannotated val/var (no `: Type` in the signature), try to infer the
    // concrete type so callers see `val foo: ReturnType` instead of `val foo = expr`.
    let raw_signature = if matches!(sym.kind, SymbolKind::PROPERTY | SymbolKind::VARIABLE)
        && !raw_signature.contains(&format!("{name}:"))
    {
        if let Some(inferred) = index.infer_variable_type_for(name, &location.uri) {
            augment_property_sig(&raw_signature, name, sym.kind, &inferred)
        } else {
            raw_signature
        }
    } else {
        raw_signature
    };

    let subst = build_subst_if_needed(index, location, sym, &raw_signature, subst_ctx, options);
    let signature = apply_subst(&raw_signature, &subst);
    let doc = if options.include_doc {
        extract_doc_comment(&data.lines, sym.selection_start() as usize).unwrap_or_default()
    } else {
        String::new()
    };

    Some(ResolvedSymbol {
        location: location.clone(),
        name: sym.name.clone(),
        kind: sym.kind,
        raw_signature,
        signature,
        subst,
        doc,
    })
}

/// Rebuild a property signature with an inferred type annotation.
///
/// Replaces the `= rhs` initializer with `: InferredType`, preserving any
/// leading modifiers (e.g. `private`, `override`).
fn augment_property_sig(raw: &str, name: &str, kind: SymbolKind, inferred: &str) -> String {
    let needle = format!(" {name}");
    let name_pos = raw.find(&needle).map(|p| p + 1).or_else(|| {
        if raw.starts_with(name) {
            Some(0)
        } else {
            None
        }
    });
    if let Some(pos) = name_pos {
        format!("{}: {inferred}", &raw[..pos + name.len()])
    } else {
        let kw = if kind == SymbolKind::VARIABLE {
            "var"
        } else {
            "val"
        };
        format!("{kw} {name}: {inferred}")
    }
}

/// Build substitution map if requested by options and context.
fn build_subst_if_needed<I: IndexRead>(
    index: &I,
    location: &Location,
    _sym: &SymbolEntry,
    _raw_sig: &str,
    subst_ctx: SubstitutionContext<'_>,
    options: &ResolveOptions,
) -> HashMap<String, String> {
    if !options.apply_subst {
        return HashMap::new();
    }

    match subst_ctx {
        SubstitutionContext::None => HashMap::new(),
        SubstitutionContext::CrossFile {
            calling_uri,
            cursor_line,
        } => build_type_param_subst_impl(
            index,
            location.uri.as_str(),
            location.range.start.line,
            CallerContext {
                uri: Some(calling_uri),
                cursor_line,
            },
        ),
        SubstitutionContext::Precomputed(m) => m.clone(),
    }
}

// ─── Substitution Builders (coordinate I/O + pure logic) ────────────────────

/// Build type-parameter substitution for cross-file lookup.
fn build_type_param_subst_impl<I: IndexRead>(
    index: &I,
    sym_uri: &str,
    sym_line: u32,
    caller: CallerContext<'_>,
) -> HashMap<String, String> {
    let Some(calling_uri) = caller.uri else {
        return HashMap::new();
    };
    if sym_uri == calling_uri {
        return HashMap::new();
    }

    index.ensure_indexed_on_demand(sym_uri);
    let Some(sym_data) = index.get_file_data(sym_uri) else {
        return HashMap::new();
    };
    let Some(container_name) = sym_data.containing_class_at(sym_line) else {
        return HashMap::new();
    };
    let Some(container_sym) = sym_data.symbols.iter().find(|s| s.name == container_name) else {
        return HashMap::new();
    };
    if container_sym.type_params.is_empty() {
        return HashMap::new();
    }

    index.ensure_indexed_on_demand(calling_uri);
    let Some(calling_data) = index.get_file_data(calling_uri) else {
        return HashMap::new();
    };

    let calling_class_line = caller
        .cursor_line
        .and_then(|line| calling_data.containing_class_at(line))
        .and_then(|name| calling_data.symbols.iter().find(|s| s.name == name))
        .map(SymbolEntry::selection_start);

    let type_args = calling_data
        .supers
        .iter()
        .find(|(line, base, _)| {
            base == &container_name
                && calling_class_line.is_none_or(|class_line| *line == class_line)
        })
        .map(|(_, _, args)| args.clone())
        .unwrap_or_default();
    if type_args.is_empty() {
        return HashMap::new();
    }

    container_sym
        .type_params
        .iter()
        .zip(type_args.iter())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// Build substitution for enclosing class's type parameters.
fn build_enclosing_class_subst_impl<I: IndexRead>(
    index: &I,
    uri: &str,
    cursor_line: u32,
) -> HashMap<String, String> {
    let data = match index.get_file_data(uri) {
        Some(d) => d,
        None => return HashMap::new(),
    };

    let class_name = match data.containing_class_at(cursor_line) {
        Some(n) => n,
        None => return HashMap::new(),
    };

    let class_sym = match data.symbols.iter().find(|s| s.name == class_name) {
        Some(s) => s,
        None => return HashMap::new(),
    };

    let class_line = class_sym.selection_start();
    let class_end_line = class_sym.range.end.line;

    // For each supertype with concrete type args, look up the BASE class's own
    // type parameters (e.g., `[T, U]` from `class Base<T, U>`), then zip with
    // the concrete args (e.g., `[Event, State]`) to build `{T→Event, U→State}`.
    // Using the enclosing class's own type_params here would be wrong: for
    // `class Child : Base<Event, State>`, Child itself has no type params.
    let mut result = HashMap::new();
    for (line, base_name, type_args) in data.supers.iter() {
        if *line != class_line || type_args.is_empty() {
            continue;
        }

        let base_type_params: Vec<String> = index
            .get_definitions(base_name)
            .and_then(|locs| locs.into_iter().next())
            .and_then(|loc| {
                index.get_file_data(loc.uri.as_str()).and_then(|base_data| {
                    base_data
                        .symbols
                        .iter()
                        .find(|s| s.name == *base_name)
                        .map(|s| s.type_params.clone())
                })
            })
            .unwrap_or_default();

        if base_type_params.is_empty() {
            continue;
        }
        for (param, arg) in base_type_params.iter().zip(type_args.iter()) {
            result.entry(param.clone()).or_insert_with(|| arg.clone());
        }
    }
    // Phase 2: collect substitutions from member property types.
    // E.g. if the enclosing class has `val reducer: DashboardProductsReducer`
    // and that type extends `FlowReducer<Event, State>`, include
    // `{Event→…, State→…}` mappings from FlowReducer's type params.
    for sym in data.symbols.iter() {
        if sym.selection_start() <= class_line {
            continue;
        }
        if sym.selection_start() > class_end_line {
            continue;
        }
        if !matches!(sym.kind, SymbolKind::FIELD | SymbolKind::PROPERTY) {
            continue;
        }
        let type_name = extract_property_type_name(&sym.detail);
        if type_name.is_empty() {
            continue;
        }
        let Some(locs) = index.get_definitions(type_name) else {
            continue;
        };
        let Some(loc) = locs.into_iter().next() else {
            continue;
        };
        let Some(prop_type_data) = index.get_file_data(loc.uri.as_str()) else {
            continue;
        };
        let Some(prop_sym) = prop_type_data.symbols.iter().find(|s| s.name == type_name) else {
            continue;
        };
        let prop_class_line = prop_sym.selection_start();
        for (line, super_name, type_args) in prop_type_data.supers.iter() {
            if *line != prop_class_line || type_args.is_empty() {
                continue;
            }
            let Some(super_locs) = index.get_definitions(super_name) else {
                continue;
            };
            let Some(super_loc) = super_locs.into_iter().next() else {
                continue;
            };
            let Some(super_file_data) = index.get_file_data(super_loc.uri.as_str()) else {
                continue;
            };
            let Some(super_sym) = super_file_data
                .symbols
                .iter()
                .find(|s| s.name == *super_name)
            else {
                continue;
            };
            for (param, arg) in super_sym.type_params.iter().zip(type_args.iter()) {
                result.entry(param.clone()).or_insert_with(|| arg.clone());
            }
        }
    }
    result
}

// ─── Indexer impl (production) ───────────────────────────────────────────────

// Implement IndexRead for Indexer: production code doesn't use the trait,
// but this enables unit tests to use a TestIndex stub.
impl IndexRead for super::Indexer {
    fn get_definitions(&self, name: &str) -> Option<Vec<Location>> {
        self.definitions.get(name).map(|rf| rf.clone())
    }

    fn get_file_data(&self, uri: &str) -> Option<Arc<FileData>> {
        self.files.get(uri).map(|rf| rf.clone())
    }

    fn resolve_locations(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
        allow_rg: bool,
    ) -> Vec<Location> {
        if allow_rg {
            let locs = self.resolve_symbol(name, qualifier, from_uri);
            // Ensure every rg-discovered file is indexed so get_file_data() succeeds.
            for loc in &locs {
                self.ensure_indexed(&loc.uri);
            }
            locs
        } else {
            if let Some(qual) = qualifier {
                // Index-only qualified resolution: resolve the qualifier without rg,
                // then search that file for `name`. Avoids silently dropping the
                // qualifier and returning results for an unrelated top-level symbol.
                let qual_locs = self.resolve_symbol_no_rg(qual, from_uri);
                if let Some(loc) = qual_locs.into_iter().next() {
                    let locs = self.find_name_in_uri(name, loc.uri.as_str());
                    if !locs.is_empty() {
                        return locs;
                    }
                }
            }
            self.resolve_symbol_no_rg(name, from_uri)
        }
    }

    fn infer_variable_type_for(&self, name: &str, uri: &Url) -> Option<String> {
        self.infer_variable_type(name, uri)
    }

    fn ensure_indexed_on_demand(&self, uri: &str) {
        // Fast path: if the file is already in the in-memory index, skip URI parsing.
        if self.files.contains_key(uri) {
            return;
        }
        // Convert string URI to Url and trigger on-demand indexing if needed.
        // Silently skips URIs that can't be parsed — they can't be indexed anyway.
        if let Ok(parsed_uri) = Url::parse(uri) {
            self.ensure_indexed(&parsed_uri);
        }
    }
}

#[cfg(test)]
#[path = "resolution_tests.rs"]
mod tests;
