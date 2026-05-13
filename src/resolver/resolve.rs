//! Symbol resolution for Kotlin (and Java) with a prioritised fallback chain.
//!
//! Resolution order
//! ────────────────
//! 1. **Local file**        — symbols defined in the same file (highest priority).
//! 2. **Explicit imports**  — `import com.example.Foo` or `import com.example.Foo as F`.
//!    Tries the `qualified` index first, then the short-name index.
//! 3. **Same package**      — all symbols in files that share the same `package` declaration
//!    are visible without imports in Kotlin.
//! 4. **Star imports**      — `import com.example.*`  checks indexed files in that package,
//!    then falls back to an `rg` search scoped to the package dir.
//! 5. **Extension functions** — `fun Receiver.name(...)` is stored as a top-level symbol
//!    named `name`; steps 1–4 already pick these up. No special
//!    handling needed beyond noting that receiver type is ignored.
//! 6. **Project-wide `rg`** — pattern `(fun|class|…)\s+NAME\b` across *.kt / *.java.
//!    Last resort; always finds stdlib-shadowing project symbols.
//!
//! Stdlib packages (`kotlin.*`, `java.*`, `android.*`, `androidx.*`) are skipped because
//! their sources aren't present in the project tree.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use tower_lsp::lsp_types::{Location, Url};

use crate::indexer::Indexer;
use crate::parser::parse_by_extension;
use crate::rg::{build_rg_pattern, parse_rg_line, rg_find_definition};
use crate::types::{CallerContext, FileData};
use crate::StrExt;

use super::fd::{fd_find_and_parse, package_prefix};
use super::find::{find_local_declaration, find_name_in_uri, find_name_in_uri_after_line};
use super::hierarchy::walk_hierarchy;
use super::infer::{infer_field_type, infer_variable_type};

/// Return `FileData` for `uri` — from the live index if indexed, otherwise parse from disk.
/// Returns `None` if the file is not indexed and not readable from disk.
/// Returns an `Arc` so callers can read without copying the full `FileData`.
pub(crate) fn ensure_file_data(idx: &Indexer, uri: &Url) -> Option<Arc<FileData>> {
    if let Some(file_data) = idx.files.get(uri.as_str()) {
        return Some(file_data.value().clone());
    }

    let path = uri.to_file_path().ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    Some(Arc::new(parse_by_extension(uri.path(), &content)))
}

// ─── auto-import helpers ──────────────────────────────────────────────────────

/// Return all importable FQNs for a simple symbol name (e.g. "Composable").
pub(crate) fn fqns_for_name(idx: &Indexer, name: &str) -> Vec<String> {
    idx.importable_fqns
        .read()
        .map(|m| m.get(name).cloned().unwrap_or_default())
        .unwrap_or_default()
}

/// Resolve `name` as seen from `from_uri`, returning all known definition
/// `Location`s in priority order.  Returns an empty vec only when nothing was
/// found by any strategy including `rg`.
pub(crate) fn resolve_symbol(
    idx: &Indexer,
    name: &str,
    qualifier: Option<&str>,
    from_uri: &Url,
) -> Vec<Location> {
    // 0. Qualified access: `AccountPickerMapper.Content` — cursor on `Content`.
    //    Resolve the qualifier to a file, then search that file for `name`.
    if let Some(qual) = qualifier {
        // For `super` and `this`, never fall through to the unqualified chain:
        // `super.method` must only look in the parent hierarchy, never via rg/index
        // of the current file (which would return the override).
        let is_keyword_qual = qual == "super" || qual == "this";
        let locs = resolve_qualified(idx, name, qual, from_uri);
        if !locs.is_empty() {
            return locs;
        }
        if is_keyword_qual {
            return vec![];
        }
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
            if !locs.is_empty() {
                return locs;
            }
        }
    }

    resolve_symbol_inner(idx, name, from_uri, true)
}

