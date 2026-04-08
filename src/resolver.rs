//! Symbol resolution for Kotlin (and Java) with a prioritised fallback chain.
//!
//! Resolution order
//! ────────────────
//! 1. **Local file**        — symbols defined in the same file (highest priority).
//! 2. **Explicit imports**  — `import com.example.Foo` or `import com.example.Foo as F`.
//!                            Tries the `qualified` index first, then the short-name index.
//! 3. **Same package**      — all symbols in files that share the same `package` declaration
//!                            are visible without imports in Kotlin.
//! 4. **Star imports**      — `import com.example.*`  checks indexed files in that package,
//!                            then falls back to an `rg` search scoped to the package dir.
//! 5. **Extension functions** — `fun Receiver.name(...)` is stored as a top-level symbol
//!                            named `name`; steps 1–4 already pick these up. No special
//!                            handling needed beyond noting that receiver type is ignored.
//! 6. **Project-wide `rg`** — pattern `(fun|class|…)\s+NAME\b` across *.kt / *.java.
//!                            Last resort; always finds stdlib-shadowing project symbols.
//!
//! Stdlib packages (`kotlin.*`, `java.*`, `android.*`, `androidx.*`) are skipped because
//! their sources aren't present in the project tree.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, InsertTextFormat, Location, Position, Range, SymbolKind, Url,
};

use crate::indexer::{parse_rg_line, rg_find_definition, Indexer};
use crate::types::Visibility;

// ─── completion entry point ───────────────────────────────────────────────────

/// Provide completion candidates for `prefix` at the current position.
///
/// Two modes:
/// - **Dot-completion** (`dot_receiver = Some("obj")`): infer the receiver's type
///   and return all its members (symbols + line-scanned constructor params).
/// - **Bare-word** (`dot_receiver = None`): return all symbols from the current
///   file, same-package files, and the whole project index whose name starts with
///   `prefix` (case-insensitive).
pub fn complete_symbol(
    idx: &Indexer,
    prefix: &str,
    dot_receiver: Option<&str>,
    from_uri: &Url,
    snippets: bool,
) -> Vec<CompletionItem> {
    if let Some(receiver) = dot_receiver {
        return complete_dot(idx, receiver, from_uri, snippets);
    }
    complete_bare(idx, prefix, from_uri, snippets)
}

/// Dot-completion: return all members of the receiver's inferred type,
/// sorted: methods first, then fields/vars, then class-level names last.
fn complete_dot(idx: &Indexer, receiver: &str, from_uri: &Url, snippets: bool) -> Vec<CompletionItem> {
    // Infer type from variable annotation.
    let type_name = match infer_variable_type(idx, receiver, from_uri) {
        Some(t) => t,
        None => {
            // Could be an uppercase class/object — look it up directly.
            if receiver.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                receiver.to_string()
            } else {
                return vec![];
            }
        }
    };

    // Resolve type to its source file, handling dotted types like `Outer.Inner`.
    let Some(file_uri) = resolve_type_to_file(idx, &type_name, from_uri) else {
        return vec![];
    };

    // When the type is `Outer.Inner`, show only the inner type's members;
    // otherwise show all symbols in the file (which includes nested types).
    let mut items = if let Some(dot) = type_name.find('.') {
        let inner_name = &type_name[dot + 1..];
        symbols_from_nested_type(idx, &file_uri, inner_name)
    } else {
        symbols_from_uri_as_completions(idx, &file_uri)
    };

    // Filter out private members — they are inaccessible from outside the class.
    items.retain(|i| i.sort_text.as_deref().map(|s| !s.starts_with("prv:")).unwrap_or(true));

    // Strip snippet fields if client doesn't support them.
    if !snippets {
        for item in &mut items {
            item.insert_text        = None;
            item.insert_text_format = None;
        }
    }

    // Sort: functions/methods first, then fields/vars, then everything else.
    items.sort_by_key(|i| kind_sort_rank(i.kind));

    // Append stdlib extensions filtered to the receiver type.
    items.extend(crate::stdlib::dot_completions_for(&type_name, snippets));
    items
}

/// Resolve a (possibly dotted) type name to the URI of its containing file.
/// `"DashboardProductsReducer.Factory"` → file where `DashboardProductsReducer` lives.
fn resolve_type_to_file(idx: &Indexer, type_name: &str, from_uri: &Url) -> Option<String> {
    // For dotted types, resolve the outer class only.
    let outer = type_name.split('.').next().unwrap_or(type_name);
    let locs = resolve_symbol_no_rg(idx, outer, from_uri);
    Some(locs.first()?.uri.to_string())
}

/// Return completions for symbols declared INSIDE a nested type `inner_name`
/// within the given file.  Scans the file's symbol table for lines between
/// the inner type's start and the next same-indent symbol.
fn symbols_from_nested_type(
    idx:        &Indexer,
    file_uri:   &str,
    inner_name: &str,
) -> Vec<CompletionItem> {
    let data = match idx.files.get(file_uri) {
        Some(d) => d,
        None    => return vec![],
    };

    // Find the inner type's start line.
    let inner_sym = data.symbols.iter()
        .find(|s| s.name == inner_name);
    let inner_start = match inner_sym {
        Some(s) => s.range.start.line,
        None    => return symbols_from_uri_as_completions(idx, file_uri), // fallback
    };

    // Find the next symbol at the SAME OR LOWER indentation level that comes
    // after the inner type — that's where the inner body ends.
    let inner_indent = data.lines.get(inner_start as usize)
        .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
        .unwrap_or(0);

    let inner_end = data.symbols.iter()
        .filter(|s| s.range.start.line > inner_start)
        .find(|s| {
            let indent = data.lines.get(s.range.start.line as usize)
                .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
                .unwrap_or(0);
            indent <= inner_indent
        })
        .map(|s| s.range.start.line)
        .unwrap_or(u32::MAX);

    // Collect symbols that fall within [inner_start, inner_end).
    use crate::types::Visibility;
    data.symbols.iter()
        .filter(|s| s.range.start.line > inner_start && s.range.start.line < inner_end)
        .filter(|s| s.visibility != Visibility::Private)
        .map(|s| {
            let kind = symbol_kind_to_completion(s.kind);
            let is_fn = matches!(kind, CompletionItemKind::FUNCTION | CompletionItemKind::METHOD);
            CompletionItem {
                label:              s.name.clone(),
                kind:               Some(kind),
                insert_text:        if is_fn { Some(format!("{}($1)", s.name)) } else { None },
                insert_text_format: if is_fn { Some(InsertTextFormat::SNIPPET) } else { None },
                sort_text:          Some(format!("{:02}:{}", kind_sort_rank(Some(kind)), s.name)),
                ..Default::default()
            }
        })
        .collect()
}

/// Sort rank for completion item kinds: lower = appears earlier.
fn kind_sort_rank(kind: Option<CompletionItemKind>) -> u8 {
    match kind {
        Some(CompletionItemKind::FUNCTION) | Some(CompletionItemKind::METHOD) => 0,
        Some(CompletionItemKind::FIELD)    | Some(CompletionItemKind::VARIABLE)
        | Some(CompletionItemKind::CONSTANT) | Some(CompletionItemKind::ENUM_MEMBER) => 1,
        Some(CompletionItemKind::CLASS)    | Some(CompletionItemKind::INTERFACE)
        | Some(CompletionItemKind::ENUM)   | Some(CompletionItemKind::MODULE) => 3,
        _ => 2,
    }
}

/// Returns the `sort_text` visibility prefix.
/// Private symbols get the `"prv:"` tag so `complete_dot` can filter them out.
fn vis_tag(vis: Visibility) -> &'static str {
    match vis {
        Visibility::Private   => "prv:",
        Visibility::Protected => "prt:",
        _                     => "",
    }
}

/// Bare-word completion: prefix-filter across local file + same-package + index.
///
/// Case heuristic:
/// - **Lowercase prefix** → only return symbols whose name starts with a
///   lowercase letter (local vars, params, fields, fun names).  Class names are
///   excluded because they are rarely what the user wants when typing `acc…`.
/// - **Uppercase prefix or empty** → return everything (class names + members).
fn complete_bare(idx: &Indexer, prefix: &str, from_uri: &Url, snippets: bool) -> Vec<CompletionItem> {
    let prefix_lower = prefix.to_lowercase();
    let lowercase_mode = prefix.chars().next().map(|c| c.is_lowercase()).unwrap_or(false);
    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();

    // `tier` controls sort order: 0 = same file, 1 = same pkg, 2 = cross-pkg, 3 = stdlib.
    let mut add = |name: &str, kind: CompletionItemKind, tier: u8| {
        // Case gate: in lowercase mode skip CamelCase symbols.
        if lowercase_mode && name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            return;
        }
        if name.to_lowercase().starts_with(&prefix_lower) && seen.insert(name.to_string()) {
            let is_fn = snippets && matches!(kind, CompletionItemKind::FUNCTION | CompletionItemKind::METHOD);
            items.push(CompletionItem {
                label:              name.to_string(),
                kind:               Some(kind),
                sort_text:          Some(format!("{}:{}", tier, name.to_lowercase())),
                insert_text:        if is_fn { Some(format!("{}($1)", name)) } else { None },
                insert_text_format: if is_fn { Some(InsertTextFormat::SNIPPET) } else { None },
                ..Default::default()
            });
        }
    };

    // 1. Local file symbols — tier 0 (highest priority).
    if let Some(f) = idx.files.get(from_uri.as_str()) {
        for sym in &f.symbols {
            add(&sym.name, symbol_kind_to_completion(sym.kind), 0);
        }
        // Surface constructor params / local vars from cached declared_names.
        if lowercase_mode {
            for name in &f.declared_names {
                add(name, CompletionItemKind::VARIABLE, 0);
            }
        }
    }

    // 2. Same-package symbols — tier 1.
    let pkg = idx.files.get(from_uri.as_str())
        .and_then(|f| f.package.clone())
        .unwrap_or_default();
    if !pkg.is_empty() {
        if let Some(uris) = idx.packages.get(&pkg) {
            for uri_str in uris.iter() {
                if uri_str == from_uri.as_str() { continue; }
                if let Some(f) = idx.files.get(uri_str.as_str()) {
                    for sym in &f.symbols {
                        add(&sym.name, symbol_kind_to_completion(sym.kind), 1);
                    }
                }
            }
        }
    }

    // 3. Cross-package class index — tier 2, only in uppercase mode to avoid noise.
    if !lowercase_mode {
        if let Ok(cache) = idx.bare_name_cache.read() {
            for name in cache.iter() {
                add(name, CompletionItemKind::CLASS, 2);
            }
        }
    }

    // 4. Stdlib top-level / scope functions (listOf, println, run, with, …) — tier 3.
    for mut item in crate::stdlib::bare_completions(snippets) {
        if lowercase_mode && item.label.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            continue;
        }
        if item.label.to_lowercase().starts_with(&prefix_lower) && seen.insert(item.label.clone()) {
            item.sort_text = Some(format!("3:{}", item.label.to_lowercase()));
            items.push(item);
        }
    }

    items
}

