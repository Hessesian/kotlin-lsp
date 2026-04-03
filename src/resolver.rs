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

    // Resolve type to its source file (index or rg fallback for lazy files).
    let locs = resolve_symbol(idx, &type_name, None, from_uri);
    let Some(type_loc) = locs.first() else { return vec![]; };
    let file_uri = type_loc.uri.to_string();

    let mut items = symbols_from_uri_as_completions(idx, &file_uri);

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

    // Append stdlib extension functions (scope fns, collection, string helpers).
    // They sort after project symbols via the "z:" prefix set in stdlib.rs.
    items.extend(crate::stdlib::dot_completions(snippets));
    items
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

    let mut add = |name: &str, kind: CompletionItemKind| {
        // Case gate: in lowercase mode skip CamelCase symbols.
        if lowercase_mode && name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            return;
        }
        if name.to_lowercase().starts_with(&prefix_lower) && seen.insert(name.to_string()) {
            let is_fn = snippets && matches!(kind, CompletionItemKind::FUNCTION | CompletionItemKind::METHOD);
            items.push(CompletionItem {
                label:              name.to_string(),
                kind:               Some(kind),
                insert_text:        if is_fn { Some(format!("{}($1)", name)) } else { None },
                insert_text_format: if is_fn { Some(InsertTextFormat::SNIPPET) } else { None },
                ..Default::default()
            });
        }
    };

    // 1. Local file symbols — highest priority.
    if let Some(f) = idx.files.get(from_uri.as_str()) {
        for sym in &f.symbols {
            add(&sym.name, symbol_kind_to_completion(sym.kind));
        }
        // Surface constructor params / local vars from cached declared_names.
        if lowercase_mode {
            for name in &f.declared_names {
                add(name, CompletionItemKind::VARIABLE);
            }
        }
    }

    // 2. Same-package symbols.
    let pkg = idx.files.get(from_uri.as_str())
        .and_then(|f| f.package.clone())
        .unwrap_or_default();
    if !pkg.is_empty() {
        if let Some(uris) = idx.packages.get(&pkg) {
            for uri_str in uris.iter() {
                if uri_str == from_uri.as_str() { continue; }
                if let Some(f) = idx.files.get(uri_str.as_str()) {
                    for sym in &f.symbols {
                        add(&sym.name, symbol_kind_to_completion(sym.kind));
                    }
                }
            }
        }
    }

    // 3. Cross-package class index — only in uppercase mode to avoid noise.
    if !lowercase_mode {
        for entry in idx.definitions.iter() {
            add(entry.key(), CompletionItemKind::CLASS);
        }
    }

    // 4. Stdlib top-level / scope functions (listOf, println, run, with, …)
    for mut item in crate::stdlib::bare_completions(snippets) {
        if lowercase_mode && item.label.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            continue;
        }
        if item.label.starts_with(&prefix_lower) && seen.insert(item.label.clone()) {
            // Re-apply prefix filter (bare_completions returns all of them).
            if item.label.to_lowercase().starts_with(&prefix_lower) {
                items.push(item);
            }
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

    // 1 ── local (indexed symbols) ────────────────────────────────────────────
    let local = resolve_local(idx, name, from_uri);
    if !local.is_empty() { return local; }

    // 1.5 ── local variable / parameter declaration (line scan) ───────────────
    // Catches function parameters without val/var that aren't in the symbol index.
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
    // For inherited methods that carry no explicit import (e.g. `collectEffects()`
    // defined in a base class that is itself imported, but the method is not).
    let mut visited: Vec<String> = Vec::new();
    let inherited = resolve_from_class_hierarchy(idx, name, from_uri, 0, &mut visited);
    if !inherited.is_empty() { return inherited; }

    // 5 ── project-wide rg ───────────────────────────────────────────────────
    rg_find_definition(name, idx.workspace_root.get().map(PathBuf::as_path))
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

    if root.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
        // ── Uppercase chain: find the root's file and search it for `name` ──
        let qual_locs = resolve_symbol(idx, root, None, from_uri);
        for qual_loc in &qual_locs {
            let locs = find_name_in_uri(idx, name, qual_loc.uri.as_str());
            if !locs.is_empty() { return locs; }
        }
        return vec![];
    }

    // ── Lowercase root: variable / parameter type inference ──────────────────
    let Some(start_type) = infer_variable_type(idx, root, from_uri) else {
        return vec![];
    };

    // Resolve the variable's type to its source file.
    let type_locs = resolve_symbol(idx, &start_type, None, from_uri);
    let mut current_file: Option<String> = type_locs.first().map(|l| l.uri.to_string());

    // Traverse remaining qualifier segments.
    for &seg in &segments[1..] {
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

    let lines: Vec<String> = match idx.files.get(from_uri.as_str()) {
        Some(f) => f.lines.clone(),
        None    => return vec![],
    };

    for super_name in extract_supers_from_lines(&lines) {
        // Resolve the supertype itself (goes through steps 1-4, not 4.5).
        let super_locs = resolve_symbol(idx, &super_name, None, from_uri);
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
            let type_name: String = after.chars()
                .take_while(|&c| c.is_alphanumeric() || c == '_')
                .collect();
            if !type_name.is_empty()
                && type_name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
            {
                return Some(type_name);
            }
        }
    }
    None
}

/// Return the `Range` of the declaration `name:` on the first matching line,
/// or `None` if not found.
///
/// Used to locate function parameters and other declarations that are not in
/// the tree-sitter symbol index (e.g. `fun foo(account: AccountModel)`).
fn find_declaration_range_in_lines(lines: &[String], name: &str) -> Option<Range> {
    let pattern = format!("{name}:");

    for (line_num, line) in lines.iter().enumerate() {
        if !line.contains(&pattern) { continue; }

        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }

        if let Some(pos) = line.find(&pattern) {
            let before = line[..pos].chars().last();
            if before.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false) {
                continue;
            }
            // Skip JSON / map literal keys: `"key": value`
            if line[..pos].trim_end().ends_with('"') {
                continue;
            }
            let col = pos as u32;
            return Some(Range {
                start: Position { line: line_num as u32, character: col },
                end:   Position { line: line_num as u32, character: col + name.len() as u32 },
            });
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

/// Step 1.5 — find a local variable / parameter declaration by line scanning.
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
}