/// Internal resolver.  When `with_hierarchy` is false step 4.5 is skipped to
/// avoid infinite recursion inside `resolve_from_class_hierarchy` (which calls
/// this function to locate each superclass, and those files would in turn call
/// the hierarchy walk again with a fresh visited-set, looping forever).
pub(crate) fn resolve_symbol_inner(
    idx: &Indexer,
    name: &str,
    from_uri: &Url,
    with_hierarchy: bool,
) -> Vec<Location> {
    // 0.5 ── on-demand index of the current file if not yet indexed ────────────
    // Ensures resolve_local and find_local_declaration work even at cold start
    // (e.g. the user invokes gd/hover before indexing has reached this file).
    if !idx.files.contains_key(from_uri.as_str()) {
        if let Ok(path) = from_uri.to_file_path() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                idx.index_content(from_uri, &content);
            }
        }
    }

    // 1 ── local (indexed symbols) ────────────────────────────────────────────
    let local = resolve_local(idx, name, from_uri);
    if !local.is_empty() {
        return local;
    }

    // 1.5 ── local variable / parameter declaration (line scan) ───────────────
    // Catches function parameters without val/var that aren't in the symbol index.
    // Also catches named lambda parameters: `{ item -> ...}` found via the
    // `name ->` pattern in find_declaration_range_in_lines.
    if !name.starts_with_uppercase() {
        let decl = find_local_declaration(idx, name, from_uri);
        if !decl.is_empty() {
            return decl;
        }
    }

    // 2 ── explicit imports ───────────────────────────────────────────────────
    let imported = resolve_via_imports(idx, name, from_uri);
    if !imported.is_empty() {
        return imported;
    }

    // 2.5 ── Swift fast path: definitions index (no package system) ───────────
    // Swift files have no package declarations, so same-package and star-import
    // steps return empty. Use the in-memory definitions index directly to avoid
    // expensive project-wide rg fallback at step 5.
    if crate::Language::from_path(from_uri.path()) == crate::Language::Swift
        && name.starts_with_uppercase()
    {
        if let Some(locs_ref) = idx.definitions.get(name) {
            let locs: Vec<Location> = locs_ref.clone();
            // Prefer definitions from .swift files when available.
            let swift_locs: Vec<Location> = locs
                .iter()
                .filter(|l| crate::Language::from_path(l.uri.path()) == crate::Language::Swift)
                .cloned()
                .collect();
            if !swift_locs.is_empty() {
                return swift_locs;
            }
            if !locs.is_empty() {
                return locs;
            }
        }
    }

    // 3 ── same package ───────────────────────────────────────────────────────
    let same_pkg = resolve_same_package(idx, name, from_uri);
    if !same_pkg.is_empty() {
        return same_pkg;
    }

    // 4 ── star imports ───────────────────────────────────────────────────────
    let star = resolve_star_imports(idx, name, from_uri);
    if !star.is_empty() {
        return star;
    }

    // 4.5 ── superclass / interface hierarchy ─────────────────────────────────
    if with_hierarchy {
        let inherited = resolve_from_class_hierarchy(idx, name, from_uri);
        if !inherited.is_empty() {
            return inherited;
        }
    }

    // 5 ── project-wide rg ───────────────────────────────────────────────────
    let (root, matcher) = idx.rg_context();
    rg_find_definition(name, root.as_deref(), matcher.as_deref())
}