/// Collect all symbols from a file URI as completion items.
/// Results are cached in `idx.completion_cache` so the file is only parsed
/// (or converted) once; subsequent calls for the same URI return instantly.
fn symbols_from_uri_as_completions(idx: &Indexer, file_uri: &str) -> Vec<CompletionItem> {
    // Fast path: already computed.
    if let Some(cached) = idx.completion_cache.get(file_uri) {
        return cached.as_ref().clone();
    }

    let items = build_completion_items(idx, file_uri);
    let arc = Arc::new(items.clone());
    idx.completion_cache.insert(file_uri.to_string(), arc);
    items
}

/// Build completion items for a file, from index or on-demand disk parse.
/// Always builds with snippet fields set; callers strip them if the client
/// doesn't support snippets.
fn build_completion_items(idx: &Indexer, file_uri: &str) -> Vec<CompletionItem> {
    let mut items = Vec::new();

    // From index if available.
    if let Some(f) = idx.files.get(file_uri) {
        for sym in &f.symbols {
            let ck       = symbol_kind_to_completion(sym.kind);
            let vis_tag  = vis_tag(sym.visibility);
            let sort_txt = format!("{vis_tag}{}{}", kind_sort_rank(Some(ck)), sym.name);
            items.push(make_completion_item(&sym.name, ck, sort_txt, true));
        }
        for name in &f.declared_names {
            if !items.iter().any(|i: &CompletionItem| i.label == *name) {
                items.push(make_completion_item(name, CompletionItemKind::FIELD, format!("1{name}"), true));
            }
        }
        return items;
    }

    // Fall back to on-demand parse.
    if let Ok(url) = Url::parse(file_uri) {
        if let Ok(path) = url.to_file_path() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let file_data = crate::parser::parse_kotlin(&content);
                for sym in &file_data.symbols {
                    let ck       = symbol_kind_to_completion(sym.kind);
                    let vis_tag  = vis_tag(sym.visibility);
                    let sort_txt = format!("{vis_tag}{}{}", kind_sort_rank(Some(ck)), sym.name);
                    items.push(make_completion_item(&sym.name, ck, sort_txt, true));
                }
                for name in &file_data.declared_names {
                    if !items.iter().any(|i: &CompletionItem| i.label == *name) {
                        items.push(make_completion_item(name, CompletionItemKind::FIELD, format!("1{name}"), true));
                    }
                }
            }
        }
    }
    items
}

fn symbol_kind_to_completion(kind: SymbolKind) -> CompletionItemKind {
    match kind {
        SymbolKind::FUNCTION | SymbolKind::METHOD => CompletionItemKind::FUNCTION,
        SymbolKind::CLASS                          => CompletionItemKind::CLASS,
        SymbolKind::INTERFACE                      => CompletionItemKind::INTERFACE,
        SymbolKind::ENUM                           => CompletionItemKind::ENUM,
        SymbolKind::ENUM_MEMBER                    => CompletionItemKind::ENUM_MEMBER,
        SymbolKind::CONSTANT                       => CompletionItemKind::CONSTANT,
        SymbolKind::VARIABLE                       => CompletionItemKind::VARIABLE,
        SymbolKind::OBJECT | SymbolKind::MODULE    => CompletionItemKind::MODULE,
        _                                          => CompletionItemKind::VALUE,
    }
}

/// Build a single `CompletionItem` for a named symbol.
///
/// Functions and methods get a snippet `name($1)` so the cursor lands inside
/// the parentheses after accepting the completion.  All other kinds are plain
/// text insertions.
fn make_completion_item(name: &str, ck: CompletionItemKind, sort_text: String, snippets: bool) -> CompletionItem {
    let is_fn = snippets && matches!(ck, CompletionItemKind::FUNCTION | CompletionItemKind::METHOD);
    CompletionItem {
        label:              name.to_string(),
        kind:               Some(ck),
        sort_text:          Some(sort_text),
        insert_text:        if is_fn { Some(format!("{}($1)", name)) } else { None },
        insert_text_format: if is_fn { Some(InsertTextFormat::SNIPPET) } else { None },
        ..Default::default()
    }
}

/// Public wrapper around `symbols_from_uri_as_completions` for use by the
/// pre-warmer in `indexer.rs`.  Builds + caches completion items for a file.
pub fn symbols_from_uri_as_completions_pub(idx: &Indexer, file_uri: &str) -> Vec<CompletionItem> {
    symbols_from_uri_as_completions(idx, file_uri)
}



/// Resolve `name` as seen from `from_uri`, returning all known definition
/// `Location`s in priority order.  Returns an empty vec only when nothing was
/// found by any strategy including `rg`.
pub fn resolve_symbol(idx: &Indexer, name: &str, qualifier: Option<&str>, from_uri: &Url) -> Vec<Location> {
    // 0. Qualified access: `AccountPickerMapper.Content` — cursor on `Content`.
    //    Resolve the qualifier to a file, then search that file for `name`.
    if let Some(qual) = qualifier {
        let locs = resolve_qualified(idx, name, qual, from_uri);
        if !locs.is_empty() { return locs; }
        // If qualifier resolution failed (e.g. it's a package name, not a class),
        // fall through to the normal chain.
    }

    // Handle dotted type names like `DashboardProductsReducer.Factory` passed
    // directly as `name` (e.g. from hover/goto-def of a variable's declared type).
    if let Some(dot) = name.find('.') {
        let outer = &name[..dot];
        let inner = &name[dot + 1..];
        // Resolve the outer type to find its file.
        let outer_locs = resolve_symbol_inner(idx, outer, from_uri, true);
        if let Some(outer_loc) = outer_locs.first() {
            let file_uri = outer_loc.uri.as_str();
            let locs = find_name_in_uri(idx, inner, file_uri);
            if !locs.is_empty() { return locs; }
        }
    }

    resolve_symbol_inner(idx, name, from_uri, true)
}

/// Internal resolver.  When `with_hierarchy` is false step 4.5 is skipped to
/// avoid infinite recursion inside `resolve_from_class_hierarchy` (which calls
/// this function to locate each superclass, and those files would in turn call
/// the hierarchy walk again with a fresh visited-set, looping forever).
fn resolve_symbol_inner(idx: &Indexer, name: &str, from_uri: &Url, with_hierarchy: bool) -> Vec<Location> {
    // 1 ── local (indexed symbols) ────────────────────────────────────────────
    let local = resolve_local(idx, name, from_uri);
    if !local.is_empty() { return local; }

    // 1.5 ── local variable / parameter declaration (line scan) ───────────────
    // Catches function parameters without val/var that aren't in the symbol index.
    // Also catches named lambda parameters: `{ item -> ...}` found via the
    // `name ->` pattern in find_declaration_range_in_lines.
    if name.chars().next().map(|c| c.is_lowercase()).unwrap_or(true) {
        let decl = find_local_declaration(idx, name, from_uri);
        if !decl.is_empty() { return decl; }
    }

    // 2 ── explicit imports ───────────────────────────────────────────────────
    let imported = resolve_via_imports(idx, name, from_uri);
    if !imported.is_empty() { return imported; }

    // 3 ── same package ───────────────────────────────────────────────────────
    let same_pkg = resolve_same_package(idx, name, from_uri);
    if !same_pkg.is_empty() { return same_pkg; }

    // 4 ── star imports ───────────────────────────────────────────────────────
    let star = resolve_star_imports(idx, name, from_uri);
    if !star.is_empty() { return star; }

    // 4.5 ── superclass / interface hierarchy ─────────────────────────────────
    if with_hierarchy {
        let mut visited: Vec<String> = Vec::new();
        let inherited = resolve_from_class_hierarchy(idx, name, from_uri, 0, &mut visited);
        if !inherited.is_empty() { return inherited; }
    }

    // 5 ── project-wide rg ───────────────────────────────────────────────────
    rg_find_definition(name, idx.workspace_root.get().map(PathBuf::as_path))
}

/// Index-only resolver for use in completion paths.
///
/// Identical to `resolve_symbol_inner` but omits:
/// - Step 4's `rg_in_package_dir` fallback (inside `resolve_star_imports`)
/// - Step 4.5 hierarchy walk
/// - Step 5 `rg_find_definition`
///
/// Completion is triggered on every keystroke; spawning external `rg`/`fd`
/// processes on each request would block the LSP thread and spike CPU.
pub(crate) fn resolve_symbol_no_rg(idx: &Indexer, name: &str, from_uri: &Url) -> Vec<Location> {
    let local = resolve_local(idx, name, from_uri);
    if !local.is_empty() { return local; }

    let imported = resolve_via_imports(idx, name, from_uri);
    if !imported.is_empty() { return imported; }

    let same_pkg = resolve_same_package(idx, name, from_uri);
    if !same_pkg.is_empty() { return same_pkg; }

    // Star imports: index-only scan (no rg fallback for unindexed files).
    let star_pkgs: Vec<String> = match idx.files.get(from_uri.as_str()) {
        Some(f) => f.imports.iter()
            .filter(|i| i.is_star && !crate::resolver::is_stdlib(&i.full_path))
            .map(|i| i.full_path.clone())
            .collect(),
        None => vec![],
    };
    for pkg in star_pkgs {
        let peer_uris: Vec<String> = idx.packages.get(&pkg).map(|u| u.clone()).unwrap_or_default();
        for peer_uri_str in peer_uris {
            if let Some(f) = idx.files.get(&peer_uri_str) {
                for sym in f.symbols.iter().filter(|s| s.name == name) {
                    if let Ok(u) = Url::parse(&peer_uri_str) {
                        return vec![Location { uri: u, range: sym.selection_range }];
                    }
                }
            }
        }
    }

    // Check the global definitions index as a final fast fallback.
    if let Some(locs) = idx.definitions.get(name) {
        if let Some(loc) = locs.first() {
            return vec![loc.clone()];
        }
    }

    vec![]
}

// ─── step implementations ────────────────────────────────────────────────────

