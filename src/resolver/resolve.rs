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

use crate::indexer::{CallShape, Indexer};
use crate::parser::parse_by_extension;
use crate::rg::{build_rg_pattern, parse_rg_line, rg_find_definition};
use crate::types::{CallerContext, FileData};
use crate::StrExt;

use super::fd::{fd_find_and_parse, import_package_prefix};
use super::find::{
    find_all_names_scoped_to_container, find_local_declaration, find_name_in_uri,
    find_name_scoped_to_container,
};
use super::hierarchy::{
    walk_hierarchy, walk_hierarchy_breadth_first, MAX_SYNC_JAR_PROMOTIONS_PER_HIERARCHY_WALK,
};
use super::infer::{infer_field_type, infer_variable_type};

/// Return `FileData` for `uri` — from the live index if indexed, otherwise parse from disk.
/// Returns `None` if the file is not indexed and not readable from disk.
/// Returns an `Arc` so callers can read without copying the full `FileData`.
///
/// Checks `indexer.files` first, then `indexer.jar_files`, then falls back to disk.
/// JAR URIs (`jar:file://...`) cannot be read from disk — when found in `jar_files`,
/// the disk fallback is skipped.
pub(crate) fn ensure_file_data(indexer: &Indexer, uri: &Url) -> Option<Arc<FileData>> {
    if let Some(file_data) = indexer.files.get(uri.as_str()) {
        return Some(file_data.value().clone());
    }

    // URI-keyed `jar_files` read: deliberately NOT gated by a Tier-2 promotion —
    // there is no URI→name reverse index to promote by. A URI for a
    // not-yet-materialized JAR therefore misses here (known limitation);
    // in practice most callers arrive with URIs a name-keyed (promoting)
    // lookup produced, so the miss window is narrow.
    if let Some(file_data) = indexer.jar_files.get(uri.as_str()) {
        return Some(file_data.value().clone());
    }

    let path = uri.to_file_path().ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    Some(Arc::new(parse_by_extension(uri.path(), &content)))
}

// ─── auto-import helpers ──────────────────────────────────────────────────────

/// Return all importable FQNs for a simple symbol name (e.g. "Composable").
pub(crate) fn fqns_for_name(indexer: &Indexer, name: &str) -> Vec<String> {
    indexer
        .importable_fqns
        .read()
        .map(|m| m.get(name).cloned().unwrap_or_default())
        .unwrap_or_default()
}

/// Which IO and fallbacks a resolution pass may use. The plan's "IoPolicy".
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolveIo {
    /// Navigation (go-to-def, hover): may spawn `fd`/`rg`, walk the class
    /// hierarchy, and index a cold file on demand. No global-defs tail fallback.
    Full,
    /// Index-only, but imports may still `fd`. No `rg`, no hierarchy, no cold
    /// index. Tail fallback: first global-defs match. (completion/highlight hot path)
    NoRg,
    /// Strictly in-memory: no `fd`, no `rg`, no hierarchy. Tail fallback:
    /// a unique global-defs match wins outright; an ambiguous set gets the
    /// same denylist-first tie-break as `HierarchyAmbiguitySafe` (see
    /// `ambiguity_safe_tail_with_denylist`), still declining if more than
    /// one candidate survives. (diagnostics keystroke path)
    IndexOnly,
    /// Same IO profile as `NoRg`, but NO global-defs tail fallback at all --
    /// only local/imports/same-package/star-imports/hierarchy count as a
    /// match. For callers that already chain their own last-resort fallback
    /// afterward (see `find_fun_return_type_reachable`): letting THIS step's
    /// tail fire first pre-empted that fallback with a match that has no
    /// more claim to correctness than an arbitrary same-named symbol
    /// anywhere in the workspace (a real production bug: a bare-name tail
    /// match here beat a differently-named function's own, more precise
    /// resolution downstream — see the caller's doc comment).
    ScopedOnly,
    /// Same IO profile as `NoRg`, but the global-defs tail fallback is
    /// ambiguity-safe: a unique match wins outright; an ambiguous (N>1)
    /// match gets two narrow tie-breaks in sequence — dropping any
    /// candidate whose declared package starts with a denylisted prefix
    /// (e.g. `com.android.internal.`), then narrowing to candidates whose
    /// own JAR is a real dependency of the calling file's module (when
    /// `workspace.json` module-dependency data is available) — before
    /// still declining if more than one candidate remains. Used only by
    /// the hierarchy walk's own recursion beyond hop 1 (`supertype_targets`),
    /// where `from_uri` becomes a `jar:` synthetic URI with no import list
    /// to disambiguate against — see
    /// `docs/superpowers/specs/2026-08-25-hierarchy-walk-unscoped-name-collision-design.md`
    /// and `docs/superpowers/specs/2026-08-25-real-workspace-json-schema-and-consumption-design.md`.
    HierarchyAmbiguitySafe,
}

/// Resolve `name` as seen from `from_uri`, returning all known definition
/// `Location`s in priority order.  Returns an empty vec only when nothing was
/// found by any strategy including `rg`.
pub(crate) fn resolve_symbol(
    indexer: &Indexer,
    name: &str,
    qualifier: Option<&str>,
    from_uri: &Url,
) -> Vec<Location> {
    resolve_symbol_with_io(indexer, name, qualifier, from_uri, ResolveIo::Full)
}

/// Same dispatch as `resolve_symbol`, but every bare-name fallback step uses
/// `ResolveIo::IndexOnly` instead of always spawning `rg`/`fd`. For a bulk
/// scan that resolves every identifier in a whole corpus (the
/// resolution-accuracy benchmark) and — by design — expects most bare/local
/// references to miss, letting each of those exhaust the full rg/fd fallback
/// chain turned a 13k-file scan into a roughly hour-long run.
pub(crate) fn resolve_symbol_index_only(
    indexer: &Indexer,
    name: &str,
    qualifier: Option<&str>,
    from_uri: &Url,
) -> Vec<Location> {
    resolve_symbol_with_io(indexer, name, qualifier, from_uri, ResolveIo::IndexOnly)
}

fn resolve_symbol_with_io(
    indexer: &Indexer,
    name: &str,
    qualifier: Option<&str>,
    from_uri: &Url,
    io: ResolveIo,
) -> Vec<Location> {
    // 0. Qualified access: `AccountPickerMapper.Content` — cursor on `Content`.
    //    Resolve the qualifier to a file, then search that file for `name`.
    if let Some(qual) = qualifier {
        // For `super` and `this`, never fall through to the unqualified chain:
        // `super.method` must only look in the parent hierarchy, never via rg/index
        // of the current file (which would return the override).
        let is_keyword_qual = qual == "super" || qual == "this";
        let locs = resolve_qualified(indexer, name, qual, from_uri, io);
        if !locs.is_empty() {
            return locs;
        }
        if is_keyword_qual {
            return vec![];
        }
        // Uppercase qualifier is a class/type name — if qualified resolution
        // failed (class not indexed, member not found), don't fall through
        // to unqualified resolution which would incorrectly match lambda params.
        if qual.starts_with_uppercase() {
            return vec![];
        }
        // If qualifier resolution failed (e.g. it's a package name, not a class),
        // fall through to the normal chain.
    }

    // Handle dotted type names like `Outer.Factory`, a package-qualified
    // `demo.Foo`, or a deeply-nested `Bar.Baz.Foo` passed directly as `name`
    // (e.g. from hover/goto-def of a variable's declared type, or the inferred
    // type of a field). Skip any leading lowercase package segments, then walk
    // the type segments by their nesting — each nested type lives in the same
    // file as its enclosing type.
    if name.contains('.') {
        let segments: Vec<&str> = name.split('.').collect();
        // Start at the first type (uppercase) segment, skipping package prefixes.
        if let Some(start) = segments.iter().position(|s| s.starts_with_uppercase()) {
            let outer_locs =
                resolve_chain(indexer, segments[start], from_uri, io, true, None, None);
            if let Some(outer_loc) = outer_locs.first() {
                // A package-qualified plain type (`demo.Foo`) has no nested
                // segments after the type — the resolved type itself is the target.
                if start + 1 == segments.len() {
                    return outer_locs;
                }
                // Walk each remaining nested segment, re-anchoring on its own
                // location so a same-named sibling elsewhere in the file
                // can't shadow it (see `find_name_scoped_to_container`).
                let mut container = outer_loc.clone();
                for seg in &segments[start + 1..] {
                    match find_name_scoped_to_container(indexer, seg, &container) {
                        Some(loc) => container = loc,
                        None => return vec![],
                    }
                }
                return vec![container];
            }
        }
    }

    resolve_chain(indexer, name, from_uri, io, true, None, None)
}

/// Resolve a call's callee name, filtering same-file candidates by `shape`
/// (see [`resolve_local`], built on `CallShape::accepts_symbol`) so an
/// enclosing declaration that shares the callee's name but can't satisfy the
/// call's arity doesn't shadow the real target. Used only by goto-definition's
/// unqualified-callee path (`Indexer::find_definition_for_call`).
pub(crate) fn resolve_callee_definition(
    indexer: &Indexer,
    name: &str,
    uri: &Url,
    shape: CallShape,
) -> Vec<Location> {
    resolve_chain(indexer, name, uri, ResolveIo::Full, true, Some(shape), None)
}