/// Returns the first Location found by scanning star-import packages.
fn find_in_star_imports(idx: &Indexer, name: &str, star_pkgs: &[String]) -> Option<Location> {
    for pkg in star_pkgs {
        if let Some(loc) = find_symbol_in_package(idx, name, pkg) {
            return Some(loc);
        }
    }
    None
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
    if !local.is_empty() {
        return local;
    }

    let imported = resolve_via_imports(idx, name, from_uri);
    if !imported.is_empty() {
        return imported;
    }

    let same_pkg = resolve_same_package(idx, name, from_uri);
    if !same_pkg.is_empty() {
        return same_pkg;
    }

    // Star imports: index-only scan (no rg fallback for unindexed files).
    let star_pkgs: Vec<String> = match idx.files.get(from_uri.as_str()) {
        Some(f) => f
            .imports
            .iter()
            .filter(|i| i.is_star && !is_stdlib(&i.full_path))
            .map(|i| i.full_path.clone())
            .collect(),
        None => vec![],
    };
    if let Some(loc) = find_in_star_imports(idx, name, &star_pkgs) {
        return vec![loc];
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
        if !locs.is_empty() {
            return locs;
        }
        return resolve_from_class_hierarchy(idx, name, from_uri);
    }

    // ── `super.member` — search superclass hierarchy only ────────────────────
    if root == "super" {
        return resolve_from_class_hierarchy(idx, name, from_uri);
    }

    if root.starts_with_uppercase() {
        // ── Uppercase chain: find the root's file and search it for `name` ──
        // Pass the qualifier's own line as a hint so that when the same field name
        // appears in multiple classes in the same file (e.g. State and Effect both
        // have `toastModel`), we pick the declaration closest *after* the qualifier
        // class definition rather than the first match in the file.
        let qual_locs = resolve_symbol(idx, root, None, from_uri);
        for qual_loc in &qual_locs {
            let after_line = qual_loc.range.start.line;
            let locs = find_name_in_uri_after_line(idx, name, qual_loc.uri.as_str(), after_line);
            if !locs.is_empty() {
                return locs;
            }
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
        None => (start_type.as_str(), None),
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
        let Some(ref uri) = current_file else {
            return vec![];
        };
        if seg.starts_with_uppercase() {
            // Nested class / companion object — likely in the same file.
            // Search current file first; fall back to a global resolve.
            let locs = find_name_in_uri(idx, seg, uri);
            current_file = if !locs.is_empty() {
                locs.first().map(|l| l.uri.to_string())
            } else {
                resolve_symbol(idx, seg, None, from_uri)
                    .first()
                    .map(|l| l.uri.to_string())
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

    // Search the resolved type's file for the target member.
    let Some(ref resolved_uri) = current_file else {
        return vec![];
    };
    let locs = find_name_in_uri(idx, name, resolved_uri);
    if !locs.is_empty() {
        return locs;
    }

    // Member not found directly — walk the superclass/interface hierarchy.
    let Ok(parsed_uri) = Url::parse(resolved_uri) else {
        return vec![];
    };
    resolve_from_class_hierarchy(idx, name, &parsed_uri)
}

/// Step 1 — symbols defined in the same source file.
fn resolve_local(idx: &Indexer, name: &str, uri: &Url) -> Vec<Location> {
    idx.files
        .get(uri.as_str())
        .map(|f| {
            f.symbols
                .iter()
                .filter(|s| s.name == name)
                .map(|s| Location {
                    uri: uri.clone(),
                    range: s.selection_range,
                })
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
        None => return vec![],
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
        let short = imp.full_path.last_segment();
        let expected_pkg = package_prefix(&imp.full_path);
        if let Some(locs) = idx.definitions.get(short) {
            let filtered: Vec<_> = locs
                .iter()
                .filter(|loc| {
                    idx.files
                        .get(loc.uri.as_str())
                        .and_then(|f| f.package.clone())
                        .map(|p| p == expected_pkg || p.starts_with(&format!("{expected_pkg}.")))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            if !filtered.is_empty() {
                return filtered;
            }
        }

        // iii) on-demand fd + parse (indexing race or file never opened).
        let (root, matcher) = idx.rg_context();
        let locs = fd_find_and_parse(name, &imp.full_path, root.as_deref(), matcher.as_deref());
        if !locs.is_empty() {
            return locs;
        }
    }
    vec![]
}

/// Step 3 — same-package visibility (no import needed in Kotlin).
///
/// Finds all indexed files sharing the same `package` declaration as `from_uri`
/// and searches their symbols.
fn resolve_same_package(idx: &Indexer, name: &str, uri: &Url) -> Vec<Location> {
    // Get package name, release the dashmap ref immediately.
    let pkg: String = match idx.files.get(uri.as_str()).and_then(|f| f.package.clone()) {
        Some(p) => p,
        None => return vec![],
    };

    let peer_uris: Vec<String> = match idx.packages.get(&pkg) {
        Some(u) => u.clone(),
        None => return vec![],
    };

    let self_str = uri.as_str();
    for peer_uri_str in peer_uris {
        if peer_uri_str == self_str {
            continue;
        }
        if let Some(f) = idx.files.get(&peer_uri_str) {
            for sym in f.symbols.iter().filter(|s| s.name == name) {
                if let Ok(u) = Url::parse(&peer_uri_str) {
                    return vec![Location {
                        uri: u,
                        range: sym.selection_range,
                    }];
                }
            }
        }
    }
    vec![]
}

/// Returns the first symbol named `name` found in the exact package `pkg`,
/// or an empty Vec if none is found.
fn symbols_in_package(idx: &Indexer, name: &str, pkg: &str) -> Vec<Location> {
    find_symbol_in_package(idx, name, pkg).map_or(vec![], |l| vec![l])
}

/// Scan all indexed files in `pkg` for the first symbol named `name`.
fn find_symbol_in_package(idx: &Indexer, name: &str, pkg: &str) -> Option<Location> {
    let peer_uris: Vec<String> = idx.packages.get(pkg).map(|u| u.clone()).unwrap_or_default();
    for peer_uri_str in peer_uris {
        if let Some(f) = idx.files.get(&peer_uri_str) {
            for sym in f.symbols.iter().filter(|s| s.name == name) {
                if let Ok(u) = Url::parse(&peer_uri_str) {
                    return Some(Location {
                        uri: u,
                        range: sym.selection_range,
                    });
                }
            }
        }
    }
    None
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
        Some(f) => f
            .imports
            .iter()
            .filter(|i| i.is_star && !is_stdlib(&i.full_path))
            .map(|i| i.full_path.clone())
            .collect(),
        None => return vec![],
    };

    for pkg in star_pkgs {
        // a) indexed files in this package
        let locs = symbols_in_package(idx, name, &pkg);
        if !locs.is_empty() {
            return locs;
        }

        // b) rg scoped to the package directory for unindexed files
        let (root, matcher) = idx.rg_context();
        let locs = rg_in_package_dir(name, &pkg, root.as_deref(), matcher.as_deref());
        if !locs.is_empty() {
            return locs;
        }
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
fn resolve_from_class_hierarchy(idx: &Indexer, name: &str, from_uri: &Url) -> Vec<Location> {
    let results = walk_hierarchy(
        idx,
        "",
        from_uri.as_str(),
        CallerContext::default(),
        4,
        |index, _, class_uri, _| find_name_in_uri(index, name, class_uri),
    );
    // Stable dedup via HashSet — diamond inheritance can produce the same location
    // via multiple paths; dedup_by only removes consecutive duplicates.
    let mut seen = HashSet::new();
    results
        .into_iter()
        .filter(|loc| {
            seen.insert((
                loc.uri.clone(),
                loc.range.start.line,
                loc.range.start.character,
            ))
        })
        .collect()
}

/// `rg` scoped to the directory that would contain `package` sources.
///
/// Package `com.example.ui` → globs `**/com/example/ui/*.{kt,java,swift}`.
/// This handles the common case where the package structure mirrors the
/// directory tree (standard Kotlin / Maven / Gradle convention).
fn rg_in_package_dir(
    name: &str,
    package: &str,
    root: Option<&Path>,
    matcher: Option<&crate::rg::IgnoreMatcher>,
) -> Vec<Location> {
    let pkg_path = package.replace('.', "/");
    let pattern = build_rg_pattern(name);

    let search_root: std::borrow::Cow<Path> = match root {
        Some(r) => std::borrow::Cow::Borrowed(r),
        None => std::borrow::Cow::Owned(std::env::current_dir().unwrap_or_default()),
    };

    let mut cmd = Command::new("rg");
    cmd.args([
        "--no-heading",
        "--with-filename",
        "--line-number",
        "--column",
    ]);
    for ext in crate::rg::SOURCE_EXTENSIONS {
        // Positive globs first — negative globs must come after to avoid being
        // overridden by later positive globs (rg: last matching glob wins).
        cmd.args(["--glob", &format!("**/{pkg_path}/*.{ext}")]);
    }
    cmd.args(["-e", &pattern]);
    cmd.arg(search_root.as_ref());

    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    let locs: Vec<Location> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_rg_line)
        .collect();
    match matcher {
        Some(m) => m.filter_locs(locs),
        None => locs,
    }
}

// ─── shared helpers ───────────────────────────────────────────────────────────

/// Returns true for packages whose sources aren't present in a typical project.
///
/// Kotlin automatically imports `kotlin.*` and `kotlin.collections.*` etc.
/// Android projects don't ship `android.*` / `androidx.*` sources by default.
/// Swift: framework imports like Foundation, UIKit, etc. have no local sources.
pub(crate) fn is_stdlib(pkg: &str) -> bool {
    // Check dotted prefixes before splitting.
    if pkg.starts_with("com.sun") {
        return true;
    }
    let first = pkg.split('.').next().unwrap_or("");
    matches!(
        first,
        "kotlin" | "java" | "javax" | "android" | "androidx" | "sun"
        // Swift standard frameworks
        | "Foundation" | "UIKit" | "SwiftUI" | "Combine" | "CoreData"
        | "CoreGraphics" | "CoreLocation" | "MapKit" | "AVFoundation"
        | "WebKit" | "StoreKit" | "GameKit" | "ARKit" | "RealityKit"
        | "Swift" | "ObjectiveC" | "Darwin" | "Dispatch" | "os"
    )
}

// ─── ResolutionChain trait ────────────────────────────────────────────────────

/// The five ordered resolution steps used by [`resolve_symbol_inner`].
///
/// Implementing this trait on a type allows the resolution chain to be driven
/// generically — e.g. in tests a lightweight stub can replace the full
/// [`Indexer`] without bringing up a real index.
///
/// Step order mirrors the chain in [`resolve_symbol_inner`]:
/// `local → via_imports → same_package → star_imports → qualified`.
///
/// TODO(G4): make `resolve_symbol_inner` generic over `ResolutionChain +
/// SymbolIndex + ScopeQuery` and migrate call-sites away from `&Indexer`.
#[allow(dead_code)]
pub(crate) trait ResolutionChain {
    fn resolve_local(&self, name: &str, uri: &Url) -> Vec<Location>;
    fn resolve_via_imports(&self, name: &str, uri: &Url) -> Vec<Location>;
    fn resolve_same_package(&self, name: &str, uri: &Url) -> Vec<Location>;
    fn resolve_star_imports(&self, name: &str, uri: &Url) -> Vec<Location>;
    fn resolve_qualified(&self, name: &str, qualifier: &str, uri: &Url) -> Vec<Location>;
}

impl ResolutionChain for Indexer {
    fn resolve_local(&self, name: &str, uri: &Url) -> Vec<Location> {
        resolve_local(self, name, uri)
    }
    fn resolve_via_imports(&self, name: &str, uri: &Url) -> Vec<Location> {
        resolve_via_imports(self, name, uri)
    }
    fn resolve_same_package(&self, name: &str, uri: &Url) -> Vec<Location> {
        resolve_same_package(self, name, uri)
    }
    fn resolve_star_imports(&self, name: &str, uri: &Url) -> Vec<Location> {
        resolve_star_imports(self, name, uri)
    }
    fn resolve_qualified(&self, name: &str, qualifier: &str, uri: &Url) -> Vec<Location> {
        resolve_qualified(self, name, qualifier, uri)
    }
}

// ─── impl Indexer wrappers ────────────────────────────────────────────────────

impl crate::indexer::Indexer {
    pub(crate) fn resolve_symbol(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
    ) -> Vec<Location> {
        resolve_symbol(self, name, qualifier, from_uri)
    }
    pub(crate) fn resolve_symbol_no_rg(&self, name: &str, from_uri: &Url) -> Vec<Location> {
        resolve_symbol_no_rg(self, name, from_uri)
    }
}