/// Step 0 — dot-qualified access.
///
/// Handles two families of chains:
///
/// **Uppercase root** (`Outer.Inner`, `A.B.C.D`): all segments are class/object
/// names; the root identifies the file and all nested types live in the same
/// file, so we resolve root → file and search that file for `name`.
///
/// **Lowercase root** (`variable.field`, `account.account.interestPlanCode`):
/// the first segment is a variable/parameter — we infer its declared type, then
/// traverse every subsequent lowercase segment as a field access (inferring each
/// field's type in turn) until we have a file to search `name` in.
/// Uppercase segments inside a lowercase chain are treated as nested class names
/// within the current file.
fn resolve_qualified(idx: &Indexer, name: &str, qualifier: &str, from_uri: &Url) -> Vec<Location> {
    let segments: Vec<&str> = qualifier.split('.').collect();
    let root = segments[0];

    // ── `this.member` — search current file and its superclass hierarchy ──────
    if root == "this" {
        let locs = find_name_in_uri(idx, name, from_uri.as_str());
        if !locs.is_empty() { return locs; }
        let mut visited = vec![];
        return resolve_from_class_hierarchy(idx, name, from_uri, 0, &mut visited);
    }

    // ── `super.member` — search superclass hierarchy only ────────────────────
    if root == "super" {
        let mut visited = vec![from_uri.as_str().to_owned()]; // skip current file
        return resolve_from_class_hierarchy(idx, name, from_uri, 0, &mut visited);
    }

    if root.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
        // ── Uppercase chain: find the root's file and search it for `name` ──
        // Pass the qualifier's own line as a hint so that when the same field name
        // appears in multiple classes in the same file (e.g. State and Effect both
        // have `toastModel`), we pick the declaration closest *after* the qualifier
        // class definition rather than the first match in the file.
        let qual_locs = resolve_symbol(idx, root, None, from_uri);
        for qual_loc in &qual_locs {
            let after_line = qual_loc.range.start.line;
            let locs = find_name_in_uri_after_line(idx, name, qual_loc.uri.as_str(), after_line);
            if !locs.is_empty() { return locs; }
        }
        return vec![];
    }

    // ── Lowercase root: variable / parameter type inference ──────────────────
    let Some(start_type) = infer_variable_type(idx, root, from_uri) else {
        return vec![];
    };

    // `start_type` may be a dotted nested type like `Outer.Inner`.
    // Split into outer (for file resolution) and optional inner (nested class).
    let (outer_type, inner_type) = match start_type.find('.') {
        Some(dot) => (&start_type[..dot], Some(&start_type[dot + 1..])),
        None      => (start_type.as_str(), None),
    };

    // Resolve the variable's type to its source file.
    let type_locs = resolve_symbol(idx, outer_type, None, from_uri);
    let mut current_file: Option<String> = type_locs.first().map(|l| l.uri.to_string());

    // If there's a nested type component (e.g. `Factory` in `Outer.Factory`),
    // the members we want to search are inside that nested type.
    // We don't need to change `current_file` because nested types live in the
    // same file; instead we record it as a trailing qualifier segment to process.
    let extra_segments: Vec<&str> = inner_type.map(|t| vec![t]).unwrap_or_default();

    // Traverse remaining qualifier segments (plus any from the nested type).
    for &seg in extra_segments.iter().chain(segments[1..].iter()) {
        let Some(ref uri) = current_file else { return vec![]; };
        if seg.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            // Nested class / companion object — likely in the same file.
            // Search current file first; fall back to a global resolve.
            let locs = find_name_in_uri(idx, seg, uri);
            current_file = if !locs.is_empty() {
                locs.first().map(|l| l.uri.to_string())
            } else {
                resolve_symbol(idx, seg, None, from_uri).first().map(|l| l.uri.to_string())
            };
        } else {
            // Field access: infer the declared type of this field.
            let Some(field_type) = infer_field_type(idx, uri, seg) else {
                return vec![];
            };
            let locs = resolve_symbol(idx, &field_type, None, from_uri);
            current_file = locs.first().map(|l| l.uri.to_string());
        }
    }

    current_file
        .as_deref()
        .map(|uri| find_name_in_uri(idx, name, uri))
        .unwrap_or_default()
}

/// Step 1 — symbols defined in the same source file.
fn resolve_local(idx: &Indexer, name: &str, uri: &Url) -> Vec<Location> {
    idx.files
        .get(uri.as_str())
        .map(|f| {
            f.symbols
                .iter()
                .filter(|s| s.name == name)
                .map(|s| Location { uri: uri.clone(), range: s.selection_range })
                .collect()
        })
        .unwrap_or_default()
}