/// The single prioritised resolution chain, parameterised by IO policy.
///
/// `resolve_symbol_with_io` (`Full`/`IndexOnly`), `resolve_symbol_no_rg` (`NoRg`)
/// and `resolve_type_index_only_simple` (`IndexOnly`) are all thin wrappers over
/// this function. The `ResolveIo` policy selects which subprocess fallbacks (`fd`/`rg`),
/// the hierarchy walk, the cold-file on-demand index, and which global-defs tail
/// fallback are permitted — see the `ResolveIo` doc-comment for the per-policy table.
///
/// The chain order is fixed (local → local-decl → imports → swift → same-package →
/// star → hierarchy → rg → tail); each step that is policy-gated simply no-ops when
/// the policy forbids it, so every policy walks the same steps in the same order.
///
/// `shape` is forwarded to step 1 only (see [`resolve_local`]) — every existing
/// caller passes `None`; only [`resolve_callee_definition`] passes a real shape.
///
/// `hierarchy_walk_origin_uri` is forwarded to the tail fallback only, and only
/// matters for [`ResolveIo::HierarchyAmbiguitySafe`] — every other caller passes
/// `None`; only [`resolve_symbol_hierarchy_ambiguity_safe`] passes the hierarchy
/// walk's real starting file (see [`crate::resolver::hierarchy::walk_hierarchy`]),
/// which can differ from `from_uri` past hop 1 of that walk.
fn resolve_chain(
    indexer: &Indexer,
    name: &str,
    from_uri: &Url,
    io: ResolveIo,
    with_hierarchy: bool,
    shape: Option<CallShape>,
    hierarchy_walk_origin_uri: Option<&Url>,
) -> Vec<Location> {
    // Behavioural knobs derived from the policy (see the `ResolveIo` table):
    //  - `full_io`: cold-index + local-decl + swift + hierarchy + project-wide rg
    //  - `allow_fd`: import resolution may spawn `fd` (everything except IndexOnly)
    //  - `star_rg`: star imports may `rg` the package dir (Full only)
    let full_io = matches!(io, ResolveIo::Full);
    let allow_fd = !matches!(io, ResolveIo::IndexOnly);
    let star_rg = matches!(io, ResolveIo::Full);

    // 0.5 ── on-demand index of the current file if not yet indexed ────────────
    // Ensures resolve_local and find_local_declaration work even at cold start
    // (e.g. the user invokes gd/hover before indexing has reached this file).
    if full_io && !indexer.files.contains_key(from_uri.as_str()) {
        if let Ok(path) = from_uri.to_file_path() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                indexer.index_content(from_uri, &content);
            }
        }
    }

    // 1 ── local (indexed symbols) ────────────────────────────────────────────
    let local = resolve_local(indexer, name, from_uri, shape);
    if !local.is_empty() {
        return local;
    }

    // 1.5 ── local variable / parameter declaration (line scan) ───────────────
    // Catches function parameters without val/var that aren't in the symbol index.
    // Also catches named lambda parameters: `{ item -> ...}` found via the
    // `name ->` pattern in find_declaration_range_in_lines.
    if full_io && !name.starts_with_uppercase() {
        let decl = find_local_declaration(indexer, name, from_uri);
        if !decl.is_empty() {
            return decl;
        }
    }

    // 2 ── explicit imports ───────────────────────────────────────────────────
    let imported = resolve_via_imports(indexer, name, from_uri, allow_fd);
    if !imported.is_empty() {
        return imported;
    }

    // 2.5 ── Swift fast path: definitions index (no package system) ───────────
    // Swift files have no package declarations, so same-package and star-import
    // steps return empty. Use the in-memory definitions index directly to avoid
    // expensive project-wide rg fallback at step 5.
    if full_io
        && crate::Language::from_path(from_uri.path()) == crate::Language::Swift
        && name.starts_with_uppercase()
    {
        if let Some(locs_ref) = indexer.definitions.get(name) {
            // Reconstitute interned `SymbolLoc`s at this boundary before filtering.
            let locs: Vec<Location> = locs_ref
                .iter()
                .filter_map(|sym_loc| indexer.file_table.location(*sym_loc))
                .collect();
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
    let same_pkg = resolve_same_package(indexer, name, from_uri);
    if !same_pkg.is_empty() {
        return same_pkg;
    }

    // 4 ── star imports ───────────────────────────────────────────────────────
    if star_rg {
        // Indexed-package scan, then `rg` scoped to the package dir for unindexed files.
        let star = resolve_star_imports(indexer, name, from_uri);
        if !star.is_empty() {
            return star;
        }
    } else {
        // Index-only scan (no rg fallback for unindexed files).
        let star_pkgs: Vec<String> = match indexer.files.get(from_uri.as_str()) {
            Some(f) => f
                .imports
                .iter()
                .filter(|i| i.is_star && !is_stdlib(&i.full_path))
                .map(|i| i.full_path.clone())
                .collect(),
            None => vec![],
        };
        if let Some(loc) = find_in_star_imports(indexer, name, &star_pkgs) {
            return vec![loc];
        }
    }

    // 4.5 ── superclass / interface hierarchy ─────────────────────────────────
    if full_io && with_hierarchy {
        let inherited = resolve_from_class_hierarchy(indexer, name, from_uri);
        if !inherited.is_empty() {
            return inherited;
        }
    }

    // 5 ── project-wide rg ───────────────────────────────────────────────────
    if full_io {
        let (root, source_roots, matcher) = indexer.rg_scope_for_path(None);
        // Skip when an explicit import for this name already went through all
        // source-tree lookups (qualified index + definitions index + fd) and came
        // up empty.  rg searches the same source tree and cannot add anything new.
        // The package-dir check is the authoritative gate: if `android/os/` doesn't
        // exist under any source root, the symbol simply isn't in the project.
        if import_package_absent_from_source_roots(
            indexer,
            name,
            from_uri,
            root.as_deref(),
            &source_roots,
        ) {
            return vec![];
        }
        let rg_locations =
            rg_find_definition(name, root.as_deref(), &source_roots, matcher.as_deref());
        let rg_result = match shape {
            // `rg` is a blind text search with no arity awareness of its own — without this,
            // a same-file, wrong-arity declaration that step 1 already ruled out can come
            // straight back here, since `rg` re-finds it by pattern match alone.
            Some(shape) => rg_locations
                .into_iter()
                .filter(|location| rg_location_satisfies_call_shape(indexer, location, name, shape))
                .collect(),
            None => rg_locations,
        };
        if !rg_result.is_empty() {
            return rg_result;
        }
        // 5.4 ── global definitions index (includes JAR symbols) ───────────────
        // `rg`/`fd` only search the *workspace's own* source tree, so a type
        // used purely through inference and never explicitly imported (Kotlin
        // doesn't require an import for that) — e.g. `scope.async { }.await()`,
        // where `Deferred` is inferred from `async`'s JAR-indexed return type
        // but no file spells out `import kotlinx.coroutines.Deferred` — was
        // unreachable here: real, measured gap. Same ambiguity-safe tie-break
        // `IndexOnly`/`HierarchyAmbiguitySafe` already use below, extended to
        // `Full` too — a unique JAR/workspace candidate wins outright; an
        // ambiguous set still declines rather than guessing. Shape-filtered
        // first, same as `rg_result` just above: an arity-incompatible
        // same-name workspace declaration (e.g. a differently-shaped local
        // overload) must not win here just because it's the sole candidate
        // this index lookup happens to return.
        let jar_tail_candidates = indexer.lookup_definitions(name);
        let jar_tail_candidates = match shape {
            Some(shape) => jar_tail_candidates
                .into_iter()
                .filter(|location| rg_location_satisfies_call_shape(indexer, location, name, shape))
                .collect(),
            None => jar_tail_candidates,
        };
        let jar_tail = ambiguity_safe_tail_with_denylist(indexer, from_uri, jar_tail_candidates);
        if !jar_tail.is_empty() {
            return jar_tail;
        }
        // 5.5 ── Kotlin built-in-type platform equivalent (last resort) ────────
        // `rg`/`fd` search the *workspace's own* source tree and can never find
        // `String`/`CharSequence`: those are compiler intrinsics with no
        // compiled `.class` file in kotlin-stdlib's JAR at all, and no
        // in-workspace source either -- see
        // docs/superpowers/specs/2026-08-27-kotlin-builtin-type-platform-mapping-design.md.
        return resolve_kotlin_builtin_type_platform_equivalent(indexer, name);
    }

    // Tail fallback — global definitions index (includes JAR symbols).
    //  - NoRg: first match.
    //  - ScopedOnly: no tail at all (empty) -- see the variant's doc comment.
    //  - IndexOnly / HierarchyAmbiguitySafe: unique match wins outright, else
    //    the same denylist-first tie-break (`ambiguity_safe_tail_with_denylist`)
    //    -- IndexOnly resolves a bare qualifier root (e.g. `resolve_qualified`'s
    //    uppercase branch resolving `String` before it can even attempt a
    //    member/extension lookup on it) just as often as the hierarchy walk's
    //    own per-hop resolution does, and hit the identical real decoy shape
    //    on the Moneta corpus (13 candidates for bare `String`, including a
    //    `com.android.internal.*`-packaged one) when it used the older,
    //    plain unique-match-only rule.
    //  - Full: never reached — it has its own equivalent tail (5.4 above,
    //    same `ambiguity_safe_tail_with_denylist` call) inside the rg branch,
    //    since Full always returns from within that `if full_io` block.
    //
    // Each non-`ScopedOnly` arm falls through to
    // `resolve_kotlin_builtin_type_platform_equivalent` when its own lookup
    // comes up empty -- see the 5.5 comment above the `Full`/rg branch.
    // `ScopedOnly` is deliberately excluded: "no tail at all" is its own
    // documented contract (its callers already have their own downstream
    // fallback), so it must not gain one here.
    match io {
        ResolveIo::Full => vec![],
        ResolveIo::ScopedOnly => vec![],
        ResolveIo::NoRg => {
            let found = indexer
                .lookup_definitions(name)
                .into_iter()
                .next()
                .map(|loc| vec![loc])
                .unwrap_or_default();
            if !found.is_empty() {
                return found;
            }
            resolve_kotlin_builtin_type_platform_equivalent(indexer, name)
        }
        ResolveIo::IndexOnly => {
            let found = ambiguity_safe_tail_with_denylist(
                indexer,
                from_uri,
                indexer.lookup_definitions(name),
            );
            if !found.is_empty() {
                return found;
            }
            resolve_kotlin_builtin_type_platform_equivalent(indexer, name)
        }
        ResolveIo::HierarchyAmbiguitySafe => {
            let found = ambiguity_safe_tail_with_denylist(
                indexer,
                hierarchy_walk_origin_uri.unwrap_or(from_uri),
                indexer.lookup_definitions(name),
            );
            if !found.is_empty() {
                return found;
            }
            resolve_kotlin_builtin_type_platform_equivalent(indexer, name)
        }
    }
}

/// Package prefixes that can never be a legitimate app-facing reference of
/// ANY kind — not just a supertype. `com.android.internal.*` is Android's
/// hidden, non-public platform-internal API surface: it never ships in the
/// public SDK a real app compiles against, so no ordinary app source can
/// reference it as a supertype, an import, a variable's type, or anything
/// else. That's a property of the package itself, not of which resolution
/// step happens to be looking a name up — the same invariant is what makes
/// this denylist safe to share across every caller of
/// [`ambiguity_safe_tail_with_denylist`] (originally only
/// [`ResolveIo::HierarchyAmbiguitySafe`]'s supertype walk, now also
/// [`ResolveIo::IndexOnly`]'s general bare-name tail), not just the one it
/// was first evidenced against.
///
/// Checked only as a last-resort tie-break once a lookup has already found
/// more than one same-named candidate. Deliberately narrow (currently a
/// single entry, the one directly evidenced by the real repro): a denylist
/// can only ever fail to help, never introduce a new wrong preference the
/// way a broader "prefer this package family" heuristic could — see the
/// design doc's self-critique. Do not grow this into a general
/// preference-ranking system.
const DENYLISTED_PACKAGE_PREFIXES: &[&str] = &["com.android.internal."];

/// Tail fallback shared by [`ResolveIo::HierarchyAmbiguitySafe`] (the
/// hierarchy walk's own per-hop resolution), [`ResolveIo::IndexOnly`]
/// (general bare-name resolution — diagnostics, `resolve_qualified`'s
/// qualifier-root lookup, etc.), and `Full`'s own equivalent tail (see
/// `resolve_chain`'s step 5.4): a unique candidate wins outright; an
/// ambiguous set gets four narrow tie-breaks in sequence — first
/// [`DENYLISTED_PACKAGE_PREFIXES`] (unconditional, project-wide), then
/// [`module_scoped_tie_break`] (real per-module Gradle dependency data, when
/// `workspace.json` provides it), then [`default_kotlin_import_tie_break`]
/// (Kotlin's own hardcoded default-import package set — a language fact, not
/// project data), then [`import_package_tie_break`] (the calling file's own
/// already-parsed import list, always available — no external data needed) —
/// before still declining unless one of them leaves exactly one candidate.
/// See the real-workspace-json-schema design doc's §5 for why denylist-first
/// is correct: it's unconditional and needs no loaded data. The remaining
/// three run in *decreasing* certainty: module-scoped narrowing is the real
/// Gradle dependency graph (most precise, but only as available as
/// `workspace.json`'s own data); Kotlin's default imports are a fixed
/// language-level fact, always available, but only relevant when a candidate
/// actually lives in one of those packages; import-package narrowing is the
/// weakest — a file merely importing a *sibling* from the right package, not
/// the ambiguous name itself, so it runs last.
///
/// Crucially, each tie-break's authority carries forward even when it
/// doesn't land on a unique winner by itself: when module-scoped narrowing
/// (or default-import narrowing) proves some candidates more plausible than
/// others without reaching uniqueness, the NEXT tie-break is only ever
/// handed that narrowed subset, never the original, unnarrowed set — a
/// candidate an earlier, stronger tie-break has already disproven (or simply
/// left out) must never be resurrected by a later, weaker signal. The
/// original, unnarrowed set only ever reaches a later tie-break when the
/// earlier one had nothing to say at all (no data, for module-scoping; no
/// candidate in a default-import package, for Kotlin's default imports —
/// see [`ModuleScopedOutcome`] and [`default_kotlin_import_tie_break`]'s own
/// doc). `origin_uri` is the real file to resolve an owning module/import
/// list from — the hierarchy walk's own starting file for
/// `HierarchyAmbiguitySafe`, or simply the caller's own `from_uri` for
/// `IndexOnly`/`Full`.
fn ambiguity_safe_tail_with_denylist(
    indexer: &Indexer,
    origin_uri: &Url,
    locations: Vec<Location>,
) -> Vec<Location> {
    if locations.len() == 1 {
        return locations;
    }
    if locations.is_empty() {
        return vec![];
    }
    let filtered: Vec<Location> = locations
        .into_iter()
        .filter(|location| !is_denylisted_package_prefix(indexer, location))
        .collect();
    if filtered.len() == 1 {
        return filtered;
    }
    if filtered.len() < 2 {
        return vec![];
    }
    let after_module_scope = match module_scoped_tie_break(indexer, origin_uri, filtered.clone()) {
        ModuleScopedOutcome::Narrowed(narrowed) if narrowed.len() == 1 => return narrowed,
        ModuleScopedOutcome::Narrowed(narrowed) => narrowed,
        ModuleScopedOutcome::NoData | ModuleScopedOutcome::NoDependenciesSurvived => filtered,
    };
    let after_default_import = default_kotlin_import_tie_break(indexer, after_module_scope);
    if after_default_import.len() == 1 {
        return after_default_import;
    }
    import_package_tie_break(indexer, origin_uri, after_default_import)
}

/// Kotlin's own default-import package set: every one of these is available
/// on every Kotlin/JVM file with no explicit `import` ever needed, by
/// definition of the language (Kotlin reference, "Default imports"). A
/// hardcoded LANGUAGE fact, not project data that could be stale or
/// incomplete — safe to trust unconditionally, the same spirit as
/// [`DENYLISTED_PACKAGE_PREFIXES`], just a preference instead of an
/// exclusion.
const KOTLIN_DEFAULT_IMPORT_PACKAGES: &[&str] = &[
    "kotlin",
    "kotlin.annotation",
    "kotlin.collections",
    "kotlin.comparisons",
    "kotlin.io",
    "kotlin.ranges",
    "kotlin.sequences",
    "kotlin.text",
    "kotlin.jvm",
    "java.lang",
];

/// Tie-break run between [`module_scoped_tie_break`] and
/// [`import_package_tie_break`]: when one or more candidates live in a
/// package Kotlin implicitly imports for every file (see
/// [`KOTLIN_DEFAULT_IMPORT_PACKAGES`]), prefer those over any candidate that
/// would need a real import — or a real receiver-typed member match, which
/// the caller already tried and failed, or this tail would never run — to be
/// reachable at all. Real, measured case: `apply`/`run` (Kotlin's own scope
/// functions, `kotlin.apply`/`kotlin.run`) collide with hundreds of
/// unrelated same-named JVM/Android members (`java.util.function.
/// Function.apply`, `Runnable.run`, countless builder `.apply()` methods)
/// across a real dependency graph — none of THOSE are ever reachable
/// without an explicit import, so a default-imported candidate is always at
/// least as plausible, and in this fallback's context (a bare-name retry
/// after receiver-scoped lookup already failed) is virtually always the
/// actually-intended target.
///
/// Only narrows, never fully declines: when no candidate is in a
/// default-import package, this is a no-op and the original set passes
/// through unchanged to the next tie-break.
fn default_kotlin_import_tie_break(indexer: &Indexer, locations: Vec<Location>) -> Vec<Location> {
    let narrowed: Vec<Location> = locations
        .iter()
        .filter(|location| {
            location_package(indexer, location)
                .is_some_and(|pkg| KOTLIN_DEFAULT_IMPORT_PACKAGES.contains(&pkg.as_str()))
        })
        .cloned()
        .collect();
    if narrowed.is_empty() {
        locations
    } else {
        narrowed
    }
}

/// Outcome of [`module_scoped_tie_break`] consulting real per-module Gradle
/// dependency data for an ambiguous candidate set — three real, distinct
/// cases the caller must not collapse into a single "declined" bucket (see
/// [`ambiguity_safe_tail_with_denylist`]'s doc comment for why the
/// distinction matters).
enum ModuleScopedOutcome {
    /// No `workspace.json` module-dependency data is available at all for
    /// `origin_uri`'s owning module (`owning_module_dependencies` returned
    /// `None`) — module-scoping has nothing to say, so the next tie-break
    /// must fall back to the original, unnarrowed candidate set.
    NoData,
    /// Dependency data WAS available, but none of the candidates are real
    /// dependencies of the calling module. Treated the same as `NoData`
    /// rather than as a proof that every candidate is wrong: a workspace's
    /// dependency data can be incomplete (see `owning_module_dependencies`'s
    /// own doc comment), so eliminating every candidate is more likely a
    /// data gap than a genuine "none of these are possible" result — the
    /// next tie-break falls back to the original, unnarrowed set too.
    NoDependenciesSurvived,
    /// Dependency data narrowed the candidates to a real, positive,
    /// non-empty subset — every remaining candidate is a proven dependency
    /// of the calling module. Length 1 is a unique winner the caller returns
    /// immediately; length > 1 is still ambiguous, but the next tie-break
    /// must only ever choose among THESE candidates, never a candidate this
    /// narrowing already ruled out.
    Narrowed(Vec<Location>),
}

/// Second tie-break for [`ambiguity_safe_tail_with_denylist`]: when the
/// denylist alone still leaves more than one candidate, narrow using the
/// hierarchy walk's real starting file's own module's real Gradle dependency
/// set (see `workspace_json::load_module_dependencies`) — a candidate
/// survives only if its own JAR's `(group, artifact, version)` is a
/// dependency of the module `hierarchy_walk_origin_uri` belongs to. See
/// [`ModuleScopedOutcome`] for the three distinct outcomes this can produce
/// and how the caller must treat each one.
fn module_scoped_tie_break(
    indexer: &Indexer,
    hierarchy_walk_origin_uri: &Url,
    locations: Vec<Location>,
) -> ModuleScopedOutcome {
    let Some(dependencies) = owning_module_dependencies(indexer, hierarchy_walk_origin_uri) else {
        return ModuleScopedOutcome::NoData;
    };
    let narrowed: Vec<Location> = locations
        .into_iter()
        .filter(|location| {
            candidate_gradle_meta(location).is_some_and(|meta| dependencies.contains(&meta))
        })
        .collect();
    if narrowed.is_empty() {
        ModuleScopedOutcome::NoDependenciesSurvived
    } else {
        ModuleScopedOutcome::Narrowed(narrowed)
    }
}

/// Third tie-break for [`ambiguity_safe_tail_with_denylist`]: when the
/// denylist and module-scoped narrowing still leave more than one candidate,
/// narrow using `origin_uri`'s own imports — not of the ambiguous name
/// itself (`resolve_via_imports` already tried that, earlier in the chain,
/// and failed, or this tail would never have been reached) but of any OTHER
/// symbol from the same package. A file that writes `import
/// kotlinx.coroutines.async` but never spells out `Deferred` (used only
/// through inference, e.g. `scope.async { }.await()`) still tells us
/// `kotlinx.coroutines` is a real, in-use package for this file — evidence
/// an unrelated same-named decoy from a package this file never otherwise
/// references (real, measured case: `com.google.firebase.components.Deferred`)
/// doesn't have. Weaker than [`module_scoped_tie_break`] (an inference from
/// unrelated imports, not the real dependency graph), so tried after it, but
/// needs no `workspace.json` data at all — narrows real cases that
/// module-scoping can't when a workspace has no module-dependency data
/// loaded (e.g. a `workspace.json` with only `sourcePaths`, no `libraries`).
///
/// A star import (`import com.foo.bar.*`) counts too, as evidence for its
/// own exact package only: `ImportEntry::full_path` for a star import is
/// already the bare package (there's no trailing symbol segment to strip,
/// unlike a non-star import's `full_path`), so it's used as-is rather than
/// through [`import_package_prefix`] — passing a star import's `full_path`
/// through that helper would wrongly strip its own last segment as if it
/// were an imported symbol, over-widening `com.foo.bar` to `com.foo`.
pub(super) fn import_package_tie_break(
    indexer: &Indexer,
    origin_uri: &Url,
    locations: Vec<Location>,
) -> Vec<Location> {
    let Some(file_data) = indexer.files.get(origin_uri.as_str()) else {
        return vec![];
    };
    let imported_packages: std::collections::HashSet<String> = file_data
        .imports
        .iter()
        .map(|i| {
            if i.is_star {
                i.full_path.clone()
            } else {
                import_package_prefix(&i.full_path)
            }
        })
        .collect();
    if imported_packages.is_empty() {
        return vec![];
    }
    let narrowed: Vec<Location> = locations
        .into_iter()
        .filter(|location| {
            location_package(indexer, location).is_some_and(|pkg| imported_packages.contains(&pkg))
        })
        .collect();
    if narrowed.len() == 1 {
        narrowed
    } else {
        vec![]
    }
}

/// Looks up `from_uri`'s owning module's real Gradle dependency set: the
/// content-root directory that is the longest-prefix match of `from_uri`'s
/// file path (see `workspace_json::load_module_dependencies`'s own doc
/// comment for why this is the correct module-identity lookup, not a
/// `build.gradle.kts`-nearest-ancestor heuristic). A cheap lookup against
/// already-loaded, pre-parsed state — no file I/O or parsing on this path.
/// Returns `None` when `from_uri` isn't a real file path (e.g. a `jar:`
/// synthetic URI — [`module_scoped_tie_break`]'s caller passes the hierarchy
/// walk's real starting file for this reason, not the current hop's own URI)
/// or no `workspace.json` module data was loaded for this workspace.
fn owning_module_dependencies(
    indexer: &Indexer,
    from_uri: &Url,
) -> Option<HashSet<crate::cli::extract_sources::GradleMeta>> {
    let file_path = crate::path_util::path_from_uri(from_uri)?;
    let dependencies_by_content_root = indexer
        .module_dependencies
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    dependencies_by_content_root
        .iter()
        .filter(|(content_root, _)| file_path.starts_with(content_root.as_path()))
        .max_by_key(|(content_root, _)| content_root.as_os_str().len())
        .map(|(_, dependencies)| dependencies.clone())
}

/// Resolves a candidate hierarchy-walk `Location`'s own JAR path to its
/// Gradle coordinates, via the existing [`crate::cli::extract_sources::parse_jar_meta`]
/// (design doc §2 point 3's cross-check opportunity). Handles both a
/// compiled-only JAR URI (`jar:file://<jar>`, no entry — how `jar_definitions`
/// locations are shaped) and a sources-JAR entry URI (`jar:file://<jar>!/<entry>`);
/// [`crate::jar_extract::parse_jar_entry_uri`] only handles the latter shape,
/// so it is not reused here. Returns `None` for a non-`jar:` URI or an
/// unparseable jar path, never a wrong guess.
fn candidate_gradle_meta(location: &Location) -> Option<crate::cli::extract_sources::GradleMeta> {
    let rest = location.uri.as_str().strip_prefix("jar:")?;
    let jar_part = rest.split_once("!/").map_or(rest, |(jar, _)| jar);
    let jar_path = crate::path_util::path_from_uri(&Url::parse(jar_part).ok()?)?;
    crate::cli::extract_sources::parse_jar_meta(&jar_path)
}

/// Whether `location` has a package matching one of
/// [`DENYLISTED_PACKAGE_PREFIXES`]. Tries [`jar_symbol_package`] first — a
/// real compiled JAR spans many packages across its symbols, so `location`'s
/// own accurate per-symbol package (the `jar_symbol_packages` side table) is
/// checked before `indexer.jar_files`' single `FileData.package`, which
/// `build_jar_file_data` derives from only the FIRST class-like symbol it
/// happens to find and is therefore not necessarily `location`'s own real
/// package. Falls back to `indexer.files` (regular source files, always
/// exactly one package each, so no per-symbol ambiguity exists) and then
/// `indexer.jar_files` (compiled-only entries pre-dating the per-symbol
/// cache, or files this side table has no entry for at all) — same two-map
/// lookup order as [`crate::indexer::infer::sig::collect_params_from_file`].
/// Locations with no known package anywhere are never treated as
/// denylisted — the tie-break must only ever remove a candidate it can
/// positively prove is denylisted.
fn is_denylisted_package_prefix(indexer: &Indexer, location: &Location) -> bool {
    let Some(package) = location_package(indexer, location) else {
        return false;
    };
    DENYLISTED_PACKAGE_PREFIXES
        .iter()
        .any(|prefix| package.starts_with(prefix))
}

/// `location`'s own real package — same three-map fallback chain
/// [`is_denylisted_package_prefix`]'s doc comment explains (per-symbol JAR
/// package first, then a regular source file's single package, then a
/// compiled-only JAR entry's first-symbol-derived package). Shared by every
/// tie-break in [`ambiguity_safe_tail_with_denylist`] that needs to compare
/// a candidate's package against something else.
fn location_package(indexer: &Indexer, location: &Location) -> Option<String> {
    jar_symbol_package(indexer, location)
        .or_else(|| {
            indexer
                .files
                .get(location.uri.as_str())
                .and_then(|f| f.package.clone())
        })
        .or_else(|| {
            indexer
                .jar_files
                .get(location.uri.as_str())
                .and_then(|f| f.package.clone())
        })
}

/// Returns the first Location found by scanning star-import packages.
fn find_in_star_imports(indexer: &Indexer, name: &str, star_pkgs: &[String]) -> Option<Location> {
    for pkg in star_pkgs {
        if let Some(loc) = find_symbol_in_package(indexer, name, pkg) {
            return Some(loc);
        }
    }
    None
}

/// Index-only resolver for use in completion paths (`ResolveIo::NoRg` — see
/// its own doc comment for the exact IO policy, since restating it here is
/// how this comment drifted out of sync with it the first time).
pub(crate) fn resolve_symbol_no_rg(indexer: &Indexer, name: &str, from_uri: &Url) -> Vec<Location> {
    resolve_chain(indexer, name, from_uri, ResolveIo::NoRg, false, None, None)
}

/// Ambiguity-safe sibling of [`resolve_symbol_no_rg`], scoped to the
/// hierarchy walk's own recursion (`supertype_targets` in
/// `resolver/hierarchy.rs`) — see [`ResolveIo::HierarchyAmbiguitySafe`].
/// Does not alter `resolve_symbol_no_rg` itself or any of its other
/// callers' behavior.
///
/// `hierarchy_walk_origin_uri` is the hierarchy walk's real starting file
/// (see [`crate::resolver::hierarchy::walk_hierarchy`]'s doc comment),
/// forwarded here from `supertype_targets` so the module-scoped tie-break
/// can still find real Gradle dependency data past hop 1, where `from_uri`
/// itself has become the previous hop's own (often `jar:`) resolved URI.
pub(crate) fn resolve_symbol_hierarchy_ambiguity_safe(
    indexer: &Indexer,
    name: &str,
    from_uri: &Url,
    hierarchy_walk_origin_uri: Option<&Url>,
) -> Vec<Location> {
    resolve_chain(
        indexer,
        name,
        from_uri,
        ResolveIo::HierarchyAmbiguitySafe,
        false,
        None,
        hierarchy_walk_origin_uri,
    )
}

/// Like [`resolve_symbol_no_rg`] but without its global-defs tail fallback --
/// for callers that already chain their own last-resort fallback afterward
/// (see [`ResolveIo::ScopedOnly`]).
pub(crate) fn resolve_symbol_scoped_only(
    indexer: &Indexer,
    name: &str,
    from_uri: &Url,
) -> Vec<Location> {
    resolve_chain(
        indexer,
        name,
        from_uri,
        ResolveIo::ScopedOnly,
        false,
        None,
        None,
    )
}

/// Index-only type resolver for the diagnostics hot path.
///
/// Same resolution chain as `resolve_symbol_no_rg` but:
/// - Skips the `fd_find_and_parse` fallback in import resolution (no subprocess spawns)
/// - Makes the global definitions fallback ambiguity-safe (returns only if exactly 1 candidate)
///
/// This keeps behavior consistent with navigation (imports + package context) without
/// the IO cost that causes timeouts when called per-`when`-expression during diagnostics.
pub(crate) fn resolve_type_index_only(
    indexer: &Indexer,
    name: &str,
    from_uri: &Url,
) -> Vec<Location> {
    // Handle dotted type names like `DashboardInvestedContract.Effect` — mirrors
    // the same pattern in `resolve_symbol` (see dotted-name block above).
    if let Some(dot) = name.find('.') {
        let outer = &name[..dot];
        let inner = &name[dot + 1..];
        // Use the full simple chain for the outer (no recursion into dotted split).
        let outer_locs = resolve_type_index_only_simple(indexer, outer, from_uri);
        if let Some(outer_loc) = outer_locs.first() {
            let locs = find_name_in_uri(indexer, inner, outer_loc.uri.as_str());
            if !locs.is_empty() {
                return locs;
            }
        }
    }

    resolve_type_index_only_simple(indexer, name, from_uri)
}

/// Inner helper: resolves a simple (non-dotted) type name using the index-only chain.
fn resolve_type_index_only_simple(indexer: &Indexer, name: &str, from_uri: &Url) -> Vec<Location> {
    resolve_chain(
        indexer,
        name,
        from_uri,
        ResolveIo::IndexOnly,
        false,
        None,
        None,
    )
}

// ─── missing-import diagnostic helpers ────────────────────────────────────────

/// Whether `from_uri` has an explicit (non-star) import whose local name is `name`
/// — `import a.b.Name` or `import a.b.Whatever as Name`. Presence alone proves the
/// name is in scope; the target FQN need not be indexed.
fn has_explicit_import(indexer: &Indexer, name: &str, from_uri: &Url) -> bool {
    indexer
        .files
        .get(from_uri.as_str())
        .map(|f| f.imports.iter().any(|i| !i.is_star && i.local_name == name))
        .unwrap_or(false)
}

/// Kotlin's implicit default-import packages (JVM target): names declared directly
/// in these are in scope in every file without an `import`. Narrower than
/// [`is_stdlib`] — `android`/`androidx`/most `java.*` are *not* auto-imported.
fn is_default_import_package(pkg: &str) -> bool {
    matches!(
        pkg,
        "kotlin"
            | "kotlin.annotation"
            | "kotlin.collections"
            | "kotlin.comparisons"
            | "kotlin.io"
            | "kotlin.ranges"
            | "kotlin.sequences"
            | "kotlin.text"
            | "kotlin.jvm"
            | "java.lang"
    )
}

/// Core `kotlin.*` types from the language's default imports (`kotlin`,
/// `kotlin.collections`, …). The companion of [`is_default_import_package`] at the
/// type level: both encode the spec-defined default-import set so a bare `Number` /
/// `List` / `Result` isn't treated as a missing import when the (rarely-indexed)
/// kotlin-stdlib jar provides no concrete symbol to confirm it.
fn is_default_import_type(name: &str) -> bool {
    matches!(
        name,
        // kotlin.* primitives & core types
        "Number" | "Byte" | "Short" | "Int" | "Long" | "Float" | "Double" | "Char"
        | "Boolean" | "String" | "CharSequence" | "Any" | "Unit" | "Nothing"
        | "Comparable" | "Enum" | "Annotation" | "Function" | "Lazy" | "Result"
        | "Pair" | "Triple" | "Throwable" | "Exception" | "Error" | "RuntimeException"
        // kotlin.collections.* (default-imported)
        | "Array" | "Iterable" | "Iterator" | "Collection" | "List" | "Set" | "Map"
        | "MutableIterable" | "MutableCollection" | "MutableList" | "MutableSet"
        | "MutableMap" | "ArrayList" | "HashMap" | "HashSet" | "LinkedHashMap"
        | "LinkedHashSet"
        // kotlin.* specialized primitive array types (default-imported, same as
        // `Array` above) -- real, measured false-positive source: `ByteArray`
        // alone was 45% of the missing-import POC's total flags on the Moneta
        // corpus before this fix.
        | "ByteArray" | "CharArray" | "ShortArray" | "IntArray" | "LongArray"
        | "FloatArray" | "DoubleArray" | "BooleanArray"
        // kotlin.sequences.*
        | "Sequence"
    )
}

/// Whether `name` is available without an import because a symbol of that name is
/// declared in a Kotlin default-import package (e.g. `kotlin.Result`, `kotlin.apply`),
/// or `name` is itself one of the core default-import types.
///
/// Checks `jar_definitions`/`definitions` directly (by package), not the narrower
/// `importable_fqns` cache — that cache only holds container-less symbols recorded
/// for auto-import completion, and top-level `kotlin.*` functions (`error`, `run`,
/// `with`, `repeat`, …) aren't reliably captured there, so a `fqns_for_name`-only
/// check would silently miss them and flag real stdlib calls as missing imports.
fn resolvable_via_default_import(indexer: &Indexer, name: &str) -> bool {
    if is_default_import_type(name) {
        return true;
    }
    // Promote-before-read (zero budget): this runs on the diagnostics/keystroke
    // path — no blocking sidecar IPC here. Without this, a default-import
    // top-level function (kotlin.error, kotlin.with, …) whose JAR hasn't been
    // materialized yet reads as absent and gets flagged as a missing import.
    let mut cache_backed_only = 0usize;
    crate::indexer::jar::ensure_jar_definitions_for(indexer, name, &mut cache_backed_only);
    if let Some(locs) = indexer.jar_definitions.get(name) {
        for loc in locs.iter() {
            if jar_symbol_package(indexer, loc)
                .as_deref()
                .is_some_and(is_default_import_package)
            {
                return true;
            }
            if indexer
                .jar_files
                .get(loc.uri.as_str())
                .and_then(|fd| fd.package.clone())
                .as_deref()
                .is_some_and(is_default_import_package)
            {
                return true;
            }
        }
    }
    if let Some(sym_locs) = indexer.definitions.get(name) {
        for sym_loc in sym_locs.iter() {
            let Some(loc) = indexer.file_table.location(*sym_loc) else {
                continue;
            };
            if indexer
                .files
                .get(loc.uri.as_str())
                .and_then(|fd| fd.package.clone())
                .as_deref()
                .is_some_and(is_default_import_package)
            {
                return true;
            }
        }
    }
    false
}

/// Strict in-scope reachability check for missing-import detection.
///
/// Answers "is `name` reachable from *this file's own scope alone*?" — i.e. via a
/// local/param declaration, an explicit import, the same package, or a non-stdlib
/// star import. Unlike [`resolve_symbol_no_rg`] it deliberately OMITS the global
/// definitions fallback (and rg): a name that exists *somewhere* in the index but
/// isn't reachable here is exactly a missing-import candidate, so we must not let the
/// global index mask it.
pub(crate) fn resolve_in_scope_strict(indexer: &Indexer, name: &str, from_uri: &Url) -> bool {
    if !resolve_local(indexer, name, from_uri, None).is_empty() {
        return true;
    }
    // An explicit import of `name` (`import a.b.Name` / `… as Alias`) brings the symbol
    // into scope — so it is NOT a missing import, even when the target FQN isn't indexed
    // (e.g. `android.widget.Button`, `java.util.Calendar` whose SDK jars aren't indexed).
    if has_explicit_import(indexer, name, from_uri) {
        return true;
    }
    // Available without an import via Kotlin's default-import packages (kotlin.*, …).
    if resolvable_via_default_import(indexer, name) {
        return true;
    }
    // Function parameters / local vals without an indexed symbol (line scan).
    if !name.starts_with_uppercase() && !find_local_declaration(indexer, name, from_uri).is_empty()
    {
        return true;
    }
    // Index-only import resolution (no fd subprocess) — covers explicit imports.
    if !resolve_via_imports(indexer, name, from_uri, false).is_empty() {
        return true;
    }
    // Star import of a *class's* members (`import Foo.*` brings in `Foo`'s nested
    // types / enum entries / companion members) — distinct from `resolve_via_imports`
    // above, which only handles package-level star imports.
    {
        let (parent, pkg) = indexer.resolve_symbol_via_import(from_uri, name);
        if parent.is_some() || pkg.is_some() {
            return true;
        }
    }
    if !resolve_same_package(indexer, name, from_uri).is_empty() {
        return true;
    }
    // A star import of a stdlib-shaped package (`java.*`/`kotlin.*`/`android.*`/
    // `androidx.*`) has no locally-indexed source for `find_in_star_imports`
    // below to search, so it's excluded from that search entirely — but for
    // THIS check (does some import plausibly cover `name`, at all), that
    // exclusion is wrong: the file compiles, so `import java.util.*` really
    // does bring `Date` into scope even though we can't confirm membership
    // one way or the other. Real, measured false positive on Moneta:
    // `Date`/`Calendar`/`Collections` were flagged as missing imports in
    // files that explicitly had `import java.util.*`.
    if let Some(file_data) = indexer.files.get(from_uri.as_str()) {
        if file_data
            .imports
            .iter()
            .any(|i| i.is_star && is_stdlib(&i.full_path))
        {
            return true;
        }
    }
    let star_pkgs: Vec<String> = match indexer.files.get(from_uri.as_str()) {
        Some(f) => f
            .imports
            .iter()
            .filter(|i| i.is_star && !is_stdlib(&i.full_path))
            .map(|i| i.full_path.clone())
            .collect(),
        None => vec![],
    };
    if find_in_star_imports(indexer, name, &star_pkgs).is_some() {
        return true;
    }
    // Members inherited from a super class/interface (e.g. `Result` from a
    // CoroutineWorker subclass) are in scope without an import.
    !resolve_from_class_hierarchy(indexer, name, from_uri).is_empty()
}

/// Whether the extension-receiver type `receiver` provides `name` as a member or an
/// in-scope extension — so a bare `name` inside `fun Receiver.f() { … }` (or an
/// implicit-receiver lambda body) is resolved by the receiver, not a missing import.
/// Index-only (no rg/fd).
///
/// Covers names declared directly on `receiver` (workspace or JAR), extensions
/// registered for it, and members inherited from its supertype chain (incl. library
/// supertypes), e.g. `fun SomeFragment.ext() { requireActivity() }`, where
/// `requireActivity` is declared on androidx `Fragment`, several levels up
/// `SomeFragment`'s chain.
pub(crate) fn receiver_provides_member(indexer: &Indexer, receiver: &str, name: &str) -> bool {
    // 1. Extension function/property declared on the receiver type.
    if indexer
        .extension_by_receiver
        .get(receiver)
        .is_some_and(|entries| entries.iter().any(|e| e.name == name))
    {
        return true;
    }
    // 2. Member of the receiver type — workspace declaration (container chain match).
    if let Some(sym_locs) = indexer.definitions.get(name) {
        if sym_locs.iter().any(|sym_loc| {
            indexer.file_table.location(*sym_loc).is_some_and(|loc| {
                enclosing_container_chain(indexer, &loc)
                    .iter()
                    .any(|c| c == receiver)
            })
        }) {
            return true;
        }
    }
    // 3. Member of a compiled/sources JAR type (the symbol's recorded container).
    // Promote-before-read (zero budget): diagnostics/keystroke path, no blocking
    // sidecar IPC — without this a lazily-materialized JAR's member reads as
    // absent and a real implicit-receiver call gets flagged as a missing import.
    let mut cache_backed_only = 0usize;
    crate::indexer::jar::ensure_jar_definitions_for(indexer, name, &mut cache_backed_only);
    if let Some(locs) = indexer.jar_definitions.get(name) {
        for loc in locs.iter() {
            let is_member = indexer
                .jar_files
                .get(loc.uri.as_str())
                .and_then(|fd| {
                    fd.symbols
                        .get(loc.range.start.line as usize)
                        .and_then(|s| s.container.clone())
                })
                .as_deref()
                == Some(receiver);
            if is_member {
                return true;
            }
        }
    }
    // 4. Inherited from the receiver type's supertype chain (incl. library supertypes).
    // Depth 24 (not the shared resolve_from_class_hierarchy's 12): validated on Moneta
    // by the original missing-import POC as real headroom, not just enough — the
    // visited-set bounds total work regardless, so there's no cost to the margin.
    for loc in indexer.lookup_definitions(receiver) {
        // Zero sidecar budget: same diagnostics/keystroke-path, no-blocking-IPC
        // intent as this function's other two promote-before-read calls above.
        let found = walk_hierarchy(
            indexer,
            receiver,
            loc.uri.as_str(),
            CallerContext::default(),
            24,
            0,
            |index, _, class_uri, _| find_name_in_uri(index, name, class_uri),
        );
        if !found.is_empty() {
            return true;
        }
    }
    false
}

// ─── step implementations ────────────────────────────────────────────────────

/// Look up an extension function by receiver base name, filtering by scope
/// (same package or explicitly imported in the caller's file).
///
/// Checks `extension_by_receiver` for matching entries, then verifies each
/// candidate is visible from `from_uri` by checking same-package or import
/// coverage. Returns the first matching extension's `Location` with an accurate
/// `selection_range`, or an empty `Vec` if none is in scope.
fn resolve_extension_in_scope(
    indexer: &Indexer,
    receiver_base: &str,
    name: &str,
    from_uri: &Url,
) -> Vec<Location> {
    // Atomic promote+read (zero budget): this helper serves goto-definition
    // AND the per-call-site diagnostics path (`resolve_member`), so blocking
    // sidecar IPC is forbidden here — cache-backed promotions are still free.
    let mut cache_backed_only = 0usize;
    let Some(entries) =
        crate::indexer::jar::extension_entries_for(indexer, receiver_base, &mut cache_backed_only)
    else {
        return vec![];
    };
    let caller_file_data = indexer.files.get(from_uri.as_str());
    let caller_file_data_ref: Option<&FileData> = caller_file_data.as_deref().map(|v| v.as_ref());
    for entry in entries.iter() {
        if entry.name != name {
            continue;
        }
        let in_scope = crate::resolver::infer::extension_is_in_scope(
            entry.package.as_ref(),
            &entry.name,
            entry.container.as_ref(),
            entry.visibility,
            entry.file_uri == from_uri.as_str(),
            caller_file_data_ref,
        );
        if in_scope {
            if let Ok(uri) = Url::parse(&entry.file_uri) {
                let range = indexer
                    .files
                    .get(&entry.file_uri)
                    .or_else(|| indexer.jar_files.get(&entry.file_uri))
                    .and_then(|fd| {
                        fd.symbols
                            .iter()
                            .find(|s| {
                                crate::resolver::infer::extension_declaration_matches(
                                    s,
                                    name,
                                    receiver_base,
                                    entry.container.as_ref(),
                                )
                            })
                            .map(|s| s.selection_range)
                    })
                    .unwrap_or_default();
                return vec![Location { uri, range }];
            }
        }
    }
    vec![]
}

/// Resolve `name(...)` as an implicit `this.name(...)` against `receiver_base`
/// — the enclosing extension function's own declared receiver type (see
/// `parser::enclosing_extension_receiver_at`) — tried only when nothing else
/// resolves `name` by plain bare-name search (imports/same-package/star/
/// hierarchy/rg have no receiver-type awareness at all, so a bare call inside
/// an extension function's own body that targets a same-named member/
/// extension of that receiver is invisible to every one of them).
///
/// Mirrors `resolve_qualified`'s member-vs-extension precedence for
/// `TypeName.member`, but shape-filters both halves: `resolve_extension_in_scope`
/// has no arity awareness of its own, and the enclosing declaration itself is
/// one of its own registered "extensions in scope" (same file) — without
/// filtering, this would just resurrect the self-shadow bug through a new path.
pub(crate) fn resolve_implicit_receiver_callee(
    indexer: &Indexer,
    receiver_base: &str,
    name: &str,
    from_uri: &Url,
    shape: CallShape,
) -> Vec<Location> {
    if let Some(loc) =
        implicit_receiver_extension_match(indexer, receiver_base, name, from_uri, shape)
    {
        return vec![loc];
    }
    implicit_receiver_member_match(indexer, receiver_base, name, from_uri, shape)
        .map(|loc| vec![loc])
        .unwrap_or_default()
}

/// The extension-in-scope half of [`resolve_implicit_receiver_callee`] — same
/// registry and in-scope check as `resolve_extension_in_scope`, plus a
/// `shape.accepts(...)` gate on each candidate's own declared arity (vararg
/// declarations are exempt: `param_counts` can't represent a vararg's true
/// unbounded upper end). Deliberately not `CallShape::accepts_symbol` — that
/// also exempts non-callable *kinds*, which would let a same-named property
/// wrongly satisfy any shape here; this is a selection loop picking the one
/// real candidate, not a rejection filter over an already-narrowed list.
fn implicit_receiver_extension_match(
    indexer: &Indexer,
    receiver_base: &str,
    name: &str,
    from_uri: &Url,
    shape: CallShape,
) -> Option<Location> {
    let mut cache_backed_only = 0usize;
    let entries =
        crate::indexer::jar::extension_entries_for(indexer, receiver_base, &mut cache_backed_only)?;
    let caller_file_data = indexer.files.get(from_uri.as_str());
    let caller_file_data_ref: Option<&FileData> = caller_file_data.as_deref().map(|v| v.as_ref());
    for entry in entries.iter() {
        if entry.name != name {
            continue;
        }
        let in_scope = crate::resolver::infer::extension_is_in_scope(
            entry.package.as_ref(),
            &entry.name,
            entry.container.as_ref(),
            entry.visibility,
            entry.file_uri == from_uri.as_str(),
            caller_file_data_ref,
        );
        if !in_scope {
            continue;
        }
        let Ok(uri) = Url::parse(&entry.file_uri) else {
            continue;
        };
        let symbol = indexer
            .files
            .get(&entry.file_uri)
            .or_else(|| indexer.jar_files.get(&entry.file_uri))
            .and_then(|fd| {
                fd.symbols
                    .iter()
                    .find(|s| {
                        crate::resolver::infer::extension_declaration_matches(
                            s,
                            name,
                            receiver_base,
                            entry.container.as_ref(),
                        )
                    })
                    .cloned()
            });
        let Some(symbol) = symbol else { continue };
        let is_vararg = symbol.params.contains("vararg ") || symbol.params.contains("vararg\t");
        if is_vararg || shape.accepts(symbol.param_counts.0, symbol.param_counts.1) {
            return Some(Location {
                uri,
                range: symbol.selection_range,
            });
        }
    }
    None
}

/// The member half of [`resolve_implicit_receiver_callee`] — resolves
/// `receiver_base` to its declaring file (import-aware, via the same
/// `resolve_symbol` the explicit-qualifier path already uses — this is what
/// makes a compiled-JAR-only receiver type work), then scans *every*
/// same-named symbol declared there (not just the first, unlike
/// `find_name_in_uri_after_line`) for one whose arity `shape` accepts.
fn implicit_receiver_member_match(
    indexer: &Indexer,
    receiver_base: &str,
    name: &str,
    from_uri: &Url,
    shape: CallShape,
) -> Option<Location> {
    for type_loc in resolve_symbol(indexer, receiver_base, None, from_uri) {
        let Some(symbol) = indexer
            .files
            .get(type_loc.uri.as_str())
            .or_else(|| indexer.jar_files.get(type_loc.uri.as_str()))
            .and_then(|fd| {
                fd.symbols
                    .iter()
                    .find(|s| {
                        s.name == name
                            && (s.params.contains("vararg ")
                                || s.params.contains("vararg\t")
                                || shape.accepts(s.param_counts.0, s.param_counts.1))
                    })
                    .cloned()
            })
        else {
            continue;
        };
        return Some(Location {
            uri: type_loc.uri,
            range: symbol.selection_range,
        });
    }
    None
}

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
fn resolve_qualified(
    indexer: &Indexer,
    name: &str,
    qualifier: &str,
    from_uri: &Url,
    io: ResolveIo,
) -> Vec<Location> {
    let segments: Vec<&str> = qualifier.split('.').collect();
    let root = segments[0];

    // ── `this.member` — search current file and its superclass hierarchy ──────
    if root == "this" {
        let locs = find_name_in_uri(indexer, name, from_uri.as_str());
        if !locs.is_empty() {
            return locs;
        }
        return resolve_from_class_hierarchy(indexer, name, from_uri);
    }

    // ── `super.member` — search superclass hierarchy only ────────────────────
    if root == "super" {
        return resolve_from_class_hierarchy(indexer, name, from_uri);
    }

    if root.starts_with_uppercase() {
        let root_base = root.last_segment();

        // Extension functions take precedence over member functions,
        // but only when they are in scope (same package or imported).
        let ext_locs = resolve_extension_in_scope(indexer, root_base, name, from_uri);
        if !ext_locs.is_empty() {
            return ext_locs;
        }

        // Then check member functions (same-file). Honors the caller's IO
        // policy — an IndexOnly caller (the resolution-accuracy benchmark's
        // own index-only path) must not spawn rg/fd resolving the qualifier
        // root any more than it may for a bare reference.
        let qual_locs = if matches!(io, ResolveIo::IndexOnly) {
            resolve_symbol_index_only(indexer, root, None, from_uri)
        } else {
            resolve_symbol(indexer, root, None, from_uri)
        };
        for qual_loc in &qual_locs {
            // `Foo.member` with `Foo` a class name (not a variable) can only reach a
            // companion-object member in Kotlin — never an instance member of `Foo`,
            // even if one shares the name. Try the companion first so a same-named
            // instance member declared earlier in the file can't shadow it.
            //
            // Only the single-segment `Foo.member` form names `root` as the
            // qualifying class. For a multi-segment qualifier like
            // `Outer.Inner.member`, `root` is `Outer` — not the class the member
            // is accessed on — so probing `Outer`'s companion would mis-resolve;
            // fall through to the nested-segment handling instead.
            if segments.len() == 1 {
                let companion_locs =
                    resolve_companion_member(indexer, name, root, qual_loc.uri.as_str());
                if !companion_locs.is_empty() {
                    return companion_locs;
                }
            }

            // Walk any remaining nested-type segments (`Event.OverdraftInput` has
            // one: `OverdraftInput`) to that specific nested class's own scope
            // before searching for `name`, so a same-named sibling member never
            // shadows the actually-requested nested type's own member.
            let mut anchor = qual_loc.clone();
            let mut anchor_class_name = root_base;
            let mut nested_segments_resolved = true;
            for &nested_segment in &segments[1..] {
                match find_name_scoped_to_container(indexer, nested_segment, &anchor) {
                    Some(location) => {
                        anchor = location;
                        anchor_class_name = nested_segment;
                    }
                    None => {
                        nested_segments_resolved = false;
                        break;
                    }
                }
            }
            if !nested_segments_resolved {
                continue;
            }

            // Every same-named candidate, not just the first match — `name`
            // may be an overloaded Java/Kotlin function, and collapsing to
            // one arbitrary overload here (before the caller's own
            // arity-based shape filtering ever runs) would make nearly
            // every real call site to a DIFFERENT overload resolve to
            // nothing (see `find_all_names_scoped_to_container`'s doc).
            let member_locs = find_all_names_scoped_to_container(indexer, name, &anchor);
            if !member_locs.is_empty() {
                return with_supertype_extension_fallback(
                    indexer,
                    member_locs,
                    anchor_class_name,
                    &anchor.uri,
                    name,
                    from_uri,
                );
            }

            // `anchor`'s own body doesn't declare `name` — it may live on a
            // superclass instead (e.g. `object Manager : AbstractManager<T>()`
            // inheriting `requireComponent`), the same situation the `this`/
            // `super` branches above already handle. Scoped to `anchor`'s own
            // class and declaring file, not `from_uri` — the qualifier and the
            // call site are commonly different files.
            let hierarchy_locs = resolve_from_class_hierarchy_scoped(
                indexer,
                name,
                anchor_class_name,
                &anchor.uri,
                from_uri,
            );
            if !hierarchy_locs.is_empty() {
                return hierarchy_locs;
            }

            // `anchor`'s own class has no member, inherited member, or
            // exact-key extension named `name` — check `anchor`'s ancestors.
            let supertype_ext_locs = resolve_extension_via_supertype_hierarchy(
                indexer,
                anchor_class_name,
                &anchor.uri,
                name,
                from_uri,
            );
            if !supertype_ext_locs.is_empty() {
                return supertype_ext_locs;
            }
        }
        // Extension functions may live in a different file than the receiver class.
        // Atomic promote+read (zero budget): `resolve_qualified` is on both the
        // goto-definition and the per-call-site diagnostics path.
        let root_base = root.last_segment();
        let mut cache_backed_only = 0usize;
        if let Some(entries) =
            crate::indexer::jar::extension_entries_for(indexer, root_base, &mut cache_backed_only)
        {
            for entry in entries.iter() {
                if entry.name == name {
                    if let Ok(uri) = Url::parse(&entry.file_uri) {
                        // Look up the symbol in the declaring file for accurate range.
                        let range = indexer
                            .files
                            .get(&entry.file_uri)
                            .or_else(|| indexer.jar_files.get(&entry.file_uri))
                            .and_then(|fd| {
                                fd.symbols
                                    .iter()
                                    .find(|s| s.name == name)
                                    .map(|s| s.selection_range)
                            })
                            .unwrap_or_default();
                        return vec![Location { uri, range }];
                    }
                }
            }
        }
        return vec![];
    }

    // ── Lowercase root: variable / parameter type inference ──────────────────
    let Some(start_type) = infer_variable_type(indexer, root, from_uri) else {
        return vec![];
    };
    // A nullable receiver resolves members from its underlying (non-null) class,
    // so drop any trailing `?` before resolving the type to a file — otherwise
    // `resolve_symbol("Confirmation?")` would find nothing.
    let start_type = start_type.strip_nullable();

    // `start_type` may be a dotted nested type like `Outer.Inner`.
    // Split into outer (for file resolution) and optional inner (nested class).
    let (outer_type, inner_type) = match start_type.find('.') {
        Some(dot) => (&start_type[..dot], Some(&start_type[dot + 1..])),
        None => (start_type, None),
    };

    // Resolve the variable's type to its source file.
    let type_locs = resolve_symbol(indexer, outer_type, None, from_uri);
    let mut current_file: Option<String> = type_locs.first().map(|l| l.uri.to_string());
    // The receiver's own base type name, tracked alongside `current_file` for
    // the in-scope extension-function fallback below — kept even when
    // `current_file` is `None` (a built-in/stdlib type like `String` has no
    // indexed declaration file, but can still have in-scope extensions).
    let mut current_type_base: String = outer_type.last_segment().to_string();

    // If there's a nested type component (e.g. `Factory` in `Outer.Factory`),
    // the members we want to search are inside that nested type.
    // We don't need to change `current_file` because nested types live in the
    // same file; instead we record each nested level as a trailing qualifier
    // segment to process. A deeply-nested type like `Scenes.Confirmation` must
    // be split per-level — searching for a literal `"Scenes.Confirmation"`
    // symbol finds nothing, since each nested class is indexed on its own name.
    let extra_segments: Vec<&str> = inner_type
        .map(|t| t.split('.').collect())
        .unwrap_or_default();

    // Traverse remaining qualifier segments (plus any from the nested type).
    for &seg in extra_segments.iter().chain(segments[1..].iter()) {
        let Some(ref uri) = current_file else {
            return vec![];
        };
        if seg.starts_with_uppercase() {
            // Nested class / companion object — likely in the same file.
            // Search current file first; fall back to a global resolve.
            let locs = find_name_in_uri(indexer, seg, uri);
            current_file = if !locs.is_empty() {
                locs.first().map(|l| l.uri.to_string())
            } else {
                resolve_symbol(indexer, seg, None, from_uri)
                    .first()
                    .map(|l| l.uri.to_string())
            };
            current_type_base = seg.to_string();
        } else {
            // Field access: infer the declared type of this field.
            let Some(field_type) = infer_field_type(indexer, uri, seg) else {
                return vec![];
            };
            let locs = resolve_symbol(indexer, &field_type, None, from_uri);
            current_file = locs.first().map(|l| l.uri.to_string());
            current_type_base = field_type.strip_nullable().last_segment().to_string();
        }
    }

    // Search the resolved type's file for the target member, then its
    // superclass/interface hierarchy — Kotlin member (including inherited)
    // resolution always shadows a same-named extension function, so both are
    // tried before falling to the extension-in-scope lookup below.
    if let Some(ref resolved_uri) = current_file {
        let locs = find_name_in_uri(indexer, name, resolved_uri);
        if !locs.is_empty() {
            return match Url::parse(resolved_uri) {
                Ok(parsed_uri) => with_supertype_extension_fallback(
                    indexer,
                    locs,
                    &current_type_base,
                    &parsed_uri,
                    name,
                    from_uri,
                ),
                Err(_) => locs,
            };
        }
        if let Ok(parsed_uri) = Url::parse(resolved_uri) {
            let hierarchy_locs = resolve_from_class_hierarchy(indexer, name, &parsed_uri);
            if !hierarchy_locs.is_empty() {
                return hierarchy_locs;
            }
        }
    }

    // No member or inherited member named `name` on the receiver's type (this
    // also covers built-in/stdlib receivers like `String`/`Int`, which have
    // no indexed declaration file at all, so `current_file` is `None`) — the
    // call may still be a same-named, receiver-scoped extension function
    // declared elsewhere in the workspace. Without this, callers fell
    // straight through to the receiver-blind global bare-name search, which
    // can't distinguish `String.toViewText` from an unrelated
    // `SomeEnum.toViewText` and simply declines when both exist — a real,
    // measured source of ambiguous member-call resolution (see the
    // 2026-08-26 resolution-accuracy investigation).
    resolve_extension_in_scope(indexer, &current_type_base, name, from_uri)
}

/// Step 1 — symbols defined in the same source file.
///
/// `shape` is `Some` only when the caller knows it's resolving a call's callee
/// (see [`resolve_callee_definition`]) — a same-file match whose arity provably
/// can't satisfy the call is dropped, so a same-named-but-wrong-arity enclosing
/// declaration doesn't shadow the real (often library) target. `None` preserves
/// today's pure name-match behaviour exactly, for every other caller.
fn resolve_local(
    indexer: &Indexer,
    name: &str,
    uri: &Url,
    shape: Option<CallShape>,
) -> Vec<Location> {
    indexer
        .files
        .get(uri.as_str())
        .map(|f| {
            f.symbols
                .iter()
                .filter(|symbol| {
                    symbol.name == name && shape.is_none_or(|shape| shape.accepts_symbol(symbol))
                })
                .map(|symbol| Location {
                    uri: uri.clone(),
                    range: symbol.selection_range,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `rg`-step counterpart to the arity check above: `rg` finds `location`
/// by blind text match, with no parsed symbol of its own, so this first has
/// to find the `SymbolEntry` `location` actually landed on — its file must
/// already be indexed (an `rg` hit in a file the index has never seen has no
/// `param_counts` to check against) and must contain a `name` symbol whose
/// range encloses the point `rg` reported. Fails open (keeps `location`)
/// whenever either lookup comes up empty, matching this module's existing
/// fail-open convention (see [`is_import_reachable`]) — arity-gating only
/// fires when it can be answered with confidence, never as a guess.
fn rg_location_satisfies_call_shape(
    indexer: &Indexer,
    location: &Location,
    name: &str,
    shape: CallShape,
) -> bool {
    let Some(file_data) = indexer.files.get(location.uri.as_str()) else {
        return true;
    };
    let Some(symbol) = file_data
        .symbols
        .iter()
        .find(|symbol| symbol.name == name && range_encloses(symbol.range, location.range))
    else {
        return true;
    };
    shape.accepts_symbol(symbol)
}

/// Package of the JAR symbol at `loc`, from the `jar_symbol_packages` side table.
/// JAR symbols use a synthetic range whose line number equals the symbol's index
/// within the jar's `FileData.symbols`, so the line indexes the package vector.
/// Returns `None` when unknown (no entry, or pre-per-symbol-package jar cache).
pub(crate) fn jar_symbol_package(indexer: &Indexer, loc: &Location) -> Option<String> {
    let packages = indexer.jar_symbol_packages.get(loc.uri.as_str())?;
    packages
        .get(loc.range.start.line as usize)
        .filter(|p| !p.is_empty())
        .cloned()
}

/// The enclosing-type chain named by a nested import, outermost-first.
///
/// `com.app.Contract.State.Idle` (symbol `Idle`) → `["Contract", "State"]`.
/// All segments before the imported `symbol`, restricted to type names (uppercase
/// first letter), so leading package segments and the symbol itself are dropped.
/// Returns an empty vec for top-level imports (no enclosing type).
fn import_container_chain(full_path: &str, symbol: &str) -> Vec<String> {
    let mut segments: Vec<&str> = full_path.split('.').collect();
    // Drop the trailing symbol segment (the import's leaf), then keep type segments.
    if segments.last() == Some(&symbol) {
        segments.pop();
    }
    segments
        .into_iter()
        .filter(|s| s.starts_with_uppercase())
        .map(|s| s.to_string())
        .collect()
}

/// The chain of enclosing container types (class/interface/object/enum/struct) for
/// the symbol declared at `loc`, outermost-first, looked up across workspace and
/// JAR files. Computed by range nesting so it handles arbitrarily deep nesting.
/// Empty when the file/symbol isn't found or the symbol is top-level.
fn enclosing_container_chain(indexer: &Indexer, loc: &Location) -> Vec<String> {
    let Some(file_data) = indexer.file_data_for(loc.uri.as_str()) else {
        return vec![];
    };
    let target = loc.range;
    let mut enclosing: Vec<&crate::types::SymbolEntry> = file_data
        .symbols
        .iter()
        .filter(|s| {
            crate::parser::is_container_kind(s.kind)
                // Exclude the symbol itself (a container can be the imported symbol).
                && s.selection_range != target
                && range_encloses(s.range, target)
        })
        .collect();
    // Outermost first: earliest start, latest end.
    enclosing.sort_by(|a, b| {
        pos_tuple(a.range.start)
            .cmp(&pos_tuple(b.range.start))
            .then_with(|| pos_tuple(b.range.end).cmp(&pos_tuple(a.range.end)))
    });
    enclosing.into_iter().map(|s| s.name.clone()).collect()
}

fn pos_tuple(p: tower_lsp::lsp_types::Position) -> (u32, u32) {
    (p.line, p.character)
}

/// Whether `outer` fully contains `inner` (start ≤ start and end ≥ end).
pub(crate) fn range_encloses(
    outer: tower_lsp::lsp_types::Range,
    inner: tower_lsp::lsp_types::Range,
) -> bool {
    pos_tuple(outer.start) <= pos_tuple(inner.start) && pos_tuple(inner.end) <= pos_tuple(outer.end)
}

/// Find `name` inside the companion object nested in `class_name`.
///
/// `Foo.member` with `Foo` a class name (not a variable) can only ever reach a
/// companion-object member in Kotlin — never an instance member of `Foo`, even
/// when one happens to share the name.
fn resolve_companion_member(
    indexer: &Indexer,
    name: &str,
    class_name: &str,
    file_uri: &str,
) -> Vec<Location> {
    let Ok(uri) = Url::parse(file_uri) else {
        return vec![];
    };
    let Some(file_data) = indexer.file_data_for(file_uri) else {
        return vec![];
    };

    if indexer.jar_files.contains_key(file_uri) {
        // A compiled JAR's synthetic FileData gives every symbol its own
        // one-line range keyed by sequential position in the sidecar's flat
        // entry list (see `build_jar_file_data`) — there is no real nesting
        // for range containment to discover, unlike a source-parsed file.
        // Match by container name instead: the sidecar's `entriesFromClass`
        // gives a companion's own class-declaration symbol `container ==
        // class_name`, and gives ITS members that companion's own bare name
        // as their container in turn — mirroring the exact shape
        // `members_for_jar_backed_type` (completion) already matches on.
        let companion_name = file_data
            .symbols
            .iter()
            .find(|symbol| {
                symbol.is_companion_object() && symbol.container.as_deref() == Some(class_name)
            })
            .map(|symbol| symbol.name.as_str());
        let Some(companion_name) = companion_name else {
            return vec![];
        };
        return file_data
            .symbols
            .iter()
            .filter(|symbol| {
                symbol.name == name && symbol.container.as_deref() == Some(companion_name)
            })
            .map(|symbol| Location {
                uri: uri.clone(),
                range: symbol.selection_range,
            })
            .collect();
    }

    // The class's full declaration range (not just its name's selection range) is
    // needed to tell which companion object belongs to it when a file has more
    // than one class.
    let Some(class_range) = file_data
        .symbols
        .iter()
        .find(|symbol| symbol.name == class_name && crate::parser::is_container_kind(symbol.kind))
        .map(|symbol| symbol.range)
    else {
        return vec![];
    };
    let Some(companion) = file_data
        .symbols
        .iter()
        .find(|symbol| symbol.is_companion_object() && range_encloses(class_range, symbol.range))
    else {
        return vec![];
    };
    file_data
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.name == name
                && symbol.range != companion.range
                && range_encloses(companion.range, symbol.range)
        })
        .map(|symbol| Location {
            uri: uri.clone(),
            range: symbol.selection_range,
        })
        .collect()
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
///        first for nested symbols (AccountPickerContract.kt before Event.kt).
///        Gated by `allow_fd`: the index-only policy passes `false` to stay
///        strictly in-memory (no subprocess spawns) while keeping sub-steps i–ii.
fn resolve_via_imports(indexer: &Indexer, name: &str, uri: &Url, allow_fd: bool) -> Vec<Location> {
    let imports: Vec<crate::types::ImportEntry> = match indexer.files.get(uri.as_str()) {
        Some(f) => f.imports.iter().filter(|i| !i.is_star).cloned().collect(),
        None => return vec![],
    };

    for imp in imports.iter().filter(|i| i.local_name == name) {
        // i) qualified index — exact FQN (works for top-level classes).
        //    `qualified` stores an interned `SymbolLoc`; reconstitute the
        //    `Location` here, at the return boundary.
        if let Some(sym_loc) = indexer.qualified.get(&imp.full_path) {
            match indexer.file_table.location(*sym_loc) {
                Some(loc) => return vec![loc],
                // Unreachable by construction: every SymbolLoc in `qualified`
                // was interned before insert and FileIds are never reused.
                // Loud in dev builds; release degrades to the fallback ladder
                // below rather than mis-resolving.
                None => debug_assert!(
                    false,
                    "qualified SymbolLoc has no file_table entry for {}",
                    imp.full_path
                ),
            }
        }

        // ii) short-name index filtered to the expected package.
        //     For `…AccountPickerContract.Event` the expected package is
        //     `…accountpicker` (all-lowercase prefix segments).
        //     This avoids returning an unrelated `Event` from another package.
        let short = imp.full_path.last_segment();
        let expected_pkg = import_package_prefix(&imp.full_path);
        // The enclosing-type chain named by a nested import, outermost-first:
        // `com.app.Contract.State.Idle` → ["Contract", "State"]. Classes can nest
        // arbitrarily deep, so we compare the *whole* chain rather than just the
        // immediate parent — `Contract.State.Sub.Idle` and `Contract.Event.Sub.Idle`
        // share the immediate container `Sub` but differ higher up.
        let expected_chain = import_container_chain(&imp.full_path, short);
        let mut all_locations: Vec<tower_lsp::lsp_types::Location> = Vec::new();
        if let Some(locs) = indexer.definitions.get(short) {
            // Reconstitute interned `SymbolLoc`s at this boundary.
            all_locations.extend(
                locs.iter()
                    .filter_map(|sym_loc| indexer.file_table.location(*sym_loc)),
            );
        }
        // Promote-before-read (zero budget): an imported name whose JAR is
        // Tier-1-only must become visible here — this exact read shipped the
        // first promote-AFTER-read ordering bug. Blocking IPC for imports
        // already happens at file open (per-import promotion), and this
        // helper also runs on keystroke/diagnostics paths, so only free
        // cache-backed promotions are allowed.
        let mut cache_backed_only = 0usize;
        crate::indexer::jar::ensure_jar_definitions_for(indexer, short, &mut cache_backed_only);
        if let Some(locs) = indexer.jar_definitions.get(short) {
            all_locations.extend(locs.iter().cloned());
        }
        if !all_locations.is_empty() {
            let mut filtered: Vec<_> = all_locations
                .iter()
                .filter(|loc| {
                    // Compiled-JAR (sidecar) symbols: filter by the sidecar's real
                    // per-symbol package (the `jar_symbol_packages` side table is
                    // populated only for compiled JARs). This keeps an
                    // `import a.b.c.remember` from also matching an unrelated
                    // `remember` in the Kotlin compiler / gradle plugin / KSP jars.
                    if let Some(pkg) = jar_symbol_package(indexer, loc) {
                        return pkg == expected_pkg || pkg.starts_with(&format!("{expected_pkg}."));
                    }
                    // Everything else — workspace, `sourcePaths` libraries, AND
                    // sources-JARs (which are `jar:…!/….kt` URIs but live in `files`
                    // with a real package) — filters by the file's package. Fail open
                    // when the package is unknown (e.g. compiled JAR on an older cache
                    // with no per-symbol package) so we never regress.
                    indexer
                        .files
                        .get(loc.uri.as_str())
                        .and_then(|f| f.package.clone())
                        .map(|p| p == expected_pkg || p.starts_with(&format!("{expected_pkg}.")))
                        .unwrap_or(true)
                })
                .cloned()
                .collect();

            // Nested-class disambiguation: when the import names an enclosing-type
            // chain (e.g. `Contract.State.Idle`), prefer candidates whose enclosing
            // container chain matches it. Two sealed classes in the same
            // package/interface can expose identically-named members (`State.Idle` vs
            // `Event.Idle`); the package filter alone keeps both, so go-to-definition
            // would jump to both. Only narrows when at least one candidate matches, so
            // a set that can't be container-resolved (e.g. JAR symbols whose synthetic
            // ranges don't line up with the symbol entry) is never emptied.
            if !expected_chain.is_empty() {
                let chain_matches: Vec<_> = filtered
                    .iter()
                    .filter(|loc| enclosing_container_chain(indexer, loc) == expected_chain)
                    .cloned()
                    .collect();
                if !chain_matches.is_empty() {
                    filtered = chain_matches;
                }
            }

            if !filtered.is_empty() {
                return filtered;
            }
        }

        // iii) on-demand fd + parse (indexing race or file never opened).
        //
        // Guard: skip when the import's package directory doesn't exist under
        // any source root.  A single stat() per import prevents spawning fd
        // processes for SDK/stdlib packages (android.os, androidx.*…) whose
        // sources are never present in the project tree.
        if allow_fd {
            let (root, source_roots, matcher) = indexer.rg_scope_for_path(None);
            if package_dir_in_source_roots(&imp.full_path, root.as_deref(), &source_roots) {
                let locs =
                    fd_find_and_parse(name, &imp.full_path, root.as_deref(), matcher.as_deref());
                if !locs.is_empty() {
                    return locs;
                }
            }
        }
    }
    vec![]
}

/// Step 3 — same-package visibility (no import needed in Kotlin).
///
/// Finds all indexed files sharing the same `package` declaration as `from_uri`
/// and searches their symbols.
fn resolve_same_package(indexer: &Indexer, name: &str, uri: &Url) -> Vec<Location> {
    // Get package name, release the dashmap ref immediately.
    let pkg: String = match indexer
        .files
        .get(uri.as_str())
        .and_then(|f| f.package.clone())
    {
        Some(p) => p,
        None => return vec![],
    };

    let peer_ids: Vec<crate::types::FileId> = match indexer.packages.get(&pkg) {
        Some(ids) => ids.clone(),
        None => return vec![],
    };

    let self_str = uri.as_str();
    for peer_id in &peer_ids {
        let Some(peer_url) = indexer.file_table.url(*peer_id) else {
            continue;
        };
        let peer_uri_str = peer_url.as_str();
        if peer_uri_str == self_str {
            continue;
        }
        if let Some(f) = indexer.files.get(peer_uri_str) {
            if let Some(sym) = f.symbols.iter().find(|s| s.name == name) {
                return vec![Location {
                    uri: (*peer_url).clone(),
                    range: sym.selection_range,
                }];
            }
        }
    }

    // Also check compiled JAR definitions for same-package symbols.
    // Promote-before-read (zero budget): serves all three resolution policies,
    // including keystroke/diagnostics paths — no blocking sidecar IPC here.
    let mut cache_backed_only = 0usize;
    crate::indexer::jar::ensure_jar_definitions_for(indexer, name, &mut cache_backed_only);
    if let Some(locs) = indexer.jar_definitions.get(name) {
        for loc in locs.iter() {
            // `jar_symbol_package` (the sidecar's real per-symbol package)
            // first, NOT `indexer.jar_files.get(...).package` — that whole-JAR
            // fallback is `build_jar_file_data`'s guess from the FIRST
            // class-like symbol's `detail` text, which the sidecar's
            // pure-Java fallback (`JavaClassVisitor`, e.g. Android's
            // AAPT-generated `R.jar`) never includes a package in (`"class
            // R"`, not `"class pkg.R"`) — so for a jar like that, this same-
            // package check could never fire at all without the per-symbol
            // table, regardless of how many symbols really live in `pkg`.
            if location_package(indexer, loc).as_ref() == Some(&pkg) {
                return vec![loc.clone()];
            }
        }
    }

    vec![]
}

/// Returns the first symbol named `name` found in the exact package `pkg`,
/// or an empty Vec if none is found.
fn symbols_in_package(indexer: &Indexer, name: &str, pkg: &str) -> Vec<Location> {
    find_symbol_in_package(indexer, name, pkg).map_or(vec![], |l| vec![l])
}

/// Scan all indexed files in `pkg` for the first symbol named `name`.
pub(crate) fn find_symbol_in_package(indexer: &Indexer, name: &str, pkg: &str) -> Option<Location> {
    let peer_ids: Vec<crate::types::FileId> = indexer
        .packages
        .get(pkg)
        .map(|ids| ids.clone())
        .unwrap_or_default();
    for peer_id in peer_ids {
        let Some(peer_url) = indexer.file_table.url(peer_id) else {
            continue;
        };
        if let Some(f) = indexer.files.get(peer_url.as_str()) {
            if let Some(sym) = f.symbols.iter().find(|s| s.name == name) {
                return Some(Location {
                    uri: (*peer_url).clone(),
                    range: sym.selection_range,
                });
            }
        }
    }

    // Also check compiled JAR definitions.
    // Promote-before-read (zero budget): star-import scans run per-name on
    // keystroke/diagnostics paths — no blocking sidecar IPC here.
    let mut cache_backed_only = 0usize;
    crate::indexer::jar::ensure_jar_definitions_for(indexer, name, &mut cache_backed_only);
    if let Some(locs) = indexer.jar_definitions.get(name) {
        for loc in locs.iter() {
            // `jar_symbol_package` (this location's own real per-symbol
            // package) first — a real JAR spans many packages, so
            // `FileData.package` (the whole synthetic file's first-symbol
            // guess, checked only as a fallback) is not necessarily this
            // specific symbol's own package. Same reasoning as
            // `is_denylisted_package_prefix`.
            let candidate_pkg = jar_symbol_package(indexer, loc).or_else(|| {
                indexer
                    .jar_files
                    .get(loc.uri.as_str())
                    .and_then(|f| f.package.clone())
            });
            if candidate_pkg.as_deref() == Some(pkg) {
                return Some(loc.clone());
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
fn resolve_star_imports(indexer: &Indexer, name: &str, uri: &Url) -> Vec<Location> {
    let star_pkgs: Vec<String> = match indexer.files.get(uri.as_str()) {
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
        let locs = symbols_in_package(indexer, name, &pkg);
        if !locs.is_empty() {
            return locs;
        }

        // b) rg scoped to the package directory for unindexed files
        let (root, _, matcher) = indexer.rg_scope_for_path(None);
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
fn resolve_from_class_hierarchy(indexer: &Indexer, name: &str, from_uri: &Url) -> Vec<Location> {
    resolve_from_class_hierarchy_scoped(indexer, name, "", from_uri, from_uri)
}

/// Like [`resolve_from_class_hierarchy`] but scoped to one specific class's
/// own declared supertypes (`start_class`) instead of every class declared in
/// `from_uri`'s file. Needed when the class and the caller can be different
/// files — `Foo.member()` where `Foo` is a type/object name, not `this`/`super`
/// (which are always resolved from inside the class they refer to, so the
/// unscoped whole-file walk was never wrong for those callers).
///
/// `start_uri` is where `start_class` is declared (commonly a JAR for a
/// library receiver type); `origin_uri` is the real call-site file, used only
/// for `walk_hierarchy`'s module-scoped ambiguity tie-break, which needs a
/// real `file://` path to map back to an owning module. The two coincide for
/// [`resolve_from_class_hierarchy`]'s callers (`this`/`super`, always resolved
/// from inside their own file) but not for [`resolve_qualified`]'s
/// `Foo.member()` callers, where `start_uri` is `Foo`'s own declaring file.
fn resolve_from_class_hierarchy_scoped(
    indexer: &Indexer,
    name: &str,
    start_class: &str,
    start_uri: &Url,
    origin_uri: &Url,
) -> Vec<Location> {
    // Deep enough for real Android/Kotlin hierarchies: app base classes often stack
    // several levels (`…Fragment → BaseFragment → … → androidx Fragment`) before the
    // library super that declares an inherited member like `requireActivity`. The
    // visited-set bounds total work regardless of depth.
    let results = walk_hierarchy(
        indexer,
        start_class,
        start_uri.as_str(),
        CallerContext {
            uri: Some(origin_uri.as_str()),
            cursor_line: None,
        },
        12,
        MAX_SYNC_JAR_PROMOTIONS_PER_HIERARCHY_WALK,
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

/// A same-named real member doesn't always satisfy the actual call's arity
/// (e.g. `navController.navigate(route = ...)`: a wrong-arity JVM member
/// `NavController.navigate(Uri)` vs. the wanted KTX extension
/// `NavController.navigate(route: String, ...)`). Appends the supertype-walk
/// extension after `member_locs` rather than replacing it — members still
/// win when arity-compatible (Kotlin's own precedence), but a shape-aware
/// caller now has the extension to fall back to instead of an empty result.
fn with_supertype_extension_fallback(
    indexer: &Indexer,
    member_locs: Vec<Location>,
    anchor_class_name: &str,
    anchor_uri: &Url,
    name: &str,
    from_uri: &Url,
) -> Vec<Location> {
    let supertype_ext_locs = resolve_extension_via_supertype_hierarchy(
        indexer,
        anchor_class_name,
        anchor_uri,
        name,
        from_uri,
    );
    if supertype_ext_locs.is_empty() {
        return member_locs;
    }
    let mut combined = member_locs;
    combined.extend(supertype_ext_locs);
    combined
}

/// Extension-lookup counterpart to [`resolve_from_class_hierarchy_scoped`]:
/// tried only when a member/inherited-member lookup on the concrete receiver
/// type already failed, so a real member always shadows a same-named
/// ancestor extension. `extension_by_receiver`/`resolve_extension_in_scope`
/// are an exact-string-key lookup on the receiver's own leaf type name — a
/// receiver like `String` (implements `CharSequence`) never matches an
/// extension keyed `"CharSequence"` without this walk, the single largest
/// measured component of the resolution-accuracy benchmark's ambiguous
/// (FilteredCandidate) bucket on a real corpus.
///
/// `origin_uri` is the real call-site file, not `start_uri` (`anchor_uri`,
/// commonly a JAR-backed receiver type) — `walk_hierarchy`'s module-scoped
/// tie-break needs a real `file://` origin to map back to an owning module,
/// which a `jar:` URI can never provide.
///
/// This walks the same chain `resolve_from_class_hierarchy_scoped` just
/// walked, at the same budget — not a doubled cost: `promote_candidates_bounded`
/// memoizes `materialized`/`materialization_failed` per JAR, so re-visiting
/// an ancestor the first walk already attempted is a free set lookup. This
/// walk's own budget only spends anything new on ancestors beyond wherever
/// the first walk's budget ran out — the deep multi-hop case (see the real
/// 4-hop `AppCompatActivity → … → Activity` shape PR #286 fixed for member
/// lookup) this fallback exists to reach.
fn resolve_extension_via_supertype_hierarchy(
    indexer: &Indexer,
    start_class: &str,
    start_uri: &Url,
    name: &str,
    origin_uri: &Url,
) -> Vec<Location> {
    // Breadth-first (`walk_hierarchy_breadth_first`), not `walk_hierarchy`'s
    // depth-first order: Kotlin's own extension resolution prefers the most
    // specific (nearest) applicable receiver type, and depth-first fully
    // explores one direct supertype's entire chain before ever touching a
    // SIBLING direct supertype — so with multiple direct supertypes (an
    // ordinary Kotlin shape, e.g. implementing several interfaces), a
    // farther ancestor down the first branch could otherwise outrank a
    // nearer, directly-implemented one down a sibling branch.
    let matches = walk_hierarchy_breadth_first(
        indexer,
        start_class,
        start_uri.as_str(),
        CallerContext {
            uri: Some(origin_uri.as_str()),
            cursor_line: None,
        },
        12,
        MAX_SYNC_JAR_PROMOTIONS_PER_HIERARCHY_WALK,
        // `super_name` is already the simple leaf name — `supertype_targets`
        // (hierarchy.rs) normalizes a fully-qualified supertype spelling
        // (`class Str : com.other.Seq`) before yielding it.
        |idx, super_name, _, _| resolve_extension_in_scope(idx, super_name, name, origin_uri),
    );
    // The breadth-first walk already stops at the nearest level with any
    // match, but two SIBLING supertypes at that same level could both have
    // one (a genuine tie Kotlin itself would flag as a compile error) —
    // take just the first, matching this function's single-location
    // contract rather than surfacing a spurious multi-candidate ambiguity.
    matches.into_iter().next().into_iter().collect()
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
    let Some(_guard) = crate::rg::try_acquire_rg_slot() else {
        log::debug!("rg_in_package_dir: at capacity, skipping {name}");
        return vec![];
    };
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

/// Returns `true` if the package directory derived from `import_path` exists as a
/// subdirectory of at least one search root.
///
/// `android.os.Bundle` → pkg_dir `android/os` → checks `{root}/android/os/`.
///
/// A single `stat()` call per root replaces the need for a hardcoded stdlib
/// blocklist: if the directory doesn't exist in the project tree, no fd/rg
/// subprocess can find anything there either.
///
/// Returns `true` (allow search) when the package prefix is empty or no roots
/// are available — the conservative fallback.
fn package_dir_in_source_roots(
    import_path: &str,
    root: Option<&std::path::Path>,
    source_roots: &[String],
) -> bool {
    let pkg = import_package_prefix(import_path);
    if pkg.is_empty() {
        return true;
    }
    let pkg_dir = pkg.replace('.', "/");
    let search_roots: Vec<&std::path::Path> = if !source_roots.is_empty() {
        source_roots
            .iter()
            .map(|s| std::path::Path::new(s.as_str()))
            .collect()
    } else if let Some(r) = root {
        vec![r]
    } else {
        return true;
    };
    search_roots.iter().any(|r| r.join(&pkg_dir).is_dir())
}

/// Returns `true` when `name` has an explicit non-star import in `uri` AND
/// that import's package directory is absent from every source root.
///
/// When both conditions hold, `resolve_via_imports` already exhausted all
/// source-tree lookups (qualified index + definitions index + fd) and came up
/// empty.  A project-wide `rg` scan of the same source tree cannot add anything.
fn import_package_absent_from_source_roots(
    indexer: &Indexer,
    name: &str,
    uri: &Url,
    root: Option<&std::path::Path>,
    source_roots: &[String],
) -> bool {
    let Some(file_data) = indexer.files.get(uri.as_str()) else {
        return false;
    };
    let Some(imp) = file_data
        .imports
        .iter()
        .find(|i| !i.is_star && i.local_name == name)
    else {
        return false;
    };
    !package_dir_in_source_roots(&imp.full_path, root, source_roots)
}

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

/// Kotlin's own "mapped types" — compiler-intrinsic built-in types with NO
/// compiled `.class` file in kotlin-stdlib's JAR at all. The Kotlin compiler
/// substitutes their real JVM platform-type equivalent directly into
/// bytecode, so a class-file-scanning indexer (this project's JAR sidecar)
/// can never find a class file that doesn't exist. Real corpus evidence: a
/// typical Android/Kotlin workspace has 13 same-named JAR/workspace
/// candidates for bare `String`, and NONE of them is the real class — see
/// `docs/superpowers/specs/2026-08-27-kotlin-builtin-type-platform-mapping-design.md`.
///
/// Deliberately narrow (`String`/`CharSequence`, the `kotlin.collections.*`
/// interfaces, and the 8 primitive scalar types, the ones directly
/// evidenced by measurement so far) — Kotlin has roughly 20 mapped types in
/// total (`Any`/`Throwable`/`Number`/`Comparable`/...), but adding the rest
/// speculatively, without real corpus evidence each one is actually hit,
/// would violate the same "evidenced-only, not a broad heuristic"
/// discipline [`DENYLISTED_PACKAGE_PREFIXES`] already established.
/// Extending this list is a mechanical follow-up once a real gap is
/// measured, not a redesign.
const KOTLIN_BUILTIN_TYPE_PLATFORM_EQUIVALENTS: &[(&str, &str)] = &[
    ("String", "java.lang.String"),
    ("CharSequence", "java.lang.CharSequence"),
    // kotlin.collections.* interfaces -- same "compiler-intrinsic mapped
    // type, no compiled .class anywhere in kotlin-stdlib's JAR" shape as
    // String/CharSequence above (verified the same way: `unzip -l
    // kotlin-stdlib-*.jar | grep List.class` etc. -> no output). Kotlin's
    // read-only/mutable pairs (`List`/`MutableList`, ...) are a compile-time
    // view over ONE real platform interface -- both map to the same target,
    // which is why e.g. `MutableList` and `List` share a value here.
    ("List", "java.util.List"),
    ("MutableList", "java.util.List"),
    ("Set", "java.util.Set"),
    ("MutableSet", "java.util.Set"),
    ("Map", "java.util.Map"),
    ("MutableMap", "java.util.Map"),
    ("Collection", "java.util.Collection"),
    ("MutableCollection", "java.util.Collection"),
    ("Iterable", "java.lang.Iterable"),
    ("MutableIterable", "java.lang.Iterable"),
    ("Iterator", "java.util.Iterator"),
    ("MutableIterator", "java.util.Iterator"),
    // Kotlin's 8 primitive scalar types -- same compiler-intrinsic shape:
    // verified none of `Int`/`Long`/`Double`/`Float`/`Boolean`/`Byte`/`Short`/`Char`
    // has a compiled `.class` file in kotlin-stdlib's JAR either. `Char` is
    // the one name mismatch (-> `Character`, not `Char`) -- handled the same
    // way `MutableList` -> `java.util.List` already is, by looking up the
    // platform type's own simple name rather than the original Kotlin one.
    ("Int", "java.lang.Integer"),
    ("Long", "java.lang.Long"),
    ("Double", "java.lang.Double"),
    ("Float", "java.lang.Float"),
    ("Boolean", "java.lang.Boolean"),
    ("Byte", "java.lang.Byte"),
    ("Short", "java.lang.Short"),
    ("Char", "java.lang.Character"),
];

/// Last-resort fallback for a Kotlin compiler-intrinsic built-in type name
/// (see [`KOTLIN_BUILTIN_TYPE_PLATFORM_EQUIVALENTS`]): every normal
/// resolution step already failed by the time any tail fallback calls this,
/// since a built-in type is never locally declared, explicitly imported, or
/// present in the workspace's own source tree — so this can only ever turn
/// an existing decline into a correct resolve, never introduce a wrong one.
///
/// Re-derives the Android SDK sources root via the already-existing
/// [`crate::workspace_json::detect_android_sdk_source_paths`] (no new
/// discovery mechanism — reuses Primitive B's own function) and, if the
/// expected `<root>/java/lang/String.java`-shaped file exists on disk,
/// indexes it on demand — the same `std::fs::read_to_string` +
/// `index_content` pattern `resolve_chain`'s own step 0.5 already uses for
/// "the caller's own file isn't indexed yet", just triggered by a known
/// built-in name instead. One-time cost per session per type: once indexed,
/// the file is permanently cached like any other, so this filesystem lookup
/// only ever runs for the first `String`/`CharSequence` resolution.
///
/// Scoped to Android projects for now — a plain JVM/non-Android Kotlin
/// project has no equivalent auto-detected `java.lang.*` source bundle;
/// locating a JDK's own bundled sources is a separate discovery problem,
/// not addressed here (see the design doc's explicit scope boundary).
pub(crate) fn resolve_kotlin_builtin_type_platform_equivalent(
    indexer: &Indexer,
    name: &str,
) -> Vec<Location> {
    let Some(&(_, platform_fqn)) = KOTLIN_BUILTIN_TYPE_PLATFORM_EQUIVALENTS
        .iter()
        .find(|&&(builtin, _)| builtin == name)
    else {
        return vec![];
    };
    // The platform type's own simple name, NOT `name` -- some entries
    // (`MutableList` -> `java.util.List`) map a Kotlin-only spelling onto a
    // real interface declared under a DIFFERENT simple name; searching the
    // target file for a symbol literally called "MutableList" would always
    // come up empty.
    let Some(platform_simple_name) = platform_fqn.rsplit('.').next() else {
        return vec![];
    };
    let Some(workspace_root) = indexer.workspace_root.get() else {
        return vec![];
    };
    let relative_path = platform_fqn.replace('.', "/") + ".java";
    for sdk_source_root in crate::workspace_json::detect_android_sdk_source_paths(&workspace_root) {
        let file_path = sdk_source_root.join(&relative_path);
        let Ok(file_uri) = Url::from_file_path(&file_path) else {
            continue;
        };
        let file_uri_str = file_uri.as_str();
        if !indexer.files.contains_key(file_uri_str) {
            let Ok(content) = std::fs::read_to_string(&file_path) else {
                continue;
            };
            indexer.index_content(&file_uri, &content);
        }
        let locs = find_name_in_uri(indexer, platform_simple_name, file_uri_str);
        if !locs.is_empty() {
            return locs;
        }
    }
    vec![]
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
    pub(crate) fn resolve_symbol_index_only(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
    ) -> Vec<Location> {
        resolve_symbol_index_only(self, name, qualifier, from_uri)
    }
    pub(crate) fn resolve_symbol_no_rg(&self, name: &str, from_uri: &Url) -> Vec<Location> {
        resolve_symbol_no_rg(self, name, from_uri)
    }

    /// Find `name` accessed through `qualifier`, restricted to the qualifier's
    /// own type: a real member (declared in the class body or inherited) and —
    /// when the qualifier root is a type name — an extension on that type, but
    /// never the unqualified bare-word fallback chain that the outer
    /// [`Indexer::resolve_symbol`] falls through to when the qualifier doesn't
    /// resolve. (It delegates to [`resolve_qualified`], which can surface an
    /// extension for an uppercase root.) Used by diagnostics that need a
    /// scoped, qualifier-anchored lookup rather than the global fallback — the
    /// caller is responsible for confirming membership when it must exclude
    /// extensions (see `is_member_of` in the nullable-dot-call diagnostic).
    pub(crate) fn resolve_member_only(
        &self,
        name: &str,
        qualifier: &str,
        from_uri: &Url,
    ) -> Vec<Location> {
        resolve_qualified(self, name, qualifier, from_uri, ResolveIo::Full)
    }
}