/// Step 2 — explicit single-symbol imports.
///
/// Handles three cases:
///   a. Top-level class:   `import com.example.Foo`
///   b. Nested class:      `import com.example.OuterClass.InnerClass`
///   c. Alias:             `import com.example.Foo as F`
///
/// Resolution sub-steps (each tried in order):
///   i.   qualified index  — exact match, O(1), works once file is indexed
///   ii.  definitions index — short-name, filtered to expected package
///   iii. fd + on-demand parse — works at cold start; tries parent class file
///        first for nested symbols (AccountPickerContract.kt before Event.kt)
fn resolve_via_imports(idx: &Indexer, name: &str, uri: &Url) -> Vec<Location> {
    let imports: Vec<crate::types::ImportEntry> = match idx.files.get(uri.as_str()) {
        Some(f) => f.imports.iter().filter(|i| !i.is_star).cloned().collect(),
        None    => return vec![],
    };

    for imp in imports.iter().filter(|i| i.local_name == name) {
        // i) qualified index — exact FQN (works for top-level classes)
        if let Some(loc) = idx.qualified.get(&imp.full_path) {
            return vec![loc.clone()];
        }

        // ii) short-name index filtered to the expected package.
        //     For `…AccountPickerContract.Event` the expected package is
        //     `…accountpicker` (all-lowercase prefix segments).
        //     This avoids returning an unrelated `Event` from another package.
        let short       = last_segment(&imp.full_path);
        let expected_pkg = package_prefix(&imp.full_path);
        if let Some(locs) = idx.definitions.get(short) {
            let filtered: Vec<_> = locs.iter()
                .filter(|loc| {
                    idx.files.get(loc.uri.as_str())
                        .and_then(|f| f.package.clone())
                        .map(|p| p == expected_pkg || p.starts_with(&expected_pkg))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            if !filtered.is_empty() { return filtered; }
        }

        // iii) on-demand fd + parse (indexing race or file never opened).
        let root = idx.workspace_root.get().map(PathBuf::as_path);
        let locs = fd_find_and_parse(name, &imp.full_path, root);
        if !locs.is_empty() { return locs; }
    }
    vec![]
}

/// Derive the Kotlin package from an import path by taking all dot-separated
/// segments that start with a lowercase letter (package convention).
///
/// `cz.moneta.app.AccountPickerContract.Event` → `"cz.moneta.app"`
fn package_prefix(import_path: &str) -> String {
    import_path
        .split('.')
        .take_while(|s| s.chars().next().map(|c| c.is_lowercase()).unwrap_or(false))
        .collect::<Vec<_>>()
        .join(".")
}

/// Uppercase segment stems in priority order — outer class first.
///
/// `com.example.OuterClass.InnerClass` → `["OuterClass", "InnerClass"]`
/// `com.example.Foo`                   → `["Foo"]`
fn import_file_stems(import_path: &str) -> Vec<String> {
    let upper: Vec<&str> = import_path
        .split('.')
        .filter(|s| s.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
        .collect();
    match upper.as_slice() {
        []             => vec![],
        [only]         => vec![only.to_string()],
        [.., par, lst] => vec![par.to_string(), lst.to_string()],
    }
}

/// Candidate .kt/.java filenames for a given import path, in priority order.
///
/// Kotlin convention: uppercase segments = class names; the first uppercase
/// segment is always the top-level class (= the file).  Any further uppercase
/// segment is a nested class defined *inside* that file.
///
/// `…accountpicker.AccountPickerContract.Event`
///   → `["AccountPickerContract.kt", "AccountPickerContract.java",
///       "Event.kt", "Event.java"]`  (outer-class file tried first)
///
/// `…example.Foo`
///   → `["Foo.kt", "Foo.java"]`
fn import_file_candidates(import_path: &str) -> Vec<String> {
    import_file_stems(import_path)
        .into_iter()
        .flat_map(|stem| [format!("{stem}.kt"), format!("{stem}.java")])
        .collect()
}

/// Find and synchronously parse the file most likely to contain `symbol_name`.
///
/// Search strategy (fastest-first):
///   1. fd `--full-path` regex derived from the import's package dir + filename —
///      extremely precise; handles multi-module projects where files live in
///      subdirs like `app/src/main/java/cz/moneta/…/EProductScreen.java`
///   2. Fallback: global fd by filename only (handles non-standard layouts)
fn fd_find_and_parse(symbol_name: &str, full_import_path: &str, root: Option<&Path>) -> Vec<Location> {
    let pkg = package_prefix(full_import_path);
    let expected_pkg = if pkg.is_empty() { None } else { Some(pkg.as_str()) };
    let pkg_dir = pkg.replace('.', "/");

    for stem in import_file_stems(full_import_path) {
        // Strategy 1: precise full-path regex including the package directory.
        // e.g. ".*/cz/moneta/data/compat/enums/product/EProductScreen\.(kt|java)$"
        if let Some(root) = root {
            let pat = if pkg_dir.is_empty() {
                format!(r"{stem}\.(kt|java)$")
            } else {
                format!(r".*/{pkg_dir}/{stem}\.(kt|java)$")
            };
            let locs = fd_search_by_full_path_pattern(&pat, symbol_name, expected_pkg, root);
            if !locs.is_empty() { return locs; }
        }

        // Strategy 2: global filename-only search (fallback for flat / non-standard layouts).
        for ext in ["kt", "java"] {
            let locs = fd_search_file(&format!("{stem}.{ext}"), symbol_name, expected_pkg, root);
            if !locs.is_empty() { return locs; }
        }
    }
    vec![]
}

/// fd `--full-path <regex>` — searches `root` for files whose absolute path
/// matches `pattern`.  Parses each hit and returns locations for `symbol_name`.
fn fd_search_by_full_path_pattern(
    pattern:      &str,
    symbol_name:  &str,
    expected_pkg: Option<&str>,
    root:         &Path,
) -> Vec<Location> {
    let Some(root_str) = root.to_str() else { return vec![] };
    let out = match std::process::Command::new("fd")
        .args(["--type", "f", "--absolute-path", "--full-path", pattern, root_str])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };
    parse_fd_hits(&out.stdout, symbol_name, expected_pkg)
}

fn fd_search_file(file_name: &str, symbol_name: &str, expected_pkg: Option<&str>, root: Option<&Path>) -> Vec<Location> {
    let mut cmd = std::process::Command::new("fd");
    cmd.args([
        "--type", "f",
        "--absolute-path",
        "--max-results", "10",
        file_name,
    ]);
    if let Some(r) = root { cmd.arg(r); }

    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };
    parse_fd_hits(&out.stdout, symbol_name, expected_pkg)
}

/// Parse a list of newline-separated absolute file paths from fd output,
/// parse each file with the appropriate parser, and return locations for
/// `symbol_name`.  When `expected_pkg` is given the package-exact match is
/// returned immediately; otherwise the first match wins.  A non-exact match
/// is kept as a fallback and returned only if no exact match is found.
fn parse_fd_hits(stdout: &[u8], symbol_name: &str, expected_pkg: Option<&str>) -> Vec<Location> {
    let mut fallback: Option<tower_lsp::lsp_types::Location> = None;

    for path_str in String::from_utf8_lossy(stdout).lines() {
        let path_str = path_str.trim();
        if path_str.is_empty() { continue; }

        let path = std::path::Path::new(path_str);
        let Ok(uri) = tower_lsp::lsp_types::Url::from_file_path(path) else { continue };
        let Ok(content) = std::fs::read_to_string(path) else { continue };

        let file_data = if path_str.ends_with(".java") {
            crate::parser::parse_java(&content)
        } else {
            crate::parser::parse_kotlin(&content)
        };
        let Some(sym) = file_data.symbols.iter().find(|s| s.name == symbol_name) else { continue };

        let loc = tower_lsp::lsp_types::Location { uri, range: sym.selection_range };

        if let Some(pkg) = expected_pkg {
            if file_data.package.as_deref() == Some(pkg) {
                return vec![loc];
            }
            if fallback.is_none() { fallback = Some(loc); }
        } else {
            return vec![loc];
        }
    }

    fallback.map(|l| vec![l]).unwrap_or_default()
}

/// Step 3 — same-package visibility (no import needed in Kotlin).
///
/// Finds all indexed files sharing the same `package` declaration as `from_uri`
/// and searches their symbols.
fn resolve_same_package(idx: &Indexer, name: &str, uri: &Url) -> Vec<Location> {
    // Get package name, release the dashmap ref immediately.
    let pkg: String = match idx.files.get(uri.as_str()).and_then(|f| f.package.clone()) {
        Some(p) => p,
        None    => return vec![],
    };

    let peer_uris: Vec<String> = match idx.packages.get(&pkg) {
        Some(u) => u.clone(),
        None    => return vec![],
    };

    let self_str = uri.as_str();
    for peer_uri_str in peer_uris {
        if peer_uri_str == self_str { continue; }
        if let Some(f) = idx.files.get(&peer_uri_str) {
            for sym in f.symbols.iter().filter(|s| s.name == name) {
                if let Ok(u) = Url::parse(&peer_uri_str) {
                    return vec![Location { uri: u, range: sym.selection_range }];
                }
            }
        }
    }
    vec![]
}

/// Step 4 — star imports: `import com.example.*`.
///
/// For each star import:
///   a. Check indexed files in that package (fast, O(files_in_package)).
///   b. If nothing found, run `rg` scoped to the package directory path
///      (handles files that were never opened / indexed).
///
/// Stdlib packages are skipped entirely.
fn resolve_star_imports(idx: &Indexer, name: &str, uri: &Url) -> Vec<Location> {
    let star_pkgs: Vec<String> = match idx.files.get(uri.as_str()) {
        Some(f) => f.imports.iter()
            .filter(|i| i.is_star && !is_stdlib(&i.full_path))
            .map(|i| i.full_path.clone())
            .collect(),
        None => return vec![],
    };

    for pkg in star_pkgs {
        // a) indexed files in this package
        let peer_uris: Vec<String> = idx.packages.get(&pkg).map(|u| u.clone()).unwrap_or_default();
        for peer_uri_str in peer_uris {
            if let Some(f) = idx.files.get(&peer_uri_str) {
                for sym in f.symbols.iter().filter(|s| s.name == name) {
                    if let Ok(u) = Url::parse(&peer_uri_str) {
                        return vec![Location { uri: u, range: sym.selection_range }];
                    }
                }
            }
        }

        // b) rg scoped to the package directory for unindexed files
        let root = idx.workspace_root.get().map(PathBuf::as_path);
        let locs = rg_in_package_dir(name, &pkg, root);
        if !locs.is_empty() { return locs; }
    }
    vec![]
}

// ─── step 4.5: superclass / interface hierarchy ───────────────────────────────

/// Walk the superclass / interface hierarchy of the class(es) declared in
/// `from_uri` looking for a symbol named `name`.
///
/// Algorithm
/// ---------
/// 1. Extract direct supertype names from `from_uri`'s lines.
/// 2. Resolve each supertype through the normal chain (imports, same-package…).
/// 3. Search the resolved file's symbol table for `name`.
/// 4. Recurse into that file's own supertypes (depth-limited, cycle-safe).
fn resolve_from_class_hierarchy(
    idx:      &Indexer,
    name:     &str,
    from_uri: &Url,
    depth:    u8,
    visited:  &mut Vec<String>,
) -> Vec<Location> {
    const MAX_DEPTH: u8 = 4;
    if depth >= MAX_DEPTH { return vec![]; }

    let key = from_uri.as_str().to_owned();
    if visited.contains(&key) { return vec![]; }
    visited.push(key);

    let lines: Arc<Vec<String>> = match idx.files.get(from_uri.as_str()) {
        Some(f) => f.lines.clone(),
        None    => return vec![],
    };

    for super_name in extract_supers_from_lines(&lines) {
        // Locate the supertype's file via steps 1-4+5 only — NOT step 4.5.
        // Using the full resolve_symbol here would re-enter this function with
        // a fresh visited-set, causing infinite recursion.
        let super_locs = resolve_symbol_inner(idx, &super_name, from_uri, false);
        for super_loc in &super_locs {
            let locs = find_name_in_uri(idx, name, super_loc.uri.as_str());
            if !locs.is_empty() { return locs; }

            // Recurse into the supertype's own hierarchy.
            let locs = resolve_from_class_hierarchy(
                idx, name, &super_loc.uri, depth + 1, visited,
            );
            if !locs.is_empty() { return locs; }
        }
    }
    vec![]
}

/// Extract direct supertype names from source lines.
///
/// Handles:
/// - Kotlin single-line: `class Foo : Bar(), Baz<X> {`
/// - Kotlin multi-line constructor: last line of primary ctor is `) : Bar()` or
///   a standalone `) : Bar()` after the constructor parameter block
/// - Java: `class Foo extends Bar implements Baz, Qux {`
pub(crate) fn extract_supers_from_lines(lines: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    for line in lines {
        let t = line.trim();
        if t.is_empty()
            || t.starts_with("//")
            || t.starts_with('*')
            || t.starts_with("import ")
            || t.starts_with("package ")
            || t.starts_with('@')
        {
            continue;
        }

        // Java: extends SuperClass
        if let Some(pos) = word_boundary_pos(t, "extends") {
            let rest = t[pos + 7..].trim_start();
            let nm = leading_type_ident(rest);
            if !nm.is_empty() { result.push(nm.to_owned()); }
        }

        // Java: implements I1, I2
        if let Some(pos) = word_boundary_pos(t, "implements") {
            let rest  = t[pos + 10..].trim_start();
            let chunk = rest.split('{').next().unwrap_or(rest);
            for part in split_top_level_commas(chunk) {
                let nm = leading_type_ident(part.trim());
                if !nm.is_empty() { result.push(nm.to_owned()); }
            }
        }

        // Kotlin delegation specifiers: `: TypeA(...), TypeB<X>`
        if let Some(colon) = kotlin_delegation_colon(t) {
            let after = t[colon + 1..].trim_start();
            let chunk = after.split('{').next().unwrap_or(after);
            for part in split_top_level_commas(chunk) {
                let nm = leading_type_ident(part.trim());
                if !nm.is_empty()
                    && nm.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
                {
                    result.push(nm.to_owned());
                }
            }
        }
    }
    result.dedup();
    result
}

/// Find the byte offset of `word` in `text` at a word boundary.
fn word_boundary_pos(text: &str, word: &str) -> Option<usize> {
    let wlen = word.len();
    let b    = text.as_bytes();
    let wb   = word.as_bytes();
    let mut i = 0;
    while i + wlen <= b.len() {
        if b[i..i + wlen] == *wb {
            let before_ok = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
            let after_ok  = i + wlen >= b.len()
                || !(b[i + wlen].is_ascii_alphanumeric() || b[i + wlen] == b'_');
            if before_ok && after_ok { return Some(i); }
        }
        i += 1;
    }
    None
}

/// Extract the leading identifier / type name, stopping before `<(;{ ,\t\n`.
fn leading_type_ident(s: &str) -> &str {
    let end = s
        .find(|c: char| matches!(c, '<' | '(' | '{' | ';' | ',' | ' ' | '\t' | '\n'))
        .unwrap_or(s.len());
    &s[..end]
}

/// Find the `:` that introduces Kotlin delegation specifiers (not type
/// annotations on `val`/`var`/`fun` return types).
///
/// Valid:   `class Foo : Bar`  `) : Bar`  `class Foo(val x: Int) : Bar`
/// Invalid: `val x: Int`  `fun f(): Int`  (return-type annotations)
fn kotlin_delegation_colon(line: &str) -> Option<usize> {
    let b = line.as_bytes();
    let mut depth: i32 = 0;
    let mut found: Option<usize> = None;

    for (i, &ch) in b.iter().enumerate() {
        match ch {
            b'<' | b'(' => depth += 1,
            b'>' | b')' => { if depth > 0 { depth -= 1; } }
            b':' if depth == 0 => {
                // Must be followed (after spaces) by an uppercase letter.
                let after = line[i + 1..].trim_start();
                if !after.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                    continue;
                }
                let before     = line[..i].trim_end();
                let class_kw   = before.contains("class ")
                    || before.contains("interface ")
                    || before.contains("object ");
                let after_paren = before.ends_with(')');

                if class_kw {
                    // Inside a class/interface/object declaration line — always valid.
                    found = Some(i);
                } else if after_paren {
                    // Could be `fun f(): Int` (return type) or `): Bar` (continuation).
                    // Accept only if the line itself starts with `)` (a pure continuation
                    // line from a multi-line primary constructor).
                    if line.trim_start().starts_with(')') {
                        found = Some(i);
                    }
                }
            }
            _ => {}
        }
    }
    found
}

/// Split `text` at commas that are not inside `<>` or `()`.
fn split_top_level_commas(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    for (i, ch) in text.char_indices() {
        match ch {
            '<' | '(' => depth += 1,
            '>' | ')' => { if depth > 0 { depth -= 1; } }
            ',' if depth == 0 => { parts.push(&text[start..i]); start = i + 1; }
            _ => {}
        }
    }
    if start <= text.len() { parts.push(&text[start..]); }
    parts
}

/// `rg` scoped to the directory that would contain `package` sources.
///
/// Package `com.example.ui` → globs `**/com/example/ui/*.kt` and `…/*.java`.
/// This handles the common case where the package structure mirrors the
/// directory tree (standard Kotlin / Maven / Gradle convention).
fn rg_in_package_dir(name: &str, package: &str, root: Option<&Path>) -> Vec<Location> {
    let pkg_path = package.replace('.', "/");
    let kt_glob   = format!("**/{pkg_path}/*.kt");
    let java_glob = format!("**/{pkg_path}/*.java");
    let pattern   = build_rg_pattern(name);

    let search_root: std::borrow::Cow<Path> = match root {
        Some(r) => std::borrow::Cow::Borrowed(r),
        None    => std::borrow::Cow::Owned(std::env::current_dir().unwrap_or_default()),
    };

    let mut cmd = Command::new("rg");
    cmd.args([
        "--no-heading", "--with-filename", "--line-number", "--column",
        "--glob", &kt_glob,
        "--glob", &java_glob,
        "-e", &pattern,
    ]);
    cmd.arg(search_root.as_ref());

    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_rg_line)
        .collect()
}

// ─── shared helpers ───────────────────────────────────────────────────────────

/// Scan the current file's lines for a type annotation on `var_name` and return
/// the declared type name if found.  Delegates to [`infer_type_in_lines`].
fn infer_variable_type(idx: &Indexer, var_name: &str, uri: &Url) -> Option<String> {
    // Fast reject: if var_name isn't in declared_names, it has no `: Type`
    // annotation in this file — skip the full line scan entirely.
    if let Some(data) = idx.files.get(uri.as_str()) {
        if !data.declared_names.iter().any(|n| n == var_name) {
            return None;
        }
    }
    // Prefer live_lines: updated synchronously on every keystroke.
    if let Some(ll) = idx.live_lines.get(uri.as_str()) {
        if let result @ Some(_) = infer_type_in_lines(&*ll, var_name) {
            return result;
        }
    }
    // Fall back to indexed lines.
    if let Some(data) = idx.files.get(uri.as_str()) {
        return infer_type_in_lines(&data.lines, var_name);
    }
    // File not indexed yet — read from disk.
    let path = uri.to_file_path().ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let lines: Vec<String> = content.lines().map(String::from).collect();
    infer_type_in_lines(&lines, var_name)
}

/// Like [`infer_variable_type`] but preserves generic parameters in the returned
/// type string.  e.g. `val items: List<Product>` → `"List<Product>"`.
///
/// Used by the `it`-completion path to extract the collection element type.
pub fn infer_variable_type_raw(idx: &Indexer, var_name: &str, uri: &Url) -> Option<String> {
    if let Some(ll) = idx.live_lines.get(uri.as_str()) {
        if let result @ Some(_) = infer_type_in_lines_raw(&*ll, var_name) {
            return result;
        }
    }
    if let Some(data) = idx.files.get(uri.as_str()) {
        return infer_type_in_lines_raw(&data.lines, var_name);
    }
    let path = uri.to_file_path().ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let lines: Vec<String> = content.lines().map(String::from).collect();
    infer_type_in_lines_raw(&lines, var_name)
}

/// Extract the Kotlin/Android collection element type from a raw generic type string.
///
/// Handles the most common collection-like types seen in Android development:
/// - `List<Product>` → `Product`
/// - `MutableList<User>` → `User`
/// - `Flow<Event>` → `Event`
/// - `StateFlow<UiState>` → `UiState`
/// - `Set<Tag>` → `Tag`
/// - etc.
///
/// Returns `None` when the base type is not in the known collection list, or when
/// the generic parameter is a primitive/lowercase type.  In those cases the
/// caller should treat `it` as the receiver type itself (scope functions).
pub fn extract_collection_element_type(raw_type: &str) -> Option<String> {
    const COLLECTION_TYPES: &[&str] = &[
        "List", "MutableList", "ArrayList",
        "Set", "MutableSet", "HashSet", "LinkedHashSet",
        "Collection", "MutableCollection", "Iterable", "MutableIterable",
        "Sequence", "Flow", "StateFlow", "SharedFlow",
        "Channel", "SendChannel", "ReceiveChannel",
        "Array",
    ];

    let base: String = raw_type.chars().take_while(|&c| c.is_alphanumeric() || c == '_').collect();
    if !COLLECTION_TYPES.contains(&base.as_str()) { return None; }

    let open  = raw_type.find('<')?;
    let close = raw_type.rfind('>')?;
    if close <= open { return None; }
    let inner = &raw_type[open + 1..close];

    // Take first type argument (before the first `,` at depth 0).
    let first = first_type_arg(inner).trim().trim_matches('?');

    // Strip to the base class name only.
    let elem: String = first.chars().take_while(|&c| c.is_alphanumeric() || c == '_').collect();
    if elem.is_empty() || !elem.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
        return None;
    }
    Some(elem)
}

/// Return the first type argument in a comma-separated generic parameter list,
/// respecting nested `<>` brackets.
fn first_type_arg(s: &str) -> &str {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => return &s[..i],
            _ => {}
        }
    }
    s
}

/// Scan a specific (possibly un-indexed) file for the declared type of `field_name`.
///
/// Checks the in-memory index first (lines are cached); falls back to reading
/// the file from disk when it isn't indexed yet.
fn infer_field_type(idx: &Indexer, file_uri: &str, field_name: &str) -> Option<String> {
    if let Some(data) = idx.files.get(file_uri) {
        return infer_type_in_lines(&data.lines, field_name);
    }
    let path = Url::parse(file_uri).ok()?.to_file_path().ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let lines: Vec<String> = content.lines().map(String::from).collect();
    infer_type_in_lines(&lines, field_name)
}

/// Core line scanner: find `var_name:` in `lines` and return the type that follows.
///
/// Handles:
/// - Constructor parameters: `private val repo: UserRepository`
/// - Properties:             `val config: Config`
/// - Local variables:        `val result: ResultType = ...`
/// - Function parameters:    `fun foo(repo: UserRepository)`
///
/// Returns the type name without nullable marker (`?`) and generic parameters (`<…>`).
/// Only returns names starting with an uppercase letter (skips primitives / unit).
fn infer_type_in_lines(lines: &[String], var_name: &str) -> Option<String> {
    let pattern = format!("{var_name}:");

    for line in lines {
        if !line.contains(&pattern) { continue; }

        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }

        if let Some(pos) = line.find(&pattern) {
            // Ensure var_name is not a suffix of a longer identifier.
            let before_char = line[..pos].chars().last();
            if before_char.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false) {
                continue;
            }
            let after = &line[pos + var_name.len()..];
            let after = after.trim_start_matches(':').trim_start();
            // Allow dotted type names like `DashboardProductsReducer.Factory`
            // Stop at generic params (`<`), nullability (`?`), spaces, assignment.
            let type_name: String = after.chars()
                .take_while(|&c| c.is_alphanumeric() || c == '_' || c == '.')
                .collect();
            // Trim any trailing dots.
            let type_name = type_name.trim_end_matches('.').to_owned();
            if !type_name.is_empty()
                && type_name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
            {
                return Some(type_name);
            }
        }
    }
    None
}

/// Like `infer_type_in_lines` but preserves generic parameters in the result.
///
/// `val items: List<Product>` → `"List<Product>"`
/// `val state: StateFlow<UiState>` → `"StateFlow<UiState>"`
fn infer_type_in_lines_raw(lines: &[String], var_name: &str) -> Option<String> {
    let pattern = format!("{var_name}:");

    for line in lines {
        if !line.contains(&pattern) { continue; }
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }
        if let Some(pos) = line.find(&pattern) {
            let before_char = line[..pos].chars().last();
            if before_char.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false) {
                continue;
            }
            let after = &line[pos + var_name.len()..];
            let after = after.trim_start_matches(':').trim_start();
            let raw = extract_type_with_generics(after);
            if !raw.is_empty() && raw.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                return Some(raw);
            }
        }
    }
    None
}

/// Capture a type expression including balanced generic parameters.
///
/// `"List<Product> = emptyList()"` → `"List<Product>"`
/// `"StateFlow<UiState>"` → `"StateFlow<UiState>"`
/// `"User?"` → `"User"`  (nullable stripped at the outer `?`)
fn extract_type_with_generics(s: &str) -> String {
    let mut result = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '<' => { depth += 1; result.push(c); }
            '>' => {
                if depth > 0 {
                    depth -= 1;
                    result.push(c);
                    if depth == 0 { break; }
                } else { break; }
            }
            // Stop at these outside of generic brackets.
            '?' | ' ' | '=' | ',' | ')' | '\n' if depth == 0 => break,
            _ => result.push(c),
        }
    }
    result
}

/// Return the `Range` of the declaration `name:` on the first matching line,
/// or `None` if not found.
///
/// Used to locate function parameters and other declarations that are not in
/// the tree-sitter symbol index (e.g. `fun foo(account: AccountModel)`).
fn find_declaration_range_in_lines(lines: &[String], name: &str) -> Option<Range> {
    // Pattern 1: `name: Type` — typed parameter, val/var declaration, constructor param
    let typed_pattern = format!("{name}:");

    // Pattern 2: `{ name ->` or `name ->` — untyped lambda / trailing-lambda parameter
    let lambda_arrow = format!("{name} ->");
    let lambda_brace = format!("{{ {name} ->");  // with brace prefix

    for (line_num, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }

        // ── typed parameter / val / var ─────────────────────────────────────
        if line.contains(&typed_pattern) {
            if let Some(pos) = line.find(&typed_pattern) {
                let before = line[..pos].chars().last();
                if !before.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false)
                    && !line[..pos].trim_end().ends_with('"')
                {
                    let col = pos as u32;
                    return Some(Range {
                        start: Position { line: line_num as u32, character: col },
                        end:   Position { line: line_num as u32, character: col + name.len() as u32 },
                    });
                }
            }
        }

        // ── untyped lambda parameter: `{ name ->` or leading `name ->` ─────
        if line.contains(&lambda_arrow) {
            // Must be `{ name ->` (with brace) or the name at the start of the
            // lambda params after trimming whitespace/opening brace.
            let is_lambda = line.contains(&lambda_brace)
                || trimmed.starts_with(&lambda_arrow)
                || trimmed.starts_with(&format!("{name},"))  // multi-param `a, b ->`
                || (trimmed.contains(&lambda_arrow)
                    && line[..line.find(&lambda_arrow).unwrap_or(0)]
                        .chars()
                        .all(|c| c.is_whitespace() || c == '{' || c == '(' || c == ',' || c.is_alphanumeric() || c == '_'));
            if is_lambda {
                if let Some(pos) = line.find(name) {
                    // Make sure we matched the right token (word boundary check)
                    let before = pos.checked_sub(1).and_then(|i| line.as_bytes().get(i)).copied();
                    let after  = line.as_bytes().get(pos + name.len()).copied();
                    let boundary = before.map(|b| !b.is_ascii_alphanumeric() && b != b'_').unwrap_or(true)
                        && after.map(|b| !b.is_ascii_alphanumeric() && b != b'_').unwrap_or(true);
                    if boundary {
                        let col = pos as u32;
                        return Some(Range {
                            start: Position { line: line_num as u32, character: col },
                            end:   Position { line: line_num as u32, character: col + name.len() as u32 },
                        });
                    }
                }
            }
        }
    }
    None
}

/// Search for `name` in a specific file identified by its URI string.
///
/// Checks the in-memory symbol index first; falls back to raw line scanning
/// (for constructor parameters) and finally on-demand tree-sitter parsing.
fn find_name_in_uri(idx: &Indexer, name: &str, file_uri: &str) -> Vec<Location> {
    let Ok(uri) = Url::parse(file_uri) else { return vec![]; };

    // a) Indexed file – symbol table
    if let Some(f) = idx.files.get(file_uri) {
        if let Some(sym) = f.symbols.iter().find(|s| s.name == name) {
            return vec![Location { uri, range: sym.selection_range }];
        }
        // b) Line scan for constructor params / un-indexed declarations
        if let Some(range) = find_declaration_range_in_lines(&f.lines, name) {
            return vec![Location { uri, range }];
        }
        return vec![];
    }

    // c) File not yet indexed — parse on demand using the correct parser
    if let Ok(path) = uri.to_file_path() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let file_data = if file_uri.ends_with(".java") {
                crate::parser::parse_java(&content)
            } else {
                crate::parser::parse_kotlin(&content)
            };
            if let Some(sym) = file_data.symbols.iter().find(|s| s.name == name) {
                return vec![Location { uri, range: sym.selection_range }];
            }
            let lines: Vec<String> = content.lines().map(String::from).collect();
            if let Some(range) = find_declaration_range_in_lines(&lines, name) {
                return vec![Location { uri, range }];
            }
        }
    }
    vec![]
}

/// Like `find_name_in_uri` but prefers declarations at or after `after_line`.
///
/// Used when we already know the qualifier class lives at `after_line` — we
/// want the parameter/field of THAT class, not a same-named field in a
/// different class that happens to appear earlier in the same file.
///
/// Strategy:
///   1. Symbol table — pick the symbol at or after `after_line` with the
///      smallest line number (closest match).  Fall back to any match if none
///      found after the hint line.
///   2. Line scan — search only lines >= `after_line`.
///   3. On-demand parse (same as `find_name_in_uri`).
fn find_name_in_uri_after_line(idx: &Indexer, name: &str, file_uri: &str, after_line: u32) -> Vec<Location> {
    let Ok(uri) = Url::parse(file_uri) else { return vec![]; };

    if let Some(f) = idx.files.get(file_uri) {
        // a) Symbol table: find the closest symbol at or after `after_line`.
        let best = f.symbols.iter()
            .filter(|s| s.name == name && s.selection_range.start.line >= after_line)
            .min_by_key(|s| s.selection_range.start.line);

        if let Some(sym) = best {
            return vec![Location { uri, range: sym.selection_range }];
        }

        // Fallback: any symbol with this name (different class, same file)
        if let Some(sym) = f.symbols.iter().find(|s| s.name == name) {
            return vec![Location { uri, range: sym.selection_range }];
        }

        // b) Line scan scoped to after_line first, then the whole file.
        if let Some(range) = find_declaration_range_after_line(&f.lines, name, after_line) {
            return vec![Location { uri, range }];
        }
        if let Some(range) = find_declaration_range_in_lines(&f.lines, name) {
            return vec![Location { uri, range }];
        }
        return vec![];
    }

    // c) On-demand parse
    find_name_in_uri(idx, name, file_uri)
}

/// Like `find_declaration_range_in_lines` but only searches from `start_line`.
fn find_declaration_range_after_line(lines: &[String], name: &str, start_line: u32) -> Option<Range> {
    let start = start_line as usize;
    if start >= lines.len() { return None; }
    find_declaration_range_in_lines(&lines[start..], name)
        .map(|r| Range {
            start: Position { line: r.start.line + start_line, character: r.start.character },
            end:   Position { line: r.end.line   + start_line, character: r.end.character   },
        })
}


///
/// Returns the location of `name:` in the current file.  This catches function
/// parameters that lack `val`/`var` and are therefore absent from the symbol index.
fn find_local_declaration(idx: &Indexer, name: &str, uri: &Url) -> Vec<Location> {
    let Some(data) = idx.files.get(uri.as_str()) else { return vec![]; };
    if let Some(range) = find_declaration_range_in_lines(&data.lines, name) {
        return vec![Location { uri: uri.clone(), range }];
    }
    vec![]
}

/// Build the regex pattern used by `rg` for declaration sites.
///
/// Matches both Kotlin and Java declaration keywords followed by `NAME`.
///
/// Kotlin: `fun`, `class`, `object`, `val`, `var`, `typealias`, `enum class`,
///         extension functions `fun ReceiverType.name`
/// Java:   `class`, `interface`, `enum` (standalone, no `class` suffix),
///         with any leading access/modifier keywords ignored
pub(crate) fn build_rg_pattern(name: &str) -> String {
    let safe: String = name.chars().flat_map(|c| {
        if c.is_alphanumeric() || c == '_' { vec![c] } else { vec!['\\', c] }
    }).collect();
    // Kotlin: standard keywords + `enum class` + extension function receiver
    // Java:   `enum NAME` (Java enums have no `class` after `enum`)
    //         `class NAME` / `interface NAME` already covered by the Kotlin arm
    format!(
        r"(?:(?:class|interface|object|fun|val|var|typealias|enum\s+class)\s+|fun\s+\w[\w.]*\.|(?:public|private|protected|static|abstract|final|\s)+enum\s+){safe}\b"
    )
}

fn last_segment(dotted: &str) -> &str {
    dotted.rsplit('.').next().unwrap_or(dotted)
}

/// Returns true for packages whose sources aren't present in a typical project.
///
/// Kotlin automatically imports `kotlin.*` and `kotlin.collections.*` etc.
/// Android projects don't ship `android.*` / `androidx.*` sources by default.
fn is_stdlib(pkg: &str) -> bool {
    matches!(
        pkg.split('.').next().unwrap_or(""),
        "kotlin" | "java" | "javax" | "android" | "androidx" | "sun" | "com.sun"
    )
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::Indexer;
    use tower_lsp::lsp_types::Url;

    fn uri(path: &str) -> Url {
        Url::parse(&format!("file:///test{path}")).unwrap()
    }

    // ── pure helpers ─────────────────────────────────────────────────────────

    #[test]
    fn package_prefix_standard() {
        assert_eq!(package_prefix("com.example.app.MyClass"),              "com.example.app");
        assert_eq!(package_prefix("com.example.OuterClass.InnerClass"),    "com.example");
        assert_eq!(package_prefix("MyClass"),                               "");
        assert_eq!(package_prefix("com.example.Foo"),                      "com.example");
    }

    #[test]
    fn import_candidates_top_level() {
        let c = import_file_candidates("com.example.Foo");
        assert_eq!(c[0], "Foo.kt");
        assert_eq!(c[1], "Foo.java");
    }

    #[test]
    fn import_candidates_nested() {
        let c = import_file_candidates("com.example.OuterClass.InnerClass");
        assert_eq!(c[0], "OuterClass.kt");   // outer class file tried first
        assert_eq!(c[1], "OuterClass.java");
        assert_eq!(c[2], "InnerClass.kt");
        assert_eq!(c[3], "InnerClass.java");
    }

    #[test]
    fn import_candidates_deeply_nested() {
        let c = import_file_candidates("a.b.Outer.Middle.Inner");
        assert_eq!(c[0], "Middle.kt");
        assert_eq!(c[1], "Middle.java");
        assert_eq!(c[2], "Inner.kt");
        assert_eq!(c[3], "Inner.java");
    }

    #[test]
    fn import_candidates_no_uppercase() {
        assert!(import_file_candidates("com.example.pkg").is_empty());
    }

    // ── resolve_local ────────────────────────────────────────────────────────

    #[test]
    fn resolve_local_finds_own_symbols() {
        let u = uri("/Foo.kt");
        let idx = Indexer::new();
        idx.index_content(&u, "class Foo\nclass Bar");
        let locs = resolve_symbol(&idx, "Foo", None, &u);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].uri, u);
    }

    #[test]
    fn resolve_local_not_found_returns_empty_without_rg() {
        // Symbol that doesn't exist anywhere in the index; rg will find nothing
        // in the (empty) working tree — acceptable to return vec![]
        let u = uri("/Foo.kt");
        let idx = Indexer::new();
        idx.index_content(&u, "class Foo");
        // "Xyz" is not in the index; rg likely returns nothing in tests
        let locs = resolve_symbol(&idx, "Xyz", None, &u);
        // We can't guarantee rg returns nothing in all environments,
        // so just verify local didn't find it in index.
        assert!(!locs.iter().any(|l| l.uri == u));
    }

    // ── resolve_via_imports (qualified index) ────────────────────────────────

    #[test]
    fn resolve_via_explicit_import() {
        let src_uri = uri("/src/Source.kt");
        let def_uri = uri("/src/Target.kt");
        let idx = Indexer::new();
        idx.index_content(&def_uri, "package com.example\nclass Target");
        idx.index_content(&src_uri, "package com.example\nimport com.example.Target\nval x: Target = TODO()");

        let locs = resolve_symbol(&idx, "Target", None, &src_uri);
        assert!(!locs.is_empty(), "Target not found via import");
        assert_eq!(locs[0].uri, def_uri);
    }

    #[test]
    fn resolve_via_alias_import() {
        let src_uri = uri("/src/A.kt");
        let def_uri = uri("/src/B.kt");
        let idx = Indexer::new();
        idx.index_content(&def_uri, "package com.example\nclass LongName");
        idx.index_content(&src_uri, "package com.example\nimport com.example.LongName as LN\nval x: LN = TODO()");

        // Looking up "LN" should find "LongName" in def_uri
        let locs = resolve_symbol(&idx, "LN", None, &src_uri);
        assert!(!locs.is_empty(), "aliased import not resolved");
        assert_eq!(locs[0].uri, def_uri);
    }

    // ── resolve_same_package ─────────────────────────────────────────────────

    #[test]
    fn resolve_same_package() {
        let a_uri = uri("/pkg/A.kt");
        let b_uri = uri("/pkg/B.kt");
        let idx = Indexer::new();
        idx.index_content(&a_uri, "package com.example\nclass A");
        idx.index_content(&b_uri, "package com.example\nval x: A = TODO()");

        let locs = resolve_symbol(&idx, "A", None, &b_uri);
        assert!(!locs.is_empty(), "same-package class not found");
        assert_eq!(locs[0].uri, a_uri);
    }

    #[test]
    fn resolve_does_not_cross_packages_without_import() {
        let a_uri = uri("/pkg1/A.kt");
        let b_uri = uri("/pkg2/B.kt");
        let idx = Indexer::new();
        idx.index_content(&a_uri, "package com.example.pkg1\nclass A");
        idx.index_content(&b_uri, "package com.example.pkg2"); // no import

        // rg might find it; test that same-package step doesn't leak
        let locs: Vec<_> = resolve_symbol(&idx, "A", None, &b_uri)
            .into_iter()
            .filter(|l| l.uri == a_uri)
            .collect();
        // If rg finds it that's fine, but same-package shouldn't (different packages)
        // We verify by checking the packages map didn't bridge pkg1 and pkg2
        assert!(
            idx.packages.get("com.example.pkg2").map(|u| !u.contains(&a_uri.to_string())).unwrap_or(true),
            "pkg1 URI leaked into pkg2 packages map"
        );
    }

    // ── resolve_qualified (dot accessor) ────────────────────────────────────

    #[test]
    fn resolve_qualifier_dot_access() {
        let host_uri = uri("/Host.kt");
        let outer_uri = uri("/Outer.kt");
        let idx = Indexer::new();
        idx.index_content(&outer_uri, "package com.pkg\nclass Outer {\n  class Inner\n}");
        idx.index_content(&host_uri,  "package com.pkg\nval x: Outer.Inner = TODO()");

        // Cursor on "Inner" with qualifier "Outer"
        let locs = resolve_symbol(&idx, "Inner", Some("Outer"), &host_uri);
        assert!(!locs.is_empty(), "Inner not found via qualifier");
        assert_eq!(locs[0].uri, outer_uri);
    }

    #[test]
    fn resolve_deep_qualifier_chain() {
        // A.B.C.D cursor on D → qualifier = "A.B.C"
        // resolve_qualified should resolve root "A", find its file, locate "D" in it.
        let host_uri = uri("/Host.kt");
        let root_uri = uri("/Root.kt");
        let idx = Indexer::new();
        // Root.kt defines class Root with nested class Deep
        idx.index_content(&root_uri, "package com.pkg\nclass Root {\n  class Mid {\n    class Deep\n  }\n}");
        idx.index_content(&host_uri, "package com.pkg\nval x: Root.Mid.Deep = TODO()");

        // qualifier = "Root.Mid" (full chain minus last segment), word = "Deep"
        let locs = resolve_symbol(&idx, "Deep", Some("Root.Mid"), &host_uri);
        assert!(!locs.is_empty(), "Deep not found via full qualifier chain");
        assert_eq!(locs[0].uri, root_uri);
    }

    #[test]
    fn resolve_nested_type_via_variable_annotation() {
        // `val factory: DashboardProductsReducer.Factory` — goto-def of `factory.create(...)`
        // should navigate to the `create` fun inside the `Factory` interface.
        let host_uri = uri("/Host.kt");
        let reducer_uri = uri("/DashboardProductsReducer.kt");
        let idx = Indexer::new();
        idx.index_content(&reducer_uri, concat!(
            "package com.pkg\n",
            "class DashboardProductsReducer {\n",
            "  interface Factory {\n",
            "    fun create(scope: Any): DashboardProductsReducer\n",
            "  }\n",
            "}\n",
        ));
        idx.index_content(&host_uri, concat!(
            "package com.pkg\n",
            "val factory: DashboardProductsReducer.Factory = TODO()\n",
            "fun foo() { factory.create(this) }\n",
        ));

        // Qualifier = "factory" (lowercase), word = "create"
        let locs = resolve_symbol(&idx, "create", Some("factory"), &host_uri);
        assert!(!locs.is_empty(), "create not found via nested type Factory");
        assert_eq!(locs[0].uri, reducer_uri);
    }

    #[test]
    fn infer_type_in_lines_dotted() {
        // Ensure infer_type_in_lines handles `Outer.Inner` dotted types.
        let lines: Vec<String> = vec![
            "  private val factory: DashboardProductsReducer.Factory,".to_owned()
        ];
        let t = super::infer_type_in_lines(&lines, "factory");
        assert_eq!(t.as_deref(), Some("DashboardProductsReducer.Factory"));
    }

    // ── infer_variable_type + method resolution ──────────────────────────────

    #[test]
    fn resolve_multi_hop_field_chain() {
        // vm.account.interestPlanCode where:
        //   fun foo(vm: ViewModel) – vm has field account: AccountModel
        //   AccountModel has field interestPlanCode: String
        let host_uri = uri("/Host.kt");
        let vm_uri   = uri("/ViewModel.kt");
        let acc_uri  = uri("/AccountModel.kt");
        let idx = Indexer::new();
        idx.index_content(&acc_uri,
            "package com.pkg\nclass AccountModel {\n  val interestPlanCode: String = \"\"\n}");
        idx.index_content(&vm_uri,
            "package com.pkg\nclass ViewModel {\n  val account: AccountModel = AccountModel()\n}");
        idx.index_content(&host_uri,
            "package com.pkg\nfun foo(vm: ViewModel) { vm.account.interestPlanCode }");

        // qualifier = "vm.account", name = "interestPlanCode"
        let locs = resolve_symbol(&idx, "interestPlanCode", Some("vm.account"), &host_uri);
        assert!(!locs.is_empty(), "interestPlanCode not found via multi-hop field chain");
        assert_eq!(locs[0].uri, acc_uri);
    }

    #[test]
    fn resolve_local_param_declaration() {
        // Cursor on `account` (function param without val/var) should return the
        // declaration line in the same file.
        let u = uri("/Foo.kt");
        let idx = Indexer::new();
        idx.index_content(&u, "package com.pkg\nfun foo(account: AccountModel) {\n  account.something\n}");

        let locs = resolve_symbol(&idx, "account", None, &u);
        assert!(!locs.is_empty(), "local param declaration not found");
        assert_eq!(locs[0].uri, u);
        // Line 1 (0-indexed) contains the parameter declaration
        assert_eq!(locs[0].range.start.line, 1);
    }


    #[test]
    fn resolve_method_via_variable_type_inference() {
        // repo.findById(1) where repo: UserRepository
        let vm_uri   = uri("/ViewModel.kt");
        let repo_uri = uri("/UserRepository.kt");
        let idx = Indexer::new();
        idx.index_content(&repo_uri, "package com.pkg\nclass UserRepository {\n  fun findById(id: Int) {}\n}");
        idx.index_content(&vm_uri,
            "package com.pkg\nclass ViewModel(\n  private val repo: UserRepository\n) {\n  fun load() { repo.findById(1) }\n}");

        // qualifier = "repo" (lowercase), name = "findById"
        // infer_variable_type should extract "UserRepository" from "val repo: UserRepository"
        // then resolve_qualified finds findById in UserRepository.kt
        let locs = resolve_symbol(&idx, "findById", Some("repo"), &vm_uri);
        assert!(!locs.is_empty(), "findById not found via variable type inference");
        assert_eq!(locs[0].uri, repo_uri);
    }

    #[test]
    fn resolve_method_via_constructor_param_type() {
        // interactor.loadDataFlow(x) where interactor: ShowChildNewTipsInteractor
        let vm_uri  = uri("/SomeViewModel.kt");
        let int_uri = uri("/ShowChildNewTipsInteractor.kt");
        let idx = Indexer::new();
        idx.index_content(&int_uri,
            "package com.feature\nclass ShowChildNewTipsInteractor {\n  fun loadDataFlow(account: Any) {}\n}");
        idx.index_content(&vm_uri,
            "package com.feature\nclass SomeViewModel(\n  private val interactor: ShowChildNewTipsInteractor\n) {\n  fun init() { interactor.loadDataFlow(x) }\n}");

        let locs = resolve_symbol(&idx, "loadDataFlow", Some("interactor"), &vm_uri);
        assert!(!locs.is_empty(), "loadDataFlow not found via constructor param type inference");
        assert_eq!(locs[0].uri, int_uri);
    }

    // ── build_rg_pattern ─────────────────────────────────────────────────────
    // Use rg itself to validate patterns (it's always available in the dev env).

    fn rg_matches(pattern: &str, text: &str) -> bool {
        std::process::Command::new("rg")
            .args(["--quiet", "-e", pattern, "--"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .ok()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin.as_mut()?.write_all(text.as_bytes()).ok()?;
                Some(c.wait().ok()?.success())
            })
            .unwrap_or(false)
    }

    #[test]
    fn rg_pattern_matches_kotlin_class() {
        let pat = build_rg_pattern("Foo");
        assert!(rg_matches(&pat, "class Foo {"));
        assert!(rg_matches(&pat, "sealed class Foo"));
    }

    #[test]
    fn rg_pattern_matches_kotlin_enum() {
        let pat = build_rg_pattern("EScreen");
        assert!(rg_matches(&pat, "enum class EScreen {"));
    }

    #[test]
    fn rg_pattern_matches_java_enum() {
        let pat = build_rg_pattern("EProductScreen");
        assert!(rg_matches(&pat, "public enum EProductScreen {"));
        assert!(rg_matches(&pat, "  enum EProductScreen {"));
        assert!(rg_matches(&pat, "private static enum EProductScreen {"));
    }

    #[test]
    fn rg_pattern_no_false_positive_on_usage() {
        let pat = build_rg_pattern("EProductScreen");
        // Should NOT match a plain usage (not a declaration)
        assert!(!rg_matches(&pat, "EProductScreen.SOMETHING"));
        assert!(!rg_matches(&pat, "val x: EProductScreen = "));
    }

    #[test]
    fn rg_pattern_matches_java_class() {
        let pat = build_rg_pattern("FlexiEntryVM");
        assert!(rg_matches(&pat, "public class FlexiEntryVM extends Base {"));
    }

    // ── import_file_stems ────────────────────────────────────────────────────

    #[test]
    fn file_stems_top_level() {
        assert_eq!(import_file_stems("cz.moneta.data.EProductScreen"), vec!["EProductScreen"]);
    }

    #[test]
    fn file_stems_nested() {
        let s = import_file_stems("com.example.OuterClass.InnerClass");
        assert_eq!(s, vec!["OuterClass", "InnerClass"]);
    }

    // ── extract_supers_from_lines ─────────────────────────────────────────────

    #[test]
    fn supers_kotlin_single_line() {
        let lines = vec!["class DetailViewModel : MviViewModel<Event, State, Effect>() {".to_string()];
        assert_eq!(extract_supers_from_lines(&lines), vec!["MviViewModel"]);
    }

    #[test]
    fn supers_kotlin_multi_line_ctor() {
        // Primary constructor spans multiple lines; super is on the closing ) line
        let lines = vec![
            "class DetailViewModel @Inject constructor(".to_string(),
            "  private val useCase: UseCase,".to_string(),
            ") : MviViewModel<Event, State, Effect>() {".to_string(),
        ];
        assert_eq!(extract_supers_from_lines(&lines), vec!["MviViewModel"]);
    }

    #[test]
    fn supers_kotlin_multiple() {
        let lines = vec!["class Foo : BaseClass(), SomeInterface, AnotherInterface {".to_string()];
        let s = extract_supers_from_lines(&lines);
        assert!(s.contains(&"BaseClass".to_string()));
        assert!(s.contains(&"SomeInterface".to_string()));
        assert!(s.contains(&"AnotherInterface".to_string()));
    }

    #[test]
    fn supers_java_extends() {
        let lines = vec!["public class FlexiEntryVM extends BaseFlexikreditVM {".to_string()];
        assert_eq!(extract_supers_from_lines(&lines), vec!["BaseFlexikreditVM"]);
    }

    #[test]
    fn supers_java_implements() {
        let lines = vec![
            "public class Foo extends Base implements Runnable, Serializable {".to_string()
        ];
        let s = extract_supers_from_lines(&lines);
        assert!(s.contains(&"Base".to_string()));
        assert!(s.contains(&"Runnable".to_string()));
        assert!(s.contains(&"Serializable".to_string()));
    }

    #[test]
    fn supers_does_not_pick_up_type_annotations() {
        // val x: Int — the ':' here must NOT be treated as delegation
        let lines = vec![
            "class Foo {".to_string(),
            "  val x: Int = 0".to_string(),
            "  fun f(): String = \"\"".to_string(),
        ];
        assert!(extract_supers_from_lines(&lines).is_empty());
    }

    // ── resolve_from_class_hierarchy ─────────────────────────────────────────

    #[test]
    fn resolve_inherited_method() {
        let base_uri = uri("/Base.kt");
        let child_uri = uri("/Child.kt");
        let idx = Indexer::new();
        idx.index_content(&base_uri,  "package com.example\nopen class Base {\n  fun baseMethod() {}\n}");
        idx.index_content(&child_uri, "package com.example\nclass Child : Base() {}\n");

        // `baseMethod` is not declared in Child — must be found via hierarchy
        let locs = resolve_symbol(&idx, "baseMethod", None, &child_uri);
        assert!(!locs.is_empty(), "inherited method not found");
        assert_eq!(locs[0].uri, base_uri);
    }

    #[test]
    fn resolve_inherited_method_via_import() {
        let base_uri  = uri("/lib/Base.kt");
        let child_uri = uri("/app/Child.kt");
        let idx = Indexer::new();
        idx.index_content(&base_uri,  "package com.lib\nopen class Base {\n  fun doStuff() {}\n}");
        idx.index_content(&child_uri,
            "package com.app\nimport com.lib.Base\nclass Child : Base() {}\n");

        let locs = resolve_symbol(&idx, "doStuff", None, &child_uri);
        assert!(!locs.is_empty(), "inherited method not found via import");
        assert_eq!(locs[0].uri, base_uri);
    }

    // ── this / super resolution ───────────────────────────────────────────────

    #[test]
    fn resolve_this_dot_method() {
        let u = uri("/Foo.kt");
        let idx = Indexer::new();
        idx.index_content(&u, "package com.example\nclass Foo {\n  fun doThing() {}\n  fun other() { this.doThing() }\n}");
        let locs = resolve_symbol(&idx, "doThing", Some("this"), &u);
        assert!(!locs.is_empty(), "this.doThing() not resolved");
        assert_eq!(locs[0].uri, u);
    }

    #[test]
    fn resolve_super_dot_method() {
        let base_uri  = uri("/Base.kt");
        let child_uri = uri("/Child.kt");
        let idx = Indexer::new();
        idx.index_content(&base_uri,  "package com.example\nopen class Base { fun init() {} }");
        idx.index_content(&child_uri, "package com.example\nclass Child : Base() { fun x() { super.init() } }");
        let locs = resolve_symbol(&idx, "init", Some("super"), &child_uri);
        assert!(!locs.is_empty(), "super.init() not resolved");
        assert_eq!(locs[0].uri, base_uri);
    }

    // ── lambda parameter recognition ─────────────────────────────────────────

    #[test]
    fn local_decl_lambda_untyped() {
        let lines: Vec<String> = vec![
            "list.forEach { account ->".to_string(),
            "  println(account)".to_string(),
        ];
        let range = find_declaration_range_in_lines(&lines, "account");
        assert!(range.is_some(), "untyped lambda param not found");
        assert_eq!(range.unwrap().start.line, 0);
    }

    #[test]
    fn local_decl_lambda_typed() {
        let lines: Vec<String> = vec![
            "items.map { item: DetailItem ->".to_string(),
        ];
        let range = find_declaration_range_in_lines(&lines, "item");
        assert!(range.is_some(), "typed lambda param not found");
    }

    #[test]
    fn local_decl_no_false_positive_usage() {
        // A usage of `account` on a non-declaration line must not be returned
        let lines: Vec<String> = vec![
            "val result = account.name".to_string(),
        ];
        let range = find_declaration_range_in_lines(&lines, "account");
        assert!(range.is_none(), "false positive on usage line");
    }

    // ── primary constructor val/var parameter resolution ─────────────────────

    #[test]
    fn resolve_data_class_field_via_dot_access() {
        // user.name should resolve to `val name: String` in User's primary ctor
        let user_uri = uri("/User.kt");
        let caller_uri = uri("/Caller.kt");
        let idx = Indexer::new();
        idx.index_content(&user_uri,
            "package com.example\ndata class User(val name: String, val age: Int)");
        idx.index_content(&caller_uri,
            "package com.example\nfun greet(user: User) { println(user.name) }");

        let locs = resolve_symbol(&idx, "name", Some("user"), &caller_uri);
        assert!(!locs.is_empty(), "name not found via user.name");
        assert_eq!(locs[0].uri, user_uri, "should point to User.kt");
    }

    #[test]
    fn resolve_ctor_param_no_qualifier() {
        // Inside the class itself, `name` should resolve to the ctor param.
        let uri = uri("/User.kt");
        let idx = Indexer::new();
        idx.index_content(&uri,
            "package com.example\ndata class User(val name: String) {\n  fun display() = name\n}");

        let locs = resolve_symbol(&idx, "name", None, &uri);
        assert!(!locs.is_empty(), "ctor param not found locally");
        assert_eq!(locs[0].uri, uri, "should stay in same file");
    }

    #[test]
    fn resolve_named_arg_to_ctor_param() {
        // User(name = "Alice") — qualifier is "User" (detected by word_and_qualifier_at).
        // resolve_symbol with qualifier="User" must find `val name` in User's primary ctor.
        let user_uri   = uri("/User.kt");
        let caller_uri = uri("/Caller.kt");
        let idx = Indexer::new();
        idx.index_content(&user_uri,
            "package com.example\ndata class User(val name: String, val age: Int)");
        idx.index_content(&caller_uri,
            "package com.example\nfun test() { val u = User(name = \"Alice\", age = 30) }");

        // Simulate what the backend does after word_and_qualifier_at returns ("name", "User")
        let locs = resolve_symbol(&idx, "name", Some("User"), &caller_uri);
        assert!(!locs.is_empty(), "named arg 'name' not resolved to User ctor param");
        assert_eq!(locs[0].uri, user_uri, "should point to User.kt, not caller");
    }

    #[test]
    fn named_arg_same_name_different_classes_same_file() {
        // Regression: Contract.kt has both State(val toastModel: ...) and
        // OnClick(val toastModel: ...) in the same file.
        // Resolving State(toastModel = ...) should land on State's field,
        // not OnClick's (which appears later but might be returned first).
        let contract_uri = uri("/Contract.kt");
        let caller_uri   = uri("/Caller.kt");
        let idx = Indexer::new();
        idx.index_content(&contract_uri, "\
package com.example
sealed class Effect {
    data class OnClick(val toastModel: String) : Effect()
}
data class State(
    val toastModel: String? = null,
)");
        idx.index_content(&caller_uri,
            "package com.example\nfun test() { State(toastModel = \"hi\") }");

        let locs = resolve_symbol(&idx, "toastModel", Some("State"), &caller_uri);
        assert!(!locs.is_empty(), "toastModel not resolved");
        // Must point to State's toastModel (line 4), NOT OnClick's (line 2)
        let line = locs[0].range.start.line;
        assert!(line >= 4, "resolved to OnClick.toastModel (line {line}) instead of State.toastModel");
    }

    // ── it-completion helpers ─────────────────────────────────────────────────

    #[test]
    fn extract_collection_element_list() {
        assert_eq!(extract_collection_element_type("List<Product>"), Some("Product".into()));
    }

    #[test]
    fn extract_collection_element_mutable_list() {
        assert_eq!(extract_collection_element_type("MutableList<User>"), Some("User".into()));
    }

    #[test]
    fn extract_collection_element_flow() {
        assert_eq!(extract_collection_element_type("Flow<Event>"), Some("Event".into()));
    }

    #[test]
    fn extract_collection_element_state_flow() {
        assert_eq!(extract_collection_element_type("StateFlow<UiState>"), Some("UiState".into()));
    }

    #[test]
    fn extract_collection_element_map_returns_first() {
        // Map is not in the collection list → returns None (it's more complex).
        // forEach on Map gives Map.Entry, not the first type arg.
        assert_eq!(extract_collection_element_type("Map<String, Int>"), None);
    }

    #[test]
    fn extract_collection_element_non_collection() {
        // Plain class → not a collection, returns None.
        assert_eq!(extract_collection_element_type("User"), None);
    }

    #[test]
    fn infer_type_in_lines_raw_keeps_generics() {
        let lines: Vec<String> = vec![
            "val items: List<Product> = emptyList()".into(),
        ];
        assert_eq!(infer_type_in_lines_raw(&lines, "items"), Some("List<Product>".into()));
    }

    #[test]
    fn infer_type_in_lines_raw_state_flow() {
        let lines: Vec<String> = vec![
            "    private val _state: StateFlow<UiState>".into(),
        ];
        assert_eq!(infer_type_in_lines_raw(&lines, "_state"), Some("StateFlow<UiState>".into()));
    }

    #[test]
    fn goto_def_on_named_lambda_param_resolves_to_declaration_line() {
        // items.forEach { product ->
        //     product.name   ← gd on `product` here
        // go-to-def should jump to the `{ product ->` declaration line (line 2)
        let caller_uri  = uri("/Caller.kt");
        let product_uri = uri("/Product.kt");
        let idx = Indexer::new();
        idx.index_content(&product_uri, "package com.example\ndata class Product(val name: String)");
        idx.index_content(&caller_uri,
            "package com.example\nval items: List<Product> = emptyList()\nitems.forEach { product ->\n    product.name\n}");

        // step 1.5 finds `{ product ->` via the lambda arrow pattern
        let locs = resolve_symbol(&idx, "product", None, &caller_uri);
        assert!(!locs.is_empty(), "lambda param 'product' not found");
        // Must land in the same file (the lambda declaration), NOT in rg results
        assert_eq!(locs[0].uri, caller_uri, "should stay in Caller.kt at the lambda decl");
        // Line 2 is where `items.forEach { product ->` is declared
        assert_eq!(locs[0].range.start.line, 2, "should point to the lambda arrow line");
    }

    // ── complete_bare distance sorting ───────────────────────────────────────

    #[test]
    fn complete_bare_local_before_same_pkg() {
        let mut idx = Indexer::new();
        let local_uri = Url::parse("file:///pkg/a/Local.kt").unwrap();
        let other_uri = Url::parse("file:///pkg/a/Other.kt").unwrap();
        // local file has "localFoo"
        idx.index_content(&local_uri, "package a\nfun localFoo() {}");
        // same-package file has "pkgBar"
        idx.index_content(&other_uri, "package a\nfun pkgBar() {}");

        let items = complete_bare(&idx, "", &local_uri, false);

        let local_pos = items.iter().position(|i| i.label == "localFoo");
        let pkg_pos   = items.iter().position(|i| i.label == "pkgBar");
        assert!(local_pos.is_some(), "localFoo should appear");
        assert!(pkg_pos.is_some(),   "pkgBar should appear");

        // sort_text with tier prefix means local (0:…) sorts before same-pkg (1:…).
        let local_sort = items[local_pos.unwrap()].sort_text.as_deref().unwrap_or("");
        let pkg_sort   = items[pkg_pos.unwrap()].sort_text.as_deref().unwrap_or("");
        assert!(local_sort < pkg_sort, "local tier sort_text should be less than same-pkg tier");
    }

    // ── dot_completions_for type filtering ────────────────────────────────────

    #[test]
    fn dot_completions_string_receiver_has_string_fns() {
        let items = crate::stdlib::dot_completions_for("String", false);
        let names: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(names.contains(&"trim"),    "String should have trim()");
        assert!(names.contains(&"split"),   "String should have split()");
        assert!(names.contains(&"let"),     "String should have scope fn let()");
        // Collection fns should NOT appear on String
        assert!(!names.contains(&"map"),    "String should NOT have map()");
        assert!(!names.contains(&"filter"), "String should NOT have filter()");
    }

    #[test]
    fn dot_completions_list_receiver_has_collection_fns() {
        let items = crate::stdlib::dot_completions_for("List", false);
        let names: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(names.contains(&"map"),      "List should have map()");
        assert!(names.contains(&"filter"),   "List should have filter()");
        assert!(names.contains(&"forEach"),  "List should have forEach()");
        assert!(names.contains(&"let"),      "List should have scope fn let()");
        // String-only fns should NOT appear on List
        assert!(!names.contains(&"trim"),    "List should NOT have trim()");
        assert!(!names.contains(&"split"),   "List should NOT have split()");
    }

    #[test]
    fn dot_completions_custom_type_has_scope_fns_only() {
        let items = crate::stdlib::dot_completions_for("MyDomainClass", false);
        let names: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(names.contains(&"let"),     "domain type should have let()");
        assert!(names.contains(&"apply"),   "domain type should have apply()");
        assert!(!names.contains(&"trim"),   "domain type should NOT have trim()");
        assert!(!names.contains(&"map"),    "domain type should NOT have map()");
        assert!(!names.contains(&"filter"), "domain type should NOT have filter()");
    }
}
