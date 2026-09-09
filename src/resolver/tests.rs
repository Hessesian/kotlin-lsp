use super::shared_fixture_tests::gradle_cache_jar_uri;
use super::*;
use crate::indexer::{CallShape, Indexer};
use crate::parser::{parse_java, parse_kotlin};
use crate::stdlib::dot_completions_for;
use tower_lsp::lsp_types::{CompletionItem, CompletionItemTag, InsertTextFormat, Url};

fn uri(path: &str) -> Url {
    Url::parse(&format!("file:///test{path}")).unwrap()
}

fn import_file_candidates(import_path: &str) -> Vec<String> {
    import_file_stems(import_path)
        .into_iter()
        .flat_map(|stem| {
            crate::rg::SOURCE_EXTENSIONS
                .iter()
                .map(move |ext| format!("{stem}.{ext}"))
        })
        .collect()
}

// ── pure helpers ─────────────────────────────────────────────────────────

#[test]
fn package_prefix_standard() {
    assert_eq!(package_prefix("com.example.app.MyClass"), "com.example.app");
    assert_eq!(
        package_prefix("com.example.OuterClass.InnerClass"),
        "com.example"
    );
    assert_eq!(package_prefix("MyClass"), "");
    assert_eq!(package_prefix("com.example.Foo"), "com.example");
}

#[test]
fn import_package_prefix_strips_lowercase_leaf() {
    // A lowercase top-level function import (`a.b.c.stringResource`) is all-lowercase,
    // so the bare `package_prefix` would swallow the function name into the "package".
    // `import_package_prefix` drops the imported symbol's own (last) segment first.
    assert_eq!(
        import_package_prefix("androidx.compose.ui.res.stringResource"),
        "androidx.compose.ui.res"
    );
    // Class / nested-class imports (uppercase leaf) are unaffected.
    assert_eq!(
        import_package_prefix("com.example.OuterClass.InnerClass"),
        "com.example"
    );
    assert_eq!(import_package_prefix("com.example.Foo"), "com.example");
}

#[test]
fn import_candidates_top_level() {
    let c = import_file_candidates("com.example.Foo");
    assert_eq!(c[0], "Foo.kt");
    assert_eq!(c[1], "Foo.java");
    assert_eq!(c[2], "Foo.swift");
}

#[test]
fn import_candidates_nested() {
    let c = import_file_candidates("com.example.OuterClass.InnerClass");
    assert_eq!(c[0], "OuterClass.kt"); // outer class file tried first
    assert_eq!(c[1], "OuterClass.java");
    assert_eq!(c[2], "OuterClass.swift");
    assert_eq!(c[3], "InnerClass.kt");
    assert_eq!(c[4], "InnerClass.java");
    assert_eq!(c[5], "InnerClass.swift");
}

#[test]
fn import_candidates_deeply_nested() {
    let c = import_file_candidates("a.b.Outer.Middle.Inner");
    assert_eq!(c[0], "Middle.kt");
    assert_eq!(c[1], "Middle.java");
    assert_eq!(c[2], "Middle.swift");
    assert_eq!(c[3], "Inner.kt");
    assert_eq!(c[4], "Inner.java");
    assert_eq!(c[5], "Inner.swift");
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

// ── resolve_callee_definition (call-shape-aware) ──────────────────────────

#[test]
fn resolve_callee_definition_skips_wrong_arity_self_reference() {
    // The reported bug: `collect(block)` (1 arg) inside `Flow<T>.collect(scope,
    // block)` (2 required args) must not resolve to the enclosing declaration.
    let u = uri("/Flow.kt");
    let idx = Indexer::new();
    idx.index_content(
        &u,
        "package com.example\n\
class CoroutineScope\n\
fun <T : Any> Flow<T>.collect(scope: CoroutineScope, block: (T) -> Unit) {\n\
    collect(block)\n\
}\n",
    );
    let shape = CallShape {
        arg_count: 1,
        trailing_lambda: false,
    };
    let locs = resolve_callee_definition(&idx, "collect", &u, shape);
    assert!(
        !locs.iter().any(|location| location.uri == u),
        "a 1-arg call must not resolve to the 2-required-arg self declaration, got: {locs:?}"
    );
}

#[test]
fn resolve_callee_definition_preserves_same_arity_self_recursion() {
    let u = uri("/Factorial.kt");
    let idx = Indexer::new();
    idx.index_content(
        &u,
        "package com.example\n\
fun factorial(n: Int): Int {\n\
    return factorial(n - 1)\n\
}\n",
    );
    let shape = CallShape {
        arg_count: 1,
        trailing_lambda: false,
    };
    let locs = resolve_callee_definition(&idx, "factorial", &u, shape);
    assert!(
        locs.iter().any(|location| location.uri == u),
        "genuine same-arity self-recursion must still resolve to itself, got: {locs:?}"
    );
}

#[test]
fn resolve_callee_definition_picks_arity_matching_same_file_overload() {
    let u = uri("/Overload.kt");
    let idx = Indexer::new();
    idx.index_content(
        &u,
        "package com.example\n\
fun greet(name: String) {}\n\
fun greet(name: String, loudly: Boolean) {\n\
    greet(name)\n\
}\n",
    );
    let shape = CallShape {
        arg_count: 1,
        trailing_lambda: false,
    };
    let locs = resolve_callee_definition(&idx, "greet", &u, shape);
    let self_file_locs: Vec<_> = locs.iter().filter(|location| location.uri == u).collect();
    assert_eq!(
        self_file_locs.len(),
        1,
        "exactly one same-file overload should satisfy a 1-arg call, got: {locs:?}"
    );
    assert_eq!(
        self_file_locs[0].range.start.line, 1,
        "must resolve to the 1-arg overload (line 1), not the 2-arg one (line 2), got: {locs:?}"
    );
}

#[test]
fn resolve_callee_definition_keeps_vararg_self_reference() {
    // `param_counts` has no vararg awareness (a vararg param counts as exactly
    // one), so without the vararg guard this would wrongly filter itself out.
    let u = uri("/Vararg.kt");
    let idx = Indexer::new();
    idx.index_content(
        &u,
        "package com.example\n\
fun sumAll(vararg numbers: Int): Int {\n\
    return sumAll(1, 2, 3)\n\
}\n",
    );
    let shape = CallShape {
        arg_count: 3,
        trailing_lambda: false,
    };
    let locs = resolve_callee_definition(&idx, "sumAll", &u, shape);
    assert!(
        locs.iter().any(|location| location.uri == u),
        "a vararg function called with more args than its param_counts total \
         must still resolve to itself, got: {locs:?}"
    );
}

/// `resolve_chain`'s step 1 (`resolve_local`) is arity-aware, but step 5's
/// project-wide `rg` fallback is a blind text search with no arity of its own
/// — without `rg_location_satisfies_call_shape`, a wrong-arity self match that
/// step 1 correctly ruled out would come straight back here, since `rg`
/// re-finds the same declaration by pattern match alone. Needs a real file on
/// disk (matching `references_tests.rs`'s pattern) since `rg` operates on the
/// filesystem, not the in-memory index — with no import/same-package/star/
/// hierarchy candidate anywhere, this is the one file `rg` can find `collect`
/// in, so reaching step 5 is guaranteed.
///
/// Plain top-level function, not the reported bug's generic extension-function
/// form: `build_rg_pattern`'s extension-function branch requires `fun
/// Receiver.name` immediately after `fun`, so `fun <T : Any> Flow<T>.collect`
/// (a type-parameter list before the receiver) doesn't match it at all — `rg`
/// finds nothing there regardless of this fix, so it can't exercise step 5.
/// That's a pre-existing, separate limitation of the rg pattern, not something
/// this fix needs to also solve.
#[test]
fn resolve_callee_definition_rg_fallback_respects_arity_too() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let src = "package com.example\n\
fun collect(scope: Int, block: Int) {\n\
    collect(block)\n\
}\n";
    let path = root.join("Collect.kt");
    std::fs::write(&path, src).unwrap();
    let collect_uri = Url::from_file_path(&path).unwrap();

    let idx = Indexer::new();
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&collect_uri, src);

    let shape = CallShape {
        arg_count: 1,
        trailing_lambda: false,
    };
    let locs = resolve_callee_definition(&idx, "collect", &collect_uri, shape);
    assert!(
        !locs.iter().any(|location| location.uri == collect_uri),
        "the rg fallback must not resurrect the arity-filtered self \
         declaration, got: {locs:?}"
    );
}

/// `resolve_symbol_index_only` must never reach `resolve_chain`'s rg/fd
/// steps: a symbol reachable only via the project-wide rg tail fallback
/// (not indexed, not imported, not same-package) resolves under the `Full`
/// policy but must resolve to nothing under `IndexOnly`.
#[test]
fn resolve_symbol_index_only_never_spawns_rg_or_fd() {
    if !rg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let caller_src = "package com.example.caller\nfun use() { OnlyFindableByRg() }\n";
    let caller_path = root.join("Caller.kt");
    std::fs::write(&caller_path, caller_src).unwrap();
    let caller_uri = Url::from_file_path(&caller_path).unwrap();

    // Deliberately never indexed via `index_content` — only reachable via
    // rg's project-wide filesystem search (resolve_chain's step 5).
    let target_src = "package com.other\nclass OnlyFindableByRg\n";
    std::fs::write(root.join("Target.kt"), target_src).unwrap();

    let idx = Indexer::new();
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&caller_uri, caller_src);

    let full = resolve_symbol(&idx, "OnlyFindableByRg", None, &caller_uri);
    assert!(
        !full.is_empty(),
        "sanity check: the Full policy's rg tail fallback must find it"
    );

    let index_only = resolve_symbol_index_only(&idx, "OnlyFindableByRg", None, &caller_uri);
    assert!(
        index_only.is_empty(),
        "IndexOnly must never spawn rg, so it can't find a target only \
         reachable via the filesystem tail fallback, got: {index_only:?}"
    );
}

/// `ResolveIo::IndexOnly`'s tail must apply the same denylist-first
/// ambiguity tie-break `ResolveIo::HierarchyAmbiguitySafe` already uses
/// (`ambiguity_safe_tail_with_denylist`), not the older plain
/// unique-match-only rule. Real, measured bug: resolving a bare qualifier
/// root like `String` (as `resolve_qualified`'s uppercase branch does before
/// it can even attempt a member/extension lookup on it) hit exactly this
/// tail on the real Moneta corpus, found 13 candidates including a
/// `com.android.internal.*`-shaped decoy, and declined outright — which in
/// turn meant a real receiver type could never resolve at all, no matter how
/// correct any downstream member/extension lookup was.
#[test]
fn resolve_symbol_index_only_tail_applies_the_denylist_tie_break() {
    let idx = Indexer::new();
    // No `!/<entry>` suffix -- real compiled-JAR-derived `jar_definitions`
    // entries key the whole JAR as one synthetic file (see `gradle_cache_jar_uri`),
    // not a per-class entry path.
    let decoy_uri = Url::parse("jar:file:///decoy.jar").unwrap();
    let real_uri = Url::parse("jar:file:///real-stdlib.jar").unwrap();
    // Denylisted decoy indexed first, matching this file's established
    // convention of seeding the wrong candidate first.
    idx.jar_definitions.insert(
        "String".to_owned(),
        vec![
            tower_lsp::lsp_types::Location {
                uri: decoy_uri.clone(),
                range: Default::default(),
            },
            tower_lsp::lsp_types::Location {
                uri: real_uri.clone(),
                range: Default::default(),
            },
        ],
    );
    idx.jar_files.insert(
        decoy_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.android.internal.telephony".to_owned()),
            ..Default::default()
        }),
    );
    idx.jar_files.insert(
        real_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("kotlin".to_owned()),
            ..Default::default()
        }),
    );

    let host_uri = uri("/Host.kt");
    idx.index_content(&host_uri, "package com.pkg\n");

    let locs = resolve_symbol_index_only(&idx, "String", None, &host_uri);
    assert_eq!(
        locs,
        vec![tower_lsp::lsp_types::Location {
            uri: real_uri,
            range: Default::default(),
        }],
        "expected the com.android.internal.* decoy to be excluded, leaving \
         the real kotlin.String as a unique match, got {locs:?}"
    );
}

/// `module_scoped_tie_break` (the second tie-break in
/// `ambiguity_safe_tail_with_denylist`) only ever returned a non-empty
/// result when its own real-Gradle-dependency-data narrowing landed on
/// exactly one candidate — when it narrowed a 3-way ambiguity down to a real
/// 2-candidate subset ({A, C}, both proven dependencies of the calling
/// module, with B proven NOT to be one), that positive narrowing was
/// discarded entirely and the THIRD tie-break (`import_package_tie_break`)
/// was re-run against the ORIGINAL, unnarrowed 3-candidate set. That let
/// `import_package_tie_break` pick B — a candidate `module_scoped_tie_break`
/// had already proven structurally impossible (its JAR isn't even a
/// dependency of the calling module) — purely because the calling file
/// happens to import an unrelated sibling symbol from B's own package. A
/// structurally impossible answer must never be chosen over either
/// declining or narrowing further among the module-scoped survivors.
#[test]
fn module_scoped_narrowing_survives_into_import_package_tie_break() {
    let idx = Indexer::new();
    let a_uri = gradle_cache_jar_uri("com.example.a", "a-lib", "1.0.0");
    let b_uri = gradle_cache_jar_uri("com.example.b", "b-lib", "1.0.0");
    let c_uri = gradle_cache_jar_uri("com.example.c", "c-lib", "1.0.0");
    idx.jar_definitions.insert(
        "Foo".to_owned(),
        vec![
            tower_lsp::lsp_types::Location {
                uri: a_uri.clone(),
                range: Default::default(),
            },
            tower_lsp::lsp_types::Location {
                uri: b_uri.clone(),
                range: Default::default(),
            },
            tower_lsp::lsp_types::Location {
                uri: c_uri.clone(),
                range: Default::default(),
            },
        ],
    );
    idx.jar_files.insert(
        a_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.example.alib".to_owned()),
            ..Default::default()
        }),
    );
    idx.jar_files.insert(
        b_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.example.blib".to_owned()),
            ..Default::default()
        }),
    );
    idx.jar_files.insert(
        c_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.example.clib".to_owned()),
            ..Default::default()
        }),
    );

    // Real Gradle dependency data for the calling file's own module: {A, C}
    // are real dependencies, B is NOT.
    let mut dependencies_by_content_root: std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashSet<crate::cli::extract_sources::GradleMeta>,
    > = std::collections::HashMap::new();
    dependencies_by_content_root.insert(
        std::path::PathBuf::from("/test"),
        std::collections::HashSet::from([
            crate::cli::extract_sources::GradleMeta {
                group: "com.example.a".to_owned(),
                artifact: "a-lib".to_owned(),
                version: "1.0.0".to_owned(),
            },
            crate::cli::extract_sources::GradleMeta {
                group: "com.example.c".to_owned(),
                artifact: "c-lib".to_owned(),
                version: "1.0.0".to_owned(),
            },
        ]),
    );
    *idx.module_dependencies.write().unwrap() = dependencies_by_content_root;

    // The calling file imports an unrelated sibling symbol from B's own
    // package — the only signal `import_package_tie_break` has to go on —
    // but never depends on B itself per the real Gradle data above.
    let host_uri = uri("/Host.kt");
    idx.index_content(
        &host_uri,
        "package com.pkg\nimport com.example.blib.SomethingElse\n",
    );

    let locs = resolve_symbol_index_only(&idx, "Foo", None, &host_uri);
    assert!(
        !locs.iter().any(|l| l.uri == b_uri),
        "must never resolve to B: module-scoped narrowing already proved \
         B is not a real dependency of the calling module, so \
         import_package_tie_break must not be allowed to pick it back up \
         from the original, unnarrowed candidate set, got {locs:?}"
    );
}

/// Companion to the test above: when module-scoped narrowing leaves a real
/// positive subset ({A, C}) and `import_package_tie_break`, run against THAT
/// narrowed subset (not the original 3-candidate set), can itself narrow
/// further to a unique winner, the fixed tail must actually find it — not
/// just avoid the wrong answer, but reach the right one.
#[test]
fn module_scoped_narrowing_lets_import_package_tie_break_reach_a_correct_unique_answer() {
    let idx = Indexer::new();
    let a_uri = gradle_cache_jar_uri("com.example.a", "a-lib", "1.0.0");
    let b_uri = gradle_cache_jar_uri("com.example.b", "b-lib", "1.0.0");
    let c_uri = gradle_cache_jar_uri("com.example.c", "c-lib", "1.0.0");
    idx.jar_definitions.insert(
        "Foo".to_owned(),
        vec![
            tower_lsp::lsp_types::Location {
                uri: a_uri.clone(),
                range: Default::default(),
            },
            tower_lsp::lsp_types::Location {
                uri: b_uri.clone(),
                range: Default::default(),
            },
            tower_lsp::lsp_types::Location {
                uri: c_uri.clone(),
                range: Default::default(),
            },
        ],
    );
    idx.jar_files.insert(
        a_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.example.alib".to_owned()),
            ..Default::default()
        }),
    );
    idx.jar_files.insert(
        b_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.example.blib".to_owned()),
            ..Default::default()
        }),
    );
    idx.jar_files.insert(
        c_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.example.clib".to_owned()),
            ..Default::default()
        }),
    );

    let mut dependencies_by_content_root: std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashSet<crate::cli::extract_sources::GradleMeta>,
    > = std::collections::HashMap::new();
    dependencies_by_content_root.insert(
        std::path::PathBuf::from("/test"),
        std::collections::HashSet::from([
            crate::cli::extract_sources::GradleMeta {
                group: "com.example.a".to_owned(),
                artifact: "a-lib".to_owned(),
                version: "1.0.0".to_owned(),
            },
            crate::cli::extract_sources::GradleMeta {
                group: "com.example.c".to_owned(),
                artifact: "c-lib".to_owned(),
                version: "1.0.0".to_owned(),
            },
        ]),
    );
    *idx.module_dependencies.write().unwrap() = dependencies_by_content_root;

    // Imports a sibling symbol from C's own package only (not A's, not B's)
    // -- among the module-scoped survivors {A, C}, only C matches.
    let host_uri = uri("/Host.kt");
    idx.index_content(
        &host_uri,
        "package com.pkg\nimport com.example.clib.SomethingElse\n",
    );

    let locs = resolve_symbol_index_only(&idx, "Foo", None, &host_uri);
    assert_eq!(
        locs,
        vec![tower_lsp::lsp_types::Location {
            uri: c_uri,
            range: Default::default(),
        }],
        "expected import_package_tie_break, run against the module-scoped \
         narrowed subset {{A, C}}, to reach the unique correct answer C, \
         got {locs:?}"
    );
}

/// Companion to the two tests above, for the NEW tie-break inserted between
/// them: when module-scoped narrowing already excludes a candidate that
/// happens to live in a Kotlin default-import package (an edge case — real
/// dependency data proves it's not actually a dependency of this module,
/// even though its package would otherwise make it the "obviously right"
/// pick), `default_kotlin_import_tie_break` must not resurrect it from the
/// original, unnarrowed set — same discipline as `import_package_tie_break`
/// already has to respect.
#[test]
fn module_scoped_narrowing_survives_into_default_kotlin_import_tie_break() {
    let idx = Indexer::new();
    let a_uri = gradle_cache_jar_uri("com.example.a", "a-lib", "1.0.0");
    let b_uri = gradle_cache_jar_uri("com.example.b", "b-lib", "1.0.0");
    let c_uri = gradle_cache_jar_uri("com.example.c", "c-lib", "1.0.0");
    idx.jar_definitions.insert(
        "Foo".to_owned(),
        vec![
            tower_lsp::lsp_types::Location {
                uri: a_uri.clone(),
                range: Default::default(),
            },
            tower_lsp::lsp_types::Location {
                uri: b_uri.clone(),
                range: Default::default(),
            },
            tower_lsp::lsp_types::Location {
                uri: c_uri.clone(),
                range: Default::default(),
            },
        ],
    );
    // A lives in a real Kotlin default-import package — normally the
    // strongest possible signal — but real dependency data below proves A
    // is NOT a dependency of the calling module at all.
    idx.jar_files.insert(
        a_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("kotlin".to_owned()),
            ..Default::default()
        }),
    );
    idx.jar_files.insert(
        b_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.example.blib".to_owned()),
            ..Default::default()
        }),
    );
    idx.jar_files.insert(
        c_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.example.clib".to_owned()),
            ..Default::default()
        }),
    );

    // Real Gradle dependency data for the calling file's own module: {B, C}
    // are real dependencies, A is NOT.
    let mut dependencies_by_content_root: std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashSet<crate::cli::extract_sources::GradleMeta>,
    > = std::collections::HashMap::new();
    dependencies_by_content_root.insert(
        std::path::PathBuf::from("/test"),
        std::collections::HashSet::from([
            crate::cli::extract_sources::GradleMeta {
                group: "com.example.b".to_owned(),
                artifact: "b-lib".to_owned(),
                version: "1.0.0".to_owned(),
            },
            crate::cli::extract_sources::GradleMeta {
                group: "com.example.c".to_owned(),
                artifact: "c-lib".to_owned(),
                version: "1.0.0".to_owned(),
            },
        ]),
    );
    *idx.module_dependencies.write().unwrap() = dependencies_by_content_root;

    let host_uri = uri("/Host.kt");
    idx.index_content(&host_uri, "package com.pkg\n");

    let locs = resolve_symbol_index_only(&idx, "Foo", None, &host_uri);
    assert!(
        !locs.iter().any(|l| l.uri == a_uri),
        "must never resolve to A: module-scoped narrowing already proved \
         A is not a real dependency of the calling module, so \
         default_kotlin_import_tie_break must not be allowed to pick it \
         back up just because its package is a Kotlin default import, \
         got {locs:?}"
    );
}

/// `import_package_tie_break` must treat a star import (`import
/// com.example.blib.*`) as real narrowing evidence for its own exact
/// package, same as an explicit non-star sibling import — a wildcard import
/// is just as much proof the calling file has visibility into that package.
/// Before this fix the function filtered `is_star` imports out entirely, so
/// a file relying only on a wildcard import contributed zero evidence and
/// this tie-break declined outright (`vec![]`), losing an otherwise-unique
/// answer.
///
/// Calls `import_package_tie_break` directly rather than going through
/// `resolve_symbol_index_only` end-to-end (the pattern the sibling tests
/// above use): `resolve_chain`'s own earlier star-import step (step 4,
/// `find_in_star_imports`) already resolves any candidate whose *own* name
/// is directly found by scanning a star-imported package, before the
/// ambiguity tail is ever reached — so a same-name candidate set staged via
/// `jar_definitions` would be resolved by that earlier step regardless of
/// this fix, making the outer test a false positive. Calling the tie-break
/// directly isolates the exact narrowing logic under test.
#[test]
fn star_import_narrows_import_package_tie_break() {
    let idx = Indexer::new();
    let a_uri = gradle_cache_jar_uri("com.example.a", "a-lib", "1.0.0");
    let b_uri = gradle_cache_jar_uri("com.example.b", "b-lib", "1.0.0");
    idx.jar_files.insert(
        a_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.example.alib".to_owned()),
            ..Default::default()
        }),
    );
    idx.jar_files.insert(
        b_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.example.blib".to_owned()),
            ..Default::default()
        }),
    );

    // The calling file has ONLY a wildcard import of B's exact package — no
    // explicit sibling import at all.
    let host_uri = uri("/Host.kt");
    idx.index_content(&host_uri, "package com.pkg\nimport com.example.blib.*\n");

    let locations = vec![
        tower_lsp::lsp_types::Location {
            uri: a_uri,
            range: Default::default(),
        },
        tower_lsp::lsp_types::Location {
            uri: b_uri.clone(),
            range: Default::default(),
        },
    ];
    let narrowed = resolve::import_package_tie_break(&idx, &host_uri, locations);
    assert_eq!(
        narrowed,
        vec![tower_lsp::lsp_types::Location {
            uri: b_uri,
            range: Default::default(),
        }],
        "a wildcard import of B's exact package should narrow the ambiguity \
         to B, same as an explicit non-star sibling import would, got {narrowed:?}"
    );
}

/// Same guarantee as `resolve_symbol_index_only_never_spawns_rg_or_fd`, but
/// for a qualified lookup (`resolve_qualified`'s uppercase branch) — Copilot
/// review on PR #274: `resolve_qualified` resolved its qualifier root via the
/// always-`Full` `resolve_symbol`, regardless of the caller's own IO policy.
#[test]
fn resolve_symbol_index_only_never_spawns_rg_or_fd_for_a_qualified_root() {
    if !rg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let caller_src = "package com.example.caller\nfun use() { OnlyFindableByRg().member() }\n";
    let caller_path = root.join("Caller.kt");
    std::fs::write(&caller_path, caller_src).unwrap();
    let caller_uri = Url::from_file_path(&caller_path).unwrap();

    // Deliberately never indexed via `index_content` — the qualifier root
    // (`OnlyFindableByRg`) is only reachable via rg's project-wide filesystem
    // search (resolve_chain's step 5).
    let target_src = "package com.other\nclass OnlyFindableByRg { fun member() = Unit }\n";
    std::fs::write(root.join("Target.kt"), target_src).unwrap();

    let idx = Indexer::new();
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&caller_uri, caller_src);

    let full = resolve_symbol(&idx, "member", Some("OnlyFindableByRg"), &caller_uri);
    assert!(
        !full.is_empty(),
        "sanity check: the Full policy's rg tail fallback must find the qualifier root"
    );

    let index_only =
        resolve_symbol_index_only(&idx, "member", Some("OnlyFindableByRg"), &caller_uri);
    assert!(
        index_only.is_empty(),
        "IndexOnly must never spawn rg resolving the qualifier root either, got: {index_only:?}"
    );
}

// ── resolve_implicit_receiver_callee ───────────────────────────────────────

/// The reported bug: `collect(block)` inside `fun <T> Flow<T>.collect(scope,
/// block) { ... collect(block) }` has no import for any top-level `collect`
/// -- the real target is `Flow`'s own interface member (reachable here only
/// via Kotlin's implicit SAM conversion of `block` to `FlowCollector<T>`, a
/// `fun interface`). `resolve_implicit_receiver_callee` must find that
/// JAR-indexed member, not the arity-incompatible self-declaration (which is
/// itself a registered "extension in scope" on `Flow`, same file).
#[test]
fn resolve_implicit_receiver_callee_finds_jar_member_not_self() {
    use crate::types::{FileData, SourceSet, SymbolEntry, Visibility};
    use std::sync::Arc;

    let u = uri("/Flow.kt");
    let idx = Indexer::new();
    let src = "package com.example\n\
import kotlinx.coroutines.flow.Flow\n\
class CoroutineScope\n\
fun <T : Any> Flow<T>.collect(scope: CoroutineScope, block: (T) -> Unit) {\n\
    collect(block)\n\
}\n";
    idx.index_content(&u, src);

    // Fake JAR-indexed Flow interface member: collect(collector: FlowCollector<T>).
    let jar_uri_str = "jar:file:///fake-coroutines.jar!/Flow.kt".to_string();
    let jar_uri = Url::parse(&jar_uri_str).unwrap();
    let range = tower_lsp::lsp_types::Range {
        start: tower_lsp::lsp_types::Position {
            line: 0,
            character: 0,
        },
        end: tower_lsp::lsp_types::Position {
            line: 0,
            character: 7,
        },
    };
    let member = SymbolEntry {
        name: "collect".to_owned(),
        kind: tower_lsp::lsp_types::SymbolKind::METHOD,
        visibility: Visibility::Public,
        range,
        selection_range: range,
        detail: "suspend fun collect(collector: FlowCollector<T>)".to_owned(),
        container: Some("Flow".to_owned()),
        params: "collector: FlowCollector<T>".to_owned(),
        param_counts: (1, 1),
        cold: crate::types::pack_cold_fields(vec![], String::new(), String::new(), String::new()),
        trailing_lambda: false,
        deprecated: false,
    };
    let flow_type = SymbolEntry {
        name: "Flow".to_owned(),
        kind: tower_lsp::lsp_types::SymbolKind::INTERFACE,
        visibility: Visibility::Public,
        range,
        selection_range: range,
        detail: "interface Flow<T>".to_owned(),
        container: None,
        params: String::new(),
        param_counts: (0, 0),
        cold: crate::types::pack_cold_fields(vec![], String::new(), String::new(), String::new()),
        trailing_lambda: false,
        deprecated: false,
    };
    idx.jar_files.insert(
        jar_uri_str.clone(),
        Arc::new(FileData {
            symbols: vec![flow_type, member],
            source_set: SourceSet::Library,
            package: Some("kotlinx.coroutines.flow".to_owned()),
            lines: Arc::new(vec![]),
            ..Default::default()
        }),
    );
    idx.jar_definitions
        .entry("Flow".to_owned())
        .or_default()
        .push(tower_lsp::lsp_types::Location {
            uri: jar_uri.clone(),
            range,
        });

    let shape = CallShape {
        arg_count: 1,
        trailing_lambda: false,
    };
    let locs = resolve_implicit_receiver_callee(&idx, "Flow", "collect", &u, shape);
    assert_eq!(
        locs.len(),
        1,
        "must resolve to exactly the JAR member, got: {locs:?}"
    );
    assert_eq!(
        locs[0].uri, jar_uri,
        "must resolve to the JAR member, not the arity-incompatible \
         self-declaration, got: {locs:?}"
    );
}

// ── resolve_via_imports (qualified index) ────────────────────────────────

#[test]
fn resolve_via_explicit_import() {
    let src_uri = uri("/src/Source.kt");
    let def_uri = uri("/src/Target.kt");
    let idx = Indexer::new();
    idx.index_content(&def_uri, "package com.example\nclass Target");
    idx.index_content(
        &src_uri,
        "package com.example\nimport com.example.Target\nval x: Target = TODO()",
    );

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
    idx.index_content(
        &src_uri,
        "package com.example\nimport com.example.LongName as LN\nval x: LN = TODO()",
    );

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
    let _locs: Vec<_> = resolve_symbol(&idx, "A", None, &b_uri)
        .into_iter()
        .filter(|l| l.uri == a_uri)
        .collect();
    // If rg finds it that's fine, but same-package shouldn't (different packages)
    // We verify by checking the packages map didn't bridge pkg1 and pkg2
    assert!(
        idx.packages
            .get("com.example.pkg2")
            .map(|ids| !ids.contains(&idx.file_table.intern(&a_uri)))
            .unwrap_or(true),
        "pkg1 URI leaked into pkg2 packages map"
    );
}

/// Regression: `resolve_same_package`'s JAR branch used to check
/// `indexer.jar_files.get(loc.uri).package` — the whole-JAR fallback package
/// `build_jar_file_data` infers from the FIRST class-like symbol's `detail`
/// text. The sidecar's pure-Java fallback (`JavaClassVisitor` in
/// `KotlinClassIndexer.kt`, used for AAPT-generated classes like Android's
/// `R` — no Kotlin metadata) emits a *bare* class detail (`"class R"`, not
/// `"class pkg.R"` the way the Kotlin-metadata path does) — so that
/// whole-JAR inference always fails (no dot to split on) and the same-package
/// JAR check could never fire for an `R.jar`-shaped compiled JAR, no matter
/// how many symbols shared its real package. The fix reads each symbol's own
/// *real* package via `jar_symbol_package` (the sidecar's per-symbol `pkg`
/// side table — see `location_package`) instead.
#[test]
fn resolve_same_package_finds_jar_symbol_with_bare_class_detail() {
    use crate::sidecar::SidecarSymbol;

    let sym = |name: &str, kind: &str, container: &str, detail: &str, pkg: &str| SidecarSymbol {
        name: name.to_owned(),
        kind: kind.to_owned(),
        container: container.to_owned(),
        detail: detail.to_owned(),
        doc: String::new(),
        type_params: vec![],
        extension_receiver_type: String::new(),
        trailing_lambda: false,
        deprecated: false,
        pkg: pkg.to_owned(),
        top_level: container.is_empty(),
        supers: vec![],
    };
    let idx = Indexer::new();
    // Two Android modules, each with its own AAPT-generated `R.jar` in its
    // own package — the real-world shape (a real workspace has one such JAR
    // per module). Bare `detail` text throughout, exactly as
    // `JavaClassVisitor.visit()`/`visitField()` emit it for a pure-Java class
    // (no package prefix) — unlike the Kotlin-metadata path's `"class pkg.Name"`.
    crate::indexer::jar::populate_from_symbols(
        &idx,
        std::path::Path::new("/fake/commonui-R.jar"),
        &[sym("R", "class", "", "class R", "cz.moneta.commonui")],
    );
    crate::indexer::jar::populate_from_symbols(
        &idx,
        std::path::Path::new("/fake/other-R.jar"),
        &[sym("R", "class", "", "class R", "com.other.app")],
    );

    let caller_uri = uri("/cz/moneta/commonui/DrawableMap.kt");
    idx.index_content(&caller_uri, "package cz.moneta.commonui\nval x = R\n");

    let locs = resolve_symbol(&idx, "R", None, &caller_uri);
    assert!(
        !locs.is_empty(),
        "same-package resolution must find the JAR-indexed R even though \
         its whole-JAR fallback package is unknown (bare class detail)"
    );
    assert!(
        locs[0].uri.as_str().contains("commonui-R.jar"),
        "must resolve to the caller's OWN module's R (matching its real \
         per-symbol package), not an unrelated same-named R from another \
         module's jar — got {:?}",
        locs[0]
    );
}

// ── resolve_qualified (dot accessor) ────────────────────────────────────

#[test]
fn resolve_qualifier_dot_access() {
    let host_uri = uri("/Host.kt");
    let outer_uri = uri("/Outer.kt");
    let idx = Indexer::new();
    idx.index_content(
        &outer_uri,
        "package com.pkg\nclass Outer {\n  class Inner\n}",
    );
    idx.index_content(&host_uri, "package com.pkg\nval x: Outer.Inner = TODO()");

    // Cursor on "Inner" with qualifier "Outer"
    let locs = resolve_symbol(&idx, "Inner", Some("Outer"), &host_uri);
    assert!(!locs.is_empty(), "Inner not found via qualifier");
    assert_eq!(locs[0].uri, outer_uri);
}

#[test]
fn resolve_qualified_class_name_prefers_companion_member_over_instance_member() {
    // `Foo.fooFunc()` where `Foo` is a class name (not a variable) can only ever
    // call a companion-object member in Kotlin — an instance member of the same
    // name is not reachable through the class name. When a class has both, the
    // companion member must win, not whichever member happens to appear first
    // (by line) in the file.
    let host_uri = uri("/Host.kt");
    let foo_uri = uri("/Foo.kt");
    let idx = Indexer::new();
    idx.index_content(
        &foo_uri,
        concat!(
            "package com.pkg\n",
            "class Foo {\n",
            "  fun fooFunc() {}\n",
            "  companion object {\n",
            "    fun fooFunc() {}\n",
            "  }\n",
            "}\n",
        ),
    );
    idx.index_content(&host_uri, "package com.pkg\nfun caller() { Foo.fooFunc() }");

    let locs = resolve_symbol(&idx, "fooFunc", Some("Foo"), &host_uri);
    assert!(
        !locs.is_empty(),
        "fooFunc not found via class-name qualifier"
    );
    assert_eq!(
        locs[0].range.start.line, 4,
        "expected the companion's fooFunc (line 4), got line {}",
        locs[0].range.start.line
    );
}

#[test]
fn resolve_qualified_class_name_prefers_private_companion_member() {
    // The companion-object detection must not assume the declaration starts with
    // `companion object` — a `private` (or otherwise modified) companion is still
    // a companion. Regression for a detail-prefix check that missed modifiers.
    let host_uri = uri("/Host.kt");
    let foo_uri = uri("/Foo.kt");
    let idx = Indexer::new();
    idx.index_content(
        &foo_uri,
        concat!(
            "package com.pkg\n",
            "class Foo {\n",
            "  fun fooFunc() {}\n",
            "  private companion object {\n",
            "    fun fooFunc() {}\n",
            "  }\n",
            "}\n",
        ),
    );
    idx.index_content(&host_uri, "package com.pkg\nfun caller() { Foo.fooFunc() }");

    let locs = resolve_symbol(&idx, "fooFunc", Some("Foo"), &host_uri);
    assert!(
        !locs.is_empty(),
        "fooFunc not found via class-name qualifier"
    );
    assert_eq!(
        locs[0].range.start.line, 4,
        "expected the private companion's fooFunc (line 4), got line {}",
        locs[0].range.start.line
    );
}

/// Regression: the enum-member-synthesis fix originally gave `entries`/
/// `values`/`valueOf` the same `range` as their enclosing enum class, which
/// made `find_name_scoped_to_container`'s self-containment guard
/// (`symbol.range != container_symbol.range`) silently exclude them —
/// `Flavor.entries` resolved to nothing even though the symbol existed in
/// the table. Must resolve through the actual type-qualified lookup path,
/// not just be present in the symbol table (a symbol-table-only check
/// missed this bug the first time).
#[test]
fn enum_type_qualified_entries_values_valueof_resolve() {
    // No literal "entries"/"values"/"valueOf" text anywhere but the enum
    // declaration itself — `find_name_scoped_to_container`'s degenerate-
    // container fallback does a raw post-declaration text scan, which could
    // paper over a range-containment bug by matching a call-site occurrence
    // instead of the synthesized symbol actually being found.
    let uri = uri("/Flavor.kt");
    let idx = Indexer::new();
    idx.index_content(
        &uri,
        concat!(
            "package app\n",
            "enum class Flavor {\n",
            "  PROD, DEV\n",
            "}\n",
        ),
    );

    assert!(
        !resolve_symbol(&idx, "entries", Some("Flavor"), &uri).is_empty(),
        "Flavor.entries did not resolve"
    );
    assert!(
        !resolve_symbol(&idx, "values", Some("Flavor"), &uri).is_empty(),
        "Flavor.values did not resolve"
    );
    assert!(
        !resolve_symbol(&idx, "valueOf", Some("Flavor"), &uri).is_empty(),
        "Flavor.valueOf did not resolve"
    );
}

#[test]
fn resolve_qualified_class_name_prefers_named_companion_member() {
    // Same as above but with an explicitly named companion (`companion object
    // Factory`), which the tree-sitter query already captures as a `@name`d
    // symbol — confirm the fix covers both the named and anonymous forms.
    let host_uri = uri("/Host.kt");
    let foo_uri = uri("/Foo.kt");
    let idx = Indexer::new();
    idx.index_content(
        &foo_uri,
        concat!(
            "package com.pkg\n",
            "class Foo {\n",
            "  fun fooFunc() {}\n",
            "  companion object Factory {\n",
            "    fun fooFunc() {}\n",
            "  }\n",
            "}\n",
        ),
    );
    idx.index_content(&host_uri, "package com.pkg\nfun caller() { Foo.fooFunc() }");

    let locs = resolve_symbol(&idx, "fooFunc", Some("Foo"), &host_uri);
    assert!(
        !locs.is_empty(),
        "fooFunc not found via class-name qualifier"
    );
    assert_eq!(
        locs[0].range.start.line, 4,
        "expected the named companion's fooFunc (line 4), got line {}",
        locs[0].range.start.line
    );
}

#[test]
fn resolve_deep_qualifier_chain() {
    // A.B.C.D cursor on D → qualifier = "A.B.C"
    // resolve_qualified should resolve root "A", find its file, locate "D" in it.
    let host_uri = uri("/Host.kt");
    let root_uri = uri("/Root.kt");
    let idx = Indexer::new();
    // Root.kt defines class Root with nested class Deep
    idx.index_content(
        &root_uri,
        "package com.pkg\nclass Root {\n  class Mid {\n    class Deep\n  }\n}",
    );
    idx.index_content(&host_uri, "package com.pkg\nval x: Root.Mid.Deep = TODO()");

    // qualifier = "Root.Mid" (full chain minus last segment), word = "Deep"
    let locs = resolve_symbol(&idx, "Deep", Some("Root.Mid"), &host_uri);
    assert!(!locs.is_empty(), "Deep not found via full qualifier chain");
    assert_eq!(locs[0].uri, root_uri);
}

#[test]
fn resolve_qualified_chain_scopes_a_colliding_middle_segment_to_its_own_outer_type() {
    // Two sibling top-level types each declare a nested `Sub` with its own
    // `target`. Resolving `Other.Sub.target` must walk to Other's own Sub, not
    // Event's Sub (declared first in the file) via an unscoped mid-chain lookup.
    let host_uri = uri("/Events.kt");
    let idx = Indexer::new();
    idx.index_content(
        &host_uri,
        concat!(
            "package com.pkg\n",
            "sealed interface Event {\n",
            "  sealed interface Sub {\n",
            "    val target: Int\n",
            "  }\n",
            "}\n",
            "sealed interface Other {\n",
            "  sealed interface Sub {\n",
            "    val target: Int\n",
            "  }\n",
            "}\n",
        ),
    );

    let locs = resolve_symbol(&idx, "target", Some("Other.Sub"), &host_uri);
    assert!(!locs.is_empty(), "Other.Sub.target not resolved");
    assert_eq!(
        locs[0].range.start.line, 8,
        "must resolve to Other's own Sub.target (line 8), not Event's (line 3)"
    );
}

#[test]
fn resolve_qualified_uppercase_root_falls_back_to_class_hierarchy() {
    // `Manager.requireComponent()` — `requireComponent` is declared only on
    // `Manager`'s generic superclass (in a different file), never overridden
    // by `Manager` itself. `resolve_qualified`'s uppercase branch must fall
    // back to `resolve_from_class_hierarchy` after its own candidate loop
    // comes up empty, the same way the `this`/`super` branches already do two
    // cases up. Kept in a separate file from `Manager` so the per-candidate
    // loop's own same-file fallback (`find_name_scoped_to_container` →
    // `find_name_in_uri_after_line`'s any-symbol-with-this-name rescue) can't
    // accidentally satisfy this test for an unrelated reason.
    let abstract_manager_uri = uri("/AbstractManager.kt");
    let manager_uri = uri("/Manager.kt");
    let idx = Indexer::new();
    idx.index_content(
        &abstract_manager_uri,
        concat!(
            "package com.example\n",
            "abstract class AbstractManager<T> {\n",
            "  fun requireComponent(): T = TODO()\n",
            "}\n",
        ),
    );
    idx.index_content(
        &manager_uri,
        "package com.example\nobject Manager : AbstractManager<String>()\n",
    );

    let locs = resolve_symbol(&idx, "requireComponent", Some("Manager"), &manager_uri);
    assert!(
        !locs.is_empty(),
        "requireComponent not found via Manager's superclass hierarchy"
    );
    assert_eq!(locs[0].uri, abstract_manager_uri);
    assert_eq!(
        locs[0].range.start.line, 2,
        "must resolve to AbstractManager's requireComponent (line 2), got {}",
        locs[0].range.start.line
    );
}

#[test]
fn resolve_qualified_uppercase_root_hierarchy_fallback_ignores_unrelated_sibling() {
    // Same fixture as `resolve_qualified_uppercase_root_falls_back_to_class_hierarchy`,
    // plus a same-named method on an unrelated class that is not one of
    // `Manager`'s declared supertypes. The hierarchy fallback must stay scoped
    // to `Manager`'s actual `: AbstractManager<...>` relationship, not perform
    // a blanket by-name rescue across the rest of the indexed workspace.
    let abstract_manager_uri = uri("/AbstractManager.kt");
    let manager_uri = uri("/Manager.kt");
    let unrelated_uri = uri("/UnrelatedThing.kt");
    let idx = Indexer::new();
    idx.index_content(
        &abstract_manager_uri,
        concat!(
            "package com.example\n",
            "abstract class AbstractManager<T> {\n",
            "  fun requireComponent(): T = TODO()\n",
            "}\n",
        ),
    );
    idx.index_content(
        &manager_uri,
        "package com.example\nobject Manager : AbstractManager<String>()\n",
    );
    idx.index_content(
        &unrelated_uri,
        concat!(
            "package com.example\n",
            "class UnrelatedThing {\n",
            "  fun requireComponent(): Int = 0\n",
            "}\n",
        ),
    );

    let locs = resolve_symbol(&idx, "requireComponent", Some("Manager"), &manager_uri);
    assert!(
        !locs.is_empty(),
        "requireComponent not found via Manager's superclass hierarchy"
    );
    assert_eq!(
        locs[0].uri, abstract_manager_uri,
        "must resolve to AbstractManager's requireComponent, not UnrelatedThing's, got {:?}",
        locs[0]
    );
}

#[test]
fn resolve_qualified_uppercase_root_hierarchy_fallback_reaches_jar_superclass() {
    // The scout report's Risks warning: `resolve_from_class_hierarchy`'s
    // `walk_hierarchy` is JAR-promotion-aware, so the fallback can walk into a
    // JAR-derived (compiled dependency) superclass, not just workspace-source
    // ones. JAR-derived stub symbols commonly have `.range == .selection_range`
    // (no real body span) — confirm resolution still succeeds rather than
    // silently failing or panicking on that degenerate range.
    use crate::sidecar::SidecarSymbol;

    let sym = |name: &str, container: &str| SidecarSymbol {
        name: name.to_owned(),
        kind: if container.is_empty() { "class" } else { "fun" }.to_owned(),
        container: container.to_owned(),
        detail: format!("{name}()"),
        doc: String::new(),
        type_params: vec![],
        extension_receiver_type: String::new(),
        trailing_lambda: false,
        deprecated: false,
        pkg: "com.lib".to_owned(),
        top_level: container.is_empty(),
        supers: vec![],
    };
    let idx = Indexer::new();
    crate::indexer::jar::populate_from_symbols(
        &idx,
        std::path::Path::new("/fake/abstract-manager.jar"),
        &[
            sym("AbstractManager", ""),
            sym("requireComponent", "AbstractManager"),
        ],
    );

    let host_uri = uri("/Manager.kt");
    idx.index_content(
        &host_uri,
        "package com.example\nobject Manager : AbstractManager()\n",
    );

    let locs = resolve_symbol(&idx, "requireComponent", Some("Manager"), &host_uri);
    assert!(
        !locs.is_empty(),
        "requireComponent not found via Manager's JAR-derived superclass"
    );
    assert!(
        locs[0].uri.as_str().contains("abstract-manager.jar"),
        "expected the JAR-derived AbstractManager.requireComponent, got {:?}",
        locs[0]
    );
}

/// `Resolver::field_type`'s supertype walk (`find_field_type_via_supertypes`)
/// is JAR-promotion-aware the same way `resolve_from_class_hierarchy` is
/// above, so it can walk into a JAR-derived superclass whose stub symbols
/// carry the same degenerate `.range == .selection_range` (no real body
/// span). `find_field_type_in_class_impl`'s symbol-table fallback (added
/// alongside the nested-type receiver fix, see
/// `indexer::jar_tests::nested_type_member_access_resolves_through_outer_type`)
/// now reads a JAR class's own member *property* straight from its indexed
/// `detail` text — the point of this test is now that the walk resolves the
/// inherited property AND still terminates cleanly instead of panicking on
/// the JAR stub's degenerate range.
#[test]
fn catalog_field_type_supertype_walk_reaches_jar_superclass_without_panicking() {
    use crate::sidecar::SidecarSymbol;

    let sym = |name: &str, kind: &str, container: &str| SidecarSymbol {
        name: name.to_owned(),
        kind: kind.to_owned(),
        container: container.to_owned(),
        detail: format!("val {name}: String"),
        doc: String::new(),
        type_params: vec![],
        extension_receiver_type: String::new(),
        trailing_lambda: false,
        deprecated: false,
        pkg: "com.lib".to_owned(),
        top_level: container.is_empty(),
        supers: vec![],
    };
    let idx = Indexer::new();
    crate::indexer::jar::populate_from_symbols(
        &idx,
        std::path::Path::new("/fake/abstract-viewmodel.jar"),
        &[
            sym("AbstractViewModel", "class", ""),
            sym("uiState", "val", "AbstractViewModel"),
        ],
    );

    let host_uri = uri("/ConcreteViewModel.kt");
    idx.index_content(
        &host_uri,
        "package com.example\nclass ConcreteViewModel : AbstractViewModel()\n",
    );

    let result =
        crate::resolver::Resolver::field_type(&idx, "ConcreteViewModel", "uiState", &host_uri);
    assert_eq!(
        result.map(|(type_name, _)| type_name).as_deref(),
        Some("String"),
        "walk into the JAR-derived superclass must resolve uiState's type \
         from its indexed detail text without panicking on the JAR stub's \
         degenerate range"
    );
}

#[test]
fn resolve_qualified_uppercase_root_hierarchy_fallback_works_from_a_different_call_site_file() {
    // Same fixture as `resolve_qualified_uppercase_root_falls_back_to_class_hierarchy`,
    // but resolved `from` a third file that neither declares `Manager` nor
    // `AbstractManager` — the realistic shape (`Manager.requireComponent()`
    // called from many files, declared in exactly one). The hierarchy fallback
    // must walk from `Manager`'s own declaring file, not the call site.
    let abstract_manager_uri = uri("/AbstractManager.kt");
    let manager_uri = uri("/Manager.kt");
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(
        &abstract_manager_uri,
        concat!(
            "package com.example\n",
            "abstract class AbstractManager<T> {\n",
            "  fun requireComponent(): T = TODO()\n",
            "}\n",
        ),
    );
    idx.index_content(
        &manager_uri,
        "package com.example\nobject Manager : AbstractManager<String>()\n",
    );
    idx.index_content(
        &caller_uri,
        "package com.example\nfun use() { Manager.requireComponent() }\n",
    );

    let locs = resolve_symbol(&idx, "requireComponent", Some("Manager"), &caller_uri);
    assert!(
        !locs.is_empty(),
        "requireComponent not found via Manager's superclass hierarchy from a different call-site file"
    );
    assert_eq!(locs[0].uri, abstract_manager_uri);
}

#[test]
fn resolve_nested_type_via_variable_annotation() {
    // `val factory: DashboardProductsReducer.Factory` — goto-def of `factory.create(...)`
    // should navigate to the `create` fun inside the `Factory` interface.
    let host_uri = uri("/Host.kt");
    let reducer_uri = uri("/DashboardProductsReducer.kt");
    let idx = Indexer::new();
    idx.index_content(
        &reducer_uri,
        concat!(
            "package com.pkg\n",
            "class DashboardProductsReducer {\n",
            "  interface Factory {\n",
            "    fun create(scope: Any): DashboardProductsReducer\n",
            "  }\n",
            "}\n",
        ),
    );
    idx.index_content(
        &host_uri,
        concat!(
            "package com.pkg\n",
            "val factory: DashboardProductsReducer.Factory = TODO()\n",
            "fun foo() { factory.create(this) }\n",
        ),
    );

    // Qualifier = "factory" (lowercase), word = "create"
    let locs = resolve_symbol(&idx, "create", Some("factory"), &host_uri);
    assert!(!locs.is_empty(), "create not found via nested type Factory");
    assert_eq!(locs[0].uri, reducer_uri);
}

#[test]
fn resolve_qualified_extension_falls_back_to_supertype_when_receiver_type_has_no_own_match() {
    // `receiver.toViewText()` where `receiver`'s static type is `Str`, which
    // implements `Seq`, and `toViewText` is declared as an extension on
    // `Seq`, not `Str` itself. `extension_by_receiver` is an exact-string-key
    // lookup on the receiver's own leaf type name (see its own doc comment),
    // so a plain `resolve_extension_in_scope(idx, "Str", ...)` finds nothing
    // -- this must instead walk `Str`'s supertype hierarchy and find the
    // extension declared on its ancestor `Seq`.
    //
    // This mirrors a real, measured Moneta bug: `fun CharSequence?.
    // toViewText()` never resolved for a `String` receiver (`String`
    // implements `CharSequence`) via this exact mechanism -- the single
    // largest component (~23%) of the resolution-accuracy benchmark's
    // ambiguous (FilteredCandidate) bucket on the real corpus.
    //
    // Critically, `toViewText` is declared in a THIRD file, separate from
    // `Seq`'s own declaration -- the normal real-world shape (an extension
    // almost never lives in the same file as the class it extends). Colocating
    // them would let `resolve_from_class_hierarchy_scoped`'s existing
    // `find_name_in_uri` (a blunt whole-file name scan with no
    // extension-vs-member awareness) accidentally find it as a false
    // positive, masking the actual bug this test targets.
    let host_uri = uri("/Host.kt");
    let str_uri = uri("/Str.kt");
    let seq_uri = uri("/Seq.kt");
    let ext_uri = uri("/SeqExtensions.kt");
    let idx = Indexer::new();
    idx.index_content(&seq_uri, "package com.pkg\ninterface Seq\n");
    idx.index_content(&str_uri, "package com.pkg\nclass Str : Seq\n");
    idx.index_content(
        &ext_uri,
        "package com.pkg\nfun Seq.toViewText(): String = TODO()\n",
    );
    idx.index_content(
        &host_uri,
        "package com.pkg\nfun foo(receiver: Str) { receiver.toViewText() }\n",
    );

    // `receiver_type: Some("Str")` mirrors exactly what `resolve_identity`
    // passes in production -- an already-resolved, capitalized type name.
    let locs = resolve_symbol(&idx, "toViewText", Some("Str"), &host_uri);
    assert!(
        !locs.is_empty(),
        "toViewText declared on supertype Seq must be found via a Str receiver"
    );
    assert_eq!(locs[0].uri, ext_uri);
}

#[test]
fn resolve_qualified_supertype_extension_fallback_prefers_the_nearest_ancestor() {
    // Copilot review finding (real): `resolve_extension_via_supertype_hierarchy`
    // used `walk_hierarchy`'s full collected `Vec`, returning EVERY matching
    // ancestor extension across the whole chain, not just the nearest one.
    // Kotlin's own extension resolution prefers the most specific applicable
    // receiver type -- if `Str : Near : Far` and BOTH `Near` and `Far` (an
    // ancestor of `Near`) declare their own `toViewText` extension, a `Str`
    // receiver must resolve to `Near`'s (the nearer, more specific one), not
    // return both as if genuinely ambiguous.
    let host_uri = uri("/Host.kt");
    let str_uri = uri("/Str.kt");
    let near_uri = uri("/Near.kt");
    let far_uri = uri("/Far.kt");
    let near_ext_uri = uri("/NearExtensions.kt");
    let far_ext_uri = uri("/FarExtensions.kt");
    let idx = Indexer::new();
    idx.index_content(&far_uri, "package com.pkg\ninterface Far\n");
    idx.index_content(&near_uri, "package com.pkg\ninterface Near : Far\n");
    idx.index_content(&str_uri, "package com.pkg\nclass Str : Near\n");
    idx.index_content(
        &far_ext_uri,
        "package com.pkg\nfun Far.toViewText(): String = TODO()\n",
    );
    idx.index_content(
        &near_ext_uri,
        "package com.pkg\nfun Near.toViewText(): String = TODO()\n",
    );
    idx.index_content(
        &host_uri,
        "package com.pkg\nfun foo(receiver: Str) { receiver.toViewText() }\n",
    );

    let locs = resolve_symbol(&idx, "toViewText", Some("Str"), &host_uri);
    assert_eq!(
        locs.len(),
        1,
        "expected exactly the nearest ancestor's extension, not every \
         ancestor's, got {locs:?}"
    );
    assert_eq!(
        locs[0].uri, near_ext_uri,
        "expected Near's toViewText (the nearer, more specific ancestor), \
         got a location in {:?}",
        locs[0].uri
    );
}

#[test]
fn resolve_qualified_supertype_extension_fallback_prefers_the_nearer_of_two_direct_supertypes() {
    // Copilot review follow-up (real, distinct from the single-chain case
    // above): depth-first traversal's "first collected" match is NOT always
    // the nearest one when a class has MULTIPLE direct supertypes -- an
    // entirely ordinary Kotlin shape (implementing several interfaces).
    // `Str : First, Second` where `First` (direct, hop 1) has no match of
    // its own but its OWN ancestor `DeepAncestor` (hop 2) does, while
    // `Second` (ALSO direct, hop 1) has its own matching extension.
    // Depth-first fully explores `First`'s entire chain (finding
    // `DeepAncestor`'s hop-2 match) before ever reaching `Second` at all --
    // so a naive "first in the collected list" pick would wrongly prefer
    // the FARTHER `DeepAncestor` over the nearer, direct `Second`.
    let host_uri = uri("/Host.kt");
    let str_uri = uri("/Str.kt");
    let first_uri = uri("/First.kt");
    let deep_ancestor_uri = uri("/DeepAncestor.kt");
    let second_uri = uri("/Second.kt");
    let deep_ext_uri = uri("/DeepExtensions.kt");
    let second_ext_uri = uri("/SecondExtensions.kt");
    let idx = Indexer::new();
    idx.index_content(
        &deep_ancestor_uri,
        "package com.pkg\ninterface DeepAncestor\n",
    );
    idx.index_content(
        &first_uri,
        "package com.pkg\ninterface First : DeepAncestor\n",
    );
    idx.index_content(&second_uri, "package com.pkg\ninterface Second\n");
    idx.index_content(&str_uri, "package com.pkg\nclass Str : First, Second\n");
    idx.index_content(
        &deep_ext_uri,
        "package com.pkg\nfun DeepAncestor.toViewText(): String = TODO()\n",
    );
    idx.index_content(
        &second_ext_uri,
        "package com.pkg\nfun Second.toViewText(): String = TODO()\n",
    );
    idx.index_content(
        &host_uri,
        "package com.pkg\nfun foo(receiver: Str) { receiver.toViewText() }\n",
    );

    let locs = resolve_symbol(&idx, "toViewText", Some("Str"), &host_uri);
    assert_eq!(
        locs.len(),
        1,
        "expected exactly the nearer direct supertype's extension, got {locs:?}"
    );
    assert_eq!(
        locs[0].uri, second_ext_uri,
        "expected Second's toViewText (a direct, hop-1 supertype) over \
         DeepAncestor's (First's own hop-2 ancestor), got a location in {:?}",
        locs[0].uri
    );
}

#[test]
fn resolve_qualified_member_on_concrete_type_still_shadows_supertype_extension() {
    // Kotlin's own precedence rule: a real member on the concrete receiver
    // type always wins over a same-named extension declared on an ancestor
    // type, even after the new supertype-extension fallback is added.
    let host_uri = uri("/Host.kt");
    let str_uri = uri("/Str.kt");
    let seq_uri = uri("/Seq.kt");
    let idx = Indexer::new();
    idx.index_content(
        &seq_uri,
        "package com.pkg\ninterface Seq\nfun Seq.toViewText(): String = TODO()\n",
    );
    idx.index_content(
        &str_uri,
        "package com.pkg\nclass Str : Seq {\n  fun toViewText(): String = \"own\"\n}\n",
    );
    idx.index_content(
        &host_uri,
        "package com.pkg\nfun foo(receiver: Str) { receiver.toViewText() }\n",
    );

    let locs = resolve_symbol(&idx, "toViewText", Some("Str"), &host_uri);
    assert!(!locs.is_empty(), "toViewText not found at all");
    assert_eq!(
        locs[0].uri, str_uri,
        "Str's own member toViewText must shadow Seq's extension, got a location in {:?}",
        locs[0].uri
    );
}

#[test]
fn resolve_qualified_appends_supertype_extension_alongside_a_wrong_arity_concrete_member() {
    // Real Moneta bug: `navController.navigate(route = ...)` where
    // `NavHostController`/`NavController` (the concrete receiver type and its
    // hierarchy) also declare a same-named member with a DIFFERENT arity
    // (real JVM overloads like `navigate(Uri)`), and the call actually wants
    // a same-named extension declared on an ancestor (the KTX
    // `NavController.navigate(route: String, ...)` extension). Before this
    // fix, `resolve_qualified` returned the wrong-arity member alone and
    // never tried the supertype-walk extension fallback at all -- once a
    // shape-aware caller correctly rejected that member, resolution came up
    // completely empty instead of falling through to the extension. This
    // test is the unshaped `resolve_symbol` layer: it must now return BOTH
    // candidates (member first, extension appended after) so a shape-aware
    // caller has something to pick from.
    let host_uri = uri("/Host.kt");
    let base_uri = uri("/Base.kt");
    let derived_uri = uri("/Derived.kt");
    let ext_uri = uri("/BaseExtensions.kt");
    let idx = Indexer::new();
    idx.index_content(&base_uri, "package com.pkg\nopen class Base\n");
    idx.index_content(
        &derived_uri,
        "package com.pkg\nclass Derived : Base() {\n  fun act(): String = \"member\"\n}\n",
    );
    idx.index_content(
        &ext_uri,
        "package com.pkg\nfun Base.act(label: String): String = TODO()\n",
    );
    idx.index_content(
        &host_uri,
        "package com.pkg\nfun foo(receiver: Derived) { receiver.act(\"x\") }\n",
    );

    let locs = resolve_symbol(&idx, "act", Some("Derived"), &host_uri);
    assert_eq!(
        locs.len(),
        2,
        "expected both the wrong-arity concrete member and the supertype \
         extension as candidates, got {locs:?}"
    );
    assert_eq!(
        locs[0].uri, derived_uri,
        "the concrete member must still come first (real Kotlin member-over-\
         extension precedence when arity isn't in question), got {:?}",
        locs[0].uri
    );
    assert_eq!(
        locs[1].uri, ext_uri,
        "the supertype extension must be appended as the second candidate, \
         got {:?}",
        locs[1].uri
    );
}

#[test]
fn resolve_qualified_supertype_extension_fallback_threads_the_real_origin_uri() {
    // Copilot review finding on PR #289: the new supertype-extension-fallback
    // `walk_hierarchy` call passed `CallerContext::default()`, so its
    // `origin_uri` (used only for module-scoped ambiguity narrowing) fell
    // back to `start_uri` -- here, the JAR-backed receiver type's own
    // declaring file, NOT the real calling file. `owning_module_dependencies`
    // can't map a `jar:` URI to any module, so whenever an ancestor's own
    // name is ambiguous and only module-scoping (not the denylist) can
    // narrow it, the walk would incorrectly decline and miss a valid
    // ancestor extension entirely.
    //
    // Reproduces the real shape: `Str` (the receiver) is itself JAR-backed
    // (mirroring `String` resolving from kotlin-stdlib), with an ambiguous
    // "Seq" supertype -- two same-named JAR candidates, distinguishable only
    // by which one is a real dependency of the CALLING file's module.
    let indexer = Indexer::new();
    let host_uri = uri("/app/Host.kt");
    indexer.index_content(
        &host_uri,
        concat!(
            "package com.app\n",
            "import com.example.Str\n",
            "import com.example.seqlib.toViewText\n",
            "fun foo(receiver: Str) { receiver.toViewText() }\n",
        ),
    );

    let str_uri = gradle_cache_jar_uri("com.example", "str-lib", "1.0.0");
    let decoy_seq_uri = gradle_cache_jar_uri("com.example.decoy", "decoy-lib", "1.0.0");
    let real_seq_uri = gradle_cache_jar_uri("com.example", "seq-lib", "2.0.0");

    indexer.jar_definitions.insert(
        "Str".to_owned(),
        vec![tower_lsp::lsp_types::Location {
            uri: str_uri.clone(),
            range: Default::default(),
        }],
    );
    // Decoy indexed first, matching this file's established convention.
    indexer.jar_definitions.insert(
        "Seq".to_owned(),
        vec![
            tower_lsp::lsp_types::Location {
                uri: decoy_seq_uri,
                range: Default::default(),
            },
            tower_lsp::lsp_types::Location {
                uri: real_seq_uri.clone(),
                range: Default::default(),
            },
        ],
    );
    indexer.jar_files.insert(
        str_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            supers: vec![(0, "Seq".to_owned(), Vec::new())],
            ..Default::default()
        }),
    );
    indexer.jar_files.insert(
        real_seq_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.example.seqlib".to_owned()),
            ..Default::default()
        }),
    );

    // Only `Host.kt`'s own module (content root `/app`) depends on the real
    // `com.example:seq-lib` artifact.
    let mut dependencies_by_content_root: std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashSet<crate::cli::extract_sources::GradleMeta>,
    > = std::collections::HashMap::new();
    dependencies_by_content_root.insert(
        std::path::PathBuf::from("/test/app"),
        std::collections::HashSet::from([crate::cli::extract_sources::GradleMeta {
            group: "com.example".to_owned(),
            artifact: "seq-lib".to_owned(),
            version: "2.0.0".to_owned(),
        }]),
    );
    *indexer.module_dependencies.write().unwrap() = dependencies_by_content_root;

    // Extension registered on the correctly-narrowed "Seq" ancestor only.
    indexer.extension_by_receiver.insert(
        "Seq".to_owned(),
        vec![crate::types::ExtensionEntry {
            file_uri: real_seq_uri.to_string(),
            name: "toViewText".to_owned(),
            kind: tower_lsp::lsp_types::SymbolKind::FUNCTION,
            detail: "fun Seq.toViewText(): String".to_owned(),
            visibility: crate::types::Visibility::Public,
            package: Some("com.example.seqlib".to_owned()),
            trailing_lambda: false,
            deprecated: false,
            container: None,
        }],
    );

    let locs = resolve_symbol(&indexer, "toViewText", Some("Str"), &host_uri);
    assert!(
        !locs.is_empty(),
        "expected the module-scoped tie-break to narrow the ambiguous Seq \
         supertype using Host.kt's own real origin, not the JAR-backed Str \
         receiver's own declaring file, and find toViewText on it"
    );
}

#[test]
fn resolve_qualified_inherited_member_lookup_threads_the_real_origin_uri() {
    // Copilot review follow-up on PR #289: the pre-existing inherited-member
    // hierarchy walk (`resolve_from_class_hierarchy_scoped`, called from the
    // same `resolve_qualified` uppercase-root branch, one step before the new
    // supertype-extension fallback) has the identical bug the extension
    // fallback was just fixed for -- it also passes `CallerContext::default()`,
    // so it also falls back to the JAR-backed receiver's own declaring file
    // as its `origin_uri`, also disabling module-scoped ambiguity narrowing
    // for a real inherited MEMBER (not just an extension) on an ambiguous
    // ancestor. Same reproduction shape as the extension-fallback test above,
    // but `Seq` declares a real member `getInfo()` instead of an extension.
    let indexer = Indexer::new();
    let host_uri = uri("/app/Host.kt");
    indexer.index_content(
        &host_uri,
        concat!(
            "package com.app\n",
            "import com.example.Str\n",
            "fun foo(receiver: Str) { receiver.getInfo() }\n",
        ),
    );

    let str_uri = gradle_cache_jar_uri("com.example", "str-lib", "1.0.0");
    let decoy_seq_uri = gradle_cache_jar_uri("com.example.decoy", "decoy-lib", "1.0.0");
    let real_seq_uri = gradle_cache_jar_uri("com.example", "seq-lib", "2.0.0");

    indexer.jar_definitions.insert(
        "Str".to_owned(),
        vec![tower_lsp::lsp_types::Location {
            uri: str_uri.clone(),
            range: Default::default(),
        }],
    );
    indexer.jar_definitions.insert(
        "Seq".to_owned(),
        vec![
            tower_lsp::lsp_types::Location {
                uri: decoy_seq_uri,
                range: Default::default(),
            },
            tower_lsp::lsp_types::Location {
                uri: real_seq_uri.clone(),
                range: Default::default(),
            },
        ],
    );
    indexer.jar_files.insert(
        str_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            supers: vec![(0, "Seq".to_owned(), Vec::new())],
            ..Default::default()
        }),
    );
    indexer.jar_files.insert(
        real_seq_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.example.seqlib".to_owned()),
            symbols: vec![crate::types::SymbolEntry {
                name: "getInfo".to_owned(),
                kind: tower_lsp::lsp_types::SymbolKind::METHOD,
                visibility: crate::types::Visibility::Public,
                range: Default::default(),
                selection_range: Default::default(),
                detail: "fun getInfo(): String".to_owned(),
                params: String::new(),
                param_counts: (0, 0),
                container: None,
                cold: None,
                trailing_lambda: false,
                deprecated: false,
            }],
            ..Default::default()
        }),
    );

    let mut dependencies_by_content_root: std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashSet<crate::cli::extract_sources::GradleMeta>,
    > = std::collections::HashMap::new();
    dependencies_by_content_root.insert(
        std::path::PathBuf::from("/test/app"),
        std::collections::HashSet::from([crate::cli::extract_sources::GradleMeta {
            group: "com.example".to_owned(),
            artifact: "seq-lib".to_owned(),
            version: "2.0.0".to_owned(),
        }]),
    );
    *indexer.module_dependencies.write().unwrap() = dependencies_by_content_root;

    let locs = resolve_symbol(&indexer, "getInfo", Some("Str"), &host_uri);
    assert!(
        !locs.is_empty(),
        "expected the module-scoped tie-break to narrow the ambiguous Seq \
         supertype using Host.kt's own real origin, not the JAR-backed Str \
         receiver's own declaring file, and find the inherited member getInfo on it"
    );
}

#[test]
fn resolve_qualified_supertype_extension_fallback_handles_a_fully_qualified_supertype_spelling() {
    // Copilot review finding on PR #289: `walk_hierarchy` yields `super_name`
    // exactly as written in the source's own delegation-specifier text
    // (`user_type_name` joins every dotted segment) -- a fully-qualified
    // supertype spelling like `class Str : com.other.Seq` produces
    // `super_name = "com.other.Seq"`, not the bare `"Seq"`. `extension_by_receiver`
    // is keyed by the receiver's SIMPLE leaf name only (every other caller in
    // this file strips qualification via `.last_segment()` before looking it
    // up, e.g. `let root_base = root.last_segment();` a few lines above this
    // fallback) -- passing the qualified name straight through would silently
    // miss the extension.
    let host_uri = uri("/Host.kt");
    let str_uri = uri("/Str.kt");
    let seq_uri = uri("/Seq.kt");
    let ext_uri = uri("/SeqExtensions.kt");
    let idx = Indexer::new();
    idx.index_content(&seq_uri, "package com.other\ninterface Seq\n");
    // Fully-qualified delegation specifier -- no import needed for this shape.
    idx.index_content(&str_uri, "package com.pkg\nclass Str : com.other.Seq\n");
    idx.index_content(
        &ext_uri,
        "package com.pkg\nfun Seq.toViewText(): String = TODO()\n",
    );
    idx.index_content(
        &host_uri,
        "package com.pkg\nfun foo(receiver: Str) { receiver.toViewText() }\n",
    );

    let locs = resolve_symbol(&idx, "toViewText", Some("Str"), &host_uri);
    assert!(
        !locs.is_empty(),
        "toViewText declared on a fully-qualified supertype spelling (com.other.Seq) \
         must still be found via a Str receiver"
    );
    assert_eq!(locs[0].uri, ext_uri);
}

#[test]
fn resolve_qualified_fully_qualified_supertype_resolves_the_named_package_not_a_same_leaf_decoy() {
    // Copilot review follow-up: naively stripping a fully-qualified supertype
    // spelling to its bare leaf (the previous commit's fix) risks resolving
    // to the WRONG class when a different, same-leaf-named symbol is also
    // reachable from the subclass's own file -- e.g. a decoy `Seq` declared
    // in the subclass's own package, shadowing the real, explicitly
    // qualified `com.other.Seq` the source actually named. A qualified
    // spelling exists specifically to disambiguate from exactly this kind
    // of same-leaf collision, so silently discarding the qualifier and
    // letting same-package resolution win would be a real regression (a
    // wrong answer), not just a missed match (no answer) -- exercised via
    // inherited-MEMBER lookup, where which specific file the walk resolves
    // to actually matters (unlike the extension-lookup path, which is keyed
    // by receiver leaf name alone regardless of which same-named class was
    // reached).
    let host_uri = uri("/Host.kt");
    let str_uri = uri("/Str.kt");
    let decoy_seq_uri = uri("/DecoySeq.kt");
    let real_seq_uri = uri("/Seq.kt");
    let idx = Indexer::new();
    // Decoy: same package as Str.kt, same leaf name "Seq" -- what a naive
    // same-package resolution of the bare leaf would find first. Declares
    // the SAME member name as the real Seq, so a wrong resolution would
    // still "succeed" (non-empty) but at the wrong location.
    idx.index_content(
        &decoy_seq_uri,
        "package com.pkg\ninterface Seq {\n  fun getInfo(): String\n}\n",
    );
    idx.index_content(
        &real_seq_uri,
        "package com.other\ninterface Seq {\n  fun getInfo(): String\n}\n",
    );
    idx.index_content(&str_uri, "package com.pkg\nclass Str : com.other.Seq\n");
    idx.index_content(
        &host_uri,
        "package com.pkg\nfun foo(receiver: Str) { receiver.getInfo() }\n",
    );

    let locs = resolve_symbol(&idx, "getInfo", Some("Str"), &host_uri);
    assert!(
        !locs.is_empty(),
        "getInfo on the correctly-qualified com.other.Seq must still resolve"
    );
    assert_eq!(
        locs[0].uri, real_seq_uri,
        "must resolve via the real com.other.Seq the source actually named, \
         not a same-package same-leaf decoy Seq, got a location in {:?}",
        locs[0].uri
    );
}

#[test]
fn resolve_qualified_nested_type_supertype_is_not_mistaken_for_a_package_qualified_one() {
    // Copilot review follow-up: `super_name.rsplit_once('.')` treats ANY
    // dotted supertype spelling as package-qualified, but a NESTED-TYPE
    // spelling (`class Str : Outer.Inner`, extending a type nested inside
    // `Outer`) is dotted too, with no package involved at all -- "Outer" is
    // a type name, not a package. Kotlin/Java convention (already relied on
    // throughout this file, e.g. `root.starts_with_uppercase()`) is the
    // discriminator: a real package segment is never uppercase-first, an
    // enclosing type's name always is. Without checking this, `Outer.Inner`
    // would incorrectly search for a package literally named "Outer" --
    // real files that declare `package Outer` are rare but not impossible,
    // and here one exists specifically to prove the wrong match.
    let host_uri = uri("/Host.kt");
    let str_uri = uri("/Str.kt");
    let real_inner_uri = uri("/RealInner.kt");
    let decoy_inner_uri = uri("/DecoyInner.kt");
    let idx = Indexer::new();
    // Decoy: a real (if unconventional) package literally named "Outer" --
    // what the buggy package-qualified fast path would incorrectly match.
    idx.index_content(
        &decoy_inner_uri,
        "package Outer\ninterface Inner {\n  fun getInfo(): String\n}\n",
    );
    // Real: same package as Str.kt, reachable via ordinary same-package
    // resolution once the nested-type spelling correctly falls through to
    // plain leaf-only resolution instead of a package-qualified lookup.
    idx.index_content(
        &real_inner_uri,
        "package com.pkg\ninterface Inner {\n  fun getInfo(): String\n}\n",
    );
    idx.index_content(&str_uri, "package com.pkg\nclass Str : Outer.Inner\n");
    idx.index_content(
        &host_uri,
        "package com.pkg\nfun foo(receiver: Str) { receiver.getInfo() }\n",
    );

    let locs = resolve_symbol(&idx, "getInfo", Some("Str"), &host_uri);
    assert!(!locs.is_empty(), "getInfo must still resolve");
    assert_eq!(
        locs[0].uri, real_inner_uri,
        "Outer.Inner must resolve as a nested-type supertype (falling through \
         to plain same-package resolution of the leaf \"Inner\"), not be \
         misread as package \"Outer\", symbol \"Inner\" -- got a location in {:?}",
        locs[0].uri
    );
}

#[test]
fn resolve_qualified_nested_type_supertype_resolves_within_its_own_named_container() {
    // Copilot review follow-up: once a dotted supertype spelling is
    // correctly recognized as a nested-type chain (not package-qualified,
    // the sibling test above), naively falling through to plain leaf-only
    // resolution of just "Inner" still risks the SAME kind of same-leaf
    // collision `find_symbol_in_package` was added to prevent for
    // package-qualified spellings -- just for nested types instead.
    //
    // The decoy is placed behind an EXPLICIT IMPORT (not same-package) so
    // this test is deterministic rather than depending on which of two
    // same-package peers a first-match scan happens to iterate first: an
    // import match is resolved by `resolve_via_imports`, a distinct,
    // earlier step than the same-package scan, so only a genuine
    // container-walk (not just "some" leaf-only resolution succeeding)
    // can make this test pass for the right reason.
    let host_uri = uri("/Host.kt");
    let str_uri = uri("/Str.kt");
    let outer_uri = uri("/Outer.kt");
    let decoy_inner_uri = uri("/DecoyInner.kt");
    let idx = Indexer::new();
    idx.index_content(
        &outer_uri,
        concat!(
            "package com.other\n",
            "class Outer {\n",
            "  interface Inner {\n",
            "    fun getInfo(): String\n",
            "  }\n",
            "}\n",
        ),
    );
    // Decoy: an unrelated top-level Inner, explicitly imported by Str.kt --
    // what plain leaf-only resolution would find (via the import step),
    // ignoring the container the source actually named.
    idx.index_content(
        &decoy_inner_uri,
        "package com.decoy\ninterface Inner {\n  fun getInfo(): String\n}\n",
    );
    idx.index_content(
        &str_uri,
        concat!(
            "package com.pkg\n",
            "import com.other.Outer\n",
            "import com.decoy.Inner\n",
            "class Str : Outer.Inner\n",
        ),
    );
    idx.index_content(
        &host_uri,
        "package com.pkg\nfun foo(receiver: Str) { receiver.getInfo() }\n",
    );

    let locs = resolve_symbol(&idx, "getInfo", Some("Str"), &host_uri);
    assert!(!locs.is_empty(), "getInfo must still resolve");
    assert_eq!(
        locs[0].uri, outer_uri,
        "Outer.Inner must resolve within Outer's own container, not the \
         explicitly-imported unrelated top-level Inner decoy -- got a \
         location in {:?}",
        locs[0].uri
    );
}

#[test]
fn resolve_qualified_package_qualified_nested_type_supertype_resolves_correctly() {
    // Copilot review finding (real): a supertype spelling combining BOTH a
    // package qualifier AND nesting (`class Str : com.other.Outer.Inner`)
    // was mishandled entirely -- the nested-type branch treated
    // `super_name.split('.').next()` ("com") as the outermost TYPE segment,
    // when it's actually a package segment. Real fix: skip leading
    // lowercase (package) segments, resolve the first uppercase segment
    // package-exactly there, then walk any remaining nested segments.
    let host_uri = uri("/Host.kt");
    let str_uri = uri("/Str.kt");
    let outer_uri = uri("/Outer.kt");
    let decoy_inner_uri = uri("/DecoyInner.kt");
    let idx = Indexer::new();
    idx.index_content(
        &outer_uri,
        concat!(
            "package com.other\n",
            "class Outer {\n",
            "  interface Inner {\n",
            "    fun getInfo(): String\n",
            "  }\n",
            "}\n",
        ),
    );
    idx.index_content(
        &decoy_inner_uri,
        "package com.decoy\ninterface Inner {\n  fun getInfo(): String\n}\n",
    );
    idx.index_content(
        &str_uri,
        concat!(
            "package com.pkg\n",
            "import com.decoy.Inner\n",
            "class Str : com.other.Outer.Inner\n",
        ),
    );
    idx.index_content(
        &host_uri,
        "package com.pkg\nfun foo(receiver: Str) { receiver.getInfo() }\n",
    );

    let locs = resolve_symbol(&idx, "getInfo", Some("Str"), &host_uri);
    assert!(!locs.is_empty(), "getInfo must still resolve");
    assert_eq!(
        locs[0].uri, outer_uri,
        "com.other.Outer.Inner must resolve within the real, \
         package-qualified Outer's own container, not the \
         explicitly-imported unrelated Inner decoy -- got a location in {:?}",
        locs[0].uri
    );
}

#[test]
fn find_symbol_in_package_uses_the_real_per_symbol_package_for_jar_candidates() {
    // Copilot review follow-up: `find_symbol_in_package`'s JAR-check branch
    // is what the qualified-supertype resolution fix relies on to be
    // package-exact -- it must not fall back to leaf-only matching just
    // because a multi-package JAR's file-level `FileData.package` (a
    // first-symbol guess covering the WHOLE synthetic file) doesn't happen
    // to equal the package actually being searched for.
    let idx = Indexer::new();
    let jar_uri = Url::parse("jar:file:///multi-pkg.jar").unwrap();
    idx.jar_definitions.insert(
        "Seq".to_owned(),
        vec![tower_lsp::lsp_types::Location {
            uri: jar_uri.clone(),
            range: Default::default(),
        }],
    );
    // File-level guess is some unrelated package; the per-symbol table
    // (line 0, matching the location's synthetic range) has the real one.
    idx.jar_files.insert(
        jar_uri.to_string(),
        std::sync::Arc::new(crate::types::FileData {
            package: Some("com.wrong.guess".to_owned()),
            ..Default::default()
        }),
    );
    idx.jar_symbol_packages
        .insert(jar_uri.to_string(), vec!["com.other".to_owned()]);

    let loc = find_symbol_in_package(&idx, "Seq", "com.other");
    assert_eq!(
        loc,
        Some(tower_lsp::lsp_types::Location {
            uri: jar_uri,
            range: Default::default(),
        }),
        "expected the real per-symbol package (com.other) to be used, not \
         the file-level first-symbol guess (com.wrong.guess), got {loc:?}"
    );
}

#[test]
fn resolve_variable_receiver_extension_disambiguates_by_receiver_type() {
    // `message.toViewText()` where `message: String` — `String` has no
    // indexed declaration file (a built-in type) and no matching member.
    // `String.toViewText` is only imported (not same-package), and an
    // unrelated `EFrequency.toViewText` also exists elsewhere in the
    // workspace with neither import nor same-package visibility from the
    // call site. Before the fix, the lowercase-root branch gave up once the
    // receiver type's own member/hierarchy search failed, so this fell
    // through all the way to the receiver-blind global-definitions tail,
    // which sees both candidates and declines (ambiguous) rather than
    // picking either. The fix must instead resolve `message`'s real type
    // (`String`) and use the existing receiver-scoped, import-aware
    // extension lookup to find the one real, in-scope candidate.
    let host_uri = Url::parse("file:///app/Host.kt").unwrap();
    let string_ext_uri = Url::parse("file:///lib/a/StringExtensions.kt").unwrap();
    let other_ext_uri = Url::parse("file:///lib/b/OtherExtensions.kt").unwrap();
    let idx = Indexer::new();
    idx.index_content(
        &string_ext_uri,
        "package com.lib.a\nfun String.toViewText(): String = this\n",
    );
    idx.index_content(
        &other_ext_uri,
        concat!(
            "package com.lib.b\n",
            "enum class EFrequency { WEEKLY }\n",
            "fun EFrequency.toViewText(): String = name\n",
        ),
    );
    idx.index_content(
        &host_uri,
        concat!(
            "package com.app\n",
            "import com.lib.a.toViewText\n",
            "val message: String = \"hi\"\n",
            "fun foo() { message.toViewText() }\n",
        ),
    );

    let locs = resolve_symbol_index_only(&idx, "toViewText", Some("message"), &host_uri);
    assert_eq!(
        locs.len(),
        1,
        "expected exactly the String.toViewText extension, got {locs:?}"
    );
    assert_eq!(locs[0].uri, string_ext_uri);
}

#[test]
fn resolve_dotted_name_traverses_deep_nesting() {
    // `Bar.Baz.Foo` passed directly as `name` (no qualifier) must walk the full
    // nested-type chain Bar → Baz → Foo, not just the first dot.
    let host_uri = uri("/Host.kt");
    let bar_uri = uri("/Bar.kt");
    let idx = Indexer::new();
    idx.index_content(
        &bar_uri,
        "package com.pkg\nclass Bar {\n  class Baz {\n    class Foo\n  }\n}",
    );
    idx.index_content(&host_uri, "package com.pkg\n");

    let locs = resolve_symbol(&idx, "Bar.Baz.Foo", None, &host_uri);
    assert!(!locs.is_empty(), "deeply-nested Bar.Baz.Foo not resolved");
    assert_eq!(locs[0].uri, bar_uri);
}

#[test]
fn resolve_dotted_name_scopes_a_nested_member_to_its_own_outer_type() {
    // Two sibling sealed types in one file each declare a `Loading` member.
    // `Event` is declared first, so a whole-file first-match lookup for
    // `UiEvent.Loading` would wrongly return `Event`'s `Loading`.
    let file_uri = uri("/Events.kt");
    let idx = Indexer::new();
    idx.index_content(
        &file_uri,
        "sealed interface Event {\n  object Loading : Event\n}\nsealed interface UiEvent {\n  object Loading : UiEvent\n}\n",
    );

    let locs = resolve_symbol(&idx, "UiEvent.Loading", None, &file_uri);
    assert!(!locs.is_empty(), "UiEvent.Loading not resolved");
    assert_eq!(
        locs[0].range.start.line, 4,
        "must resolve to UiEvent's own Loading (line 4), not Event's (line 1)"
    );
}

#[test]
fn resolve_dotted_name_skips_leading_package_segments() {
    // `demo.Foo` — the leading lowercase package segment must be skipped so the
    // type `Foo` resolves.
    let host_uri = uri("/Host.kt");
    let foo_uri = uri("/Foo.kt");
    let idx = Indexer::new();
    idx.index_content(&foo_uri, "package demo\nclass Foo");
    idx.index_content(&host_uri, "package demo\n");

    let locs = resolve_symbol(&idx, "demo.Foo", None, &host_uri);
    assert!(!locs.is_empty(), "package-qualified demo.Foo not resolved");
    assert_eq!(locs[0].uri, foo_uri);
}

#[test]
fn infer_type_in_lines_dotted() {
    // Ensure infer_type_in_lines handles `Outer.Inner` dotted types.
    let lines: Vec<String> =
        vec!["  private val factory: DashboardProductsReducer.Factory,".to_owned()];
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
    let vm_uri = uri("/ViewModel.kt");
    let acc_uri = uri("/AccountModel.kt");
    let idx = Indexer::new();
    idx.index_content(
        &acc_uri,
        "package com.pkg\nclass AccountModel {\n  val interestPlanCode: String = \"\"\n}",
    );
    idx.index_content(
        &vm_uri,
        "package com.pkg\nclass ViewModel {\n  val account: AccountModel = AccountModel()\n}",
    );
    idx.index_content(
        &host_uri,
        "package com.pkg\nfun foo(vm: ViewModel) { vm.account.interestPlanCode }",
    );

    // qualifier = "vm.account", name = "interestPlanCode"
    let locs = resolve_symbol(&idx, "interestPlanCode", Some("vm.account"), &host_uri);
    assert!(
        !locs.is_empty(),
        "interestPlanCode not found via multi-hop field chain"
    );
    assert_eq!(locs[0].uri, acc_uri);
}

#[test]
fn resolve_local_param_declaration() {
    // Cursor on `account` (function param without val/var) should return the
    // declaration line in the same file.
    let u = uri("/Foo.kt");
    let idx = Indexer::new();
    idx.index_content(
        &u,
        "package com.pkg\nfun foo(account: AccountModel) {\n  account.something\n}",
    );

    let locs = resolve_symbol(&idx, "account", None, &u);
    assert!(!locs.is_empty(), "local param declaration not found");
    assert_eq!(locs[0].uri, u);
    // Line 1 (0-indexed) contains the parameter declaration
    assert_eq!(locs[0].range.start.line, 1);
}

#[test]
fn resolve_method_via_variable_type_inference() {
    // repo.findById(1) where repo: UserRepository
    let vm_uri = uri("/ViewModel.kt");
    let repo_uri = uri("/UserRepository.kt");
    let idx = Indexer::new();
    idx.index_content(
        &repo_uri,
        "package com.pkg\nclass UserRepository {\n  fun findById(id: Int) {}\n}",
    );
    idx.index_content(&vm_uri,
            "package com.pkg\nclass ViewModel(\n  private val repo: UserRepository\n) {\n  fun load() { repo.findById(1) }\n}");

    // qualifier = "repo" (lowercase), name = "findById"
    // infer_variable_type should extract "UserRepository" from "val repo: UserRepository"
    // then resolve_qualified finds findById in UserRepository.kt
    let locs = resolve_symbol(&idx, "findById", Some("repo"), &vm_uri);
    assert!(
        !locs.is_empty(),
        "findById not found via variable type inference"
    );
    assert_eq!(locs[0].uri, repo_uri);
}

#[test]
fn resolve_method_via_constructor_param_type() {
    // interactor.loadDataFlow(x) where interactor: ShowChildNewTipsInteractor
    let vm_uri = uri("/SomeViewModel.kt");
    let int_uri = uri("/ShowChildNewTipsInteractor.kt");
    let idx = Indexer::new();
    idx.index_content(&int_uri,
            "package com.feature\nclass ShowChildNewTipsInteractor {\n  fun loadDataFlow(account: Any) {}\n}");
    idx.index_content(&vm_uri,
            "package com.feature\nclass SomeViewModel(\n  private val interactor: ShowChildNewTipsInteractor\n) {\n  fun init() { interactor.loadDataFlow(x) }\n}");

    let locs = resolve_symbol(&idx, "loadDataFlow", Some("interactor"), &vm_uri);
    assert!(
        !locs.is_empty(),
        "loadDataFlow not found via constructor param type inference"
    );
    assert_eq!(locs[0].uri, int_uri);
}

#[test]
fn resolve_method_via_interface_hierarchy() {
    // repo.contactAddressSetup() where repo: IGoldConversionRepository
    // contactAddressSetup is defined in IBaseRepository (superinterface)
    let vm_uri = uri("/ViewModel.kt");
    let repo_uri = uri("/IGoldConversionRepository.kt");
    let base_uri = uri("/IBaseRepository.kt");
    let idx = Indexer::new();
    idx.index_content(
        &base_uri,
        "package com.pkg\ninterface IBaseRepository {\n  fun contactAddressSetup(): String\n}",
    );
    idx.index_content(&repo_uri,
            "package com.pkg\ninterface IGoldConversionRepository : IBaseRepository {\n  fun goldPrice(): Double\n}");
    idx.index_content(&vm_uri,
            "package com.pkg\nclass ViewModel(\n  private val repo: IGoldConversionRepository\n) {\n  fun init() { repo.contactAddressSetup() }\n}");

    let locs = resolve_symbol(&idx, "contactAddressSetup", Some("repo"), &vm_uri);
    assert!(
        !locs.is_empty(),
        "contactAddressSetup not found via interface hierarchy"
    );
    assert_eq!(locs[0].uri, base_uri, "should resolve to IBaseRepository");
}

// ── build_rg_pattern ─────────────────────────────────────────────────────
// Use rg itself to validate patterns (it's always available in the dev env).

fn rg_available() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

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
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }
    let pat = build_rg_pattern("Foo");
    assert!(rg_matches(&pat, "class Foo {"));
    assert!(rg_matches(&pat, "sealed class Foo"));
}

#[test]
fn rg_pattern_matches_kotlin_enum() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }
    let pat = build_rg_pattern("EScreen");
    assert!(rg_matches(&pat, "enum class EScreen {"));
}

#[test]
fn rg_pattern_matches_java_enum() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }
    let pat = build_rg_pattern("EProductScreen");
    assert!(rg_matches(&pat, "public enum EProductScreen {"));
    assert!(rg_matches(&pat, "  enum EProductScreen {"));
    assert!(rg_matches(&pat, "private static enum EProductScreen {"));
}

#[test]
fn rg_pattern_no_false_positive_on_usage() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }
    let pat = build_rg_pattern("EProductScreen");
    // Should NOT match a plain usage (not a declaration)
    assert!(!rg_matches(&pat, "EProductScreen.SOMETHING"));
    assert!(!rg_matches(&pat, "val x: EProductScreen = "));
}

#[test]
fn rg_pattern_matches_java_class() {
    if !rg_available() {
        eprintln!("skipping: rg not available");
        return;
    }
    let pat = build_rg_pattern("FlexiEntryVM");
    assert!(rg_matches(&pat, "public class FlexiEntryVM extends Base {"));
}

// ── import_file_stems ────────────────────────────────────────────────────

#[test]
fn file_stems_top_level() {
    assert_eq!(
        import_file_stems("cz.moneta.data.EProductScreen"),
        vec!["EProductScreen"]
    );
}

#[test]
fn file_stems_nested() {
    let s = import_file_stems("com.example.OuterClass.InnerClass");
    assert_eq!(s, vec!["OuterClass", "InnerClass"]);
}

// ── supers CST extraction (via parse_kotlin / parse_java) ────────────────

fn kotlin_supers(src: &str) -> Vec<String> {
    parse_kotlin(src)
        .supers
        .into_iter()
        .map(|(_, n, _)| n)
        .collect()
}

#[test]
fn supers_kotlin_single_line() {
    let s = kotlin_supers("class DetailViewModel : MviViewModel<Event, State, Effect>() {}");
    assert!(s.contains(&"MviViewModel".to_string()), "got {s:?}");
}

#[test]
fn supers_kotlin_nested_generic_type() {
    // Outer<T>.Inner should yield "Outer.Inner", not just "Outer".
    let s = kotlin_supers("class Foo : Outer<T>.Inner() {}");
    assert!(
        s.iter().any(|n| n == "Outer.Inner" || n == "Outer"),
        "got {s:?}"
    );
}

#[test]
fn supers_kotlin_multi_line_ctor() {
    let src = "class DetailViewModel @Inject constructor(\n  private val useCase: UseCase,\n) : MviViewModel<Event, State, Effect>() {}";
    let s = kotlin_supers(src);
    assert!(s.contains(&"MviViewModel".to_string()), "got {s:?}");
}

#[test]
fn supers_kotlin_multiple() {
    let src = "class Foo : BaseClass(), SomeInterface, AnotherInterface {}";
    let s = kotlin_supers(src);
    assert!(s.contains(&"BaseClass".to_string()), "got {s:?}");
    assert!(s.contains(&"SomeInterface".to_string()), "got {s:?}");
    assert!(s.contains(&"AnotherInterface".to_string()), "got {s:?}");
}

#[test]
fn supers_java_extends() {
    let src = "public class FlexiEntryVM extends BaseFlexikreditVM {}";
    let s: Vec<String> = parse_java(src)
        .supers
        .into_iter()
        .map(|(_, n, _)| n)
        .collect();
    assert!(s.contains(&"BaseFlexikreditVM".to_string()), "got {s:?}");
}

#[test]
fn supers_java_implements() {
    let src = "public class Foo extends Base implements Runnable, Serializable {}";
    let s: Vec<String> = parse_java(src)
        .supers
        .into_iter()
        .map(|(_, n, _)| n)
        .collect();
    assert!(s.contains(&"Base".to_string()), "got {s:?}");
    assert!(s.contains(&"Runnable".to_string()), "got {s:?}");
    assert!(s.contains(&"Serializable".to_string()), "got {s:?}");
}

#[test]
fn supers_java_generic_extends() {
    let java = |src: &str| -> Vec<String> {
        parse_java(src)
            .supers
            .into_iter()
            .map(|(_, n, _)| n)
            .collect()
    };

    let s = java("public class Foo extends Base<String> {}");
    assert!(
        s.contains(&"Base".to_string()),
        "generic extends, got {s:?}"
    );

    let s = java("public class Foo extends pkg.Base<String> {}");
    assert!(
        s.contains(&"pkg.Base".to_string()) || s.contains(&"Base".to_string()),
        "qualified generic extends, got {s:?}"
    );

    let s = java("public class Foo extends Base<String> implements Runnable {}");
    assert!(
        s.contains(&"Base".to_string()),
        "generic extends+implements, got {s:?}"
    );
    assert!(
        s.contains(&"Runnable".to_string()),
        "generic extends+implements, got {s:?}"
    );
}

#[test]
fn supers_does_not_pick_up_type_annotations() {
    let src = "class Foo {\n  val x: Int = 0\n  fun f(): String = \"\"\n}";
    let s = kotlin_supers(src);
    assert!(s.is_empty(), "should have no supers, got {s:?}");
}

// ── resolve_from_class_hierarchy ─────────────────────────────────────────

#[test]
fn resolve_inherited_method() {
    let base_uri = uri("/Base.kt");
    let child_uri = uri("/Child.kt");
    let idx = Indexer::new();
    idx.index_content(
        &base_uri,
        "package com.example\nopen class Base {\n  fun baseMethod() {}\n}",
    );
    idx.index_content(&child_uri, "package com.example\nclass Child : Base() {}\n");

    // `baseMethod` is not declared in Child — must be found via hierarchy
    let locs = resolve_symbol(&idx, "baseMethod", None, &child_uri);
    assert!(!locs.is_empty(), "inherited method not found");
    assert_eq!(locs[0].uri, base_uri);
}

#[test]
fn resolve_inherited_method_via_import() {
    let base_uri = uri("/lib/Base.kt");
    let child_uri = uri("/app/Child.kt");
    let idx = Indexer::new();
    idx.index_content(
        &base_uri,
        "package com.lib\nopen class Base {\n  fun doStuff() {}\n}",
    );
    idx.index_content(
        &child_uri,
        "package com.app\nimport com.lib.Base\nclass Child : Base() {}\n",
    );

    let locs = resolve_symbol(&idx, "doStuff", None, &child_uri);
    assert!(!locs.is_empty(), "inherited method not found via import");
    assert_eq!(locs[0].uri, base_uri);
}

// ── this / super resolution ───────────────────────────────────────────────

#[test]
fn resolve_this_dot_method() {
    let u = uri("/Foo.kt");
    let idx = Indexer::new();
    idx.index_content(
        &u,
        "package com.example\nclass Foo {\n  fun doThing() {}\n  fun other() { this.doThing() }\n}",
    );
    let locs = resolve_symbol(&idx, "doThing", Some("this"), &u);
    assert!(!locs.is_empty(), "this.doThing() not resolved");
    assert_eq!(locs[0].uri, u);
}

#[test]
fn resolve_super_dot_method() {
    let base_uri = uri("/Base.kt");
    let child_uri = uri("/Child.kt");
    let idx = Indexer::new();
    idx.index_content(
        &base_uri,
        "package com.example\nopen class Base { fun init() {} }",
    );
    idx.index_content(
        &child_uri,
        "package com.example\nclass Child : Base() { fun x() { super.init() } }",
    );
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
    let lines: Vec<String> = vec!["items.map { item: DetailItem ->".to_string()];
    let range = find_declaration_range_in_lines(&lines, "item");
    assert!(range.is_some(), "typed lambda param not found");
}

#[test]
fn local_decl_no_false_positive_usage() {
    // A usage of `account` on a non-declaration line must not be returned
    let lines: Vec<String> = vec!["val result = account.name".to_string()];
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
    idx.index_content(
        &user_uri,
        "package com.example\ndata class User(val name: String, val age: Int)",
    );
    idx.index_content(
        &caller_uri,
        "package com.example\nfun greet(user: User) { println(user.name) }",
    );

    let locs = resolve_symbol(&idx, "name", Some("user"), &caller_uri);
    assert!(!locs.is_empty(), "name not found via user.name");
    assert_eq!(locs[0].uri, user_uri, "should point to User.kt");
}

#[test]
fn resolve_ctor_param_no_qualifier() {
    // Inside the class itself, `name` should resolve to the ctor param.
    let uri = uri("/User.kt");
    let idx = Indexer::new();
    idx.index_content(
        &uri,
        "package com.example\ndata class User(val name: String) {\n  fun display() = name\n}",
    );

    let locs = resolve_symbol(&idx, "name", None, &uri);
    assert!(!locs.is_empty(), "ctor param not found locally");
    assert_eq!(locs[0].uri, uri, "should stay in same file");
}

#[test]
fn resolve_named_arg_to_ctor_param() {
    // User(name = "Alice") — qualifier is "User" (detected by word_and_qualifier_at).
    // resolve_symbol with qualifier="User" must find `val name` in User's primary ctor.
    let user_uri = uri("/User.kt");
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(
        &user_uri,
        "package com.example\ndata class User(val name: String, val age: Int)",
    );
    idx.index_content(
        &caller_uri,
        "package com.example\nfun test() { val u = User(name = \"Alice\", age = 30) }",
    );

    // Simulate what the backend does after word_and_qualifier_at returns ("name", "User")
    let locs = resolve_symbol(&idx, "name", Some("User"), &caller_uri);
    assert!(
        !locs.is_empty(),
        "named arg 'name' not resolved to User ctor param"
    );
    assert_eq!(locs[0].uri, user_uri, "should point to User.kt, not caller");
}

#[test]
fn named_arg_same_name_different_classes_same_file() {
    // Regression: Contract.kt has both State(val toastModel: ...) and
    // OnClick(val toastModel: ...) in the same file.
    // Resolving State(toastModel = ...) should land on State's field,
    // not OnClick's (which appears later but might be returned first).
    let contract_uri = uri("/Contract.kt");
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(
        &contract_uri,
        "\
package com.example
sealed class Effect {
    data class OnClick(val toastModel: String) : Effect()
}
data class State(
    val toastModel: String? = null,
)",
    );
    idx.index_content(
        &caller_uri,
        "package com.example\nfun test() { State(toastModel = \"hi\") }",
    );

    let locs = resolve_symbol(&idx, "toastModel", Some("State"), &caller_uri);
    assert!(!locs.is_empty(), "toastModel not resolved");
    // Must point to State's toastModel (line 4), NOT OnClick's (line 2)
    let line = locs[0].range.start.line;
    assert!(
        line >= 4,
        "resolved to OnClick.toastModel (line {line}) instead of State.toastModel"
    );
}

// ── qualified access with uppercase class qualifier (extension fn fallthrough bug) ──

/// Regression: `Modifier.padding()` with cursor on `padding` where Modifier is an
/// indexed object/class and `padding()` is an **extension function** defined in a
/// *different* file.  `resolve_qualified` previously only searched the Modifier
/// class file for `padding`, so extension functions in other files were never
/// found.  The test checks that the extension function IS found via the
/// `extension_by_receiver` index.
#[test]
fn resolve_extension_fn_on_uppercase_qualifier() {
    // Modifier.kt defines the Modifier class/object
    let modifier_uri = uri("/Modifier.kt");
    // Padding.kt defines `fun Modifier.padding(...)` as an extension function
    let padding_uri = uri("/Padding.kt");
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();

    idx.index_content(
        &modifier_uri,
        "package androidx.compose.ui\n\
         object Modifier",
    );
    idx.index_content(
        &padding_uri,
        "package androidx.compose.ui\n\
         fun Modifier.padding(horizontal: Int = 0, vertical: Int = 0): Modifier = this",
    );
    idx.index_content(
        &caller_uri,
        "package com.example\n\
         fun render() {\n\
             Modifier.padding()\n\
         }",
    );

    // Resolving `padding` with qualifier `Modifier` should find the extension
    // function in Padding.kt, NOT return empty.
    let locs = resolve_symbol(&idx, "padding", Some("Modifier"), &caller_uri);
    assert!(
        !locs.is_empty(),
        "extension function Modifier.padding() not found; resolve_qualified only \
         searched the Modifier class file, missing extension fns in other files"
    );
    assert_eq!(
        locs[0].uri, padding_uri,
        "should point to Padding.kt where the extension function is defined, got {:?}",
        locs[0].uri
    );
}

/// Regression: `Modifier.padding()` with cursor on `padding` where `Modifier` is
/// NOT indexed at all (e.g. external unindexed library).  After
/// `resolve_qualified` returned empty, `resolve_symbol` fell through to
/// `resolve_symbol_inner` which scanned the current file.  If the current file had
/// a lambda parameter named `padding` (e.g. `{ padding -> ... }`), the fallthrough
/// incorrectly returned the lambda param location.
///
/// Expected behavior: when the qualifier is an uppercase identifier (class name)
/// that simply wasn't found in the index, the resolver should return empty rather
/// than falling through to local resolution.
#[test]
fn qualified_access_uppercase_fallthrough_does_not_match_lambda_param() {
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();

    // Modifier is NOT indexed (simulates external library).
    // The caller has both `Modifier.padding()` and a lambda `{ padding -> ... }`.
    idx.index_content(
        &caller_uri,
        "package com.example\n\
         class MyWidget {\n\
             fun render() {\n\
                 Box().apply { padding ->\n\
                     this@MyWidget.size = padding\n\
                 }\n\
                 Modifier.padding()\n\
             }\n\
         }",
    );

    // Resolving `padding` with qualifier `Modifier` — since Modifier is not
    // indexed, qualified resolution fails.  The fallthrough must NOT pick up
    // the lambda parameter `padding` from the apply block.
    let locs = resolve_symbol(&idx, "padding", Some("Modifier"), &caller_uri);
    assert!(
        locs.is_empty(),
        "qualified access with unindexed uppercase qualifier should return empty, \
         not fall through to lambda param; got {} location(s)",
        locs.len()
    );
}

/// Regression: `Modifier.padding()` where both Modifier and the extension
/// function are indexed.  Verifies the definition resolution chain works
/// end-to-end: resolve_symbol finds it, and the SymbolEntry carries a
/// non-empty detail (return type info).
#[test]
fn resolve_extension_fn_return_type_via_uppercase_qualifier() {
    let modifier_uri = uri("/Modifier.kt");
    let padding_uri = uri("/Padding.kt");
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();

    idx.index_content(
        &modifier_uri,
        "package com.example\n\
         object Modifier",
    );
    idx.index_content(
        &padding_uri,
        "package com.example\n\
         fun Modifier.padding(horizontal: Int = 0, vertical: Int = 0): Modifier = this",
    );
    idx.index_content(
        &caller_uri,
        "package com.example\n\
         fun render() {\n\
             val result = Modifier.padding()\n\
         }",
    );

    // The extension function definition should be findable.
    let locs = resolve_symbol(&idx, "padding", Some("Modifier"), &caller_uri);
    assert!(
        !locs.is_empty(),
        "Modifier.padding() extension fn not found"
    );
    assert_eq!(locs[0].uri, padding_uri);

    // The file data for Padding.kt should contain the padding symbol with
    // a detail that includes "padding" (confirming the symbol was indexed).
    use crate::indexer::resolution::IndexRead;
    let file_data = idx.get_file_data(padding_uri.as_str());
    assert!(file_data.is_some(), "Padding.kt not in file index");
    let data = file_data.unwrap();
    let has_padding = data.symbols.iter().any(|s| s.name == "padding");
    assert!(
        has_padding,
        "SymbolEntry for 'padding' not found in Padding.kt symbols; \
         return type detail will be empty"
    );
    // Verify the symbol has a non-empty detail with return type info.
    let symbol = data.symbols.iter().find(|s| s.name == "padding").unwrap();
    assert!(
        !symbol.detail.is_empty(),
        "SymbolEntry for 'padding' should have non-empty detail"
    );
    assert!(
        symbol.detail.contains("Modifier"),
        "SymbolEntry detail for 'padding' should contain return type 'Modifier', got: {}",
        symbol.detail
    );
}

/// Verifies that return type inference for extension functions works via
/// `find_extension_fn_return_type`.  The text-based inference path
/// (`find_method_return_type`) previously only checked `container == Some(type_base)`,
/// which missed extension functions (container=None).  This test confirms
/// the extension fn return type IS available when queried correctly.
#[test]
fn extension_fn_return_type_inference_works() {
    let modifier_uri = uri("/Modifier.kt");
    let padding_uri = uri("/Padding.kt");
    let idx = Indexer::new();

    idx.index_content(
        &modifier_uri,
        "package com.example\n\
         object Modifier",
    );
    idx.index_content(
        &padding_uri,
        "package com.example\n\
         fun Modifier.padding(horizontal: Int = 0, vertical: Int = 0): Modifier = this",
    );

    // Direct lookup via find_extension_fn_return_type should work
    let ret =
        crate::resolver::infer::find_extension_fn_return_type(&idx, "Modifier", "padding", None);
    assert_eq!(
        ret,
        Some("Modifier".to_string()),
        "find_extension_fn_return_type should resolve Modifier.padding() -> Modifier"
    );

    // find_method_return_type should now find it (falls back to extension fn lookup)
    let ret_via_container =
        crate::resolver::infer::find_method_return_type(&idx, "Modifier", "padding", None);
    assert_eq!(
        ret_via_container,
        Some("Modifier".to_string()),
        "find_method_return_type should find extension functions via fallback"
    );
}

/// Verifies the COMPREHENSIVE dispatch `find_method_return_type_for_type`
/// (used by CST chain) correctly finds extension function return types.
#[test]
fn method_return_type_for_type_finds_extension_fns() {
    let modifier_uri = uri("/Modifier.kt");
    let padding_uri = uri("/Padding.kt");
    let idx = Indexer::new();

    idx.index_content(
        &modifier_uri,
        "package com.example\n\
         object Modifier",
    );
    idx.index_content(
        &padding_uri,
        "package com.example\n\
         fun Modifier.padding(horizontal: Int = 0, vertical: Int = 0): Modifier = this",
    );

    // The comprehensive dispatch used by CST chain inference
    use crate::indexer::InferDeps;
    let ret = idx.find_method_return_type_for_type("Modifier", "padding", &padding_uri);
    assert_eq!(
        ret,
        Some("Modifier".to_string()),
        "find_method_return_type_for_type should find extension fn return type"
    );
}

// ── it-completion helpers ─────────────────────────────────────────────────

#[test]
fn extract_collection_element_list() {
    assert_eq!(
        extract_collection_element_type("List<Product>"),
        Some("Product".into())
    );
}

#[test]
fn extract_collection_element_mutable_list() {
    assert_eq!(
        extract_collection_element_type("MutableList<User>"),
        Some("User".into())
    );
}

#[test]
fn extract_collection_element_flow() {
    assert_eq!(
        extract_collection_element_type("Flow<Event>"),
        Some("Event".into())
    );
}

#[test]
fn extract_collection_element_state_flow() {
    assert_eq!(
        extract_collection_element_type("StateFlow<UiState>"),
        Some("UiState".into())
    );
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
    let lines: Vec<String> = vec!["val items: List<Product> = emptyList()".into()];
    assert_eq!(
        infer_type_in_lines_raw(&lines, "items"),
        Some("List<Product>".into())
    );
}

#[test]
fn infer_type_in_lines_raw_state_flow() {
    let lines: Vec<String> = vec!["    private val _state: StateFlow<UiState>".into()];
    assert_eq!(
        infer_type_in_lines_raw(&lines, "_state"),
        Some("StateFlow<UiState>".into())
    );
}

#[test]
fn infer_type_in_lines_raw_by_lazy_single_line() {
    // `val repo by lazy { UserRepository() }` — no explicit annotation
    let lines: Vec<String> = vec!["    private val repo by lazy { UserRepository() }".into()];
    assert_eq!(
        infer_type_in_lines_raw(&lines, "repo"),
        Some("UserRepository".into())
    );
}

#[test]
fn infer_type_in_lines_raw_explicit_annotation_takes_priority() {
    // `val repo: UserRepository by lazy { ... }` — annotation wins (first scan)
    let lines: Vec<String> =
        vec!["    private val repo: UserRepository by lazy { UserRepository() }".into()];
    assert_eq!(
        infer_type_in_lines_raw(&lines, "repo"),
        Some("UserRepository".into())
    );
}

#[test]
fn infer_type_in_lines_constructor_call() {
    // `val viewModel = DashboardViewModel()` — no annotation
    let lines: Vec<String> = vec!["    val viewModel = DashboardViewModel()".into()];
    assert_eq!(
        infer_type_in_lines(&lines, "viewModel"),
        Some("DashboardViewModel".into())
    );
}

#[test]
fn infer_type_in_lines_raw_constructor_call() {
    let lines: Vec<String> = vec!["    val viewModel = DashboardViewModel()".into()];
    assert_eq!(
        infer_type_in_lines_raw(&lines, "viewModel"),
        Some("DashboardViewModel".into())
    );
}

#[test]
fn infer_type_in_lines_class_literal_retrofit() {
    // `val api = retrofit.create(DashboardApi::class.java)` — class literal *inside parens*
    // should resolve to DashboardApi via the narrow pattern-3 path.
    let lines: Vec<String> = vec!["    val api = retrofit.create(DashboardApi::class.java)".into()];
    assert_eq!(
        infer_type_in_lines(&lines, "api"),
        Some("DashboardApi".into())
    );
}

#[test]
fn infer_type_in_lines_raw_class_literal_kotlin() {
    // `val api = retrofit.create(DashboardApi::class)` (no .java suffix)
    let lines: Vec<String> = vec!["    val api = retrofit.create(DashboardApi::class)".into()];
    assert_eq!(
        infer_type_in_lines_raw(&lines, "api"),
        Some("DashboardApi".into())
    );
}

#[test]
fn infer_type_in_lines_bare_class_literal_not_matched() {
    // `val key = SomeType::class` — bare class reference: key is KClass<SomeType>,
    // NOT SomeType.  The narrow pattern-3 only triggers when ::class is inside parens.
    let lines: Vec<String> = vec!["    val key = SomeType::class".into()];
    assert_eq!(infer_type_in_lines(&lines, "key"), None);
}

#[test]
fn infer_type_in_lines_di_inject() {
    // `val repo by inject<UserRepository>()` — Koin DI pattern
    let lines: Vec<String> = vec!["    val repo = inject<UserRepository>()".into()];
    assert_eq!(
        infer_type_in_lines(&lines, "repo"),
        Some("UserRepository".into())
    );
}

#[test]
fn infer_type_annotation_still_wins_over_rhs() {
    // Explicit annotation takes priority over RHS inference
    let lines: Vec<String> = vec!["    val repo: UserRepository = OtherRepository()".into()];
    assert_eq!(
        infer_type_in_lines(&lines, "repo"),
        Some("UserRepository".into())
    );
}

#[test]
fn infer_type_rhs_no_false_positive_lowercase() {
    // `val x = someFactory.create()` — lowercase constructor → no inference
    let lines: Vec<String> = vec!["    val x = someFactory.create()".into()];
    assert_eq!(infer_type_in_lines(&lines, "x"), None);
}

#[test]
fn infer_type_rhs_no_false_positive_equality() {
    // `if (x == SomeType())` must not match as an assignment
    let lines: Vec<String> = vec!["    if (x == SomeType()) {".into()];
    assert_eq!(infer_type_in_lines(&lines, "x"), None);
}

#[test]
fn resolve_method_via_class_literal_type_inference() {
    // `val api = retrofit.create(DashboardApi::class.java)` — no annotation
    // dot-completion on `api.someMethod()` should resolve into DashboardApi
    let api_uri = uri("/DashboardApi.kt");
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(
        &api_uri,
        "package com.example\ninterface DashboardApi {\n    fun loadData(): String\n}",
    );
    idx.index_content(&caller_uri,
            "package com.example\nval retrofit = TODO()\nval api = retrofit.create(DashboardApi::class.java)\nfun test() { api.loadData() }");

    let locs = resolve_symbol(&idx, "loadData", Some("api"), &caller_uri);
    assert!(
        !locs.is_empty(),
        "loadData not found via class literal type inference"
    );
    assert_eq!(locs[0].uri, api_uri);
}

// ── method return type inference (infer_variable_type) ───────────────────

#[test]
fn infer_variable_type_method_return_type() {
    // `val response = accountApiService.getAccountDetail(body)` where
    // accountApiService: AccountApiService is annotated in the same file
    let service_uri = uri("/AccountApiService.kt");
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(&service_uri,
            "package com.example\ninterface AccountApiService {\n    fun getAccountDetail(body: AccountDetailRequestBody): Response<AccountDetail>\n}");
    idx.index_content(&caller_uri,
            "package com.example\nclass Repo(val accountApiService: AccountApiService) {\n    fun load() {\n        val response = accountApiService.getAccountDetail(AccountDetailRequestBody(123))\n    }\n}");

    let result = infer_variable_type(&idx, "response", &caller_uri);
    assert_eq!(
        result,
        Some("Response<AccountDetail>".into()),
        "should infer return type via method lookup"
    );
}

#[test]
fn infer_variable_type_unannotated_snapshot_no_declared_names_rejection() {
    // Verify that the declared_names fast-reject no longer blocks unannotated vars
    // when only a snapshot (no live_lines) is available.
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(
        &caller_uri,
        "package com.example\nval vm = DashboardViewModel()",
    );

    // `vm` has no `:` annotation, so declared_names would not contain it.
    // It must still be resolved via the assignment scan.
    let result = infer_variable_type(&idx, "vm", &caller_uri);
    assert_eq!(
        result,
        Some("DashboardViewModel".into()),
        "unannotated var must still be resolved from snapshot"
    );
}

#[test]
fn goto_def_on_named_lambda_param_resolves_to_declaration_line() {
    // items.forEach { product ->
    //     product.name   ← gd on `product` here
    // go-to-def should jump to the `{ product ->` declaration line (line 2)
    let caller_uri = uri("/Caller.kt");
    let product_uri = uri("/Product.kt");
    let idx = Indexer::new();
    idx.index_content(
        &product_uri,
        "package com.example\ndata class Product(val name: String)",
    );
    idx.index_content(&caller_uri,
            "package com.example\nval items: List<Product> = emptyList()\nitems.forEach { product ->\n    product.name\n}");

    // step 1.5 finds `{ product ->` via the lambda arrow pattern
    let locs = resolve_symbol(&idx, "product", None, &caller_uri);
    assert!(!locs.is_empty(), "lambda param 'product' not found");
    // Must land in the same file (the lambda declaration), NOT in rg results
    assert_eq!(
        locs[0].uri, caller_uri,
        "should stay in Caller.kt at the lambda decl"
    );
    // Line 2 is where `items.forEach { product ->` is declared
    assert_eq!(
        locs[0].range.start.line, 2,
        "should point to the lambda arrow line"
    );
}

// ── complete_dot scoping — no local fns leak ─────────────────────────────

#[test]
fn dot_complete_does_not_leak_top_level_fns() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///a/Keys.kt").unwrap();
    idx.index_content(&uri, "package a\n\nobject ProductKey {\n    val CARD = \"card\"\n    val LOAN = \"loan\"\n    fun fromString(s: String) = s\n}\n\nfun topLevelHelper() {}\n");

    // Simulate a variable typed as ProductKey in another file.
    let caller_uri = Url::parse("file:///a/Caller.kt").unwrap();
    idx.index_content(&caller_uri, "package a\nval key: ProductKey = TODO()");

    let items = complete_dot(&idx, "ProductKey", &caller_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(labels.contains(&"fromString"), "member fun should appear");
    assert!(labels.contains(&"CARD"), "member val should appear");
    assert!(
        !labels.contains(&"topLevelHelper"),
        "top-level fn must NOT leak into dot completions"
    );
}

#[test]
fn dot_complete_includes_inherited_members() {
    // `AccountDetailResponseBody` extends `Account` (Java-style parent).
    // Dot-completion on an instance of `AccountDetailResponseBody` must include
    // fields declared in the parent `Account` class.
    let account_uri = uri("/Account.kt");
    let response_uri = uri("/AccountDetailResponseBody.kt");
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();

    idx.index_content(&account_uri,
            "package com.example\nopen class Account {\n    val accountName: String = \"\"\n    val accountId: String = \"\"\n}");
    idx.index_content(&response_uri,
            "package com.example\ndata class AccountDetailResponseBody(\n    val feePlanName: String?\n) : Account()");
    idx.index_content(
        &caller_uri,
        "package com.example\nval resp: AccountDetailResponseBody = TODO()",
    );

    let items = complete_dot(&idx, "AccountDetailResponseBody", &caller_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();

    // Direct members
    assert!(
        labels.contains(&"feePlanName"),
        "direct field should appear"
    );
    // Inherited members from Account
    assert!(
        labels.contains(&"accountName"),
        "inherited field from parent must appear"
    );
    assert!(
        labels.contains(&"accountId"),
        "inherited field from parent must appear"
    );
}

// ── object with annotated getter properties ───────────────────────────────

#[test]
fn dot_complete_object_with_annotated_getter_properties() {
    // Issue #125: Compose's MaterialTheme file declares BOTH `fun MaterialTheme(...)`
    // AND `object MaterialTheme { ... }`. The old `find()` picked the function first,
    // returning empty completions. The fix: prefer type-kind symbols over functions.
    let idx = Indexer::new();

    // Mirrors the real MaterialTheme.kt: function first, object second, same file.
    let lib_uri = Url::parse("file:///lib/MaterialTheme.kt").unwrap();
    idx.index_content(
        &lib_uri,
        "package androidx.compose.material3\n\n\
         @Composable\n\
         fun MaterialTheme(\n    colorScheme: ColorScheme = MaterialTheme.colorScheme,\n    content: @Composable () -> Unit\n) {}\n\n\
         object MaterialTheme {\n\
             val colorScheme: ColorScheme\n\
                 @Composable get() = LocalColorScheme.current\n\n\
             val typography: Typography\n\
                 @Composable get() = LocalTypography.current\n\n\
             val shapes: Shapes\n\
                 @Composable get() = LocalShapes.current\n\
         }\n",
    );

    let caller_uri = Url::parse("file:///app/Screen.kt").unwrap();
    idx.index_content(
        &caller_uri,
        "package com.example\nimport androidx.compose.material3.MaterialTheme\nfun screen() {}",
    );

    let items = complete_dot(&idx, "MaterialTheme", &caller_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(
        labels.contains(&"colorScheme"),
        "object member should appear (not function body); got: {labels:?}"
    );
    assert!(
        labels.contains(&"typography"),
        "typography should appear; got: {labels:?}"
    );
    assert!(
        labels.contains(&"shapes"),
        "shapes should appear; got: {labels:?}"
    );
}

// ── complete_bare distance sorting ───────────────────────────────────────

#[test]
fn complete_bare_local_before_same_pkg() {
    let idx = Indexer::new();
    let local_uri = Url::parse("file:///pkg/a/Local.kt").unwrap();
    let other_uri = Url::parse("file:///pkg/a/Other.kt").unwrap();
    // local file has "localFoo"
    idx.index_content(&local_uri, "package a\nfun localFoo() {}");
    // same-package file has "pkgBar"
    idx.index_content(&other_uri, "package a\nfun pkgBar() {}");

    let (items, _) = complete_bare(&idx, "", &local_uri, false, false, None);

    let local_pos = items.iter().position(|i| i.label == "localFoo");
    let pkg_pos = items.iter().position(|i| i.label == "pkgBar");
    assert!(local_pos.is_some(), "localFoo should appear");
    assert!(pkg_pos.is_some(), "pkgBar should appear");

    // sort_text with tier prefix means local (0:…) sorts before same-pkg (1:…).
    let local_sort = items[local_pos.unwrap()].sort_text.as_deref().unwrap_or("");
    let pkg_sort = items[pkg_pos.unwrap()].sort_text.as_deref().unwrap_or("");
    assert!(
        local_sort < pkg_sort,
        "local tier sort_text should be less than same-pkg tier"
    );
}

#[test]
fn complete_bare_test_symbols_visible_only_to_test_callers() {
    let idx = Indexer::new();
    let main_uri = Url::parse("file:///workspace/src/main/kotlin/a/Main.kt").unwrap();
    let test_uri = Url::parse("file:///workspace/src/test/kotlin/a/TestCaller.kt").unwrap();
    let helper_uri = Url::parse("file:///workspace/src/test/kotlin/a/TestHelper.kt").unwrap();

    idx.index_content(&main_uri, "package a\nfun mainCaller() {}");
    idx.index_content(&test_uri, "package a\nfun testCaller() {}");
    idx.index_content(&helper_uri, "package a\nfun testOnlyHelper() {}");

    let (main_items, _) = complete_bare(&idx, "testOnly", &main_uri, false, false, None);
    assert!(
        main_items.iter().all(|item| item.label != "testOnlyHelper"),
        "main callers must not see same-package test symbols: {main_items:?}"
    );

    let (test_items, _) = complete_bare(&idx, "testOnly", &test_uri, false, false, None);
    assert!(
        test_items.iter().any(|item| item.label == "testOnlyHelper"),
        "test callers must see same-package test symbols: {test_items:?}"
    );
}

// ── dot_completions_for type filtering ────────────────────────────────────

#[test]
fn dot_completions_string_receiver_has_string_fns() {
    let items = dot_completions_for("String", false);
    let names: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(names.contains(&"trim"), "String should have trim()");
    assert!(names.contains(&"split"), "String should have split()");
    assert!(names.contains(&"let"), "String should have scope fn let()");
    // Collection fns should NOT appear on String
    assert!(!names.contains(&"map"), "String should NOT have map()");
    assert!(
        !names.contains(&"filter"),
        "String should NOT have filter()"
    );
}

#[test]
fn dot_completions_list_receiver_has_collection_fns() {
    let items = dot_completions_for("List", false);
    let names: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(names.contains(&"map"), "List should have map()");
    assert!(names.contains(&"filter"), "List should have filter()");
    assert!(names.contains(&"forEach"), "List should have forEach()");
    assert!(names.contains(&"let"), "List should have scope fn let()");
    // String-only fns should NOT appear on List
    assert!(!names.contains(&"trim"), "List should NOT have trim()");
    assert!(!names.contains(&"split"), "List should NOT have split()");
}

#[test]
fn dot_completions_custom_type_has_scope_fns_only() {
    let items = dot_completions_for("MyDomainClass", false);
    let names: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(names.contains(&"let"), "domain type should have let()");
    assert!(names.contains(&"apply"), "domain type should have apply()");
    assert!(
        !names.contains(&"trim"),
        "domain type should NOT have trim()"
    );
    assert!(!names.contains(&"map"), "domain type should NOT have map()");
    assert!(
        !names.contains(&"filter"),
        "domain type should NOT have filter()"
    );
}

// ── supers CST extraction – annotation handling ──────────────────────────

#[test]
fn extract_supers_annotation_same_line() {
    let s = kotlin_supers("@Suppress(\"unused\") class Foo : Bar {}");
    assert!(s.contains(&"Bar".to_string()), "got {s:?}");
}

#[test]
fn extract_supers_annotation_separate_line() {
    let src = "@Module\nclass Foo : Bar, Baz {}";
    let s = kotlin_supers(src);
    assert!(s.contains(&"Bar".to_string()), "got {s:?}");
    assert!(s.contains(&"Baz".to_string()), "got {s:?}");
}

#[test]
fn extract_supers_field_inject_annotation() {
    let s = kotlin_supers("@field:Inject\nclass Foo {}");
    assert!(
        s.is_empty(),
        "annotation-only line should produce no supers, got {s:?}"
    );
}

#[test]
fn extract_supers_multiple_annotations() {
    let src = "@Module\n@Provides\nclass FooModule : BaseModule() {}";
    let s = kotlin_supers(src);
    assert!(s.contains(&"BaseModule".to_string()), "got {s:?}");
}

// ── auto-import helpers ───────────────────────────────────────────────────

fn make_import_entry(
    full_path: &str,
    local_name: &str,
    is_star: bool,
) -> crate::types::ImportEntry {
    crate::types::ImportEntry {
        full_path: full_path.to_string(),
        local_name: local_name.to_string(),
        is_star,
    }
}

#[test]
fn already_imported_exact() {
    let imports = vec![make_import_entry("com.example.Foo", "Foo", false)];
    assert!(already_imported("com.example.Foo", &imports));
}

#[test]
fn already_imported_alias_not_counted() {
    // `import com.example.Foo as Bar` — Foo is not usable as Foo
    let imports = vec![make_import_entry("com.example.Foo", "Bar", false)];
    assert!(!already_imported("com.example.Foo", &imports));
}

#[test]
fn already_imported_star() {
    let imports = vec![make_import_entry("com.example", "*", true)];
    assert!(already_imported("com.example.Foo", &imports));
}

#[test]
fn already_imported_star_wrong_pkg() {
    let imports = vec![make_import_entry("com.other", "*", true)];
    assert!(!already_imported("com.example.Foo", &imports));
}

#[test]
fn import_insertion_after_last_import() {
    let lines = vec![
        "package com.example".to_string(),
        "".to_string(),
        "import com.example.Bar".to_string(),
        "import com.example.Baz".to_string(),
        "".to_string(),
        "class Foo {}".to_string(),
    ];
    assert_eq!(import_insertion_line(&lines), 4); // line after last import
}

#[test]
fn import_insertion_after_package_no_imports() {
    let lines = vec![
        "package com.example".to_string(),
        "".to_string(),
        "class Foo {}".to_string(),
    ];
    assert_eq!(import_insertion_line(&lines), 1); // line after package
}

#[test]
fn import_insertion_at_top_no_package_no_imports() {
    let lines = vec!["class Foo {}".to_string()];
    assert_eq!(import_insertion_line(&lines), 0);
}

#[test]
fn auto_import_completion_adds_edit() {
    let idx = Indexer::new();
    // Library file in a different package.
    let lib_uri = uri("/lib/Composable.kt");
    idx.index_content(
        &lib_uri,
        "package androidx.compose.runtime\nannotation class Composable",
    );
    // Current file — different package, no imports.
    let cur_uri = uri("/app/Screen.kt");
    idx.index_content(
        &cur_uri,
        "package com.example.app\n\nfun Screen() {\n    Comp\n}",
    );

    let (items, _) = complete_symbol(&idx, "Comp", None, &cur_uri, false, None);
    let import_item = items.iter().find(|i| i.label == "Composable");
    assert!(
        import_item.is_some(),
        "Composable should appear in completions"
    );
    let edits = import_item.unwrap().additional_text_edits.as_ref();
    assert!(edits.is_some(), "additionalTextEdits should be present");
    let edit_text = &edits.unwrap()[0].new_text;
    assert!(
        edit_text.contains("import androidx.compose.runtime.Composable"),
        "edit should add correct import, got: {edit_text}"
    );
}

#[test]
fn auto_import_skipped_when_already_imported() {
    let idx = Indexer::new();
    let lib_uri = uri("/lib/Foo.kt");
    idx.index_content(&lib_uri, "package com.lib\nclass Foo");
    let cur_uri = uri("/app/Bar.kt");
    // Already imports com.lib.Foo.
    idx.index_content(
        &cur_uri,
        "package com.app\nimport com.lib.Foo\nclass Bar { val f: Foo = Foo() }",
    );

    let (items, _) = complete_symbol(&idx, "Foo", None, &cur_uri, false, None);
    let foo_items: Vec<_> = items.iter().filter(|i| i.label == "Foo").collect();
    // May appear (from tier-0/1 or tier-2 without edit) but must not have an import edit.
    for item in &foo_items {
        assert!(
            item.additional_text_edits.is_none()
                || item.additional_text_edits.as_ref().unwrap().is_empty(),
            "already-imported symbol must not carry an import edit"
        );
    }
}

#[test]
fn auto_import_skipped_same_package() {
    let idx = Indexer::new();
    let lib_uri = uri("/app/Foo.kt");
    idx.index_content(&lib_uri, "package com.example\nclass Foo");
    let cur_uri = uri("/app/Bar.kt");
    idx.index_content(&cur_uri, "package com.example\nclass Bar");

    let (items, _) = complete_symbol(&idx, "Foo", None, &cur_uri, false, None);
    // Foo is in the same package — any completion item for it must have no import edit.
    for item in items.iter().filter(|i| i.label == "Foo") {
        assert!(
            item.additional_text_edits.is_none()
                || item.additional_text_edits.as_ref().unwrap().is_empty(),
            "same-package symbol must not carry an import edit"
        );
    }
}

#[test]
fn same_package_test_helpers_appear_when_completing_from_test_file() {
    let idx = Indexer::new();
    let helper_uri = uri("/src/test/kotlin/com/example/TestHelpers.kt");
    idx.index_content(&helper_uri, "package com.example\nfun helperThing() = Unit");
    let cur_uri = uri("/src/test/kotlin/com/example/CurrentTest.kt");
    idx.index_content(&cur_uri, "package com.example\nclass CurrentTest");

    let (items, _) = complete_symbol(&idx, "hel", None, &cur_uri, false, None);
    assert!(
        items.iter().any(|item| item.label == "helperThing"),
        "expected same-package helper from sibling test file in completions"
    );
}

#[test]
fn auto_import_two_packages_two_items() {
    let idx = Indexer::new();
    idx.index_content(
        &uri("/m3/Button.kt"),
        "package androidx.compose.material3\nclass Button",
    );
    idx.index_content(
        &uri("/m1/Button.kt"),
        "package androidx.compose.material\nclass Button",
    );
    let cur_uri = uri("/app/Screen.kt");
    idx.index_content(&cur_uri, "package com.example\nfun screen() {}");

    let (items, _) = complete_symbol(&idx, "Button", None, &cur_uri, false, None);
    let button_items: Vec<_> = items.iter().filter(|i| i.label == "Button").collect();
    assert_eq!(
        button_items.len(),
        2,
        "Two Button symbols from different packages should yield two items"
    );
    let details: Vec<_> = button_items
        .iter()
        .filter_map(|i| i.detail.as_deref())
        .collect();
    assert!(
        details.iter().any(|d| d.contains("material3")),
        "One item should mention material3"
    );
    assert!(
        details
            .iter()
            .any(|d| d.contains("material") && !d.contains("material3")),
        "One item should mention material"
    );
}

/// Identically-named candidates from different packages must be tellable
/// apart in the completion LIST itself, not only via `detail` (which many
/// clients render only for the selected item — and which the materialized
/// path replaces with a signature). When the client advertised
/// `labelDetailsSupport`, every cross-package item carries its package
/// qualifier in the LSP-standard `labelDetails.description` slot.
#[test]
fn cross_package_items_carry_package_hint_in_label_details() {
    let idx = Indexer::new();
    idx.client_label_details_support
        .store(true, std::sync::atomic::Ordering::Relaxed);
    idx.index_content(
        &uri("/m3/Button.kt"),
        "package androidx.compose.material3\nclass Button",
    );
    idx.index_content(
        &uri("/m1/Button.kt"),
        "package androidx.compose.material\nclass Button",
    );
    let cur_uri = uri("/app/Screen.kt");
    idx.index_content(&cur_uri, "package com.example\nfun screen() {}");

    let (items, _) = complete_symbol(&idx, "Button", None, &cur_uri, false, None);
    let hints: Vec<_> = items
        .iter()
        .filter(|i| i.label == "Button")
        .map(|i| {
            i.label_details
                .as_ref()
                .and_then(|ld| ld.description.as_deref())
                .unwrap_or_else(|| panic!("every cross-package item needs a package hint: {i:?}"))
        })
        .collect();
    assert!(
        hints.contains(&"androidx.compose.material3")
            && hints.contains(&"androidx.compose.material"),
        "each candidate must carry its own package in labelDetails.description; got {hints:?}"
    );
}

#[test]
fn caps_mode_hides_lowercase_functions() {
    let idx = Indexer::new();
    let cur_uri = uri("/app/Screen.kt");
    // File with both a class and a lowercase function.
    idx.index_content(
        &cur_uri,
        "package com.example\nclass Column\nfun collectAsState() {}",
    );

    let (items, _) = complete_symbol(&idx, "Col", None, &cur_uri, false, None);
    // Column (uppercase) should appear.
    assert!(
        items.iter().any(|i| i.label == "Column"),
        "Column should appear in caps mode"
    );
    // collectAsState (lowercase) should NOT appear when typing uppercase prefix.
    assert!(
        !items.iter().any(|i| i.label == "collectAsState"),
        "lowercase function must not appear when typing uppercase prefix"
    );
}

#[test]
fn lowercase_mode_hides_classes() {
    let idx = Indexer::new();
    let cur_uri = uri("/app/Screen.kt");
    idx.index_content(
        &cur_uri,
        "package com.example\nclass Column\nfun collectAsState() {}",
    );

    let (items, _) = complete_symbol(&idx, "col", None, &cur_uri, false, None);
    // collectAsState (lowercase) should appear.
    assert!(
        items.iter().any(|i| i.label == "collectAsState"),
        "lowercase function should appear in lowercase mode"
    );
    // Column (uppercase) should NOT appear when typing lowercase prefix.
    assert!(
        !items.iter().any(|i| i.label == "Column"),
        "CamelCase class must not appear when typing lowercase prefix"
    );
}

#[test]
fn tier2_suppressed_when_name_visible_in_current_file() {
    let idx = Indexer::new();
    idx.index_content(&uri("/lib/Foo.kt"), "package com.lib\nclass Foo");
    let cur_uri = uri("/app/Bar.kt");
    idx.index_content(&cur_uri, "package com.example\nclass Foo");

    let (items, _) = complete_symbol(&idx, "Foo", None, &cur_uri, false, None);
    let foo_items: Vec<_> = items.iter().filter(|i| i.label == "Foo").collect();
    assert_eq!(
        foo_items.len(),
        1,
        "Foo defined in current file must not generate a duplicate tier-2 item"
    );
    assert!(
        foo_items[0].additional_text_edits.is_none()
            || foo_items[0]
                .additional_text_edits
                .as_ref()
                .unwrap()
                .is_empty(),
        "tier-0 item must not carry an import edit"
    );
}

// ── match_score ────────────────────────────────────────────────────────────

#[test]
fn match_score_prefix_is_best() {
    assert_eq!(match_score("Column", "Col"), Some(0));
    assert_eq!(match_score("column", "col"), Some(0));
}

#[test]
fn match_score_acronym_is_second() {
    // CB → ColumnButton (C=Column, B=Button)
    assert_eq!(match_score("ColumnButton", "CB"), Some(1));
    // mSF → myStateFlow
    assert_eq!(match_score("myStateFlow", "mSF"), Some(1));
    // underscore-prefixed private fields: _ColumnButton, _myStateFlow
    assert_eq!(match_score("_ColumnButton", "CB"), Some(1));
    assert_eq!(match_score("_myStateFlow", "mSF"), Some(1));
}

#[test]
fn match_score_substring_is_third() {
    assert_eq!(match_score("RecyclerView", "View"), Some(2));
}

#[test]
fn match_score_no_match_returns_none() {
    assert_eq!(match_score("Column", "xyz"), None);
}

#[test]
fn match_score_prefix_beats_acronym_in_sort() {
    let idx = Indexer::new();
    let cur_uri = uri("/app/Screen.kt");
    // Column → prefix match for "Col"; ColumnButton → acronym for "CB" but prefix for "Col"
    idx.index_content(
        &cur_uri,
        "package com.example\nclass Column\nclass ColumnButton",
    );

    let (items, _) = complete_symbol(&idx, "Col", None, &cur_uri, false, None);
    let col_pos = items.iter().position(|i| i.label == "Column").unwrap();
    let colbtn_pos = items
        .iter()
        .position(|i| i.label == "ColumnButton")
        .unwrap();
    // Both are prefix matches; Column (shorter) should sort before ColumnButton lexicographically.
    assert!(
        col_pos < colbtn_pos || {
            // Accept either order — both are score-0, lexicographic tie-break.
            let a = items[col_pos].sort_text.as_deref().unwrap_or("");
            let b = items[colbtn_pos].sort_text.as_deref().unwrap_or("");
            a <= b
        },
        "Column should sort ≤ ColumnButton for prefix 'Col'"
    );
}

#[test]
fn tier2_fires_for_single_char_prefix() {
    let idx = Indexer::new();
    idx.index_content(&uri("/lib/Foo.kt"), "package com.lib\nclass Column");
    let cur_uri = uri("/app/Bar.kt");
    idx.index_content(&cur_uri, "package com.example\n");

    // Single char 'C' — tier-2 now fires for single-char starts-with matches,
    // so Column (cross-pkg) IS returned (score 0: case-insensitive prefix match).
    // Being a cross-package symbol it must carry an auto-import edit.
    let (items, _) = complete_symbol(&idx, "C", None, &cur_uri, false, None);
    assert!(
        items
            .iter()
            .any(|i| i.label == "Column" && i.additional_text_edits.is_some()),
        "tier-2 must fire for single-char prefix and include auto-import edit"
    );

    // Two chars 'Co' — tier-2 also fires.
    let (items, _) = complete_symbol(&idx, "Co", None, &cur_uri, false, None);
    assert!(
        items.iter().any(|i| i.label == "Column"),
        "tier-2 must fire for prefix length >= 2"
    );
}

#[test]
fn tier2_single_char_excludes_camel_acronym_noise() {
    let idx = Indexer::new();
    // "SomeButton" would camel-acronym-match "B" (score 1) but must NOT appear
    // for single-char prefix — only starts-with (score 0) is allowed.
    idx.index_content(
        &uri("/lib/Foo.kt"),
        "package com.lib\nclass SomeButton\nclass Button",
    );
    let cur_uri = uri("/app/Bar.kt");
    idx.index_content(&cur_uri, "package com.example\n");

    let (items, _) = complete_symbol(&idx, "B", None, &cur_uri, false, None);
    assert!(
        items.iter().any(|i| i.label == "Button"),
        "Button (starts with B) must appear"
    );
    assert!(
        !items.iter().any(|i| i.label == "SomeButton"),
        "SomeButton (camel-acronym score 1) must not appear for single-char prefix"
    );
}

#[test]
fn result_cap_sets_hit_cap() {
    let idx = Indexer::new();
    let cur_uri = uri("/app/Screen.kt");
    // Generate 600 unique class names → exceeds COMPLETION_CAP (500).
    let src = (0..600)
        .map(|i| format!("class Cls{i:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    idx.index_content(&cur_uri, &format!("package com.example\n{src}"));

    let (items, hit_cap) = complete_symbol(&idx, "Cls", None, &cur_uri, false, None);
    assert!(
        hit_cap,
        "hit_cap should be true when result count exceeds COMPLETION_CAP"
    );
    assert_eq!(
        items.len(),
        crate::resolver::COMPLETION_CAP,
        "items must be truncated to cap"
    );
}

#[test]
fn annotation_context_hides_functions() {
    let idx = Indexer::new();
    let cur_uri = uri("/app/Screen.kt");
    idx.index_content(
        &cur_uri,
        "package com.example\nannotation class Composable\nfun composable() {}",
    );

    let line = "@Composable";
    let prefix = "Composable";
    let annotation_only = is_annotation_context(line, prefix);
    assert!(annotation_only, "should detect annotation context");

    let (items, _) = complete_symbol_with_context(&idx, prefix, None, &cur_uri, false, true, None);
    // Annotation class should appear.
    assert!(
        items.iter().any(|i| i.label == "Composable"),
        "annotation class Composable must appear"
    );
    // Lowercase function should not appear.
    assert!(
        !items.iter().any(|i| i.label == "composable"),
        "function composable must not appear in annotation context"
    );
}

#[test]
fn annotation_empty_prefix_returns_cross_package_annotations() {
    // Bug #122 — typing `@` alone (empty prefix) must not return empty list,
    // otherwise the editor closes the session and subsequent chars don't reopen it.
    let idx = Indexer::new();
    let cur_uri = uri("/app/src/Screen.kt");
    let other_uri = uri("/app/other/Annotations.kt");
    idx.index_content(&cur_uri, "package com.example.src\nclass Screen");
    idx.index_content(
        &other_uri,
        "package com.example.other\nannotation class Composable",
    );

    // Empty prefix, annotation_only = true (simulates user typing `@` alone).
    let (items, _) = complete_symbol_with_context(&idx, "", None, &cur_uri, false, true, None);
    assert!(
        items.iter().any(|i| i.label == "Composable"),
        "cross-package annotation class must appear with empty prefix in annotation context; got: {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
}

#[test]
fn annotation_context_hides_stdlib_functions() {
    // Bug: collect_stdlib() does not check annotation_only, so stdlib functions
    // (println, listOf, TODO, live templates like `fun`) appear in annotation context.
    let idx = Indexer::new();
    let cur_uri = uri("/app/src/Screen.kt");
    idx.index_content(&cur_uri, "package com.example\nclass Screen");

    let (items, _) = complete_symbol_with_context(&idx, "", None, &cur_uri, true, true, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(
        !labels.contains(&"println"),
        "stdlib function println must not appear in annotation context; got: {labels:?}"
    );
    assert!(
        !labels.contains(&"listOf"),
        "stdlib function listOf must not appear in annotation context; got: {labels:?}"
    );
    assert!(
        !labels.contains(&"fun"),
        "live template 'fun' must not appear in annotation context; got: {labels:?}"
    );
}

#[test]
fn camel_mode_hides_screaming_snake() {
    let idx = Indexer::new();
    let cur_uri = uri("/app/Screen.kt");
    idx.index_content(&cur_uri,
            "package com.example\nclass ChildDashboardViewModel\nconst val CHILD_DASHBOARD_MAX = 10\nval CHILD_COUNT = 5");

    // Typing CamelCase prefix — SCREAMING_SNAKE constants must not appear.
    let (items, _) = complete_symbol(&idx, "Child", None, &cur_uri, false, None);
    assert!(
        items.iter().any(|i| i.label == "ChildDashboardViewModel"),
        "CamelCase class must appear"
    );
    assert!(
        !items.iter().any(|i| i.label == "CHILD_DASHBOARD_MAX"),
        "SCREAMING_SNAKE constant must be hidden in camel_mode"
    );
    assert!(
        !items.iter().any(|i| i.label == "CHILD_COUNT"),
        "SCREAMING_SNAKE val must be hidden in camel_mode"
    );

    // Typing all-uppercase prefix — SCREAMING_SNAKE constants may appear.
    let (items2, _) = complete_symbol(&idx, "CHILD", None, &cur_uri, false, None);
    assert!(
        items2.iter().any(|i| i.label == "CHILD_DASHBOARD_MAX"),
        "SCREAMING_SNAKE constant must appear when prefix is uppercase"
    );
}

#[test]
fn long_prefix_tier2_not_crowded_out() {
    // Even when the same-package has many substring-matching symbols,
    // a cross-package prefix match must survive with a 4+ char prefix.
    let idx = Indexer::new();
    let cur_uri = uri("/app/pkg/Screen.kt");
    let other_uri = uri("/app/pkg/Other.kt");
    let cross_uri = uri("/app/other/Cross.kt");

    // 60 same-pkg classes that contain "child" as substring but don't start with it.
    let same_pkg: String = (0..60)
        .map(|i| format!("class Something{i}Child"))
        .collect::<Vec<_>>()
        .join("\n");
    idx.index_content(&cur_uri, "package com.example\n");
    idx.index_content(&other_uri, &format!("package com.example\n{same_pkg}"));
    // Cross-package class with prefix match.
    idx.index_content(
        &cross_uri,
        "package com.other\nclass ChildDashboardViewModel",
    );

    // Short prefix (2 chars): substring allowed, cross-pkg fires.
    let (short, _) = complete_symbol(&idx, "Ch", None, &cur_uri, false, None);
    assert!(
        short.iter().any(|i| i.label == "ChildDashboardViewModel"),
        "cross-pkg must appear for short prefix"
    );

    // Long prefix (5 chars): substring suppressed for tier-0/1 — cross-pkg prefix match wins.
    let (long, _) = complete_symbol(&idx, "Child", None, &cur_uri, false, None);
    assert!(
        long.iter().any(|i| i.label == "ChildDashboardViewModel"),
        "cross-pkg prefix match must survive long prefix even with many same-pkg substring hits"
    );
    // Same-pkg substring hits (Something*Child) must be absent for long prefix.
    assert!(
        !long
            .iter()
            .any(|i| i.label.ends_with("Child") && i.label.starts_with("Something")),
        "same-pkg substring matches must be filtered for long prefix"
    );
}

#[test]
fn library_file_appears_in_cross_package_completion() {
    // Regression: library (sourcePaths) symbols must appear in bare-word completion
    // even when they live in a different package from the current file.
    let idx = Indexer::new();
    let cur_uri = uri("/project/src/Screen.kt");
    let lib_uri: Url = "file:///home/user/.kmp-lsp/sources/compose/Composable.kt"
        .parse()
        .unwrap();
    let col_uri: Url = "file:///home/user/.kmp-lsp/sources/compose/Column.kt"
        .parse()
        .unwrap();

    idx.index_content(
        &lib_uri,
        "package androidx.compose.runtime\nannotation class Composable",
    );
    idx.index_content(
        &col_uri,
        "package androidx.compose.foundation.layout\nfun Column() {}",
    );
    idx.index_content(&cur_uri, "package com.example\n");

    let (items, _) = complete_bare(&idx, "Comp", &cur_uri, false, false, None);
    assert!(
        items.iter().any(|i| i.label == "Composable"),
        "Composable from library file must appear for prefix 'Comp'"
    );

    // Import edit must be included so the editor can auto-import the symbol.
    let composable = items.iter().find(|i| i.label == "Composable").unwrap();
    assert!(
        composable.additional_text_edits.is_some(),
        "Composable completion must include an auto-import text edit"
    );

    let (items2, _) = complete_bare(&idx, "Col", &cur_uri, false, false, None);
    assert!(
        items2.iter().any(|i| i.label == "Column"),
        "Column (fun) from library file must appear for prefix 'Col'"
    );
}

#[test]
fn cross_file_type_subst_multi_class_same_file() {
    // Regression test: when multiple classes in one file extend the same generic base
    // with different type args, completion must pick the correct substitution based on
    // which class the caller is in (via cursor_line).
    let idx = Indexer::new();

    let base_uri = Url::parse("file:///a/Base.kt").unwrap();
    idx.index_content(
        &base_uri,
        "package a\nclass Base<T> {\n  fun get(): T = TODO()\n}",
    );

    let caller_uri = Url::parse("file:///a/Caller.kt").unwrap();
    // Two classes in same file, each extends Base with different type arg
    idx.index_content(
        &caller_uri,
        "package a\n\
         class CallerA : Base<String>() {\n\
             fun testA() { val x = Base<String>()\n\
         }\n\
         }\n\
         \n\
         class CallerB : Base<Int>() {\n\
             fun testB() { val x = Base<Int>()\n\
         }\n\
         }",
    );

    // For CallerA (around line 2-3), Base members should show String substitution
    // This test verifies cursor_line is threaded through completion → symbols_from_nested_type
    // → completion_item_for_nested_symbol → cross_file_type_subst
    let items_a = complete_dot(&idx, "Base", &caller_uri, false, Some(2));
    let get_item_a = items_a.iter().find(|i| i.label == "get");
    assert!(
        get_item_a.is_some(),
        "get method should be in completion items for CallerA"
    );
    let detail_a = get_item_a.unwrap().detail.as_deref().unwrap_or("");
    assert!(
        detail_a.contains("String"),
        "CallerA (Base<String>) should substitute T→String in detail, got: {detail_a}"
    );
    assert!(
        !detail_a.contains(": T"),
        "CallerA detail should not contain unresolved T, got: {detail_a}"
    );

    // For CallerB (around line 6-7), Base members should show Int substitution
    let items_b = complete_dot(&idx, "Base", &caller_uri, false, Some(6));
    let get_item_b = items_b.iter().find(|i| i.label == "get");
    assert!(
        get_item_b.is_some(),
        "get method should be in completion items for CallerB"
    );
    let detail_b = get_item_b.unwrap().detail.as_deref().unwrap_or("");
    assert!(
        detail_b.contains("Int"),
        "CallerB (Base<Int>) should substitute T→Int in detail, got: {detail_b}"
    );
    assert!(
        !detail_b.contains(": T"),
        "CallerB detail should not contain unresolved T, got: {detail_b}"
    );

    // Cursor line threading must produce different substitutions for each class.
    assert_ne!(
        detail_a, detail_b,
        "CallerA and CallerB completions should differ (String vs Int substitution)"
    );

    // Both should have the method, but with potentially different type substitutions
    // (if the caller_cursor_line is correctly applied to pick the right class definition).
    assert_eq!(
        items_a.len(),
        items_b.len(),
        "both completions should return same number of items"
    );
}

/// Regression: `symbols_from_nested_type`'s "declared inside `type_name`'s range"
/// membership check has no depth limit — a member declared inside a *nested*
/// type's own body (e.g. `Success.userData`, several lines inside
/// `MainActivityUiState`'s outer braces) still counts as textually "inside"
/// `MainActivityUiState`'s range, so it leaked into the sealed interface's own
/// member list. That, in turn, leaked into every OTHER sealed subtype's
/// *inherited*-member completion via `walk_hierarchy` — so a `Loading`-typed
/// (smart-cast-narrowed) receiver offered `Success`'s own `userData` and
/// `extra()`, both of which are compile errors if selected.
#[test]
fn sealed_subtype_member_completion_does_not_leak_sibling_subtype_members() {
    let idx = Indexer::new();
    let file_uri = uri("/MainActivityUiState.kt");
    idx.index_content(
        &file_uri,
        "package app\n\
         sealed interface MainActivityUiState {\n\
         \x20 data object Loading : MainActivityUiState\n\
         \x20 data class Success(val userData: String) : MainActivityUiState {\n\
         \x20   fun extra() = userData\n\
         \x20 }\n\
         \x20 val shared: Boolean get() = true\n\
         }",
    );
    let items = complete_dot(&idx, "Loading", &file_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"userData"),
        "Loading must not inherit sibling subtype Success's own member: {labels:?}"
    );
    assert!(
        !labels.contains(&"extra"),
        "Loading must not inherit sibling subtype Success's own method: {labels:?}"
    );
    assert!(
        !labels.contains(&"copy"),
        "Loading must not inherit sibling subtype Success's synthesized copy(): {labels:?}"
    );
    assert!(
        labels.contains(&"shared"),
        "Loading must still inherit the sealed interface's own direct member: {labels:?}"
    );
}

/// Companion regression to the one above: a nested type is a legitimate direct
/// member of its enclosing type when the receiver IS the enclosing type's own
/// name (`MainActivityUiState.Success` / `.Loading` are valid Kotlin — nested
/// type references, not instance member access). The depth-restriction fix
/// must not make a type exclude *itself* just because its own range trivially
/// satisfies "is inside a nested type's range" against its own entry.
#[test]
fn nested_type_completion_still_lists_sibling_nested_types_by_enclosing_type_name() {
    let idx = Indexer::new();
    let file_uri = uri("/MainActivityUiState.kt");
    idx.index_content(
        &file_uri,
        "package app\n\
         sealed interface MainActivityUiState {\n\
         \x20 data object Loading : MainActivityUiState\n\
         \x20 data class Success(val userData: String) : MainActivityUiState\n\
         }",
    );
    let items = complete_dot(&idx, "MainActivityUiState", &file_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Loading"),
        "MainActivityUiState.Loading is a valid nested-type reference: {labels:?}"
    );
    assert!(
        labels.contains(&"Success"),
        "MainActivityUiState.Success is a valid nested-type reference: {labels:?}"
    );
}

/// Pre-existing bug in the same problem family as the two regressions above,
/// found (not yet fixed) during the review that led to the container-based
/// rewrite, and fixed here as a natural consequence of the
/// `MembershipContext` split: `symbols_from_nested_type` used to answer
/// "what does `inner_name` declare" identically whether the caller meant
/// "the receiver IS `inner_name`" or "`inner_name` is an ancestor being
/// folded into a DESCENDANT's inherited members" (`collect_inherited_dot_completion_items`'s
/// `walk_hierarchy` callback). A nested type declaration IS a legitimate
/// direct member of its own enclosing type (`Top.Leaf1` — see the test
/// above) but is NEVER an inherited instance member of a descendant
/// (`mid.Leaf1` isn't valid Kotlin) — so when `Mid` (itself nested inside
/// `Top`, a common sealed-hierarchy shape) inherits `Top`'s members via
/// `walk_hierarchy`, `Top`'s own nested-type declarations — its sibling
/// `Leaf1`, and even `Mid` itself — leaked into `Mid`'s own inherited
/// completion list as if they were instance members.
#[test]
fn sibling_nested_type_does_not_leak_into_inherited_completion() {
    let idx = Indexer::new();
    let file_uri = uri("/Top.kt");
    idx.index_content(
        &file_uri,
        "package app\n\
         sealed class Top {\n\
         \x20 class Mid : Top() {\n\
         \x20   fun midMethod() {}\n\
         \x20 }\n\
         \x20 class Leaf1 : Top()\n\
         \x20 fun topMethod() {}\n\
         }",
    );
    let items = complete_dot(&idx, "Mid", &file_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"midMethod"),
        "Mid's own direct member must still appear: {labels:?}"
    );
    assert!(
        labels.contains(&"topMethod"),
        "Mid must still inherit Top's own instance member: {labels:?}"
    );
    assert!(
        !labels.contains(&"Leaf1"),
        "a sibling nested type must not leak into Mid's inherited completion: {labels:?}"
    );
    assert!(
        !labels.contains(&"Mid"),
        "Mid must not leak into its own inherited completion via its enclosing type: {labels:?}"
    );
}

/// Regression: Kotlin resolves `Outer.member` through `Outer`'s own companion
/// object when `Outer` has no such member itself (implicit companion
/// forwarding) — a very common idiom, doubly so paired with sealed
/// hierarchies (factory functions / constants on the companion). An earlier,
/// range-containment-based version of the sibling-subtype-leak fix above
/// treated a companion object as just another nested container type and
/// blanket-excluded its members from the enclosing class's own completion,
/// breaking this. The `container`-based implementation must special-case
/// companion forwarding explicitly, since `copy.container` is the companion's
/// own name ("Companion"/a named companion), not the enclosing class's.
#[test]
fn companion_object_members_still_appear_on_enclosing_type_completion() {
    let idx = Indexer::new();
    let file_uri = uri("/Widget.kt");
    idx.index_content(
        &file_uri,
        "package app\n\
         class Widget {\n\
         \x20 companion object {\n\
         \x20   fun create(): Widget = Widget()\n\
         \x20   val DEFAULT_ID: Int = 0\n\
         \x20 }\n\
         \x20 fun instanceMethod() {}\n\
         }",
    );
    let items = complete_dot(&idx, "Widget", &file_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"create"),
        "Widget.create() forwards through the companion object: {labels:?}"
    );
    assert!(
        labels.contains(&"DEFAULT_ID"),
        "Widget.DEFAULT_ID forwards through the companion object: {labels:?}"
    );
    assert!(
        labels.contains(&"instanceMethod"),
        "Widget's own direct member must still appear: {labels:?}"
    );
}

/// Regression found by the container-based rewrite's own review: the
/// companion-object detection in `symbols_from_nested_type` matched by
/// PREFIX (`detail.starts_with("companion object")`), so a modified
/// companion — `private companion object`, or any other leading modifier —
/// never matched, and its members silently stopped forwarding to the
/// enclosing type's own completion. `resolve::resolve_companion_member`
/// (`resolve.rs`) had already fixed the identical gap for its own,
/// independent companion lookup via a token-based match (see
/// `resolve_qualified_class_name_prefers_private_companion_member`);
/// `is_companion_object_symbol` applies the same fix here.
#[test]
fn companion_object_forwarding_survives_a_private_companion() {
    let idx = Indexer::new();
    let file_uri = uri("/Widget2.kt");
    idx.index_content(
        &file_uri,
        "package app\n\
         class Widget2 {\n\
         \x20 private companion object {\n\
         \x20   fun create(): Widget2 = Widget2()\n\
         \x20 }\n\
         }",
    );
    let items = complete_dot(&idx, "Widget2", &file_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"create"),
        "a private companion's create() must still forward: {labels:?}"
    );
}

/// Regression: `detail` is raw declaration text, so a comment sitting between
/// the modifiers and the keywords lands in it. Testing for a lone `companion`
/// token therefore classified `private /* companion */ object Registry` as a
/// companion, forwarding an ordinary nested object's members onto the
/// enclosing type. The two keywords must be adjacent.
#[test]
fn a_comment_mentioning_companion_does_not_make_an_object_a_companion() {
    let idx = Indexer::new();
    let file_uri = uri("/Registry.kt");
    idx.index_content(
        &file_uri,
        "package app\n\
         class Host {\n\
         \x20 private /* companion */ object Registry {\n\
         \x20   fun registryOnly(): Int = 1\n\
         \x20 }\n\
         }",
    );
    let items = complete_dot(&idx, "Host", &file_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"registryOnly"),
        "a plain nested object's members must not forward onto the enclosing type: {labels:?}"
    );
}

/// Regression: Kotlin gives every anonymous `companion object { }` the same
/// implicit name ("Companion"), so two unrelated classes in the same file
/// each have a companion literally named "Companion" — their members share
/// that same `container` string. Folding a companion's members in by
/// container-NAME match alone (without also checking which specific
/// companion instance a member's range belongs to) would leak one class's
/// companion members into a completely different class's completion.
#[test]
fn companion_object_forwarding_does_not_leak_across_classes_sharing_the_implicit_name() {
    let idx = Indexer::new();
    let file_uri = uri("/Widgets.kt");
    idx.index_content(
        &file_uri,
        "package app\n\
         class Alpha {\n\
         \x20 companion object {\n\
         \x20   fun alphaOnly(): Alpha = Alpha()\n\
         \x20 }\n\
         }\n\
         class Beta {\n\
         \x20 companion object {\n\
         \x20   fun betaOnly(): Beta = Beta()\n\
         \x20 }\n\
         }",
    );
    let items = complete_dot(&idx, "Alpha", &file_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"alphaOnly"),
        "Alpha's own companion member must still forward: {labels:?}"
    );
    assert!(
        !labels.contains(&"betaOnly"),
        "Beta's companion (same implicit 'Companion' name) must not leak into Alpha's completion: {labels:?}"
    );
}

#[test]
fn is_screaming_snake_cases() {
    assert!(is_screaming_snake("MAX_SIZE"));
    assert!(is_screaming_snake("CHILD_DASHBOARD_MAX"));
    assert!(is_screaming_snake("A"));
    assert!(!is_screaming_snake("ChildDashboard"));
    assert!(!is_screaming_snake("maxSize"));
    assert!(!is_screaming_snake("_")); // no letters
    assert!(!is_screaming_snake("123")); // no letters
}

#[test]
fn is_annotation_context_detection() {
    assert!(is_annotation_context("@Composable", "Composable"));
    assert!(is_annotation_context("  @Comp", "Comp"));
    assert!(!is_annotation_context("Composable", "Composable")); // no @
                                                                 // "@" alone — cursor right after the trigger character, empty prefix
    assert!(is_annotation_context("@", ""));
    assert!(is_annotation_context("  @", ""));
}

// ── ReceiverType::from_raw ────────────────────────────────────────────────

#[test]
fn receiver_type_simple() {
    let rt = infer::ReceiverType::from_raw("MyClass".to_string());
    assert_eq!(rt.raw, "MyClass");
    assert_eq!(rt.qualified, "MyClass");
    assert_eq!(rt.outer, "MyClass");
    assert_eq!(rt.leaf, "MyClass");
    assert!(!rt.nullable);
}

#[test]
fn receiver_type_with_generics() {
    let rt = infer::ReceiverType::from_raw("Flow<UiState>".to_string());
    assert_eq!(rt.raw, "Flow<UiState>");
    assert_eq!(rt.qualified, "Flow");
    assert_eq!(rt.outer, "Flow");
    assert_eq!(rt.leaf, "Flow");
    assert!(!rt.nullable);
}

#[test]
fn receiver_type_nullable_simple() {
    let rt = infer::ReceiverType::from_raw("User?".to_string());
    assert_eq!(rt.raw, "User?");
    assert_eq!(rt.qualified, "User");
    assert_eq!(rt.outer, "User");
    assert_eq!(rt.leaf, "User");
    assert!(rt.nullable);
}

#[test]
fn receiver_type_nullable_generic() {
    let rt = infer::ReceiverType::from_raw("StateFlow<UiState>?".to_string());
    assert_eq!(rt.raw, "StateFlow<UiState>?");
    assert_eq!(rt.qualified, "StateFlow");
    assert_eq!(rt.outer, "StateFlow");
    assert_eq!(rt.leaf, "StateFlow");
    assert!(rt.nullable);
}

#[test]
fn receiver_type_nullable_only_in_generic_arg_is_not_nullable() {
    // A `?` inside a generic argument must not make the outer type nullable —
    // `Box<String?>` is a non-null `Box`. Regression for the nullable-dot-call
    // diagnostic, which keys on `ReceiverType::nullable`.
    let rt = infer::ReceiverType::from_raw("Box<String?>".to_string());
    assert_eq!(rt.qualified, "Box");
    assert!(
        !rt.nullable,
        "inner `?` must not make the outer type nullable"
    );

    // A trailing `?` after the generics still is nullable.
    let rt = infer::ReceiverType::from_raw("List<String?>?".to_string());
    assert_eq!(rt.qualified, "List");
    assert!(rt.nullable);
}

#[test]
fn receiver_type_dotted_nested() {
    let rt = infer::ReceiverType::from_raw("Outer.Inner".to_string());
    assert_eq!(rt.raw, "Outer.Inner");
    assert_eq!(rt.qualified, "Outer.Inner");
    assert_eq!(rt.outer, "Outer");
    assert_eq!(rt.leaf, "Inner");
    assert!(!rt.nullable);
}

#[test]
fn receiver_type_dotted_with_generics() {
    let rt = infer::ReceiverType::from_raw("Outer.Inner<Param>".to_string());
    assert_eq!(rt.raw, "Outer.Inner<Param>");
    assert_eq!(rt.qualified, "Outer.Inner");
    assert_eq!(rt.outer, "Outer");
    assert_eq!(rt.leaf, "Inner");
    assert!(!rt.nullable);
}

#[test]
fn receiver_type_generic_with_params() {
    let rt = infer::ReceiverType::from_raw("OneYearOlderInteractor<Params>".to_string());
    assert_eq!(rt.qualified, "OneYearOlderInteractor");
    assert_eq!(rt.outer, "OneYearOlderInteractor");
    assert_eq!(rt.leaf, "OneYearOlderInteractor");
    assert!(!rt.nullable);
}

#[test]
fn supers_swift_multiple_conformances() {
    let src = "class Foo: UIViewController, Sendable {}";
    let s: Vec<String> = crate::parser::parse_swift(src)
        .supers
        .into_iter()
        .map(|(_, n, _)| n)
        .collect();
    assert!(
        s.contains(&"UIViewController".to_string()),
        "missing UIViewController, got {s:?}"
    );
    assert!(
        s.contains(&"Sendable".to_string()),
        "missing Sendable, got {s:?}"
    );
}

// ─── smart cast narrowing tests ───────────────────────────────────────────────

#[test]
fn smart_cast_when_branch() {
    let lines: Vec<String> = vec![
        "fun handle(event: Event) {",
        "    when (event) {",
        "        is Event.OnClick -> {",
        "            event.doSomething()",
        "        }",
        "    }",
        "}",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    // Line 3 is inside `is Event.OnClick` branch
    let result = infer_lines::smart_cast_type_at_line(&lines, "event", 3);
    assert_eq!(
        result,
        Some(infer_lines::SmartCast::TypeTest("Event.OnClick".to_owned()))
    );
}

#[test]
fn smart_cast_when_branch_same_line() {
    let lines: Vec<String> = vec![
        "fun handle(event: Event) {",
        "    when (event) {",
        "        is Event.OnClick -> event.doSomething()",
        "    }",
        "}",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    // Cursor on the branch line itself
    let result = infer_lines::smart_cast_type_at_line(&lines, "event", 2);
    assert_eq!(
        result,
        Some(infer_lines::SmartCast::TypeTest("Event.OnClick".to_owned()))
    );
}

#[test]
fn smart_cast_if_is() {
    let lines: Vec<String> = vec![
        "fun handle(event: Event) {",
        "    if (event is Event.OnInput) {",
        "        event.text",
        "    }",
        "}",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    let result = infer_lines::smart_cast_type_at_line(&lines, "event", 2);
    assert_eq!(
        result,
        Some(infer_lines::SmartCast::TypeTest("Event.OnInput".to_owned()))
    );
}

#[test]
fn smart_cast_no_match_wrong_var() {
    let lines: Vec<String> = vec![
        "fun handle(event: Event) {",
        "    when (event) {",
        "        is Event.OnClick -> {",
        "            other.doSomething()",
        "        }",
        "    }",
        "}",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    // "other" is not the when subject
    let result = infer_lines::smart_cast_type_at_line(&lines, "other", 3);
    assert_eq!(result, None);
}

#[test]
fn smart_cast_when_no_subject_outside_branch() {
    let lines: Vec<String> = vec![
        "fun handle(event: Event) {",
        "    when (event) {",
        "        is Event.OnClick -> {}",
        "        is Event.OnInput -> {}",
        "    }",
        "    event.normalCall()",
        "}",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    // Line 5 is outside the when block
    let result = infer_lines::smart_cast_type_at_line(&lines, "event", 5);
    assert_eq!(result, None);
}

#[test]
fn smart_cast_if_does_not_leak_from_closed_nested_block() {
    let lines: Vec<String> = vec![
        "fun handle(event: Event) {",
        "    if (event is Event.OnInput) {",
        "        if (event is Event.OnClick) {",
        "            event.doSomething()",
        "        }",
        "        event.text",
        "    }",
        "}",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    let result = infer_lines::smart_cast_type_at_line(&lines, "event", 5);
    assert_eq!(
        result,
        Some(infer_lines::SmartCast::TypeTest("Event.OnInput".to_owned()))
    );
}

#[test]
fn smart_cast_if_requires_whole_word_variable_match() {
    let lines: Vec<String> = vec![
        "fun handle(event: Event, someevent: Event) {",
        "    if (someevent is Event.OnInput) {",
        "        event.toString()",
        "    }",
        "}",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    let result = infer_lines::smart_cast_type_at_line(&lines, "event", 2);
    assert_eq!(result, None);
}

#[test]
fn smart_cast_if_preserves_generic_types_with_commas() {
    let lines: Vec<String> = vec![
        "fun handle(value: Any) {",
        "    if (value is Map<String, List<Int>>) {",
        "        value.entries",
        "    }",
        "}",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    let result = infer_lines::smart_cast_type_at_line(&lines, "value", 2);
    assert_eq!(
        result,
        Some(infer_lines::SmartCast::TypeTest(
            "Map<String, List<Int>>".to_owned()
        ))
    );
}
#[test]
fn smart_cast_nested_when_on_same_line() {
    let lines: Vec<String> = vec![
        "fun handle(event: DashboardEvent) {",
        "    when (event) {",
        "        is Banner -> when (event.events) {",
        "            is SalespointInputEvent.OnCloseClick -> {",
        "                event.events.doSomething()",
        "            }",
        "        }",
        "    }",
        "}",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    // event.events on line 4 should be narrowed to SalespointInputEvent.OnCloseClick
    let result = infer_lines::smart_cast_type_at_line(&lines, "event.events", 4);
    assert_eq!(
        result,
        Some(infer_lines::SmartCast::TypeTest(
            "SalespointInputEvent.OnCloseClick".to_owned()
        )),
    );

    // event on line 4 should be narrowed to Banner (from outer when)
    let result2 = infer_lines::smart_cast_type_at_line(&lines, "event", 4);
    assert_eq!(
        result2,
        Some(infer_lines::SmartCast::TypeTest("Banner".to_owned()))
    );
}

// ── Completion ordering ────────────────────────────────────────────────────
//
// These tests verify the sort_text tier scheme (ascending = highest priority):
//   "0{score}{name}"  → tier 0: local file symbols
//   "1{score}{name}"  → tier 1: same-package symbols
//   "2{score}:{name}" → tier 2: cross-package symbols
//   "3{score}:{name}" → tier 3: stdlib / bare completions
//   "y:{name}"        → live templates (snippets=true only)
//   "z:{name}"        → scope functions / top-level stdlib fns
//
// Keywords ("true"/"false"/"null"/"this"/"super") are added by PR #126.
// See: https://github.com/Hessesian/kmp-lsp/pull/126

fn sort_text_of<'a>(items: &'a [tower_lsp::lsp_types::CompletionItem], label: &str) -> &'a str {
    items
        .iter()
        .find(|i| i.label == label)
        .and_then(|i| i.sort_text.as_deref())
        .unwrap_or_else(|| panic!("label {label:?} not found in completion items"))
}

/// Returns sorted labels from a completion list, for deterministic assertions.
fn sorted_labels(items: &[tower_lsp::lsp_types::CompletionItem]) -> Vec<&str> {
    let mut labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    labels.sort_unstable();
    labels
}

#[track_caller]
fn assert_labels_contain(items: &[tower_lsp::lsp_types::CompletionItem], expected: &[&str]) {
    let missing: Vec<_> = expected
        .iter()
        .filter(|&&e| !items.iter().any(|i| i.label == e))
        .collect();
    assert!(
        missing.is_empty(),
        "completion items missing: {missing:?}\ngot: {:?}",
        sorted_labels(items)
    );
}

#[track_caller]
fn assert_labels_exclude(items: &[tower_lsp::lsp_types::CompletionItem], forbidden: &[&str]) {
    let leaked: Vec<_> = forbidden
        .iter()
        .filter(|&&f| items.iter().any(|i| i.label == f))
        .collect();
    assert!(
        leaked.is_empty(),
        "these labels must NOT appear: {leaked:?}\ngot: {:?}",
        sorted_labels(items)
    );
}

#[test]
fn sort_text_tier_ordering_local_beats_stdlib() {
    // A locally-defined function must sort before any stdlib bare completion.
    let idx = Indexer::new();
    let file_uri = uri("/pkg/a/Main.kt");
    idx.index_content(&file_uri, "package a\nfun myLocalFun() {}");

    let (items, _) = complete_bare(&idx, "", &file_uri, false, false, None);

    let local_sort = sort_text_of(&items, "myLocalFun");
    // Stdlib items get "3{score}:{name}" — anything in tier 0/1/2 starts with "0"/"1"/"2"
    assert!(
        local_sort.starts_with('0') || local_sort.starts_with('1'),
        "local fun sort_text should be tier 0 or 1, got: {local_sort:?}"
    );
    assert!(
        local_sort < "3",
        "local fun sort_text ({local_sort:?}) must be less than stdlib tier '3'"
    );
}

#[test]
fn sort_text_tier_ordering_pkg_beats_cross_pkg() {
    // Within complete_bare, same-package symbols are tier 1 ("1{score}{name}")
    // while the caller's own file symbols are tier 0 ("0{score}{name}").
    // Verify that a same-pkg (but not local-file) symbol sorts after a local one.
    let idx = Indexer::new();
    let caller_uri = uri("/pkg/a/Caller.kt");
    let peer_uri = uri("/pkg/a/Peer.kt");
    idx.index_content(&caller_uri, "package a\nfun localFoo() {}");
    idx.index_content(&peer_uri, "package a\nfun pkgBar() {}");

    let (items, _) = complete_bare(&idx, "", &caller_uri, false, false, None);

    let local_sort = sort_text_of(&items, "localFoo");
    let pkg_sort = sort_text_of(&items, "pkgBar");
    assert!(
        local_sort < pkg_sort,
        "local tier ({local_sort:?}) must sort before same-pkg tier ({pkg_sort:?})"
    );
    assert!(
        local_sort.starts_with('0'),
        "local symbol should be tier 0, got: {local_sort:?}"
    );
    assert!(
        pkg_sort.starts_with('1'),
        "same-pkg symbol should be tier 1, got: {pkg_sort:?}"
    );
}

// See: https://github.com/Hessesian/kmp-lsp/pull/126
//
// This test is EXPECTED TO FAIL on main until PR #126 is merged.
// It documents that "true", "false", and "null" are missing from bare completions.
// Once merged the `#[ignore]` tag should be removed.
#[test]
fn regression_126_bare_completions_include_kotlin_literals() {
    let idx = Indexer::new();
    let file_uri = uri("/pkg/Main.kt");
    idx.index_content(&file_uri, "package pkg\nfun foo() {}");

    // Empty prefix — all completions returned; literals must be present.
    let (items, _) = complete_bare(&idx, "", &file_uri, false, false, None);
    assert_labels_contain(&items, &["true", "false", "null"]);

    // Prefix "t" — matches "true" by starts_with.
    let (t_items, _) = complete_bare(&idx, "t", &file_uri, false, false, None);
    assert_labels_contain(&t_items, &["true"]);
    assert_labels_exclude(&t_items, &["false", "null"]);

    // Prefix "f" — matches "false".
    let (f_items, _) = complete_bare(&idx, "f", &file_uri, false, false, None);
    assert_labels_contain(&f_items, &["false"]);

    // Prefix "nu" — matches "null".
    let (n_items, _) = complete_bare(&idx, "nu", &file_uri, false, false, None);
    assert_labels_contain(&n_items, &["null"]);
}

// See: https://github.com/Hessesian/kmp-lsp/pull/126
//
// This test is EXPECTED TO FAIL on main until PR #126 is merged.
// It also verifies the sort_text tier for keywords: because `collect_stdlib` reassigns
// sort_text for every item in `bare_completions()`, keywords receive "3{score}:{name}"
// (same tier as `println`/`listOf`), NOT the "a:{name}" prefix set in `build_bare_completions`.
// Once the PR is merged, remove `#[ignore]` and confirm the sort_text prefix is "3".
#[test]
fn regression_126_keyword_sort_text_is_stdlib_tier() {
    let idx = Indexer::new();
    let file_uri = uri("/pkg/Main.kt");
    idx.index_content(&file_uri, "package pkg\nfun foo() {}");

    let (items, _) = complete_bare(&idx, "true", &file_uri, false, false, None);
    let true_sort = sort_text_of(&items, "true");

    // Keywords flow through collect_stdlib which overwrites sort_text with "3{score}:{name}".
    // "3" tier means they sort AFTER local/pkg/cross-pkg symbols but in the same band as
    // other stdlib items (listOf, println, etc.).
    assert!(
        true_sort.starts_with('3'),
        "keyword sort_text should be tier 3 (stdlib band), got: {true_sort:?}"
    );
}

#[test]
fn sort_text_named_arg_prefix_is_001() {
    // Named-arg completions use "001:{name}" sort prefix — verify the constant is correct
    // so that named args always sort before all real symbol tiers (0/1/2/3).
    assert!(
        "001:foo" < "0foo",
        "named-arg prefix must beat tier-0 sort_text"
    );
    assert!(
        "001:foo" < "3foo",
        "named-arg prefix must beat tier-3 sort_text"
    );
    assert!(
        "001:foo" < "a:foo",
        "named-arg prefix must beat 'a:' keyword prefix"
    );
    assert!(
        "a:foo" < "y:foo",
        "keyword 'a:' prefix must beat live-template 'y:' prefix"
    );
    assert!(
        "a:foo" < "z:foo",
        "keyword 'a:' prefix must beat scope-fun 'z:' prefix"
    );
    assert!(
        "y:foo" < "z:foo",
        "live-template 'y:' prefix must beat scope-fun 'z:'"
    );
}

#[test]
fn bare_completion_includes_this_extensions_inside_subclass() {
    // See: extension properties like `val ViewModel.viewModelScope` should appear
    // as bare-word completions inside a class that inherits from ViewModel.
    let idx = Indexer::new();

    // Library file defining the extension property.
    let lib_uri = Url::parse("file:///sdk/ViewModel.kt").unwrap();
    idx.index_content(
        &lib_uri,
        "package androidx.lifecycle\nopen class ViewModel\nval ViewModel.viewModelScope: Int get() = 0",
    );

    // App ViewModel that inherits from ViewModel.
    let vm_uri = Url::parse("file:///app/DashboardViewModel.kt").unwrap();
    idx.index_content(
        &vm_uri,
        concat!(
            "package app\n",
            "import androidx.lifecycle.ViewModel\n",
            "class DashboardViewModel : ViewModel() {\n",
            "    fun load() {\n",
            "        val s = viewModelScope\n", // cursor is on line 4 (0-based)
            "    }\n",
            "}\n",
        ),
    );

    // Request completion on line 4 (inside the function body of DashboardViewModel).
    let (items, _) = complete_bare(&idx, "viewModel", &vm_uri, false, false, Some(4));
    assert_labels_contain(&items, &["viewModelScope"]);
}

#[test]
fn bare_completion_extension_property_not_function_snippet() {
    // Extension *properties* must not get a `name($1)` snippet — they are values, not callables.
    let idx = Indexer::new();
    let lib_uri = Url::parse("file:///sdk/ViewModel.kt").unwrap();
    idx.index_content(
        &lib_uri,
        "package androidx.lifecycle\nopen class ViewModel\nval ViewModel.viewModelScope: Int get() = 0",
    );
    let vm_uri = Url::parse("file:///app/DashboardViewModel.kt").unwrap();
    idx.index_content(
        &vm_uri,
        concat!(
            "package app\n",
            "import androidx.lifecycle.ViewModel\n",
            "class DashboardViewModel : ViewModel() {\n",
            "    fun load() {\n",
            "        val s = viewModelScope\n",
            "    }\n",
            "}\n",
        ),
    );
    let (items, _) = complete_bare(&idx, "viewModel", &vm_uri, true, false, Some(4));
    let item = items
        .iter()
        .find(|i| i.label == "viewModelScope")
        .expect("viewModelScope must appear");
    assert!(
        item.insert_text.is_none(),
        "extension property must not have a snippet insert_text, got: {:?}",
        item.insert_text
    );
}

#[test]
fn infer_extension_property_type_for_dot_completion() {
    // viewModelScope.launch: after `viewModelScope.`, the type must be inferred as
    // CoroutineScope so that extension functions on CoroutineScope (e.g. `launch`) appear.
    let idx = Indexer::new();
    let lib_uri = Url::parse("file:///sdk/ViewModel.kt").unwrap();
    idx.index_content(
        &lib_uri,
        "package androidx.lifecycle\nopen class ViewModel\nval ViewModel.viewModelScope: CoroutineScope get() = TODO()",
    );
    let vm_uri = Url::parse("file:///app/DashboardViewModel.kt").unwrap();
    idx.index_content(
        &vm_uri,
        concat!(
            "package app\n",
            "import androidx.lifecycle.ViewModel\n",
            "class DashboardViewModel : ViewModel() {\n",
            "    fun load() {}\n",
            "}\n",
        ),
    );

    let result = infer_variable_type(&idx, "viewModelScope", &vm_uri);
    assert_eq!(
        result,
        Some("CoroutineScope".into()),
        "viewModelScope type must be inferred as CoroutineScope via extension property lookup"
    );
}

#[test]
fn complete_dot_viewmodelscope_shows_launch() {
    // End-to-end: `viewModelScope.` inside a ViewModel subclass must return `launch`.
    // This tests the full chain:
    //   1. `viewModelScope` type resolved via find_extension_property_type
    //      (via extension_by_receiver["ViewModel"])
    //   2. `launch` found via extension_fn_completions
    //      (via extension_by_receiver["CoroutineScope"])
    // Both extension_by_receiver entries are populated via source indexing here,
    // which mirrors what JAR indexing does after the sidecar fix.
    let idx = Indexer::new();

    // Simulate lifecycle-viewmodel-ktx: viewModelScope property
    let lib_uri = Url::parse("file:///sdk/lifecycle.kt").unwrap();
    idx.index_content(
        &lib_uri,
        "package androidx.lifecycle\nopen class ViewModel\nval ViewModel.viewModelScope: CoroutineScope get() = TODO()",
    );

    // Simulate kotlinx.coroutines: launch extension on CoroutineScope
    let coroutines_uri = Url::parse("file:///sdk/coroutines.kt").unwrap();
    idx.index_content(
        &coroutines_uri,
        "package kotlinx.coroutines\ninterface CoroutineScope\nfun CoroutineScope.launch(block: suspend () -> Unit): Job = TODO()",
    );

    let vm_uri = Url::parse("file:///app/DashboardViewModel.kt").unwrap();
    idx.index_content(
        &vm_uri,
        concat!(
            "package app\n",
            "import androidx.lifecycle.ViewModel\n",
            "class DashboardProductsViewModel : ViewModel() {\n",
            "    fun load() { viewModelScope.launch {} }\n",
            "}\n",
        ),
    );

    let items = complete_dot(&idx, "viewModelScope", &vm_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"launch"),
        "expected `launch` in viewModelScope. completions, got: {labels:?}"
    );
}

// ── JAR-path extension completion test ───────────────────────────────────────

#[test]
fn jar_extension_appears_in_dot_completion() {
    // Verify that extension functions inserted via the JAR path (build_jar_file_data)
    // appear in dot-completion.  This mirrors the real flow where the sidecar indexes
    // kotlinx-coroutines and the `launch` function is stored with a `jar:file://` URI.
    let idx = Indexer::new();

    // Source-index ViewModel so walk_hierarchy can find it.
    idx.index_content(
        &Url::parse("file:///sdk/ViewModel.kt").unwrap(),
        "package androidx.lifecycle\nopen class ViewModel",
    );

    // Simulate JAR-indexed extensions (what build_jar_file_data does):
    // 1. val ViewModel.viewModelScope: CoroutineScope
    idx.extension_by_receiver
        .entry("ViewModel".to_owned())
        .or_default()
        .push(crate::types::ExtensionEntry {
            file_uri: "jar:file:///lifecycle-ktx.jar/ViewModel.class".to_owned(),
            name: "viewModelScope".to_owned(),
            kind: tower_lsp::lsp_types::SymbolKind::PROPERTY,
            detail: "val ViewModel.viewModelScope: CoroutineScope".to_owned(),
            visibility: crate::types::Visibility::Public,
            package: Some("androidx.lifecycle".to_owned()),
            trailing_lambda: false,
            deprecated: false,
            container: None,
        });

    // 2. fun CoroutineScope.launch(block: suspend CoroutineScope.() -> Unit): Job
    idx.extension_by_receiver
        .entry("CoroutineScope".to_owned())
        .or_default()
        .push(crate::types::ExtensionEntry {
            file_uri: "jar:file:///coroutines-core.jar/Builders.class".to_owned(),
            name: "launch".to_owned(),
            kind: tower_lsp::lsp_types::SymbolKind::FUNCTION,
            detail: "fun CoroutineScope.launch(block: suspend CoroutineScope.() -> Unit): Job"
                .to_owned(),
            visibility: crate::types::Visibility::Public,
            package: Some("kotlinx.coroutines".to_owned()),
            trailing_lambda: true,
            deprecated: false,
            container: None,
        });

    let vm_uri = Url::parse("file:///app/MyViewModel.kt").unwrap();
    idx.index_content(
        &vm_uri,
        concat!(
            "package app\n",
            "import androidx.lifecycle.ViewModel\n",
            "class MyViewModel : ViewModel() {\n",
            "    fun load() { viewModelScope.launch {} }\n",
            "}\n",
        ),
    );

    let items = complete_dot(&idx, "viewModelScope", &vm_uri, true, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(
        labels.contains(&"launch"),
        "expected regular `launch` item from JAR path, got: {labels:?}"
    );
    assert!(
        labels.contains(&"launch { }"),
        "expected trailing-lambda `launch {{ }}` item from JAR path, got: {labels:?}"
    );
}

#[test]
fn member_extension_is_excluded_from_generic_dot_completion() {
    // Regression guard: `container: Some(_)` (a Compose-style member extension
    // such as `interface ColumnScope { fun Modifier.weight(...) }`) must not
    // enter the generic receiver-type dot-completion path. Kotlin has no
    // import syntax for an interface member, so offering it here would pair
    // the completion with an auto-import edit `weight`'s real declaration can
    // never satisfy.
    let idx = Indexer::new();
    idx.extension_by_receiver
        .entry("Modifier".to_owned())
        .or_default()
        .push(crate::types::ExtensionEntry {
            file_uri: "file:///sdk/ColumnScope.kt".to_owned(),
            name: "weight".to_owned(),
            kind: tower_lsp::lsp_types::SymbolKind::METHOD,
            detail: "fun Modifier.weight(weight: Float): Modifier".to_owned(),
            visibility: crate::types::Visibility::Public,
            package: Some("androidx.compose.foundation.layout".to_owned()),
            trailing_lambda: false,
            deprecated: false,
            container: Some("ColumnScope".to_owned()),
        });

    let caller_uri = Url::parse("file:///app/Screen.kt").unwrap();
    idx.index_content(
        &caller_uri,
        concat!(
            "package app\n",
            "class Modifier\n",
            "fun caller(m: Modifier) {\n",
            "    m.weight\n",
            "}\n",
        ),
    );

    let items = complete_dot(&idx, "Modifier", &caller_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"weight"),
        "a member extension must not be offered through generic dot completion: {labels:?}"
    );
}

/// Regression test for a real lazy-JAR-loading gap: `extension_by_receiver`
/// is populated exclusively by Tier-2 materialization (`build_jar_file_data`)
/// — Tier 1's `populate_tier1_from_manifest` never wrote to it, and neither
/// `extension_fn_completions` nor `complete_bare`'s ancestor-extension loop
/// had any Tier-1 check or promotion call before reading it. Post-flip, a
/// not-yet-materialized JAR's extension methods (e.g. `viewModelScope`, in a
/// real project living in a separate `-ktx` artifact from `ViewModel`
/// itself) were silently invisible to completion. Fixed by adding a
/// receiver-type-keyed Tier-1 index (`jar_extension_receivers`) and wiring
/// both completion call sites to call
/// `ensure_jar_materialized_for_extension_receiver` before reading
/// `extension_by_receiver`.
///
/// This test uses a fake, nonexistent jar path with no real sidecar, so it
/// can only prove the promotion ATTEMPT genuinely fires for the receiver
/// type walked (observable via `materialization_failed`) — the same
/// limitation every other Tier-1-promotion test in this plan hits under
/// identical constraints (see `Task 8`/`Task 9`/`Task 10`'s decoy tests). A
/// real successful promotion (and thus `viewModelScope` actually appearing)
/// needs a real Kotlin-compiled fixture JAR + a live sidecar — integration-
/// test territory, out of scope for this unit test.
#[test]
fn extension_completion_attempts_promotion_for_a_tier1_only_receiver() {
    let idx = Indexer::new();
    idx.index_content(
        &Url::parse("file:///sdk/ViewModel.kt").unwrap(),
        "package androidx.lifecycle\nopen class ViewModel",
    );

    // Simulate Tier 1 (manifest-only) knowledge of the extension's JAR: it's
    // interned and jar_extension_receivers knows it declares an extension on
    // "ViewModel" — matching what `build_jar_manifest`/
    // `populate_tier1_from_manifest` would now actually produce for a
    // manifest entry with `extension_receiver: Some("ViewModel")` — but
    // `extension_by_receiver` (Tier 2) is deliberately NOT seeded, matching
    // a real not-yet-materialized JAR.
    let jar_id = idx.jar_table.intern("/fake/lifecycle-ktx.jar");
    idx.jar_extension_receivers
        .entry("ViewModel".to_owned())
        .or_default()
        .push(jar_id);

    let vm_uri = Url::parse("file:///app/MyViewModel.kt").unwrap();
    idx.index_content(
        &vm_uri,
        concat!(
            "package app\n",
            "import androidx.lifecycle.ViewModel\n",
            "class MyViewModel : ViewModel() {\n",
            "    fun load() { viewModelScope.toString() }\n",
            "}\n",
        ),
    );

    // "ViewModel" (uppercase-leading) hits `resolve_dot_receiver_type`'s
    // type-name fast path directly, bypassing variable/extension-property
    // inference entirely — this drives `extension_fn_completions` with
    // receiver_type = "ViewModel" directly, exactly the site under test.
    let _ = complete_dot(&idx, "ViewModel", &vm_uri, true, None);

    assert!(
        idx.materialization_failed.contains(&jar_id),
        "dot-completion on a ViewModel-typed receiver must attempt \
         promotion for a JAR that Tier 1 says declares an extension on an \
         ancestor type — observable here via materialization_failed for \
         the fake jar path, proving the attempt happened rather than being \
         silently skipped"
    );
}

/// Companion to `extension_completion_attempts_promotion_for_a_tier1_only_receiver`,
/// covering `complete_bare`'s SEPARATE ancestor-extension loop (implicit
/// `this`-context extension completion inside a subclass body) — the second
/// of the two unwired `extension_by_receiver` call sites. Mirrors
/// `bare_completion_includes_this_extensions_inside_subclass` above, but
/// with the extension living in a Tier-1-only JAR instead of a source file.
#[test]
fn bare_completion_attempts_promotion_for_a_tier1_only_this_extension() {
    let idx = Indexer::new();
    idx.index_content(
        &Url::parse("file:///sdk/ViewModel.kt").unwrap(),
        "package androidx.lifecycle\nopen class ViewModel",
    );

    let jar_id = idx.jar_table.intern("/fake/lifecycle-ktx.jar");
    idx.jar_extension_receivers
        .entry("ViewModel".to_owned())
        .or_default()
        .push(jar_id);

    let vm_uri = Url::parse("file:///app/DashboardViewModel.kt").unwrap();
    idx.index_content(
        &vm_uri,
        concat!(
            "package app\n",
            "import androidx.lifecycle.ViewModel\n",
            "class DashboardViewModel : ViewModel() {\n",
            "    fun load() {\n",
            "        val s = viewModelScope\n", // cursor is on line 4 (0-based)
            "    }\n",
            "}\n",
        ),
    );

    let (_items, _) = complete_bare(&idx, "viewModel", &vm_uri, false, false, Some(4));

    assert!(
        idx.materialization_failed.contains(&jar_id),
        "bare-word completion inside a DashboardViewModel method must \
         attempt promotion for a JAR that Tier 1 says declares an \
         extension on an ancestor type (implicit `this` receiver) — \
         observable here via materialization_failed for the fake jar path"
    );
}

/// Regression test for the second post-ship lazy-JAR gap report: INHERITED
/// regular members (e.g. `setState` on a library `MviViewModel` base class)
/// disappeared from completion. Root cause: `supertype_targets`
/// (hierarchy.rs) resolves each super-class name via `resolve_symbol_no_rg`,
/// which reads `jar_definitions` (Tier 2) with no Tier-1 promotion — so a
/// hierarchy walk dead-ends at any not-yet-materialized JAR ancestor, and
/// none of its members are ever collected. Both dot-completion
/// (`collect_inherited_dot_completion_items`) and bare completion
/// (`collect_this_extensions`' ancestor cache) flow through the same walk.
///
/// Fake/nonexistent jar path + no sidecar, so this proves the promotion
/// ATTEMPT fires (via `materialization_failed`) — the established pattern
/// for this plan's promotion tests.
#[test]
fn hierarchy_walk_attempts_promotion_for_a_tier1_only_super_class() {
    let idx = Indexer::new();

    let jar_id = idx.jar_table.intern("/fake/mvi-lib.jar");
    idx.jar_bare_names
        .entry("MviViewModel".to_owned())
        .or_default()
        .push(jar_id);

    let app_uri = Url::parse("file:///app/MyViewModel.kt").unwrap();
    idx.index_content(
        &app_uri,
        concat!(
            "package app\n",
            "import lib.MviViewModel\n",
            "class MyViewModel : MviViewModel() {\n",
            "    fun load() {}\n",
            "}\n",
        ),
    );

    // Uppercase type name hits `resolve_dot_receiver_type`'s type-name fast
    // path; `MyViewModel` itself is workspace-declared, so the receiver file
    // resolves and `collect_inherited_dot_completion_items` runs the
    // hierarchy walk into the Tier-1-only super class.
    let _ = complete_dot(&idx, "MyViewModel", &app_uri, true, None);

    assert!(
        idx.materialization_failed.contains(&jar_id),
        "walking a workspace class's hierarchy into a super class that only \
         exists in a Tier-1-only JAR must attempt promotion of that JAR \
         (so its inherited members, e.g. `setState`, become completable) — \
         not silently dead-end at the unresolvable super"
    );
}

/// Companion to the hierarchy test above, for the RECEIVER TYPE itself:
/// completing on a value whose type is declared only in a Tier-1-only JAR
/// (e.g. a `Modifier`- or `MviViewModel`-typed variable). Root cause:
/// `resolve_dot_receiver_file` resolves the receiver type via
/// `resolve_symbol_no_rg` (Tier-2 `jar_definitions` reads, no promotion) —
/// when it fails, BOTH direct-member and inherited-member completion are
/// skipped wholesale for that receiver.
#[test]
fn dot_completion_attempts_promotion_for_a_tier1_only_receiver_type() {
    let idx = Indexer::new();

    let jar_id = idx.jar_table.intern("/fake/mvi-lib.jar");
    idx.jar_bare_names
        .entry("MviViewModel".to_owned())
        .or_default()
        .push(jar_id);

    let app_uri = Url::parse("file:///app/Screen.kt").unwrap();
    idx.index_content(&app_uri, "package app\nfun show() {}\n");

    let _ = complete_dot(&idx, "MviViewModel", &app_uri, true, None);

    assert!(
        idx.materialization_failed.contains(&jar_id),
        "dot-completion on a type declared only in a Tier-1-only JAR must \
         attempt promotion of that JAR before resolving the receiver's \
         declaring file — otherwise direct and inherited members are both \
         silently skipped"
    );
}

/// End-to-end reproduction of the "setState visible on hover but not in
/// completion" report. The library base class exists TWICE: parsed
/// sources-jar data (real ranges, in `files`/`qualified`) and a compiled
/// JAR (Tier-1-only at first). Dot-completion's hierarchy walk promotes the
/// compiled JAR (cache-backed, so promotion genuinely materializes it even
/// in this sidecar-less test) — and `populate_from_symbols`' unconditional
/// `qualified.insert` then clobbered the sources-backed entry with a
/// synthetic one-line location, so `symbols_from_nested_type`'s
/// range-nesting found no members. Hover kept working (name-keyed lookup),
/// which is exactly the reported disparity.
#[test]
fn inherited_members_survive_on_demand_materialization_of_the_base_class_jar() {
    let tmp = tempfile::tempdir().expect("tempdir");
    crate::indexer::test_helpers::with_xdg_cache(tmp.path(), || {
        let jar_path = tmp.path().join("mvi-lib.jar");
        std::fs::write(&jar_path, b"fake jar bytes").expect("write fake jar");
        let jar_path_key = jar_path.to_string_lossy().to_string();

        // The compiled JAR's sidecar symbols for the same base class.
        let compiled = vec![
            crate::sidecar::SidecarSymbol {
                name: "MviViewModel".to_owned(),
                kind: "class".to_owned(),
                container: String::new(),
                detail: "class MviViewModel".to_owned(),
                doc: String::new(),
                type_params: Vec::new(),
                extension_receiver_type: String::new(),
                trailing_lambda: false,
                deprecated: false,
                pkg: "com.lib".to_owned(),
                top_level: true,
                supers: vec![],
            },
            crate::sidecar::SidecarSymbol {
                name: "setState".to_owned(),
                kind: "fun".to_owned(),
                container: "MviViewModel".to_owned(),
                detail: "fun setState(reducer: S.() -> S)".to_owned(),
                doc: String::new(),
                type_params: Vec::new(),
                extension_receiver_type: String::new(),
                trailing_lambda: false,
                deprecated: false,
                pkg: "com.lib".to_owned(),
                top_level: false,
                supers: vec![],
            },
        ];
        let entry = crate::indexer::jar_cache::make_cache_entry(&jar_path, compiled)
            .expect("cache entry for existing file");
        let mut entries = std::collections::HashMap::new();
        entries.insert(jar_path_key.clone(), entry);
        crate::indexer::jar_cache::save_jar_cache(&entries);

        let idx = Indexer::new();
        // Tier 1 knows the compiled JAR declares MviViewModel — this is what
        // the hierarchy walk's promotion gate keys on.
        let jar_id = idx.jar_table.intern(&jar_path_key);
        idx.jar_bare_names
            .entry("MviViewModel".to_owned())
            .or_default()
            .push(jar_id);

        // The sources-JAR pipeline already parsed the real base class.
        let sources_uri = Url::parse("file:///sources/com/lib/MviViewModel.kt").unwrap();
        idx.index_content(
            &sources_uri,
            concat!(
                "package com.lib\n",
                "open class MviViewModel {\n",
                "    fun setState(reducer: Int) {}\n",
                "}\n",
            ),
        );

        // The user's subclass.
        let app_uri = Url::parse("file:///app/MyViewModel.kt").unwrap();
        idx.index_content(
            &app_uri,
            concat!(
                "package app\n",
                "import com.lib.MviViewModel\n",
                "class MyViewModel : MviViewModel() {\n",
                "    fun load() {}\n",
                "}\n",
            ),
        );

        let items = complete_dot(&idx, "MyViewModel", &app_uri, true, None);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();

        assert!(
            idx.materialized.contains(&jar_id),
            "precondition: the hierarchy walk must have promoted the \
             cache-backed compiled JAR (otherwise this test isn't \
             exercising the clobber scenario at all); got labels {labels:?}"
        );
        assert!(
            labels.contains(&"setState"),
            "inherited setState must remain completable after the base \
             class's compiled JAR materializes on demand — the parsed \
             sources-backed qualified entry must not be clobbered by the \
             synthetic one; got: {labels:?}"
        );
    });
}

/// Deprecated and internal library overloads must be filtered out of
/// dot-completion, leaving only the current public `launch` (plus its
/// trailing-lambda form). Mirrors Android Studio, which hides the deprecated
/// binary-compat shims and internal impl helpers coroutines ships.
#[test]
fn library_deprecated_internal_extensions_hidden() {
    let idx = Indexer::new();
    idx.index_content(
        &Url::parse("file:///sdk/ViewModel.kt").unwrap(),
        "package androidx.lifecycle\nopen class ViewModel",
    );
    idx.extension_by_receiver
        .entry("ViewModel".to_owned())
        .or_default()
        .push(crate::types::ExtensionEntry {
            file_uri: "jar:file:///lifecycle-ktx.jar/ViewModel.class".to_owned(),
            name: "viewModelScope".to_owned(),
            kind: tower_lsp::lsp_types::SymbolKind::PROPERTY,
            detail: "val ViewModel.viewModelScope: CoroutineScope".to_owned(),
            visibility: crate::types::Visibility::Public,
            package: Some("androidx.lifecycle".to_owned()),
            trailing_lambda: false,
            deprecated: false,
            container: None,
        });

    let mk = |detail: &str, vis, deprecated| crate::types::ExtensionEntry {
        file_uri: "jar:file:///coroutines-core.jar/Builders.class".to_owned(),
        name: "launch".to_owned(),
        kind: tower_lsp::lsp_types::SymbolKind::FUNCTION,
        detail: detail.to_owned(),
        visibility: vis,
        package: Some("kotlinx.coroutines".to_owned()),
        trailing_lambda: true,
        deprecated,
        container: None,
    };
    {
        let mut slot = idx
            .extension_by_receiver
            .entry("CoroutineScope".to_owned())
            .or_default();
        // Current public overload — should appear.
        slot.push(mk(
            "fun CoroutineScope.launch(block: suspend () -> Unit): Job",
            crate::types::Visibility::Public,
            false,
        ));
        // Deprecated binary-compat shim — should be hidden (library + deprecated).
        slot.push(mk(
            "fun CoroutineScope.launch(parent: Job, block: suspend () -> Unit): Job",
            crate::types::Visibility::Public,
            true,
        ));
        // Internal impl helper — should be hidden (library + internal).
        slot.push(mk(
            "fun CoroutineScope.launch(impl: Int): Job",
            crate::types::Visibility::Internal,
            false,
        ));
    }

    let vm_uri = Url::parse("file:///app/MyViewModel.kt").unwrap();
    idx.index_content(
        &vm_uri,
        concat!(
            "package app\n",
            "import androidx.lifecycle.ViewModel\n",
            "class MyViewModel : ViewModel() {\n",
            "    fun load() { viewModelScope.launch {} }\n",
            "}\n",
        ),
    );

    let items = complete_dot(&idx, "viewModelScope", &vm_uri, true, None);
    let launch_items: Vec<&CompletionItem> = items
        .iter()
        .filter(|i| i.label == "launch" || i.label == "launch { }")
        .collect();
    let labels: Vec<&str> = launch_items.iter().map(|i| i.label.as_str()).collect();
    // Exactly the current overload's two ergonomic forms — deprecated + internal gone.
    assert_eq!(
        launch_items.len(),
        2,
        "expected only launch() + launch {{ }} for the current overload, got: {labels:?}"
    );
    assert!(
        launch_items.iter().all(|i| i.tags.is_none()),
        "no item should be tagged deprecated"
    );
}

/// Overloads of one library extension collapse to a single completion entry
/// (plus its trailing-lambda form). Reproduces the coroutines 1.11.0 sidecar
/// artifact where `CoroutineScope.launch` is emitted three times with bogus
/// first-param types (`CoroutineContext`, `Job`, `NonCancellable`); the user
/// should see only `launch` + `launch { }`, not three of each.
#[test]
fn extension_overloads_collapse_to_single_entry() {
    let idx = Indexer::new();
    idx.index_content(
        &Url::parse("file:///sdk/ViewModel.kt").unwrap(),
        "package androidx.lifecycle\nopen class ViewModel",
    );
    idx.extension_by_receiver
        .entry("ViewModel".to_owned())
        .or_default()
        .push(crate::types::ExtensionEntry {
            file_uri: "jar:file:///lifecycle-ktx.jar/ViewModel.class".to_owned(),
            name: "viewModelScope".to_owned(),
            kind: tower_lsp::lsp_types::SymbolKind::PROPERTY,
            detail: "val ViewModel.viewModelScope: CoroutineScope".to_owned(),
            visibility: crate::types::Visibility::Public,
            package: Some("androidx.lifecycle".to_owned()),
            trailing_lambda: false,
            deprecated: false,
            container: None,
        });
    let mk = |first_param: &str, pkg: &str, defaults: bool| crate::types::ExtensionEntry {
        file_uri: "jar:file:///coroutines-core.jar/Builders.class".to_owned(),
        name: "launch".to_owned(),
        kind: tower_lsp::lsp_types::SymbolKind::FUNCTION,
        detail: if defaults {
            format!("fun CoroutineScope.launch(context: {first_param} = EmptyCoroutineContext, block: suspend () -> Unit): Job")
        } else {
            format!(
                "fun CoroutineScope.launch(context: {first_param}, block: suspend () -> Unit): Job"
            )
        },
        visibility: crate::types::Visibility::Public,
        package: Some(pkg.to_owned()),
        trailing_lambda: true,
        deprecated: false,
        container: None,
    };
    {
        let mut slot = idx
            .extension_by_receiver
            .entry("CoroutineScope".to_owned())
            .or_default();
        // Compiled-JAR overloads (no defaults) under one inferred package…
        slot.push(mk("CoroutineContext", "kotlinx.coroutines", false));
        slot.push(mk("Job", "kotlinx.coroutines", false));
        slot.push(mk("NonCancellable", "kotlinx.coroutines", false));
        // …plus the sources-JAR copy of the SAME function with default values and
        // a different (exact) package. Must still collapse into the single entry.
        slot.push(mk("CoroutineContext", "kotlinx.coroutines.core", true));
    }

    let vm_uri = Url::parse("file:///app/MyViewModel.kt").unwrap();
    idx.index_content(
        &vm_uri,
        concat!(
            "package app\n",
            "import androidx.lifecycle.ViewModel\n",
            "class MyViewModel : ViewModel() {\n",
            "    fun load() { viewModelScope.launch {} }\n",
            "}\n",
        ),
    );

    let items = complete_dot(&idx, "viewModelScope", &vm_uri, true, None);
    let n_plain = items.iter().filter(|i| i.label == "launch").count();
    let n_lambda = items.iter().filter(|i| i.label == "launch { }").count();
    assert_eq!(n_plain, 1, "expected exactly one `launch`, got {n_plain}");
    assert_eq!(
        n_lambda, 1,
        "expected exactly one `launch {{ }}`, got {n_lambda}"
    );
}

/// Deprecated WORKSPACE extensions are kept (you may still call your own code
/// mid-migration) but tagged Deprecated and sorted to the bottom. Uses the same
/// `viewModelScope → CoroutineScope` resolution the test above relies on, then
/// adds a workspace-sourced deprecated extension on `CoroutineScope`.
#[test]
fn deprecated_workspace_extension_kept_tagged() {
    let idx = Indexer::new();
    idx.index_content(
        &Url::parse("file:///sdk/ViewModel.kt").unwrap(),
        "package androidx.lifecycle\nopen class ViewModel",
    );
    // JAR property resolves viewModelScope → CoroutineScope.
    idx.extension_by_receiver
        .entry("ViewModel".to_owned())
        .or_default()
        .push(crate::types::ExtensionEntry {
            file_uri: "jar:file:///lifecycle-ktx.jar/ViewModel.class".to_owned(),
            name: "viewModelScope".to_owned(),
            kind: tower_lsp::lsp_types::SymbolKind::PROPERTY,
            detail: "val ViewModel.viewModelScope: CoroutineScope".to_owned(),
            visibility: crate::types::Visibility::Public,
            package: Some("androidx.lifecycle".to_owned()),
            trailing_lambda: false,
            deprecated: false,
            container: None,
        });
    // A WORKSPACE-sourced deprecated extension on CoroutineScope (file:// URI).
    idx.index_content(
        &Url::parse("file:///app/ext.kt").unwrap(),
        concat!(
            "package kotlinx.coroutines\n",
            "@Deprecated(\"use newWork\")\n",
            "fun CoroutineScope.legacyWork() {}\n",
        ),
    );

    let vm_uri = Url::parse("file:///app/MyViewModel.kt").unwrap();
    idx.index_content(
        &vm_uri,
        concat!(
            "package app\n",
            "import androidx.lifecycle.ViewModel\n",
            "class MyViewModel : ViewModel() {\n",
            "    fun load() { viewModelScope.legacyWork() }\n",
            "}\n",
        ),
    );

    let items = complete_dot(&idx, "viewModelScope", &vm_uri, false, None);
    let legacy = items
        .iter()
        .find(|i| i.label == "legacyWork")
        .expect("deprecated workspace extension should still be offered");
    assert_eq!(
        legacy.tags.as_deref(),
        Some(&[CompletionItemTag::DEPRECATED][..]),
        "workspace deprecated item should carry the Deprecated tag"
    );
    assert!(
        legacy
            .sort_text
            .as_deref()
            .is_some_and(|s| s.starts_with("99:")),
        "deprecated item should sort to the bottom, got: {:?}",
        legacy.sort_text
    );
}

#[test]
fn trailing_lambda_completion_offered() {
    let idx = Indexer::new();

    let ext_uri = Url::parse("file:///sdk/collections.kt").unwrap();
    idx.index_content(
        &ext_uri,
        concat!(
            "package sdk\n",
            "interface Items\n",
            "fun Items.each(block: (String) -> Unit): Unit = TODO()\n",
        ),
    );

    let app_uri = Url::parse("file:///app/Main.kt").unwrap();
    idx.index_content(
        &app_uri,
        concat!(
            "package app\n",
            "import sdk.Items\n",
            "import sdk.each\n",
            "fun use(items: Items) { items.each }\n",
        ),
    );

    let items = complete_dot(&idx, "items", &app_uri, true, Some(3));
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();

    assert!(labels.contains(&"each"), "regular form missing: {labels:?}");
    assert!(
        labels.contains(&"each { }"),
        "lambda form missing: {labels:?}"
    );

    let lam = items.iter().find(|i| i.label == "each { }").unwrap();
    assert_eq!(lam.insert_text.as_deref(), Some("each { $1 }"));
    assert_eq!(lam.insert_text_format, Some(InsertTextFormat::SNIPPET));
}

// ── Issue 1: Generic type parameter inference for function calls ──────────

/// `retrofit.create<ApiClass>()` should infer the return type as `ApiClass`
/// via the explicit type argument, not leave it as raw `T`.
#[test]
fn infer_type_in_lines_generic_create_with_type_arg() {
    // Retrofit-style: fun <T> create(service: Class<T>): T
    // When called as create<ApiClass>(ApiClass::class.java), the return type
    // should be ApiClass (substituted from the explicit type argument).
    let lines: Vec<String> =
        vec!["    val api = retrofit.create<ApiClass>(ApiClass::class.java)".into()];
    assert_eq!(
        infer_type_in_lines(&lines, "api"),
        Some("ApiClass".into()),
        "generic create<T>() with explicit type arg should infer T=ApiClass"
    );
}

/// `retrofit.create<ApiClass>()` without class literal should also infer
/// the return type from the explicit type argument alone.
#[test]
fn infer_type_in_lines_generic_create_type_arg_only() {
    let lines: Vec<String> = vec!["    val api = retrofit.create<ApiClass>()".into()];
    assert_eq!(
        infer_type_in_lines(&lines, "api"),
        Some("ApiClass".into()),
        "generic create<T>() with type arg only should infer T=ApiClass"
    );
}

/// Existing DI patterns should still work after removing the hardcoded allowlist.
#[test]
fn infer_type_in_lines_di_get_still_works() {
    let lines: Vec<String> = vec!["    val repo = get<UserRepository>()".into()];
    assert_eq!(
        infer_type_in_lines(&lines, "repo"),
        Some("UserRepository".into()),
        "DI get<T>() should still infer correctly"
    );
}

// ── Issue 2: Extension function precedence over member functions ─────────

/// When an extension function is imported with the same name as a member,
/// goto-definition should resolve to the extension, not the member.
#[test]
fn resolve_imported_extension_preferred_over_member() {
    let service_uri = uri("/Service.kt");
    let ext_uri = uri("/ServiceExtensions.kt");
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();

    idx.index_content(
        &service_uri,
        "package com.example\n\
         class Service {\n\
             fun execute() { /* member */ }\n\
         }",
    );
    idx.index_content(
        &ext_uri,
        "package com.example.ext\n\
         fun Service.execute() { /* extension */ }",
    );
    idx.index_content(
        &caller_uri,
        "package com.example.app\n\
         import com.example.ext.execute\n\
         fun test() {\n\
             Service().execute()\n\
         }",
    );

    // Resolving `execute` with qualifier `Service` should find the extension,
    // not the member.
    let locs = resolve_symbol(&idx, "execute", Some("Service"), &caller_uri);
    assert!(!locs.is_empty(), "extension function should be found");
    assert_eq!(
        locs[0].uri, ext_uri,
        "should resolve to extension function, not member"
    );
}

/// When no extension exists, member functions should still resolve correctly.
#[test]
fn resolve_member_when_no_extension() {
    let service_uri = uri("/Service.kt");
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();

    idx.index_content(
        &service_uri,
        "package com.example\n\
         class Service {\n\
             fun execute() { /* member */ }\n\
         }",
    );
    idx.index_content(
        &caller_uri,
        "package com.example\n\
         fun test() {\n\
             Service().execute()\n\
         }",
    );

    let locs = resolve_symbol(&idx, "execute", Some("Service"), &caller_uri);
    assert!(!locs.is_empty(), "member function should be found");
    assert_eq!(locs[0].uri, service_uri, "should resolve to member");
}

#[test]
fn when_branch_delete_no_timeout() {
    // Regression: deleting a branch from a when expression should not cause
    // timeouts on subsequent actions. This test verifies that the indexer
    // and resolver handle the deletion correctly without hanging.
    let uri = uri("/test.kt");
    let idx = Indexer::new();

    // Initial index with a when expression containing two branches
    let src_v1 = "\
sealed class Event
object OnClick : Event()
object OnLongPress : Event()

fun handle(event: Event) {
    when (event) {
        is OnClick -> println(\"click\")
        is OnLongPress -> println(\"long press\")
    }
}
";
    idx.index_content(&uri, src_v1);

    // Verify initial resolution works
    let locs = resolve_symbol(&idx, "handle", None, &uri);
    assert!(!locs.is_empty(), "handle should be found");

    // Now delete the second branch (simulating user editing the file)
    let src_v2 = "\
sealed class Event
object OnClick : Event()
object OnLongPress : Event()

fun handle(event: Event) {
    when (event) {
        is OnClick -> println(\"click\")
    }
}
";
    idx.index_content(&uri, src_v2);

    // Verify resolution still works after branch deletion — this should NOT timeout
    let locs = resolve_symbol(&idx, "handle", None, &uri);
    assert!(
        !locs.is_empty(),
        "handle should still be found after branch deletion"
    );

    // Verify the sealed class subtypes are still correct
    let subtypes = idx.subtypes.get("Event");
    assert!(subtypes.is_some(), "Event subtypes should still be indexed");
}

/// Reproduction: go-def / hover on an annotation USAGE (`@Composable`) imported
/// from a library must resolve to the annotation-class declaration, not fall
/// through to the rg text-fallback (which lands on a comment).
#[test]
fn annotation_usage_resolves_via_import() {
    let idx = Indexer::new();
    let lib = Url::parse("file:///lib/Composable.kt").unwrap();
    idx.index_content(
        &lib,
        "package androidx.compose.runtime\n\n@MustBeDocumented\nannotation class Composable\n",
    );
    let use_uri = Url::parse("file:///app/Greeting.kt").unwrap();
    idx.index_content(
        &use_uri,
        concat!(
            "package app\n",
            "import androidx.compose.runtime.Composable\n",
            "\n",
            "@Composable\n",
            "fun Greeting() {}\n",
        ),
    );
    let locs = idx.find_definition_qualified("Composable", None, &use_uri);
    assert!(
        locs.iter().any(|l| l.uri == lib),
        "annotation usage should resolve to its declaration; got: {:?}",
        locs.iter().map(|l| l.uri.as_str()).collect::<Vec<_>>()
    );
}

/// Regression: a compiled-JAR symbol whose per-jar inferred package does not
/// match the import (because a multi-package jar like androidx.compose.runtime
/// gets one inferred package for all its symbols) must still resolve via the
/// exact-FQN import, instead of being filtered out and falling to the rg
/// text-fallback (which lands on a comment). This is the `@Composable` case.
#[test]
fn jar_symbol_resolves_despite_wrong_per_jar_package() {
    use crate::types::FileData;
    use std::sync::Arc;

    let idx = Indexer::new();
    let jar_uri = "jar:file:///compose-runtime.jar!/androidx/compose/runtime/Composable.class";
    idx.jar_definitions
        .entry("Composable".to_string())
        .or_default()
        .push(tower_lsp::lsp_types::Location {
            uri: Url::parse(jar_uri).unwrap(),
            range: tower_lsp::lsp_types::Range::default(),
        });
    // Per-jar inferred package is some OTHER compose package — does not match
    // the import's `androidx.compose.runtime`.
    idx.jar_files.insert(
        jar_uri.to_string(),
        Arc::new(FileData {
            package: Some("androidx.compose.ui".to_string()),
            ..Default::default()
        }),
    );

    let use_uri = Url::parse("file:///app/Greeting.kt").unwrap();
    idx.index_content(
        &use_uri,
        "package app\nimport androidx.compose.runtime.Composable\n@Composable\nfun G() {}\n",
    );

    let locs = idx.find_definition_qualified("Composable", None, &use_uri);
    assert!(
        locs.iter().any(|l| l.uri.as_str().starts_with("jar:")),
        "JAR symbol should resolve despite per-jar package mismatch; got {:?}",
        locs.iter().map(|l| l.uri.as_str()).collect::<Vec<_>>()
    );
}

// ── SCOPE_FUNCTIONS fallback (Category C) ───────────────────────────────────

/// `SCOPE_FUNCTIONS` (`let`/`also`/`run`/`apply`/`takeIf`/`takeUnless`) are
/// receiver-generic stdlib extensions (`<T> T.apply { ... }`), never a member of
/// any specific receiver type. A receiver-typed lookup for one of them must fall
/// back to the same bare (unqualified) resolution a plain reference to that stdlib
/// name would use, once the ordinary receiver-scoped search finds no member.
#[test]
fn find_definition_qualified_falls_back_to_bare_lookup_for_scope_functions() {
    use crate::types::FileData;
    use std::sync::Arc;

    let idx = Indexer::new();
    let jar_uri = "jar:file:///kotlin-stdlib.jar!/kotlin/StandardKt.class";
    idx.jar_definitions
        .entry("apply".to_string())
        .or_default()
        .push(tower_lsp::lsp_types::Location {
            uri: Url::parse(jar_uri).unwrap(),
            range: tower_lsp::lsp_types::Range::default(),
        });
    idx.jar_files.insert(
        jar_uri.to_string(),
        Arc::new(FileData {
            package: Some("kotlin".to_string()),
            ..Default::default()
        }),
    );

    let use_uri = Url::parse("file:///app/Show.kt").unwrap();
    idx.index_content(
        &use_uri,
        concat!(
            "package app\n",
            "import kotlin.apply\n",
            "class SomeBuilderResult\n",
            "class Builder { fun build(): SomeBuilderResult = SomeBuilderResult() }\n",
            "fun show() {\n",
            "    Builder().build().apply { show() }\n",
            "}\n",
        ),
    );

    let locs = idx.find_definition_qualified("apply", Some("SomeBuilderResult"), &use_uri);
    assert!(
        locs.iter().any(|l| l.uri.as_str() == jar_uri),
        "receiver-typed `apply` must fall back to the bare stdlib declaration when \
         the receiver type has no member named `apply`; got {:?}",
        locs.iter().map(|l| l.uri.as_str()).collect::<Vec<_>>()
    );
}

/// Real, measured Moneta collision: `apply`/`run` (Kotlin's own scope
/// functions) collide with hundreds of unrelated same-named JVM/Android
/// methods across a real dependency graph (`java.util.function.Function
/// .apply`, `Runnable.run`, countless builder `.apply()` methods) — none of
/// which are ever reachable without an explicit import, unlike `kotlin.apply`
/// which needs none (Kotlin's own default-import list). Unlike the sibling
/// test above (which explicitly writes `import kotlin.apply` — not
/// representative of real code, which almost never spells out an import for
/// an implicitly-available stdlib scope function), this reproduces the REAL
/// shape: multiple same-named candidates, no `workspace.json` module data
/// (Moneta's own real corpus state — confirmed no `libraries`/`dependencies`
/// section), and no explicit import of anything relevant at all.
#[test]
fn find_definition_qualified_prefers_kotlin_default_import_over_unimported_decoy() {
    use crate::types::FileData;
    use std::sync::Arc;

    let idx = Indexer::new();
    let kotlin_jar_uri = "jar:file:///kotlin-stdlib.jar!/kotlin/StandardKt.class";
    let decoy_jar_uri = "jar:file:///decoy-lib.jar!/com/example/Decoy.class";
    idx.jar_definitions
        .entry("apply".to_string())
        .or_default()
        .extend([
            tower_lsp::lsp_types::Location {
                uri: Url::parse(kotlin_jar_uri).unwrap(),
                range: tower_lsp::lsp_types::Range::default(),
            },
            tower_lsp::lsp_types::Location {
                uri: Url::parse(decoy_jar_uri).unwrap(),
                range: tower_lsp::lsp_types::Range::default(),
            },
        ]);
    idx.jar_files.insert(
        kotlin_jar_uri.to_string(),
        Arc::new(FileData {
            package: Some("kotlin".to_string()),
            ..Default::default()
        }),
    );
    idx.jar_files.insert(
        decoy_jar_uri.to_string(),
        Arc::new(FileData {
            package: Some("com.example".to_string()),
            ..Default::default()
        }),
    );

    let use_uri = Url::parse("file:///app/Show.kt").unwrap();
    // Deliberately NO `import kotlin.apply` and no other import that would
    // help the sibling-import-package tie-break either — matching real code,
    // which never spells out an import for an implicit default-import.
    idx.index_content(
        &use_uri,
        concat!(
            "package app\n",
            "class SomeBuilderResult\n",
            "class Builder { fun build(): SomeBuilderResult = SomeBuilderResult() }\n",
            "fun show() {\n",
            "    Builder().build().apply { show() }\n",
            "}\n",
        ),
    );

    let locs = idx.find_definition_qualified("apply", Some("SomeBuilderResult"), &use_uri);
    assert!(
        locs.iter().any(|l| l.uri.as_str() == kotlin_jar_uri),
        "must prefer the kotlin default-import candidate over the decoy when \
         ambiguous and nothing is explicitly imported; got {:?}",
        locs.iter().map(|l| l.uri.as_str()).collect::<Vec<_>>()
    );
    assert!(
        !locs.iter().any(|l| l.uri.as_str() == decoy_jar_uri),
        "must not include the decoy candidate; got {:?}",
        locs.iter().map(|l| l.uri.as_str()).collect::<Vec<_>>()
    );
}

/// The resolution-accuracy benchmark's own `resolve_identity_with_io(..., true)`
/// path calls `find_definition_qualified_index_only`, not `find_definition_qualified` —
/// so the scope-function fallback must apply there too, or the benchmark this fix
/// was built for never actually benefits from it (Copilot review on PR #277).
#[test]
fn find_definition_qualified_index_only_falls_back_to_bare_lookup_for_scope_functions() {
    use crate::types::FileData;
    use std::sync::Arc;

    let idx = Indexer::new();
    let jar_uri = "jar:file:///kotlin-stdlib.jar!/kotlin/StandardKt.class";
    idx.jar_definitions
        .entry("apply".to_string())
        .or_default()
        .push(tower_lsp::lsp_types::Location {
            uri: Url::parse(jar_uri).unwrap(),
            range: tower_lsp::lsp_types::Range::default(),
        });
    idx.jar_files.insert(
        jar_uri.to_string(),
        Arc::new(FileData {
            package: Some("kotlin".to_string()),
            ..Default::default()
        }),
    );

    let use_uri = Url::parse("file:///app/Show.kt").unwrap();
    idx.index_content(
        &use_uri,
        concat!(
            "package app\n",
            "import kotlin.apply\n",
            "class SomeBuilderResult\n",
            "class Builder { fun build(): SomeBuilderResult = SomeBuilderResult() }\n",
            "fun show() {\n",
            "    Builder().build().apply { show() }\n",
            "}\n",
        ),
    );

    let locs =
        idx.find_definition_qualified_index_only("apply", Some("SomeBuilderResult"), &use_uri);
    assert!(
        locs.iter().any(|l| l.uri.as_str() == jar_uri),
        "index-only receiver-typed `apply` must also fall back to the bare stdlib \
         declaration; got {:?}",
        locs.iter().map(|l| l.uri.as_str()).collect::<Vec<_>>()
    );
}

/// Decoy: a receiver type that declares its OWN member named `apply` must still
/// resolve to that member — the scope-function fallback may only fire after the
/// ordinary receiver-scoped search has already failed, never unconditionally for
/// every name in `SCOPE_FUNCTIONS`.
#[test]
fn find_definition_qualified_prefers_own_member_over_scope_function_fallback() {
    use crate::types::FileData;
    use std::sync::Arc;

    let idx = Indexer::new();
    let jar_uri = "jar:file:///kotlin-stdlib.jar!/kotlin/StandardKt.class";
    idx.jar_definitions
        .entry("apply".to_string())
        .or_default()
        .push(tower_lsp::lsp_types::Location {
            uri: Url::parse(jar_uri).unwrap(),
            range: tower_lsp::lsp_types::Range::default(),
        });
    idx.jar_files.insert(
        jar_uri.to_string(),
        Arc::new(FileData {
            package: Some("kotlin".to_string()),
            ..Default::default()
        }),
    );

    let use_uri = Url::parse("file:///app/Config.kt").unwrap();
    idx.index_content(
        &use_uri,
        concat!(
            "package app\n",
            "import kotlin.apply\n",
            "class ConfigBuilder {\n",
            "    fun apply(): ConfigBuilder = this\n",
            "}\n",
            "fun show() {\n",
            "    ConfigBuilder().apply()\n",
            "}\n",
        ),
    );

    let locs = idx.find_definition_qualified("apply", Some("ConfigBuilder"), &use_uri);
    assert!(
        !locs.is_empty() && locs.iter().all(|l| l.uri == use_uri),
        "must resolve to ConfigBuilder's own `apply()`, not the stdlib scope-function \
         fallback; got {:?}",
        locs.iter().map(|l| l.uri.as_str()).collect::<Vec<_>>()
    );
}

/// Real, measured bug (Moneta's `FormatUtil.java`, 6 real `formatAmount`
/// overloads): a Java class's overloaded method always resolved to exactly
/// ONE arbitrary candidate via `find_name_scoped_to_container`'s `.find()`
/// (Java method symbols land in reverse source order in `file_data.symbols`,
/// so `.find()` always picked the highest-arity, last-declared overload) —
/// which then failed arity-based shape filtering (`shape_filter_locations`)
/// for nearly every real call site, since callers overwhelmingly use the
/// OTHER overloads. Qualified member lookup must hand the caller ALL
/// same-named candidates so shape filtering — not first-match order — picks
/// the right one.
#[test]
fn find_definition_qualified_returns_all_overload_candidates() {
    let idx = Indexer::new();
    let use_uri = Url::parse("file:///app/FormatUtil.java").unwrap();
    idx.index_content(
        &use_uri,
        concat!(
            "package app;\n",
            "public class FormatUtil {\n",
            "    public static String formatAmount(java.math.BigDecimal a) { return null; }\n",
            "    public static String formatAmount(java.math.BigDecimal a, int b) { return null; }\n",
            "    public static String formatAmount(java.math.BigDecimal a, int b, boolean c) { return null; }\n",
            "}\n",
        ),
    );

    let locs = idx.find_definition_qualified("formatAmount", Some("FormatUtil"), &use_uri);
    assert_eq!(
        locs.len(),
        3,
        "must return all 3 overload candidates, not collapse to the single \
         highest-arity/last-declared one; got {:?}",
        locs
    );
}

/// Same bug class as [`find_definition_qualified_returns_all_overload_candidates`],
/// but for a JAR-derived (compiled) class -- real, measured Moneta gap:
/// `org.junit.Assert.fail()`/`.assertEquals(a, b)` (0-arg and 2-arg calls)
/// resolved to nothing.
///
/// A JAR-derived class's synthetic `FileData` is flat: every symbol gets its
/// own single-line `range == selection_range`, so a class's own "container"
/// symbol range never truly *encloses* its members the way a real parsed
/// source file's does. `find_all_names_scoped_to_container`'s primary
/// range-containment path therefore always misses for a JAR class, falling
/// through to `find_name_in_uri_after_line` -- which, before this fix,
/// returned only the ONE closest same-named entry after the class's own
/// line, silently dropping every other overload. A real call site to any
/// overload OTHER than that one arbitrary pick always failed arity-based
/// shape filtering downstream.
#[test]
fn find_definition_qualified_returns_all_jar_overload_candidates() {
    let idx = Indexer::new();
    let compiled = vec![
        jar_sidecar_symbol(
            "Assert",
            "class",
            "",
            "class org.junit.Assert",
            "org.junit",
            false,
        ),
        // Deliberately list the 1-arg overload FIRST, matching real JUnit
        // source order (`fail(String)` declared before `fail()`) -- the old
        // "closest line after the class" fallback would pick this one and
        // permanently shadow the 0-arg overload.
        jar_sidecar_symbol(
            "fail",
            "fun",
            "Assert",
            "fun fail(p0: String)",
            "org.junit",
            false,
        ),
        jar_sidecar_symbol("fail", "fun", "Assert", "fun fail()", "org.junit", false),
        jar_sidecar_symbol(
            "assertEquals",
            "fun",
            "Assert",
            "fun assertEquals(p0: Object, p1: Object, p2: Object)",
            "org.junit",
            false,
        ),
        jar_sidecar_symbol(
            "assertEquals",
            "fun",
            "Assert",
            "fun assertEquals(p0: Object, p1: Object)",
            "org.junit",
            false,
        ),
    ];
    crate::indexer::jar::populate_from_symbols(
        &idx,
        "/home/test/.gradle/caches/junit-4.13.2.jar".as_ref(),
        &compiled,
    );

    let use_uri = Url::parse("file:///app/SomeTest.kt").unwrap();
    idx.index_content(
        &use_uri,
        concat!(
            "package app\n",
            "import org.junit.Assert\n",
            "class SomeTest {\n",
            "    fun t() { Assert.fail() }\n",
            "}\n",
        ),
    );

    let fail_locs = idx.find_definition_qualified_index_only("fail", Some("Assert"), &use_uri);
    assert_eq!(
        fail_locs.len(),
        2,
        "must return both fail() overloads from the JAR class, not collapse \
         to the single closest-line pick; got {:?}",
        fail_locs
    );

    let assert_eq_locs =
        idx.find_definition_qualified_index_only("assertEquals", Some("Assert"), &use_uri);
    assert_eq!(
        assert_eq_locs.len(),
        2,
        "must return both assertEquals overloads from the JAR class; got {:?}",
        assert_eq_locs
    );
}

/// Primitive D (member-extension visibility): `fun Modifier.weight(...)`
/// declared as a MEMBER of `interface ColumnScope` — the real Compose
/// `ColumnScope`/`Modifier.weight` shape — must resolve via goto-definition
/// even though the caller file imports nothing from `ColumnScope`'s package.
/// Kotlin has no import syntax for a member of an interface, so real call
/// sites never import `weight`; `extension_is_in_scope` must not reject it
/// on the ordinary top-level-extension import rule.
#[test]
fn member_extension_function_resolves_without_import() {
    let idx = Indexer::new();
    let scope_uri = Url::parse("file:///compose/ColumnScope.kt").unwrap();
    idx.index_content(
        &scope_uri,
        concat!(
            "package androidx.compose.foundation.layout\n",
            "interface ColumnScope {\n",
            "    fun Modifier.weight(weight: Float): Modifier\n",
            "}\n",
        ),
    );

    // No `import androidx.compose.foundation.layout.ColumnScope` (and Kotlin has
    // no per-member import syntax for it anyway) — a real Compose call site.
    let use_uri = Url::parse("file:///app/Screen.kt").unwrap();
    idx.index_content(
        &use_uri,
        concat!(
            "package app\n",
            "fun screen() {\n",
            "    Column {\n",
            "        Modifier.weight(1f)\n",
            "    }\n",
            "}\n",
        ),
    );

    // The declaration's own `selection_range`, read directly off the index —
    // the expected value the goto-definition `Location` below must match.
    // Asserting only the URI (as this test originally did) let a
    // `Range::default()` fallback — pointing at the right file but the wrong
    // spot — slip through undetected.
    let expected_range = idx
        .files
        .get(scope_uri.as_str())
        .and_then(|fd| {
            fd.symbols
                .iter()
                .find(|s| s.name == "weight")
                .map(|s| s.selection_range)
        })
        .expect("weight must be indexed in ColumnScope.kt");

    let locs = idx.find_definition_qualified("weight", Some("Modifier"), &use_uri);
    let matched = locs.iter().find(|l| l.uri == scope_uri);
    assert!(
        matched.is_some(),
        "Modifier.weight(...) must resolve to ColumnScope's member extension \\
         declaration despite the caller file importing nothing from its package; \\
         got {:?}",
        locs.iter().map(|l| l.uri.as_str()).collect::<Vec<_>>()
    );
    assert_eq!(
        matched.unwrap().range,
        expected_range,
        "goto-definition must point at weight's actual declaration range, not a \\
         Range::default() fallback from a container.is_none() lookup mismatch"
    );
}

/// Primitive D follow-up: the member-extension short-circuit in
/// `extension_is_in_scope` unconditionally returns `true` whenever
/// `entry_container.is_some()`, without ever consulting `ExtensionEntry::visibility`
/// — so a `private fun Modifier.weight(...)` declared as a member of
/// `ColumnScope` would be treated as in scope for a completely unrelated
/// file/package that has no business seeing it. Kotlin's `private`/`protected`
/// access rules are an axis entirely separate from the "no live dispatch-receiver
/// tracking" trade-off Primitive D's own design intentionally accepted.
///
/// Tested directly against `extension_is_in_scope`, not through
/// `find_definition_qualified`, for the same reason given on the decoy test
/// below: `resolve_qualified` has its own separate, pre-existing, unrelated
/// fallback for a receiver type with no indexed class declaration (matches the
/// first same-named extension entry with no scope check of any kind) that
/// would otherwise mask which mechanism actually rejected the call.
#[test]
fn private_member_extension_in_another_file_is_out_of_scope() {
    use crate::resolver::infer::extension_is_in_scope;
    use crate::types::{FileData, Visibility};

    let caller_file_data = FileData {
        package: Some("app".to_string()),
        ..Default::default()
    };
    let extension_package = "androidx.compose.foundation.layout".to_string();
    let container = "ColumnScope".to_string();

    assert!(
        !extension_is_in_scope(
            Some(&extension_package),
            "weight",
            Some(&container),
            Visibility::Private,
            false, // caller is not the declaring file
            Some(&caller_file_data),
        ),
        "a private member extension must not be in scope for an unrelated caller \\
         in a different file"
    );
}

/// The same-file counterpart to the test above: Kotlin's own visibility rules
/// permit access to a `private` declaration from within its own file, so
/// `is_same_file: true` must still return `true` here — the fix must not
/// blanket-reject every private member extension regardless of caller.
#[test]
fn private_member_extension_in_same_file_is_in_scope() {
    use crate::resolver::infer::extension_is_in_scope;
    use crate::types::{FileData, Visibility};

    let caller_file_data = FileData {
        package: Some("androidx.compose.foundation.layout".to_string()),
        ..Default::default()
    };
    let extension_package = "androidx.compose.foundation.layout".to_string();
    let container = "ColumnScope".to_string();

    assert!(
        extension_is_in_scope(
            Some(&extension_package),
            "weight",
            Some(&container),
            Visibility::Private,
            true, // caller is the declaring file
            Some(&caller_file_data),
        ),
        "a private member extension must remain in scope from its own declaring file"
    );
}

/// Decoy 1 (regression guard, highest-value test in this primitive):
/// an ORDINARY top-level extension function (`container == None`) declared
/// in an unimported package must still be rejected by `extension_is_in_scope`
/// exactly as before this primitive — `extension_is_in_scope` is shared by
/// every extension-function lookup in the codebase, not just member
/// extensions, so this proves the new member-extension short-circuit does not
/// accidentally widen visibility for the ordinary case.
///
/// Tests `extension_is_in_scope` directly rather than through
/// `find_definition_qualified`: `resolve_qualified` (`resolver/resolve.rs`)
/// has its own pre-existing, unrelated fallback for a receiver type with no
/// indexed class declaration (matches the first same-named extension entry
/// with no scope check at all) — a separate, already-existing gap this
/// primitive does not touch. Routing this decoy through the full
/// goto-definition pipeline would risk conflating that unrelated gap with
/// the specific regression this decoy is meant to guard.
#[test]
fn top_level_extension_function_in_unimported_package_still_out_of_scope() {
    use crate::resolver::infer::extension_is_in_scope;
    use crate::types::FileData;

    let caller_file_data = FileData {
        package: Some("app".to_string()),
        ..Default::default()
    };
    let extension_package = "com.example.lib".to_string();

    assert!(
        !extension_is_in_scope(
            Some(&extension_package),
            "myExtension",
            None, // ordinary top-level extension: container == None
            crate::types::Visibility::Public,
            false,
            Some(&caller_file_data),
        ),
        "an ordinary top-level extension in an unimported package must remain \\
         out of scope"
    );
}

/// The direct, pipeline-independent counterpart to the two tests above: a
/// MEMBER extension (`entry_container` is `Some`) in a package the caller
/// neither shares nor imports must still be in scope per the D2 rule. Tested
/// directly against `extension_is_in_scope` (not through
/// `find_definition_qualified`) for the same reason given on the decoy test
/// above — this isolates the D2 short-circuit itself from the unrelated
/// pipeline fallback.
#[test]
fn member_extension_in_unimported_package_is_in_scope_via_container_short_circuit() {
    use crate::resolver::infer::extension_is_in_scope;
    use crate::types::FileData;

    let caller_file_data = FileData {
        package: Some("app".to_string()),
        ..Default::default()
    };
    let extension_package = "androidx.compose.foundation.layout".to_string();
    let container = "ColumnScope".to_string();

    assert!(
        extension_is_in_scope(
            Some(&extension_package),
            "weight",
            Some(&container),
            crate::types::Visibility::Public,
            false,
            Some(&caller_file_data),
        ),
        "a member extension must be in scope regardless of package/import \\
         coverage — Kotlin has no import syntax for a member of an interface"
    );
}

/// Decoy 2 (documented pre-existing limitation, not a regression): two
/// unrelated interfaces both declaring a member extension named `weight` on
/// the same receiver type. This primitive inherits the same "first match
/// wins, no overload-set semantics" limitation `resolve_extension_in_scope`
/// already has for ordinary top-level extensions — not a new gap.
#[test]
fn ambiguous_member_extension_name_collision_first_match_wins() {
    let idx = Indexer::new();
    let first_uri = Url::parse("file:///compose/ColumnScope.kt").unwrap();
    idx.index_content(
        &first_uri,
        concat!(
            "package androidx.compose.foundation.layout\n",
            "interface ColumnScope {\n",
            "    fun Modifier.weight(weight: Float): Modifier\n",
            "}\n",
        ),
    );
    let second_uri = Url::parse("file:///lib/UnrelatedScope.kt").unwrap();
    idx.index_content(
        &second_uri,
        concat!(
            "package com.example.lib\n",
            "interface UnrelatedScope {\n",
            "    fun Modifier.weight(weight: Float): Modifier\n",
            "}\n",
        ),
    );

    let use_uri = Url::parse("file:///app/Screen.kt").unwrap();
    idx.index_content(
        &use_uri,
        concat!(
            "package app\n",
            "fun screen() {\n",
            "    Modifier.weight(1f)\n",
            "}\n",
        ),
    );

    let locs = idx.find_definition_qualified("weight", Some("Modifier"), &use_uri);
    assert_eq!(
        locs.len(),
        1,
        "colliding same-named member extensions resolve to a single first \\
         match, not an overload set -- a pre-existing, unchanged limitation; \\
         got {:?}",
        locs.iter().map(|l| l.uri.as_str()).collect::<Vec<_>>()
    );
}

/// `import androidx.compose.runtime.remember` must resolve to the compose
/// top-level `remember`, not a same-named symbol in an unrelated jar (the Kotlin
/// compiler / gradle plugin / KSP all ship a `remember`). Driven by the sidecar's
/// real per-symbol package (`jar_symbol_packages`) + the corrected top-level FQN.
#[test]
fn import_resolves_jar_symbol_to_correct_package() {
    use crate::sidecar::SidecarSymbol;

    let mk = |name: &str, container: &str, pkg: &str, top_level: bool| SidecarSymbol {
        name: name.into(),
        kind: "fun".into(),
        container: container.into(),
        detail: format!("fun {name}()"),
        doc: String::new(),
        type_params: vec![],
        extension_receiver_type: String::new(),
        trailing_lambda: false,
        deprecated: false,
        pkg: pkg.into(),
        top_level,
        supers: vec![],
    };

    let idx = Indexer::new();
    crate::indexer::jar::populate_from_symbols(
        &idx,
        std::path::Path::new("/fake/runtime.jar"),
        &[mk(
            "remember",
            "ComposablesKt",
            "androidx.compose.runtime",
            true,
        )],
    );
    crate::indexer::jar::populate_from_symbols(
        &idx,
        std::path::Path::new("/fake/kotlin-compiler.jar"),
        &[mk(
            "remember",
            "VariableStorage",
            "org.jetbrains.kotlin.fir",
            false,
        )],
    );

    let caller = Url::parse("file:///app/Foo.kt").unwrap();
    idx.index_content(
        &caller,
        "package app\nimport androidx.compose.runtime.remember\n",
    );
    let locs = resolve_symbol(&idx, "remember", None, &caller);

    assert!(!locs.is_empty(), "remember should resolve");
    assert!(
        locs.iter().all(|l| l.uri.as_str().contains("runtime.jar")),
        "must resolve only to the compose-runtime jar, got: {:?}",
        locs.iter().map(|l| l.uri.as_str()).collect::<Vec<_>>()
    );
}

/// Two sealed classes with identically-named members inside one interface
/// (the MVI `Contract` pattern: `State.Idle` and `Event.Idle`). An explicit
/// nested-class import (`Contract.State.Idle`) must resolve go-to-definition to
/// the member of the *imported* enclosing type only — not both. The container
/// segment in the import (`State`) disambiguates same-package members sharing a
/// short name. Regression guard: the per-symbol-JAR-package rewrite dropped this
/// container filter, so go-def returned both `Idle` objects.
#[test]
fn import_disambiguates_same_package_nested_member_by_container() {
    let idx = Indexer::new();
    let contract = uri("/Contract.kt");
    let use_uri = uri("/ui/Use.kt");
    idx.index_content(
        &contract,
        "package com.app\ninterface Contract {\n  sealed class State {\n    object Idle : State()\n  }\n  sealed class Event {\n    object Idle : Event()\n  }\n}",
    );
    idx.index_content(
        &use_uri,
        "package com.app.ui\nimport com.app.Contract.State.Idle\nval x = Idle",
    );
    let locs = resolve_symbol(&idx, "Idle", None, &use_uri);
    assert_eq!(
        locs.len(),
        1,
        "expected only State.Idle; got {:?}",
        locs.iter().map(|l| l.range.start.line).collect::<Vec<_>>()
    );
    // `State.Idle` is the object on line 3 (0-indexed); `Event.Idle` is line 6.
    assert_eq!(locs[0].range.start.line, 3);
}

/// Deeply-nested variant: the disambiguating container differs *above* the
/// immediate parent. `Contract.State.Sub.Idle` and `Contract.Event.Sub.Idle`
/// share the immediate container `Sub`, so only a full enclosing-chain match
/// resolves the import to the right one.
#[test]
fn import_disambiguates_deeply_nested_member_by_full_chain() {
    let idx = Indexer::new();
    let contract = uri("/Contract.kt");
    let use_uri = uri("/ui/Use.kt");
    idx.index_content(
        &contract,
        "package com.app\n\
         interface Contract {\n\
         \x20 sealed class State {\n\
         \x20   sealed class Sub {\n\
         \x20     object Idle : Sub()\n\
         \x20   }\n\
         \x20 }\n\
         \x20 sealed class Event {\n\
         \x20   sealed class Sub {\n\
         \x20     object Idle : Sub()\n\
         \x20   }\n\
         \x20 }\n\
         }",
    );
    idx.index_content(
        &use_uri,
        "package com.app.ui\nimport com.app.Contract.State.Sub.Idle\nval x = Idle",
    );
    let locs = resolve_symbol(&idx, "Idle", None, &use_uri);
    assert_eq!(
        locs.len(),
        1,
        "expected only State.Sub.Idle; got {:?}",
        locs.iter().map(|l| l.range.start.line).collect::<Vec<_>>()
    );
    // State.Sub.Idle is the object on line 4 (0-indexed); Event.Sub.Idle is line 9.
    assert_eq!(locs[0].range.start.line, 4);
}

/// IndexOnly path (diagnostics / `fill_when`, via `resolve_type_index_only`) must
/// disambiguate nested sealed-class members exactly like navigation does. Two
/// sibling sealed classes expose identically-named members one level below a
/// shared immediate container (`State.Sub.Idle` vs `Event.Sub.Idle`, both nested
/// in a `Sub`); only a *whole enclosing-chain* match resolves the import to the
/// right one. The retired index-only import clone compared only the immediate
/// parent (`Sub`), so it kept both — this guards the unified chain routing
/// IndexOnly through the rich `resolve_via_imports(.., allow_fd=false)`.
#[test]
fn index_only_import_disambiguates_deeply_nested_member_by_full_chain() {
    let idx = Indexer::new();
    let contract = uri("/Contract.kt");
    let use_uri = uri("/ui/Use.kt");
    idx.index_content(
        &contract,
        "package com.app\n\
         interface Contract {\n\
         \x20 sealed class State {\n\
         \x20   sealed class Sub {\n\
         \x20     object Idle : Sub()\n\
         \x20   }\n\
         \x20 }\n\
         \x20 sealed class Event {\n\
         \x20   sealed class Sub {\n\
         \x20     object Idle : Sub()\n\
         \x20   }\n\
         \x20 }\n\
         }",
    );
    idx.index_content(
        &use_uri,
        "package com.app.ui\nimport com.app.Contract.State.Sub.Idle\nval x = Idle",
    );
    let locs = resolve::resolve_type_index_only(&idx, "Idle", &use_uri);
    assert_eq!(
        locs.len(),
        1,
        "expected only State.Sub.Idle; got {:?}",
        locs.iter().map(|l| l.range.start.line).collect::<Vec<_>>()
    );
    // State.Sub.Idle is the object on line 4 (0-indexed); Event.Sub.Idle is line 9.
    assert_eq!(locs[0].range.start.line, 4);
}

#[test]
fn jar_to_jar_supertype_walk_resolves_inherited_member() {
    // Base + its member live in one JAR; Child (which extends Base via `supers`)
    // lives in a *different* JAR. A member inherited Child→Base is only reachable
    // by following the JAR class's recorded supertypes across the JAR boundary.
    let sym = |name: &str, kind: &str, container: &str, supers: Vec<String>| {
        crate::sidecar::SidecarSymbol {
            name: name.into(),
            kind: kind.into(),
            container: container.into(),
            detail: format!("{kind} {name}"),
            doc: String::new(),
            type_params: vec![],
            extension_receiver_type: String::new(),
            trailing_lambda: false,
            deprecated: false,
            pkg: "lib".into(),
            top_level: container.is_empty(),
            supers,
        }
    };
    let idx = Indexer::new();
    crate::indexer::jar::populate_from_symbols(
        &idx,
        std::path::Path::new("/fake/base.jar"),
        &[
            sym("Base", "class", "", vec![]),
            sym("baseMethod", "fun", "Base", vec![]),
        ],
    );
    crate::indexer::jar::populate_from_symbols(
        &idx,
        std::path::Path::new("/fake/child.jar"),
        &[sym("Child", "class", "", vec!["Base".to_owned()])],
    );

    let file = uri("/ws/Widget.kt");
    idx.index_content(
        &file,
        "package ws\nclass Widget : Child {\n  fun use() { baseMethod() }\n}",
    );

    let locs = resolve_symbol(&idx, "baseMethod", None, &file);
    assert!(
        locs.iter().any(|l| l.uri.as_str().contains("base.jar")),
        "baseMethod must resolve via the JAR→JAR supertype walk (Widget → Child → Base); got {locs:?}"
    );
}

/// Mirrors `jar_declaration_scope_finds_a_tier1_only_symbol_after_promotion_attempt`
/// (`indexer/lookup_tests.rs`) and `get_definitions_attempts_promotion_for_a_tier1_only_symbol`
/// (`indexer/resolution_tests.rs`): no real sidecar is available in a unit
/// test, so this pins the CONTRACT that `complete_bare`'s cross-package
/// collection attempts `ensure_jar_materialized` for a Tier-1-only candidate
/// that matches the completion prefix (observable via
/// `materialization_failed` being populated for the fake jar path) rather
/// than only ever offering the name-only stub from the `rebuild_bare_name_cache`
/// Tier-1 merge.
#[test]
fn complete_bare_attempts_promotion_for_a_tier1_only_candidate() {
    let idx = Indexer::new();
    let cur_uri = uri("/project/Screen.kt");
    idx.index_content(&cur_uri, "package com.example\n");

    let jar_id = idx.jar_table.intern("/nonexistent/fixture.jar");
    idx.jar_bare_names
        .entry("LazyLibType".to_owned())
        .or_default()
        .push(jar_id);

    let (items, _) = complete_bare(&idx, "LazyLib", &cur_uri, false, false, None);
    assert!(
        items.iter().any(|i| i.label == "LazyLibType"),
        "a Tier-1-only candidate must still be offered by name even when \
         promotion fails; got {items:?}"
    );
    assert!(
        idx.materialization_failed.contains(&jar_id),
        "complete_bare must attempt promotion for a Tier-1-only candidate \
         that matches the completion prefix, not just read jar_bare_names \
         for the name-only stub"
    );
}

/// Task 12 review finding 2: a single completion request must not attempt an
/// unbounded number of synchronous, blocking `ensure_jar_materialized` calls
/// — each one is a real sidecar IPC round trip, and a short/ambiguous prefix
/// can match many Tier-1-only candidates at once (measured against a real
/// ~756-JAR Gradle cache: ~17 sequential promotions, ~20s for one completion
/// response). Seeds more Tier-1-only candidates than the promotion cap, all
/// matching the same prefix, each backed by a distinct nonexistent JAR path
/// (so every attempted promotion fails and is recorded in
/// `materialization_failed` — the observable signal for "an attempt was
/// made"). Asserts the number of attempted promotions is bounded, while every
/// matched candidate is still offered by name (Task 9's Tier-1 merge already
/// guarantees this independent of promotion).
#[test]
fn complete_bare_bounds_synchronous_promotion_attempts_per_request() {
    let idx = Indexer::new();
    let cur_uri = uri("/project/Screen.kt");
    idx.index_content(&cur_uri, "package com.example\n");

    const CANDIDATE_COUNT: usize = MAX_SYNC_JAR_PROMOTIONS_PER_COMPLETION + 3;
    let mut jar_ids = Vec::with_capacity(CANDIDATE_COUNT);
    for i in 0..CANDIDATE_COUNT {
        let jar_id = idx
            .jar_table
            .intern(&format!("/nonexistent/fixture{i}.jar"));
        idx.jar_bare_names
            .entry(format!("LazyLibType{i}"))
            .or_default()
            .push(jar_id);
        jar_ids.push(jar_id);
    }

    let (items, _) = complete_bare(&idx, "LazyLib", &cur_uri, false, false, None);

    for i in 0..CANDIDATE_COUNT {
        let label = format!("LazyLibType{i}");
        assert!(
            items.iter().any(|item| item.label == label),
            "every Tier-1-only candidate matching the prefix must still be \
             offered by name, promoted or not; missing {label} — got {items:?}"
        );
    }

    let attempted = jar_ids
        .iter()
        .filter(|id| idx.materialization_failed.contains(id))
        .count();
    assert!(
        attempted <= MAX_SYNC_JAR_PROMOTIONS_PER_COMPLETION,
        "complete_bare must cap synchronous promotion attempts at {} per \
         request; {attempted} of {CANDIDATE_COUNT} candidates were attempted",
        MAX_SYNC_JAR_PROMOTIONS_PER_COMPLETION
    );
}

/// A common receiver type (e.g. "String") can be declared on by many
/// library JARs — `jar_extension_receivers[receiver]` fanning out to more
/// candidates than a single completion request should pay a blocking
/// sidecar round trip for. Review finding on the extension-completion fix:
/// without a cap, `extension_fn_completions` could reintroduce the same
/// cold-start stall `complete_bare_bounds_synchronous_promotion_attempts_per_request`
/// above already guards against for cross-package completion.
#[test]
fn extension_completion_bounds_synchronous_promotion_attempts_per_request() {
    let idx = Indexer::new();
    idx.index_content(
        &Url::parse("file:///sdk/Widget.kt").unwrap(),
        "package sdk\nopen class Widget",
    );

    const CANDIDATE_COUNT: usize = MAX_SYNC_JAR_PROMOTIONS_PER_COMPLETION + 3;
    let mut jar_ids = Vec::with_capacity(CANDIDATE_COUNT);
    for i in 0..CANDIDATE_COUNT {
        let jar_id = idx
            .jar_table
            .intern(&format!("/nonexistent/ext-fixture{i}.jar"));
        // All CANDIDATE_COUNT JARs declare an extension on the SAME
        // receiver — matching the real "String"/"Iterable" fan-out
        // scenario, not CANDIDATE_COUNT distinct receivers.
        idx.jar_extension_receivers
            .entry("Widget".to_owned())
            .or_default()
            .push(jar_id);
        jar_ids.push(jar_id);
    }

    let app_uri = Url::parse("file:///app/Screen.kt").unwrap();
    idx.index_content(
        &app_uri,
        concat!(
            "package app\n",
            "import sdk.Widget\n",
            "class Screen : Widget() {\n",
            "    fun load() { toString() }\n",
            "}\n",
        ),
    );

    let _ = complete_dot(&idx, "Widget", &app_uri, true, None);

    let attempted = jar_ids
        .iter()
        .filter(|id| idx.materialization_failed.contains(id))
        .count();
    assert!(
        attempted <= MAX_SYNC_JAR_PROMOTIONS_PER_COMPLETION,
        "extension_fn_completions must cap synchronous promotion attempts \
         at {} per request across ALL ancestors, even when they all fan \
         out to the same receiver; {attempted} of {CANDIDATE_COUNT} \
         candidates were attempted",
        MAX_SYNC_JAR_PROMOTIONS_PER_COMPLETION
    );
}

/// Review finding on the post-ship fix wave: `supertype_targets` promoted
/// each super-class name with the UNBUDGETED `ensure_jar_materialized`, and
/// the hierarchy walk runs on paths with no budget of their own — inference
/// (`resolve_from_class_hierarchy`, depth 12; `find_extension_property_type`,
/// depth 8 — both fanned out per name by inlay hints) and bare completion's
/// inherited-members collector. Every distinct un-cached ancestor JAR paid a
/// blocking sidecar round trip with no per-walk ceiling — the same cold-start
/// stall pathology the completion caps exist for, reachable around them.
/// Seeds a source-resolvable inheritance chain where every super name ALSO
/// collides with a Tier-1-only candidate backed by a nonexistent JAR (so
/// every attempt fails observably into `materialization_failed`), and
/// asserts one walk's attempts are bounded.
#[test]
fn hierarchy_walk_bounds_synchronous_promotion_attempts_per_walk() {
    // XDG isolation: the promotion probe lazily decodes the on-disk jar
    // cache — without this the test reads the developer's real one.
    let tmp = tempfile::tempdir().expect("tempdir");
    crate::indexer::test_helpers::with_xdg_cache(tmp.path(), || {
        let idx = Indexer::new();

        const CHAIN_LENGTH: usize =
            crate::resolver::hierarchy::MAX_SYNC_JAR_PROMOTIONS_PER_HIERARCHY_WALK + 3;
        let mut jar_ids = Vec::with_capacity(CHAIN_LENGTH);
        for i in 0..CHAIN_LENGTH {
            let class_uri = Url::parse(&format!("file:///sdk/Base{i}.kt")).unwrap();
            let parent = i + 1;
            let content = if i + 1 < CHAIN_LENGTH {
                format!("package sdk\nopen class Base{i} : Base{parent}()")
            } else {
                format!("package sdk\nopen class Base{i}")
            };
            idx.index_content(&class_uri, &content);
            // Each super name in the chain also has a cold compiled-JAR
            // candidate — the promotion gate passes, and an unbudgeted walk
            // would attempt every single one.
            let jar_id = idx
                .jar_table
                .intern(&format!("/nonexistent/hierarchy-fixture{i}.jar"));
            idx.jar_bare_names
                .entry(format!("Base{i}"))
                .or_default()
                .push(jar_id);
            jar_ids.push(jar_id);
        }

        let _ = crate::resolver::walk_hierarchy(
            &idx,
            "Base0",
            "file:///sdk/Base0.kt",
            crate::types::CallerContext::default(),
            CHAIN_LENGTH,
            crate::resolver::MAX_SYNC_JAR_PROMOTIONS_PER_HIERARCHY_WALK,
            |_, _, _, _| Vec::<()>::new(),
        );

        let attempted = jar_ids
            .iter()
            .filter(|id| idx.materialization_failed.contains(id))
            .count();
        assert!(
            attempted <= crate::resolver::hierarchy::MAX_SYNC_JAR_PROMOTIONS_PER_HIERARCHY_WALK,
            "one hierarchy walk must cap synchronous promotion attempts at {}; \
         {attempted} of {CHAIN_LENGTH} cold ancestors were attempted",
            crate::resolver::hierarchy::MAX_SYNC_JAR_PROMOTIONS_PER_HIERARCHY_WALK
        );
    });
}

/// A Tier-1-only candidate whose promotion to Tier 2 has already succeeded
/// (simulated here — the decoy test above already pins the *attempt* against
/// a nonexistent JAR, which always fails in a unit test with no sidecar) must
/// have its completion item's `detail` built from the real materialized
/// signature, not the import-qualifier-only stub. Seeds `jar_definitions` /
/// `jar_files` / `jar_symbol_packages` via `populate_from_symbols` — the same
/// production path Tier-2 materialization uses — so this pins the contract
/// against the real data shape, not an invented one.
/// Shared fixture: `LazyLibType`, a materialized (Tier-2) jar class in
/// package `com.fake.lib`, plus a workspace file in another package.
/// Seeds `jar_definitions`/`jar_files`/`jar_symbol_packages` via
/// `populate_from_symbols` — the same production path Tier-2
/// materialization uses — so the tests pin the contract against the real
/// data shape, not an invented one.
fn materialized_lazylib_fixture() -> (Indexer, tower_lsp::lsp_types::Url) {
    use crate::sidecar::SidecarSymbol;

    let idx = Indexer::new();
    let cur_uri = uri("/project/Screen.kt");
    idx.index_content(&cur_uri, "package com.example\n");

    // Tier 1: manifest-time registration (Task 6), as `build_jar_manifest`
    // would produce for a symbol whose manifest entry carries a package.
    let jar_id = idx.jar_table.intern("/fake/lib.jar");
    idx.jar_bare_names
        .entry("LazyLibType".to_owned())
        .or_default()
        .push(jar_id);
    idx.jar_qualified
        .insert("com.fake.lib.LazyLibType".to_owned(), jar_id);

    // Tier 2: materialized data, as a successful `ensure_jar_materialized`
    // would produce.
    crate::indexer::jar::populate_from_symbols(
        &idx,
        std::path::Path::new("/fake/lib.jar"),
        &[SidecarSymbol {
            name: "LazyLibType".into(),
            kind: "class".into(),
            container: String::new(),
            detail: "class com.fake.lib.LazyLibType".into(),
            doc: "Real doc comment.".into(),
            type_params: vec![],
            extension_receiver_type: String::new(),
            trailing_lambda: false,
            deprecated: false,
            pkg: "com.fake.lib".into(),
            top_level: true,
            supers: vec![],
        }],
    );
    (idx, cur_uri)
}

#[test]
fn add_cross_package_symbol_uses_real_detail_for_a_promoted_candidate() {
    let (idx, cur_uri) = materialized_lazylib_fixture();
    idx.client_label_details_support
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let (items, _) = complete_bare(&idx, "LazyLib", &cur_uri, false, false, None);
    let item = items
        .iter()
        .find(|i| i.label == "LazyLibType")
        .unwrap_or_else(|| panic!("LazyLibType must be offered; got {items:?}"));
    assert_eq!(
        item.detail.as_deref(),
        Some("class com.fake.lib.LazyLibType"),
        "a candidate backed by an already-materialized JAR symbol must show \
         its real signature as `detail`, not just the import qualifier stub \
         (`com.fake.lib`); got {:?}",
        item.detail
    );
    assert!(
        item.data.is_some(),
        "a materialized candidate should also carry resolve-time data so \
         completionItem/resolve can enrich its documentation, matching the \
         Tier 0/1 pattern in collect_local_file/collect_same_package"
    );
    // The user-visible regression behind the labelDetails work: once `detail`
    // becomes the real signature, the package must survive somewhere the
    // completion LIST renders — five materialized `Modifier`s were
    // indistinguishable.
    assert_eq!(
        item.label_details
            .as_ref()
            .and_then(|ld| ld.description.as_deref()),
        Some("com.fake.lib"),
        "a materialized candidate must keep its package visible via \
         labelDetails.description alongside the signature `detail`"
    );
}

/// Clients that never render `labelDetails` (Helix's menu is label + kind
/// only, and it doesn't advertise `labelDetailsSupport`) must still get the
/// package for a materialized candidate — folded into `detail` as a
/// Kotlin-style header line, which such clients DO render in their doc
/// popup. This is the exact live report: "not seeing the package when
/// javadoc is present, only when it's missing."
#[test]
fn package_folds_into_detail_when_client_lacks_label_details() {
    let (idx, cur_uri) = materialized_lazylib_fixture();
    // Default flag state: no labelDetailsSupport advertised.

    let (items, _) = complete_bare(&idx, "LazyLib", &cur_uri, false, false, None);
    let item = items
        .iter()
        .find(|i| i.label == "LazyLibType")
        .unwrap_or_else(|| panic!("LazyLibType must be offered; got {items:?}"));
    assert_eq!(
        item.detail.as_deref(),
        Some("package com.fake.lib\nclass com.fake.lib.LazyLibType"),
        "without labelDetailsSupport the package must be folded into `detail`"
    );
    assert!(
        item.label_details.is_none(),
        "labelDetails must not be sent to a client that didn't advertise \
         support for it"
    );
}

/// `completionItem/resolve` re-derives `detail` from the enriched signature
/// — it must PRESERVE the folded `package …` header line, or the hint
/// vanishes the moment the client resolves the selected item (Helix
/// advertises `resolve_support: [detail]` and applies the resolved value).
#[test]
fn resolve_preserves_folded_package_line_in_detail() {
    let (idx, cur_uri) = materialized_lazylib_fixture();

    let (items, _) = complete_bare(&idx, "LazyLib", &cur_uri, false, false, None);
    let item = items
        .iter()
        .find(|i| i.label == "LazyLibType")
        .unwrap_or_else(|| panic!("LazyLibType must be offered; got {items:?}"))
        .clone();

    let resolved = crate::features::completion::resolve_completion_item(item, &idx);
    let detail = resolved.detail.as_deref().unwrap_or_default();
    assert!(
        detail.starts_with("package com.fake.lib\n"),
        "resolve must keep the folded package header line; got {detail:?}"
    );
    assert!(
        detail.contains("class com.fake.lib.LazyLibType"),
        "resolve must still carry the signature; got {detail:?}"
    );
}

/// A Tier-1-only (unmaterialized) candidate is served as a stub — package
/// `detail`, no signature, no docs. The stub must carry its FQN in `data`
/// so `completionItem/resolve` can materialize it on demand (the LSP-
/// intended lazy path: the user has SELECTED this one item, so the cost is
/// one candidate, not a list-wide fan-out). Live report this pins: "package
/// is there but not signature nor docs" — the selected stub had no data, so
/// resolve was a silent no-op.
#[test]
fn stub_cross_package_item_carries_fqn_data_for_resolve() {
    let idx = Indexer::new();
    let cur_uri = uri("/project/Screen.kt");
    idx.index_content(&cur_uri, "package com.example\n");

    // Tier 1 only — never materialized.
    let jar_id = idx.jar_table.intern("/fake/lib.jar");
    idx.jar_bare_names
        .entry("LazyLibType".to_owned())
        .or_default()
        .push(jar_id);
    idx.jar_qualified
        .insert("com.fake.lib.LazyLibType".to_owned(), jar_id);

    let (items, _) = complete_bare(&idx, "LazyLib", &cur_uri, false, false, None);
    let item = items
        .iter()
        .find(|i| i.label == "LazyLibType")
        .unwrap_or_else(|| panic!("LazyLibType must be offered; got {items:?}"));
    assert_eq!(
        item.data
            .as_ref()
            .and_then(|d| d.get(crate::resolver::complete::DATA_FQN))
            .and_then(|v| v.as_str()),
        Some("com.fake.lib.LazyLibType"),
        "a stub candidate must carry its FQN so resolve can materialize it"
    );
}

/// End-to-end for the stub-resolve path: with a FRESH on-disk jar-symbol
/// cache (materialization is pure in-memory, no sidecar), resolving the
/// stub must materialize the candidate and return real signature `detail`
/// (folded, no labelDetailsSupport) plus documentation.
#[test]
fn resolve_materializes_a_stub_candidate_from_cache() {
    let tmp = tempfile::tempdir().expect("tempdir");
    crate::indexer::test_helpers::with_xdg_cache(tmp.path(), || {
        let jar_path = tmp.path().join("fake-lib.jar");
        std::fs::write(&jar_path, b"fake jar bytes").expect("write fake jar");
        let jar_key = jar_path.to_string_lossy().to_string();
        let meta = std::fs::metadata(&jar_path).expect("metadata");
        let mtime = meta
            .modified()
            .expect("mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch");

        let mut entries = std::collections::HashMap::new();
        entries.insert(
            jar_key.clone(),
            crate::indexer::jar_cache::JarCacheEntry {
                mtime_secs: mtime.as_secs(),
                mtime_nanos: mtime.subsec_nanos(),
                file_size: meta.len(),
                symbols: vec![crate::sidecar::SidecarSymbol {
                    name: "LazyLibType".into(),
                    kind: "class".into(),
                    container: String::new(),
                    detail: "class com.fake.lib.LazyLibType".into(),
                    doc: "Real doc comment.".into(),
                    type_params: vec![],
                    extension_receiver_type: String::new(),
                    trailing_lambda: false,
                    deprecated: false,
                    pkg: "com.fake.lib".into(),
                    top_level: true,
                    supers: vec![],
                }],
            },
        );
        crate::indexer::jar_cache::save_jar_cache(&entries);

        let idx = Indexer::new();
        let cur_uri = uri("/project/Screen.kt");
        idx.index_content(&cur_uri, "package com.example\n");
        let jar_id = idx.jar_table.intern(&jar_key);
        idx.jar_bare_names
            .entry("LazyLibType".to_owned())
            .or_default()
            .push(jar_id);
        idx.jar_qualified
            .insert("com.fake.lib.LazyLibType".to_owned(), jar_id);

        // Simulate the served stub: FQN-only data, package detail.
        let stub = tower_lsp::lsp_types::CompletionItem {
            label: "LazyLibType".into(),
            detail: Some("com.fake.lib".into()),
            data: Some(
                serde_json::json!({crate::resolver::complete::DATA_FQN: "com.fake.lib.LazyLibType"}),
            ),
            ..Default::default()
        };

        let resolved = crate::features::completion::resolve_completion_item(stub, &idx);
        assert_eq!(
            resolved.detail.as_deref(),
            Some("package com.fake.lib\nclass com.fake.lib.LazyLibType"),
            "resolve must materialize the stub and fold package + signature"
        );
        let doc_text = match &resolved.documentation {
            Some(tower_lsp::lsp_types::Documentation::MarkupContent(mc)) => mc.value.clone(),
            other => panic!("expected markdown documentation; got {other:?}"),
        };
        assert!(
            doc_text.contains("Real doc comment."),
            "resolve must surface the doc comment; got {doc_text:?}"
        );
    });
}

/// Live reproduction of the persisting user report: bare completion of an
/// INHERITED member (`setSt` → `setState`) inside a subclass whose base
/// class comes from a compiled JAR — in BOTH variants: with parsed sources
/// data present, and compiled-only (no sources jar published).
#[test]
fn repro_bare_completion_inherited_member_compiled_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    crate::indexer::test_helpers::with_xdg_cache(tmp.path(), || {
        let jar_path = tmp.path().join("mvi-lib.jar");
        std::fs::write(&jar_path, b"fake jar bytes").expect("write fake jar");
        let jar_path_key = jar_path.to_string_lossy().to_string();
        let compiled = vec![
            crate::sidecar::SidecarSymbol {
                name: "MviViewModel".to_owned(),
                kind: "class".to_owned(),
                container: String::new(),
                detail: "class MviViewModel".to_owned(),
                doc: String::new(),
                type_params: Vec::new(),
                extension_receiver_type: String::new(),
                trailing_lambda: false,
                deprecated: false,
                pkg: "com.lib".to_owned(),
                top_level: true,
                supers: vec![],
            },
            crate::sidecar::SidecarSymbol {
                name: "setState".to_owned(),
                kind: "fun".to_owned(),
                container: "MviViewModel".to_owned(),
                detail: "fun setState(reducer: S.() -> S)".to_owned(),
                doc: String::new(),
                type_params: Vec::new(),
                extension_receiver_type: String::new(),
                trailing_lambda: false,
                deprecated: false,
                pkg: "com.lib".to_owned(),
                top_level: false,
                supers: vec![],
            },
        ];
        let entry =
            crate::indexer::jar_cache::make_cache_entry(&jar_path, compiled).expect("cache entry");
        let mut entries = std::collections::HashMap::new();
        entries.insert(jar_path_key.clone(), entry);
        crate::indexer::jar_cache::save_jar_cache(&entries);

        let idx = Indexer::new();
        let jar_id = idx.jar_table.intern(&jar_path_key);
        // Tier 1 as build_jar_manifest would produce it: BOTH names.
        for name in ["MviViewModel", "setState"] {
            idx.jar_bare_names
                .entry(name.to_owned())
                .or_default()
                .push(jar_id);
        }
        idx.jar_qualified
            .insert("com.lib.MviViewModel".to_owned(), jar_id);
        idx.jar_qualified
            .insert("com.lib.MviViewModel.setState".to_owned(), jar_id);

        let app_uri = Url::parse("file:///app/MyViewModel.kt").unwrap();
        idx.index_content(
            &app_uri,
            concat!(
                "package app\n",
                "import com.lib.MviViewModel\n",
                "class MyViewModel : MviViewModel() {\n",
                "    fun load() {\n",
                "        setSt\n",
                "    }\n",
                "}\n",
            ),
        );

        let (items, _) = complete_bare(&idx, "setSt", &app_uri, false, false, Some(4));
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            idx.materialized.contains(&jar_id),
            "precondition: the hierarchy walk must have promoted the cache-backed jar"
        );
        assert!(
            labels.contains(&"setState"),
            "bare completion of inherited setState (compiled-only base class) — got: {labels:?}"
        );

        // Dot-completion must enumerate the same inherited member via the
        // container-based branch (synthetic jar FileData has no ranges to nest).
        let dot_items = complete_dot(&idx, "MyViewModel", &app_uri, true, None);
        let dot_labels: Vec<&str> = dot_items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            dot_labels.contains(&"setState"),
            "dot completion of inherited setState (compiled-only base class) — got: {dot_labels:?}"
        );
    });
}

/// Control experiment: same scenario but EAGER (pre-flip-equivalent)
/// population via populate_from_symbols directly — did bare completion of
/// an inherited compiled-JAR member EVER work?
#[test]
fn control_bare_completion_inherited_member_eager_population() {
    let idx = Indexer::new();
    let compiled = vec![
        crate::sidecar::SidecarSymbol {
            name: "MviViewModel".to_owned(),
            kind: "class".to_owned(),
            container: String::new(),
            detail: "class MviViewModel".to_owned(),
            doc: String::new(),
            type_params: Vec::new(),
            extension_receiver_type: String::new(),
            trailing_lambda: false,
            deprecated: false,
            pkg: "com.lib".to_owned(),
            top_level: true,
            supers: vec![],
        },
        crate::sidecar::SidecarSymbol {
            name: "setState".to_owned(),
            kind: "fun".to_owned(),
            container: "MviViewModel".to_owned(),
            detail: "fun setState(reducer: S.() -> S)".to_owned(),
            doc: String::new(),
            type_params: Vec::new(),
            extension_receiver_type: String::new(),
            trailing_lambda: false,
            deprecated: false,
            pkg: "com.lib".to_owned(),
            top_level: false,
            supers: vec![],
        },
    ];
    crate::indexer::jar::populate_from_symbols(
        &idx,
        "/home/test/.gradle/caches/mvi-lib-1.0.jar".as_ref(),
        &compiled,
    );

    let app_uri = Url::parse("file:///app/MyViewModel.kt").unwrap();
    idx.index_content(
        &app_uri,
        concat!(
            "package app\n",
            "import com.lib.MviViewModel\n",
            "class MyViewModel : MviViewModel() {\n",
            "    fun load() {\n",
            "        setSt\n",
            "    }\n",
            "}\n",
        ),
    );

    let (items, _) = complete_bare(&idx, "setSt", &app_uri, false, false, Some(4));
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"setState"),
        "bare completion of inherited setState under eager population — got: {labels:?}"
    );
}

/// Compact `SidecarSymbol` builder for the jar member-enumeration tests below.
fn jar_sidecar_symbol(
    name: &str,
    kind: &str,
    container: &str,
    detail: &str,
    pkg: &str,
    deprecated: bool,
) -> crate::sidecar::SidecarSymbol {
    crate::sidecar::SidecarSymbol {
        name: name.to_owned(),
        kind: kind.to_owned(),
        container: container.to_owned(),
        detail: detail.to_owned(),
        doc: String::new(),
        type_params: Vec::new(),
        extension_receiver_type: String::new(),
        trailing_lambda: false,
        deprecated,
        pkg: pkg.to_owned(),
        top_level: container.is_empty(),
        supers: vec![],
    }
}

/// The exact wave-7 user scenario, warm variant: chained-call completion
/// `Modifier.padding().padd…` where `Modifier` and its extensions live in a
/// fully materialized compiled JAR. The chain needs `padding`'s return type
/// (extension fn on Modifier returning Modifier) to resolve the receiver of
/// the second dot. Pins the whole path: CST receiver derivation (speculative
/// marker parse → call_expression receiver) → `CstQuery::expr_type` extension
/// return type → dot completion on the resulting Modifier, offering its
/// extensions.
#[test]
fn chained_extension_call_completion_from_compiled_jar() {
    let idx = Indexer::new();
    let mut padding = jar_sidecar_symbol(
        "padding",
        "fun",
        "",
        "fun Modifier.padding(all: Dp): Modifier",
        "lib",
        false,
    );
    padding.extension_receiver_type = "Modifier".to_owned();
    let mut vertical_scroll = jar_sidecar_symbol(
        "verticalScroll",
        "fun",
        "",
        "fun Modifier.verticalScroll(state: ScrollState): Modifier",
        "lib",
        false,
    );
    vertical_scroll.extension_receiver_type = "Modifier".to_owned();
    let compiled = vec![
        jar_sidecar_symbol(
            "Modifier",
            "interface",
            "",
            "interface lib.Modifier",
            "lib",
            false,
        ),
        padding,
        vertical_scroll,
    ];
    crate::indexer::jar::populate_from_symbols(
        &idx,
        "/home/test/.gradle/caches/compose-foundation-1.0.jar".as_ref(),
        &compiled,
    );

    let app_uri = Url::parse("file:///app/Screen.kt").unwrap();
    idx.index_content(
        &app_uri,
        concat!(
            "package app\n",
            "import lib.Modifier\n",
            "import lib.padding\n",
            "import lib.verticalScroll\n",
            "fun screen() {\n",
            "    Modifier.padding().padd\n",
            "}\n",
        ),
    );

    // Cursor at the end of `.padd` on line 5 (0-based): the pipeline derives
    // the `Modifier.padding()` call receiver from the CST and resolves it.
    let (items, _) = crate::features::completion::run_completions(
        &idx,
        &app_uri,
        tower_lsp::lsp_types::Position::new(5, 27),
        false,
    );
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"padding"),
        "chained-call completion must resolve padding's return type \
         (Modifier) and offer Modifier's extensions — got: {labels:?}"
    );
}

/// Broken mid-edit state (live probe scenario C): an unclosed `if (...) {`
/// above the cursor leaves the rest of the file brace-imbalanced; the
/// speculative marker parse must still derive the `Modifier.fillMaxSize()`
/// call receiver and resolve it through the jar extension set.
#[test]
fn broken_state_chain_completion_with_unclosed_brace() {
    let idx = Indexer::new();
    let mut padding = jar_sidecar_symbol(
        "padding",
        "fun",
        "PaddingKt",
        "fun Modifier.padding(all: Dp): Modifier",
        "lib",
        false,
    );
    padding.top_level = true;
    padding.extension_receiver_type = "Modifier".to_owned();
    let mut fill_max_size = jar_sidecar_symbol(
        "fillMaxSize",
        "fun",
        "SizeKt",
        "fun Modifier.fillMaxSize(): Modifier",
        "lib",
        false,
    );
    fill_max_size.top_level = true;
    fill_max_size.extension_receiver_type = "Modifier".to_owned();
    let compiled = vec![
        jar_sidecar_symbol(
            "Modifier",
            "interface",
            "",
            "interface lib.Modifier",
            "lib",
            false,
        ),
        padding,
        fill_max_size,
    ];
    crate::indexer::jar::populate_from_symbols(
        &idx,
        "/home/test/.gradle/caches/compose-foundation-3.0.jar".as_ref(),
        &compiled,
    );
    let app_uri = Url::parse("file:///app/Screen.kt").unwrap();
    idx.index_content(
        &app_uri,
        concat!(
            "package app\n",
            "import lib.Modifier\n",
            "import lib.padding\n",
            "import lib.fillMaxSize\n",
            "fun screen(question: String) {\n",
            "    if (question != null) {\n",
            "        val z = Modifier.fillMaxSize().padd\n",
            "    other()\n",
            "    more()\n",
            "}\n",
        ),
    );
    let (items, _) = crate::features::completion::run_completions(
        &idx,
        &app_uri,
        tower_lsp::lsp_types::Position::new(6, 43),
        false,
    );
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"padding"),
        "broken-state chain completion — got: {labels:?}"
    );
}

/// The user's ACTUAL editing scenario (wave 7d, proven by a live LSP probe
/// against the real project): a MULTILINE fluent chain, completing on a
/// continuation line. The continuation line has no receiver of its own —
/// the pipeline must reconstruct the chain from the lines above and resolve
/// it exactly like the single-line form the sibling test covers.
#[test]
fn multiline_chain_completion_from_compiled_jar() {
    let idx = Indexer::new();
    let mut padding = jar_sidecar_symbol(
        "padding",
        "fun",
        "PaddingKt",
        "fun Modifier.padding(all: Dp): Modifier",
        "lib",
        false,
    );
    padding.top_level = true;
    padding.extension_receiver_type = "Modifier".to_owned();
    let mut vertical_scroll = jar_sidecar_symbol(
        "verticalScroll",
        "fun",
        "ScrollKt",
        "fun Modifier.verticalScroll(state: ScrollState): Modifier",
        "lib",
        false,
    );
    vertical_scroll.top_level = true;
    vertical_scroll.extension_receiver_type = "Modifier".to_owned();
    let mut fill_max_size = jar_sidecar_symbol(
        "fillMaxSize",
        "fun",
        "SizeKt",
        "fun Modifier.fillMaxSize(): Modifier",
        "lib",
        false,
    );
    fill_max_size.top_level = true;
    fill_max_size.extension_receiver_type = "Modifier".to_owned();
    let compiled = vec![
        jar_sidecar_symbol(
            "Modifier",
            "interface",
            "",
            "interface lib.Modifier",
            "lib",
            false,
        ),
        padding,
        vertical_scroll,
        fill_max_size,
    ];
    crate::indexer::jar::populate_from_symbols(
        &idx,
        "/home/test/.gradle/caches/compose-foundation-2.0.jar".as_ref(),
        &compiled,
    );

    let app_uri = Url::parse("file:///app/Screen.kt").unwrap();
    idx.index_content(
        &app_uri,
        concat!(
            "package app\n",
            "import lib.Modifier\n",
            "import lib.padding\n",
            "import lib.verticalScroll\n",
            "import lib.fillMaxSize\n",
            "fun screen() {\n",
            "    Column(\n",
            "        modifier = Modifier\n",
            "            .fillMaxSize()\n",
            "            .verticalScroll(rememberScrollState())\n",
            "            .padd\n",
            "    )\n",
            "}\n",
        ),
    );

    // Cursor at the end of `.padd` on the continuation line (0-based 10).
    let (items, _) = crate::features::completion::run_completions(
        &idx,
        &app_uri,
        tower_lsp::lsp_types::Position::new(10, 17),
        false,
    );
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"padding"),
        "completion on a multiline-chain continuation line must reconstruct \
         the chain from the lines above and offer Modifier's extensions — \
         got: {labels:?}"
    );
}

/// Wave-7 root cause, reproduced from the REAL foundation-layout cache
/// entry: `ExtensionEntry.package` used the per-JAR inferred package, and
/// that inference takes the first class-like symbol with a dotted detail —
/// which in foundation-layout is a package-less NESTED class detail
/// (`class ContextualFlowColumnOverflow.Visible`), so every extension in
/// the jar got package `"ContextualFlowColumnOverflow"`. The scope filter
/// then rejected the explicitly imported `padding` (real package
/// `androidx.compose.foundation.layout`), chain inference returned None,
/// and `Modifier.padding().padd…` completion came back empty. Extension
/// entries must carry the sidecar's real per-symbol `pkg`.
#[test]
fn extension_entries_carry_the_real_per_symbol_package() {
    let idx = Indexer::new();
    let mut padding = jar_sidecar_symbol(
        "padding",
        "fun",
        "PaddingKt",
        "fun Modifier.padding(all: Dp): Modifier",
        "androidx.compose.foundation.layout",
        false,
    );
    padding.top_level = true;
    padding.extension_receiver_type = "Modifier".to_owned();
    let compiled = vec![
        // A nested class whose detail has NO package prefix — the real jar's
        // first dotted class-like detail, which poisons the per-jar package
        // inference ("ContextualFlowColumnOverflow" parses as `pkg.Type`).
        jar_sidecar_symbol(
            "Visible",
            "class",
            "ContextualFlowColumnOverflow",
            "class ContextualFlowColumnOverflow.Visible",
            "androidx.compose.foundation.layout",
            false,
        ),
        padding,
    ];
    crate::indexer::jar::populate_from_symbols(
        &idx,
        "/home/test/.gradle/caches/foundation-layout-1.0.aar".as_ref(),
        &compiled,
    );

    let app_uri = Url::parse("file:///app/Screen.kt").unwrap();
    idx.index_content(
        &app_uri,
        concat!(
            "package app\n",
            "import androidx.compose.ui.Modifier\n",
            "import androidx.compose.foundation.layout.padding\n",
            "fun screen() {\n",
            "    Modifier.padding().padd\n",
            "}\n",
        ),
    );

    let return_type = crate::resolver::infer::find_extension_fn_return_type(
        &idx,
        "Modifier",
        "padding",
        Some(&app_uri),
    );
    assert_eq!(
        return_type.as_deref(),
        Some("Modifier"),
        "an explicitly imported extension must be in scope regardless of \
         what the per-jar package inference produced"
    );
}

/// Review finding on the container-based jar member enumeration: the sidecar
/// records each member's declaring class by SIMPLE name, and one synthetic
/// `FileData` spans the whole JAR — so two top-level classes with the same
/// simple name in different packages of one JAR had their members MERGED.
/// The per-symbol package side table (`jar_symbol_packages`) is index-aligned
/// with the symbols and must disambiguate: with the caller importing
/// `com.a.Foo`, only `com.a.Foo`'s members belong in the list.
#[test]
fn jar_member_enumeration_does_not_merge_same_simple_name_classes() {
    let idx = Indexer::new();
    let compiled = vec![
        jar_sidecar_symbol("Foo", "class", "", "class com.a.Foo", "com.a", false),
        jar_sidecar_symbol("alpha", "fun", "Foo", "fun alpha(): Int", "com.a", false),
        jar_sidecar_symbol("Foo", "class", "", "class com.b.Foo", "com.b", false),
        jar_sidecar_symbol("beta", "fun", "Foo", "fun beta(): Int", "com.b", false),
    ];
    crate::indexer::jar::populate_from_symbols(
        &idx,
        "/home/test/.gradle/caches/two-foos-1.0.jar".as_ref(),
        &compiled,
    );

    let app_uri = Url::parse("file:///app/MyFoo.kt").unwrap();
    idx.index_content(
        &app_uri,
        concat!(
            "package app\n",
            "import com.a.Foo\n",
            "class MyFoo : Foo() {\n",
            "    fun load() {}\n",
            "}\n",
        ),
    );

    let dot_items = complete_dot(&idx, "MyFoo", &app_uri, true, None);
    let dot_labels: Vec<&str> = dot_items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        dot_labels.contains(&"alpha"),
        "the imported com.a.Foo's own member must be offered — got: {dot_labels:?}"
    );
    assert!(
        !dot_labels.contains(&"beta"),
        "com.b.Foo's member must NOT leak into com.a.Foo's completion just \
         because the classes share a simple name in one JAR — got: {dot_labels:?}"
    );
}

/// Review finding: the jar member-enumeration branch filtered only
/// `Visibility::Private` — vacuous for JAR symbols (always `Public`) — and
/// ignored `deprecated`, which the sidecar populates. Project policy hides
/// deprecated library symbols from completion entirely (same as the direct
/// jar-definitions path and bare completion's stub path).
#[test]
fn jar_member_enumeration_hides_deprecated_members() {
    let idx = Indexer::new();
    let compiled = vec![
        jar_sidecar_symbol("Widget", "class", "", "class lib.Widget", "lib", false),
        jar_sidecar_symbol("fresh", "fun", "Widget", "fun fresh(): Int", "lib", false),
        jar_sidecar_symbol("legacy", "fun", "Widget", "fun legacy(): Int", "lib", true),
    ];
    crate::indexer::jar::populate_from_symbols(
        &idx,
        "/home/test/.gradle/caches/widget-lib-1.0.jar".as_ref(),
        &compiled,
    );

    let app_uri = Url::parse("file:///app/MyWidget.kt").unwrap();
    idx.index_content(
        &app_uri,
        concat!(
            "package app\n",
            "import lib.Widget\n",
            "class MyWidget : Widget() {\n",
            "    fun load() {}\n",
            "}\n",
        ),
    );

    let dot_items = complete_dot(&idx, "MyWidget", &app_uri, true, None);
    let dot_labels: Vec<&str> = dot_items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        dot_labels.contains(&"fresh"),
        "non-deprecated inherited jar member must be offered — got: {dot_labels:?}"
    );
    assert!(
        !dot_labels.contains(&"legacy"),
        "deprecated jar members must be hidden from completion — got: {dot_labels:?}"
    );
}

/// Review M2 on the member-enumeration disambiguation: a NESTED class import
/// (`import com.example.Outer.Config`) names container segments the naive
/// `strip_suffix(".Config")` treats as the package — deriving
/// `com.example.Outer` while the members' real package is `com.example`, so
/// every member of the imported class was filtered out. The filter must use
/// import-coverage semantics (`ImportEntry::covers`), which already
/// understand intermediate container segments.
#[test]
fn jar_member_enumeration_supports_nested_class_imports() {
    let idx = Indexer::new();
    let compiled = vec![
        jar_sidecar_symbol(
            "Outer",
            "class",
            "",
            "class com.example.Outer",
            "com.example",
            false,
        ),
        jar_sidecar_symbol(
            "Config",
            "class",
            "Outer",
            "class com.example.Outer.Config",
            "com.example",
            false,
        ),
        jar_sidecar_symbol(
            "mode",
            "fun",
            "Config",
            "fun mode(): Int",
            "com.example",
            false,
        ),
    ];
    crate::indexer::jar::populate_from_symbols(
        &idx,
        "/home/test/.gradle/caches/nested-lib-1.0.jar".as_ref(),
        &compiled,
    );

    let app_uri = Url::parse("file:///app/UseConfig.kt").unwrap();
    idx.index_content(
        &app_uri,
        concat!(
            "package app\n",
            "import com.example.Outer.Config\n",
            "class MyConfig : Config() {\n",
            "    fun load() {}\n",
            "}\n",
        ),
    );

    let dot_items = complete_dot(&idx, "MyConfig", &app_uri, true, None);
    let dot_labels: Vec<&str> = dot_items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        dot_labels.contains(&"mode"),
        "members of a nested class imported via its container path must \
         survive the package disambiguation — got: {dot_labels:?}"
    );
}

/// Review M2 second variant: an import of a DIFFERENT library's same-named
/// class must not filter this jar's members to zero — when the import
/// covers none of the candidate members, fall back to the declaring class
/// symbol's own package.
#[test]
fn jar_member_enumeration_falls_back_when_the_import_points_elsewhere() {
    let idx = Indexer::new();
    let compiled = vec![
        jar_sidecar_symbol("Widget", "class", "", "class lib.Widget", "lib", false),
        jar_sidecar_symbol("render", "fun", "Widget", "fun render(): Int", "lib", false),
    ];
    crate::indexer::jar::populate_from_symbols(
        &idx,
        "/home/test/.gradle/caches/widget-lib-2.0.jar".as_ref(),
        &compiled,
    );

    let app_uri = Url::parse("file:///app/UseWidget.kt").unwrap();
    idx.index_content(
        &app_uri,
        concat!(
            "package app\n",
            "import other.vendor.Widget\n",
            "class MyWidget : Widget() {\n",
            "    fun load() {}\n",
            "}\n",
        ),
    );

    let dot_items = complete_dot(&idx, "MyWidget", &app_uri, true, None);
    let dot_labels: Vec<&str> = dot_items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        dot_labels.contains(&"render"),
        "an import that covers none of the jar's members must not empty the \
         enumeration — got: {dot_labels:?}"
    );
}

/// Mid-typing named-param completion: multi-line lambda with BOTH braces
/// unclosed. Exercises the whole repaired path: lambda_params_at_col
/// broken-tree fall-through → is_lambda_param → complete_lambda_dot →
/// find_named_lambda_param_type (repair-wired).
#[test]
fn named_param_completion_survives_unclosed_lambda() {
    let idx = Indexer::new();
    let app_uri = Url::parse("file:///app/U.kt").unwrap();
    let src = "package app\n\
               class Item { val price: Int = 0 }\n\
               fun f(items: List<Item>) {\n\
                   items.map { item ->\n\
                       item.\n";
    idx.index_content(&app_uri, src);
    idx.store_live_tree(&app_uri, src);
    idx.set_live_lines(&app_uri, src);
    // line 4 = "item." (continuation-eaten indent), cursor after the dot.
    let (items, _) = crate::features::completion::run_completions(
        &idx,
        &app_uri,
        tower_lsp::lsp_types::Position::new(4, 5),
        false,
    );
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.iter().any(|l| l.starts_with("price")),
        "named-param completion in the broken state — got: {labels:?}"
    );
}

// ── missing-import diagnostic helpers ────────────────────────────────────────

#[test]
fn resolve_in_scope_strict_true_for_explicit_import() {
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(
        &caller_uri,
        "package app\n\
         import com.example.Foo\n\
         fun use() { Foo() }\n",
    );
    assert!(resolve_in_scope_strict(&idx, "Foo", &caller_uri));
}

#[test]
fn resolve_in_scope_strict_false_for_unimported_indexed_name() {
    // `Foo` is a real, indexed symbol — but in a different package with no
    // import, same-package relationship, or star import bringing it into
    // `Caller.kt`'s scope. A missing-import candidate must resolve to false
    // here even though the name IS otherwise known to the index.
    let foo_uri = uri("/lib/Foo.kt");
    let caller_uri = uri("/app/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(&foo_uri, "package com.example.lib\nclass Foo");
    idx.index_content(&caller_uri, "package app\nfun use() { Foo() }\n");
    assert!(!resolve_in_scope_strict(&idx, "Foo", &caller_uri));
}

#[test]
fn resolve_in_scope_strict_true_via_default_import_type() {
    // `Result` is a core kotlin.* type, in scope everywhere without an import.
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(
        &caller_uri,
        "package app\nfun use(): Result<Int> = TODO()\n",
    );
    assert!(resolve_in_scope_strict(&idx, "Result", &caller_uri));
}

/// Real, measured false positive on the Moneta corpus: `ByteArray` (a
/// specialized primitive array type, in scope everywhere without an import
/// exactly like `Result` above) accounted for 74 of 164 total flags (45%)
/// in the `missing-import` POC diagnostic before this fix — `is_default_import_type`
/// allowlisted `Array` itself plus the boxed collection/primitive types, but
/// missed all 8 specialized primitive array types Kotlin also default-imports.
#[test]
fn resolve_in_scope_strict_true_via_default_import_type_for_primitive_array_types() {
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(
        &caller_uri,
        "package app\nfun use(bytes: ByteArray) = bytes\n",
    );
    for name in [
        "ByteArray",
        "CharArray",
        "ShortArray",
        "IntArray",
        "LongArray",
        "FloatArray",
        "DoubleArray",
        "BooleanArray",
    ] {
        assert!(
            resolve_in_scope_strict(&idx, name, &caller_uri),
            "expected {name} to be in scope via Kotlin's default import, was flagged as missing"
        );
    }
}

/// Real, measured false positive on the Moneta corpus: a file with an
/// explicit `import java.util.*` still had `Date`/`Calendar`/`Collections`
/// flagged as missing imports. Root cause: the star-import-coverage check
/// filtered OUT any star import whose package `is_stdlib()` (`java.*`,
/// `kotlin.*`, `android.*`, `androidx.*`) before even considering it,
/// because that filter's ORIGINAL purpose was "don't waste an `rg`/source-tree
/// search on a package with no local source to search" — a reasonable
/// optimization for the SEARCH itself, but wrong for this EXISTENCE check:
/// the code compiles, so `import java.util.*` genuinely does bring `Date`
/// into scope even though we have no local source to verify it against.
#[test]
fn resolve_in_scope_strict_true_via_stdlib_star_import() {
    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(
        &caller_uri,
        "package app\nimport java.util.*\nfun use() { Date() }\n",
    );
    assert!(
        resolve_in_scope_strict(&idx, "Date", &caller_uri),
        "import java.util.* must cover Date, even though java.util has no \
         locally-indexed source to confirm membership against"
    );
}

/// Regression: `resolvable_via_default_import` must check `jar_definitions`
/// directly (by package), not the narrower `importable_fqns` cache — a
/// top-level `kotlin.*` FUNCTION (e.g. `error`, `run`, `with`, `repeat`) is not
/// reliably captured there, so a name-only lookup via `fqns_for_name` would
/// silently miss it and flag a real stdlib call as a missing import.
#[test]
fn resolve_in_scope_strict_true_via_jar_indexed_default_import_function() {
    use crate::types::FileData;
    use std::sync::Arc;

    let caller_uri = uri("/Caller.kt");
    let idx = Indexer::new();
    idx.index_content(&caller_uri, "package app\nfun use() { error(\"x\") }\n");

    // Simulate the kotlin-stdlib jar indexing a top-level `error` function.
    let jar_uri = "jar:file:///kotlin-stdlib.jar!/kotlin/PreconditionsKt.class";
    idx.jar_definitions
        .entry("error".to_string())
        .or_default()
        .push(tower_lsp::lsp_types::Location {
            uri: Url::parse(jar_uri).unwrap(),
            range: tower_lsp::lsp_types::Range::default(),
        });
    idx.jar_files.insert(
        jar_uri.to_string(),
        Arc::new(FileData {
            package: Some("kotlin".to_string()),
            ..Default::default()
        }),
    );

    assert!(
        resolve_in_scope_strict(&idx, "error", &caller_uri),
        "kotlin.error is a default-import top-level function — must not be flagged"
    );
}

/// Regression: `resolvable_via_default_import` must promote a Tier-1-only
/// (not-yet-materialized) JAR candidate before reading `jar_definitions`,
/// not read it directly. The test above seeds `jar_definitions` straight —
/// which passes even without the promote-before-read call, since the data is
/// already there. This one seeds only `jar_bare_names` (the real Tier-1
/// signal a lazily-loaded JAR starts in) and asserts the promotion actually
/// ran (`idx.materialized`), the way `find_fun_return_type_reachable`'s own
/// Tier-1-promotion regression test does.
#[test]
fn resolve_in_scope_strict_promotes_a_tier1_only_default_import_jar_candidate() {
    let tmp = tempfile::tempdir().expect("tempdir");
    crate::indexer::test_helpers::with_xdg_cache(tmp.path(), || {
        let jar_path = tmp.path().join("kotlin-stdlib.jar");
        std::fs::write(&jar_path, b"fake jar bytes").expect("write fake jar");
        let jar_path_key = jar_path.to_string_lossy().to_string();

        let symbols = vec![crate::sidecar::SidecarSymbol {
            name: "error".to_owned(),
            kind: "fun".to_owned(),
            container: String::new(),
            detail: "fun error(message: Any): Nothing".to_owned(),
            doc: String::new(),
            type_params: Vec::new(),
            extension_receiver_type: String::new(),
            trailing_lambda: false,
            deprecated: false,
            pkg: "kotlin".to_owned(),
            top_level: true,
            supers: vec![],
        }];
        let entry = crate::indexer::jar_cache::make_cache_entry(&jar_path, symbols)
            .expect("cache entry for existing file");
        let mut entries = std::collections::HashMap::new();
        entries.insert(jar_path_key.clone(), entry);
        crate::indexer::jar_cache::save_jar_cache(&entries);

        let idx = Indexer::new();
        let jar_id = idx.jar_table.intern(&jar_path_key);
        idx.jar_bare_names
            .entry("error".to_owned())
            .or_default()
            .push(jar_id);

        let caller_uri = uri("/Caller.kt");
        idx.index_content(&caller_uri, "package app\nfun use() { error(\"x\") }\n");

        assert!(
            resolve_in_scope_strict(&idx, "error", &caller_uri),
            "kotlin.error must resolve as default-import even before Tier-2 materialization"
        );
        assert!(
            idx.materialized.contains(&jar_id),
            "resolve_in_scope_strict must promote a fresh-cache-backed Tier-1-only \
             candidate, not read jar_definitions directly"
        );
    });
}

/// Regression: `receiver_provides_member`'s JAR-member step (3) must likewise
/// promote before reading `jar_definitions`/`jar_files`, not read them
/// directly — same contract, different call site.
#[test]
fn receiver_provides_member_promotes_a_tier1_only_jar_member() {
    let tmp = tempfile::tempdir().expect("tempdir");
    crate::indexer::test_helpers::with_xdg_cache(tmp.path(), || {
        let jar_path = tmp.path().join("some-lib.jar");
        std::fs::write(&jar_path, b"fake jar bytes").expect("write fake jar");
        let jar_path_key = jar_path.to_string_lossy().to_string();

        let symbols = vec![crate::sidecar::SidecarSymbol {
            name: "someMethod".to_owned(),
            kind: "fun".to_owned(),
            container: "SomeClass".to_owned(),
            detail: "fun someMethod(): Unit".to_owned(),
            doc: String::new(),
            type_params: Vec::new(),
            extension_receiver_type: String::new(),
            trailing_lambda: false,
            deprecated: false,
            pkg: "lib".to_owned(),
            top_level: false,
            supers: vec![],
        }];
        let entry = crate::indexer::jar_cache::make_cache_entry(&jar_path, symbols)
            .expect("cache entry for existing file");
        let mut entries = std::collections::HashMap::new();
        entries.insert(jar_path_key.clone(), entry);
        crate::indexer::jar_cache::save_jar_cache(&entries);

        let idx = Indexer::new();
        let jar_id = idx.jar_table.intern(&jar_path_key);
        idx.jar_bare_names
            .entry("someMethod".to_owned())
            .or_default()
            .push(jar_id);

        assert!(
            receiver_provides_member(&idx, "SomeClass", "someMethod"),
            "someMethod is a real JAR member of SomeClass, even before Tier-2 materialization"
        );
        assert!(
            idx.materialized.contains(&jar_id),
            "receiver_provides_member must promote a fresh-cache-backed Tier-1-only \
             candidate, not read jar_definitions directly"
        );
    });
}

#[test]
fn receiver_provides_member_true_for_extension_function() {
    let modifier_uri = uri("/Modifier.kt");
    let padding_uri = uri("/Padding.kt");
    let idx = Indexer::new();
    idx.index_content(
        &modifier_uri,
        "package androidx.compose.ui\nobject Modifier",
    );
    idx.index_content(
        &padding_uri,
        "package androidx.compose.ui\nfun Modifier.padding(v: Int): Modifier = this\n",
    );
    assert!(receiver_provides_member(&idx, "Modifier", "padding"));
}

#[test]
fn receiver_provides_member_false_for_unrelated_name() {
    let modifier_uri = uri("/Modifier.kt");
    let idx = Indexer::new();
    idx.index_content(
        &modifier_uri,
        "package androidx.compose.ui\nobject Modifier",
    );
    assert!(!receiver_provides_member(&idx, "Modifier", "notAMember"));
}

/// Regression: `container` stores only the immediate parent's simple NAME,
/// not a unique identity — two different nested types sharing a simple name
/// in one file (`A.Config`/`B.Config`) are valid Kotlin (Kotlin only forbids
/// a name collision among *top-level* declarations in one file, not among
/// unrelated types' own nested members) and would have their members merged
/// by a container-name-only check. `type_symbol` is already the one specific
/// instance the outer lookup resolved to, so `members_for_workspace_type`
/// must also confirm a candidate's range falls inside that SPECIFIC
/// instance's own range, not just any same-named one.
#[test]
fn same_named_nested_types_in_different_classes_do_not_merge_members() {
    let idx = Indexer::new();
    let file_uri = uri("/Configs.kt");
    idx.index_content(
        &file_uri,
        "package app\n\
         class A {\n\
         \x20 class Config {\n\
         \x20   val fromA = 1\n\
         \x20 }\n\
         }\n\
         class B {\n\
         \x20 class Config {\n\
         \x20   val fromB = 2\n\
         \x20 }\n\
         }",
    );
    let items = complete_dot(&idx, "Config", &file_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    let has_a = labels.contains(&"fromA");
    let has_b = labels.contains(&"fromB");
    assert!(
        has_a != has_b,
        "expected exactly one of A.Config/B.Config's own members, not both (merged) or neither: {labels:?}"
    );
}

/// Regression: matching a `data object` in a `when` is written as equality
/// (`Loading ->`), not a type test (`is Loading ->`) — the idiomatic form,
/// since an object has exactly one instance. Only `is` branches narrowed, so
/// the subject kept its sealed-interface type and the object's own members
/// were missing from completion.
#[test]
fn when_equality_branch_on_an_object_narrows_the_subject() {
    let idx = Indexer::new();
    let file_uri = uri("/Ui.kt");
    idx.index_content(
        &file_uri,
        "package app\n\
         sealed interface Ui {\n\
         \x20 data object Loading : Ui {\n\
         \x20   val progress = 0\n\
         \x20 }\n\
         \x20 data class Ready(val value: Int) : Ui\n\
         }\n\
         fun render(state: Ui) {\n\
         \x20 when (state) {\n\
         \x20   Ui.Loading -> state.\n\
         \x20   else -> {}\n\
         \x20 }\n\
         }",
    );
    let items = complete_dot(&idx, "state", &file_uri, false, Some(9));
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"progress"),
        "an object-equality branch must narrow to that object: {labels:?}"
    );
}

/// An enum entry is a value, not a type, so `Color.RED ->` must NOT narrow —
/// treating the label as a type would resolve to the entry (which has no
/// members) and blank the completion list instead of offering the enum's own.
#[test]
fn when_equality_branch_on_an_enum_entry_does_not_narrow() {
    let idx = Indexer::new();
    let file_uri = uri("/Color.kt");
    idx.index_content(
        &file_uri,
        "package app\n\
         enum class Color {\n\
         \x20 RED, GREEN;\n\
         \x20 fun describe(): String = name\n\
         }\n\
         fun pick(color: Color) {\n\
         \x20 when (color) {\n\
         \x20   Color.RED -> color.\n\
         \x20   else -> {}\n\
         \x20 }\n\
         }",
    );
    let items = complete_dot(&idx, "color", &file_uri, false, Some(7));
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"describe"),
        "must still offer Color's own members; narrowing to the entry (which \
         has none) would blank the list: {labels:?}"
    );
}

/// Regression: the backward line scan has no idea whether it is inside a
/// `when`, and a lambda parameter on its own line looks exactly like a branch
/// label. Accepting a bare `Element ->` let an unrelated object's members be
/// offered for a receiver the branch never narrowed — worse than not narrowing
/// at all. Only a qualified label is accepted, since a lambda parameter is
/// always a simple identifier.
#[test]
fn a_lambda_parameter_is_not_mistaken_for_a_when_branch_label() {
    let idx = Indexer::new();
    let file_uri = uri("/Scan.kt");
    idx.index_content(
        &file_uri,
        "package app\n\
         object Element { fun unrelatedMember() {} }\n\
         sealed interface Ui\n\
         object Busy : Ui { fun busyOnly() {} }\n\
         fun render(state: Ui) {\n\
         \x20 when (state) {\n\
         \x20   is Busy -> {\n\
         \x20     listOf(1).forEach {\n\
         \x20       Element ->\n\
         \x20       state.\n\
         \x20     }\n\
         \x20   }\n\
         \x20 }\n\
         }",
    );
    let items = complete_dot(&idx, "state", &file_uri, false, Some(9));
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"unrelatedMember"),
        "a lambda parameter must not narrow the subject to that name: {labels:?}"
    );
}

/// Regression: confirming the branch label names an object must respect the
/// file's own imports and package. Scanning every definition sharing the
/// simple name let an unrelated `object Idle` in another package validate an
/// enum entry named `Idle`, narrowing the subject to a type it has nothing to
/// do with and blanking the completion list.
#[test]
fn an_unrelated_same_named_object_does_not_validate_an_enum_entry_branch() {
    let idx = Indexer::new();
    let other_uri = uri("/other/Idle.kt");
    let app_uri = uri("/app/Conn.kt");
    idx.index_content(
        &other_uri,
        "package other\nobject Idle { fun unrelatedMember() {} }\n",
    );
    idx.index_content(
        &app_uri,
        "package app\n\
         enum class Conn {\n\
         \x20 Idle,\n\
         \x20 Active;\n\
         \x20 fun describe(): String = name\n\
         }\n\
         fun show(conn: Conn) {\n\
         \x20 when (conn) {\n\
         \x20   Conn.Idle -> conn.\n\
         \x20   else -> {}\n\
         \x20 }\n\
         }",
    );
    let items = complete_dot(&idx, "conn", &app_uri, false, Some(8));
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"describe"),
        "an unrelated same-named object must not validate an enum entry: {labels:?}"
    );
}

/// Regression, from a production stack overflow with a 65,127-frame core
/// dump: inferring a variable's type infers its initializer, which resolves
/// the identifiers in it, which infers *their* variables. A self-referential
/// initializer closed that into an unbounded loop and aborted the server
/// during ordinary editing.
///
/// A depth cap could not catch it — the cycle runs back through
/// `infer_expr_type`, a public entry point that restarts its depth counter at
/// zero every lap — so this is guarded by refusing to re-enter a resolution
/// already in flight.
#[test]
fn a_self_referential_initializer_does_not_recurse_forever() {
    let idx = Indexer::new();
    let file_uri = uri("/Cycle.kt");
    let src = "package app\nfun f() {\n    val a = a\n}\n";
    idx.index_content(&file_uri, src);
    idx.store_live_tree(&file_uri, src);
    // Must terminate rather than exhaust the stack.
    let _ = crate::resolver::infer::infer_variable_type_from_cst(&idx, "a", &file_uri);
}

/// The mutual case: two declarations whose initializers reference each other.
#[test]
fn mutually_referential_initializers_do_not_recurse_forever() {
    let idx = Indexer::new();
    let file_uri = uri("/Cycle2.kt");
    let src = "package app\nfun f() {\n    val a = b\n    val b = a\n}\n";
    idx.index_content(&file_uri, src);
    idx.store_live_tree(&file_uri, src);
    let _ = crate::resolver::infer::infer_variable_type_from_cst(&idx, "a", &file_uri);
}

/// A name that is never declared makes `find_prop_initializer` search the
/// whole file rather than returning early, so its recursion reaches the
/// tree's full depth.
#[test]
fn the_initializer_search_survives_a_pathologically_deep_file() {
    let n = 60_000; // ~100x MAX_CST_DESCENT_DEPTH (512)
    let mut src = String::from("package app\nfun f() {\n    val x = 1");
    for _ in 0..n {
        src.push_str("+1");
    }
    src.push_str("\n}\n");

    let handle = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024) // match Linux's default main-thread stack
        .spawn(move || {
            let idx = Indexer::new();
            let file_uri = uri("/Deep.kt");
            idx.index_content(&file_uri, &src);
            idx.store_live_tree(&file_uri, &src);
            crate::resolver::infer::infer_variable_type_from_cst(&idx, "never_declared", &file_uri)
        })
        .unwrap();
    // A stack overflow aborts the process rather than failing this join.
    let found = handle.join().expect("must not overflow the stack");
    assert_eq!(found, None, "the name is not declared anywhere in the file");
}

// ─── Kotlin built-in-type platform-equivalent fallback ───────────────────────
//
// `kotlin.String`/`kotlin.CharSequence` are compiler intrinsics with no
// compiled `.class` file anywhere in kotlin-stdlib's JAR (verified via
// `unzip -l kotlin-stdlib-*.jar | grep String.class` -> no output — see
// docs/superpowers/specs/2026-08-27-kotlin-builtin-type-platform-mapping-design.md).
// `resolve_kotlin_builtin_type_platform_equivalent` is the last-resort
// fallback that indexes the real platform declaration from the Android SDK
// sources bundle on demand.

/// Builds a fake Android SDK layout (`local.properties` + `sdk/sources/
/// android-<api>/<relative_java_path>`) under `root`, matching the exact
/// shape `detect_android_sdk_source_paths` looks for (see
/// `sdk_dir_from_local_properties_finds_sdk_dot_dir` in
/// `workspace_json_tests.rs`, the precedent this fixture follows).
fn write_fake_android_sdk_source(root: &std::path::Path, relative_java_path: &str, content: &str) {
    let fake_sdk = root.join("sdk");
    let file_path = fake_sdk
        .join("sources")
        .join("android-34")
        .join(relative_java_path);
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    std::fs::write(&file_path, content).unwrap();
    std::fs::write(
        root.join("local.properties"),
        format!("sdk.dir={}\n", fake_sdk.display()),
    )
    .unwrap();
}

#[test]
fn resolve_kotlin_builtin_type_platform_equivalent_indexes_java_lang_string_on_demand() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fake_android_sdk_source(
        root,
        "java/lang/String.java",
        "package java.lang;\npublic final class String implements CharSequence {\n}\n",
    );

    let idx = Indexer::new();
    idx.workspace_root.set(root.to_path_buf());

    let locs = resolve_kotlin_builtin_type_platform_equivalent(&idx, "String");
    assert_eq!(
        locs.len(),
        1,
        "expected the on-demand-indexed java.lang.String declaration, got {locs:?}"
    );
    assert!(
        locs[0].uri.path().ends_with("java/lang/String.java"),
        "expected the real platform source file, got {:?}",
        locs[0].uri
    );

    // The on-demand indexing must persist (step 0.5's own contract), not
    // just parse ad hoc — a later hierarchy walk from `String` needs its
    // supertypes (`CharSequence` here) available from `indexer.files`.
    assert!(
        idx.files.contains_key(locs[0].uri.as_str()),
        "expected the file to be permanently cached after the first resolution"
    );
}

#[test]
fn resolve_kotlin_builtin_type_platform_equivalent_ignores_unmapped_names() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fake_android_sdk_source(
        root,
        "java/lang/String.java",
        "package java.lang;\npublic final class String {\n}\n",
    );

    let idx = Indexer::new();
    idx.workspace_root.set(root.to_path_buf());

    // "SomeRandomClass" is not one of Kotlin's mapped built-in types, so the
    // fallback must not fire even though an SDK is present.
    let locs = resolve_kotlin_builtin_type_platform_equivalent(&idx, "SomeRandomClass");
    assert!(
        locs.is_empty(),
        "expected no fallback for an unmapped name, got {locs:?}"
    );
}

#[test]
fn resolve_kotlin_builtin_type_platform_equivalent_returns_empty_without_an_sdk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // No local.properties, no fake SDK -- `detect_android_sdk_source_paths`
    // must come back empty (short of a real SDK on the CI machine's own env
    // vars, which `no_sdk_returns_empty` in workspace_json_tests.rs already
    // documents as a possible, harmless false pass).
    let idx = Indexer::new();
    idx.workspace_root.set(root.to_path_buf());

    let locs = resolve_kotlin_builtin_type_platform_equivalent(&idx, "String");
    assert!(
        locs.is_empty()
            || std::env::var("ANDROID_HOME").is_ok()
            || std::env::var("ANDROID_SDK_ROOT").is_ok(),
        "expected no fallback without any SDK, got {locs:?}"
    );
}

/// End-to-end through the real public entry point (`resolve_symbol`, the
/// `Full` policy that spawns `rg`) -- the flagship scenario the whole design
/// doc exists for: resolving a bare `String` qualifier root when nothing in
/// the workspace's own source explicitly names it, matching the real
/// `toViewText` receiver-resolution gap found on the Moneta corpus.
#[test]
fn resolve_symbol_full_policy_falls_back_to_the_builtin_type_platform_equivalent() {
    if !rg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fake_android_sdk_source(
        root,
        "java/lang/String.java",
        "package java.lang;\npublic final class String implements CharSequence {\n}\n",
    );

    let caller_src = "package com.example\nfun use(s: String) { }\n";
    let caller_path = root.join("Caller.kt");
    std::fs::write(&caller_path, caller_src).unwrap();
    let caller_uri = Url::from_file_path(&caller_path).unwrap();

    let idx = Indexer::new();
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&caller_uri, caller_src);

    let locs = resolve_symbol(&idx, "String", None, &caller_uri);
    assert_eq!(
        locs.len(),
        1,
        "expected the builtin-type fallback to resolve bare String, got {locs:?}"
    );
    assert!(
        locs[0].uri.path().ends_with("java/lang/String.java"),
        "expected the real platform source file, got {:?}",
        locs[0].uri
    );
}

/// The same fallback must also surface in dot-completion, not just goto-def/
/// hover: `extension_fn_completions` resolves the receiver class via
/// `resolve_symbol_no_rg` and walks its real supertypes to build the
/// ancestor set an extension's receiver is matched against. Before this fix,
/// `resolve_symbol_no_rg(idx, "String", ..)` came back empty, so a `String`
/// receiver's ancestor set was just `{"String"}` -- an extension declared on
/// `CharSequence` (e.g. the real `toViewText`) could never match and so
/// never appeared as a suggestion while typing.
#[test]
fn resolve_kotlin_builtin_type_platform_equivalent_surfaces_supertype_extensions_in_dot_completion()
{
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fake_android_sdk_source(
        root,
        "java/lang/String.java",
        "package java.lang;\npublic final class String implements CharSequence {\n}\n",
    );
    // The walk from String to CharSequence must itself resolve CharSequence's
    // own declaration (via the same builtin-type fallback) to become a real
    // hop -- without a real java.lang.CharSequence.java on disk too (as the
    // real Android SDK sources bundle always has), the walk dead-ends at
    // String and this test would pass for the wrong reason.
    write_fake_android_sdk_source(
        root,
        "java/lang/CharSequence.java",
        "package java.lang;\npublic interface CharSequence {\n}\n",
    );

    let idx = Indexer::new();
    idx.workspace_root.set(root.to_path_buf());

    // Simulate an indexed extension declared on CharSequence, the same shape
    // `jar_extension_appears_in_dot_completion` uses for a JAR-sourced one.
    idx.extension_by_receiver
        .entry("CharSequence".to_owned())
        .or_default()
        .push(crate::types::ExtensionEntry {
            file_uri: "file:///app/ViewText.kt".to_owned(),
            name: "toViewText".to_owned(),
            kind: tower_lsp::lsp_types::SymbolKind::FUNCTION,
            detail: "fun CharSequence?.toViewText(): String".to_owned(),
            visibility: crate::types::Visibility::Public,
            package: Some("app".to_owned()),
            trailing_lambda: false,
            deprecated: false,
            container: None,
        });

    let caller_src = "package app\nfun use(s: String) { s }\n";
    let caller_path = root.join("Caller.kt");
    std::fs::write(&caller_path, caller_src).unwrap();
    let caller_uri = Url::from_file_path(&caller_path).unwrap();
    idx.index_content(&caller_uri, caller_src);

    let items = complete_dot(&idx, "s", &caller_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"toViewText"),
        "expected the CharSequence extension to appear for a String receiver, got: {labels:?}"
    );
}

/// `MutableList` maps to the SAME real platform type as `List`
/// (`java.util.List` — Kotlin's mutable/immutable distinction is a
/// compile-time-only view over one real JVM interface literally named
/// `List`, never `MutableList`). A lookup that searches the target file for
/// a symbol named `name` (the ORIGINAL Kotlin spelling) rather than the
/// resolved platform type's own simple name would search `List.java` for a
/// symbol called `"MutableList"` and always come up empty -- this must
/// search by the platform type's own simple name instead.
#[test]
fn resolve_kotlin_builtin_type_platform_equivalent_maps_mutablelist_to_the_real_list_interface() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fake_android_sdk_source(
        root,
        "java/util/List.java",
        "package java.util;\npublic interface List<E> extends Collection<E> {\n}\n",
    );

    let idx = Indexer::new();
    idx.workspace_root.set(root.to_path_buf());

    let locs = resolve_kotlin_builtin_type_platform_equivalent(&idx, "MutableList");
    assert_eq!(
        locs.len(),
        1,
        "expected MutableList to resolve to the real java.util.List declaration, got {locs:?}"
    );
    assert!(
        locs[0].uri.path().ends_with("java/util/List.java"),
        "expected the real platform source file, got {:?}",
        locs[0].uri
    );
}

/// `Map` -> `java.util.Map`, the plain (non-Mutable-aliased) case, matching
/// the same on-demand-index-and-find shape already proven for `String`.
#[test]
fn resolve_kotlin_builtin_type_platform_equivalent_resolves_map_to_the_real_java_util_map() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fake_android_sdk_source(
        root,
        "java/util/Map.java",
        "package java.util;\npublic interface Map<K, V> {\n}\n",
    );

    let idx = Indexer::new();
    idx.workspace_root.set(root.to_path_buf());

    let locs = resolve_kotlin_builtin_type_platform_equivalent(&idx, "Map");
    assert_eq!(locs.len(), 1, "expected Map to resolve, got {locs:?}");
    assert!(locs[0].uri.path().ends_with("java/util/Map.java"));
}

/// End-to-end through the real supertype-hierarchy walk (`extension_fn_completions`,
/// via `resolve_symbol_no_rg`): an extension declared on `Iterable` must
/// surface for a `List`-typed receiver, since `java.util.List` really does
/// extend `Collection` which really does extend `Iterable` -- proving the
/// collection interfaces resolve to declarations with correct, walkable
/// supertype chains, not just a bare unlinked file.
#[test]
fn resolve_kotlin_builtin_type_platform_equivalent_surfaces_iterable_extensions_for_a_list_receiver(
) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fake_android_sdk_source(
        root,
        "java/util/List.java",
        "package java.util;\npublic interface List<E> extends Collection<E> {\n}\n",
    );
    write_fake_android_sdk_source(
        root,
        "java/util/Collection.java",
        "package java.util;\npublic interface Collection<E> extends Iterable<E> {\n}\n",
    );
    write_fake_android_sdk_source(
        root,
        "java/lang/Iterable.java",
        "package java.lang;\npublic interface Iterable<T> {\n}\n",
    );

    let idx = Indexer::new();
    idx.workspace_root.set(root.to_path_buf());
    idx.extension_by_receiver
        .entry("Iterable".to_owned())
        .or_default()
        .push(crate::types::ExtensionEntry {
            file_uri: "file:///app/Extensions.kt".to_owned(),
            name: "secondOrNull".to_owned(),
            kind: tower_lsp::lsp_types::SymbolKind::FUNCTION,
            detail: "fun <T> Iterable<T>.secondOrNull(): T?".to_owned(),
            visibility: crate::types::Visibility::Public,
            package: Some("app".to_owned()),
            trailing_lambda: false,
            deprecated: false,
            container: None,
        });

    let caller_src = "package app\nfun use(items: List<Int>) { items }\n";
    let caller_path = root.join("Caller.kt");
    std::fs::write(&caller_path, caller_src).unwrap();
    let caller_uri = Url::from_file_path(&caller_path).unwrap();
    idx.index_content(&caller_uri, caller_src);

    let items = complete_dot(&idx, "items", &caller_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"secondOrNull"),
        "expected the Iterable extension to appear for a List receiver, got: {labels:?}"
    );
}

// ─── named companion-object members on a JAR-derived type ────────────────────
//
// Real-world regression: Timber's entire public API (`d`, `e`, `i`, `w`,
// `tag`, `plant`, ...) lives inside `companion object Forest : Tree()` --
// a NAMED companion, not the default unnamed one. The sidecar's own
// `entriesFromClass` gives the companion's class-declaration symbol
// `container == "Timber"` and gives its members `container == "Forest"`
// (its own bare name) -- mirroring exactly how a source-parsed file's
// `assign_containers` names a nested symbol's container. `resolve_companion_member`
// must recognize a JAR-backed file (whose synthetic per-entry ranges carry no
// real nesting for range-containment to discover) and match by this
// container-name chain instead of range containment.

fn fake_sidecar_symbol(
    name: &str,
    kind: &str,
    container: &str,
    detail: &str,
    supers: Vec<String>,
) -> crate::sidecar::SidecarSymbol {
    crate::sidecar::SidecarSymbol {
        name: name.to_owned(),
        kind: kind.to_owned(),
        container: container.to_owned(),
        detail: detail.to_owned(),
        doc: String::new(),
        type_params: Vec::new(),
        extension_receiver_type: String::new(),
        trailing_lambda: false,
        deprecated: false,
        pkg: "timber.log".to_owned(),
        top_level: container.is_empty(),
        supers,
    }
}

/// Builds a fake compiled-JAR fixture matching exactly the shape
/// `entriesFromClass` produces for `class Timber { companion object Forest :
/// Tree() { fun d(...) } }` -- PLUS an unrelated decoy class with its own
/// companion object that ALSO declares a member named `d`, positioned
/// between Timber and its real Forest/d in the synthetic per-entry line
/// order. `find_name_in_uri_after_line` (an existing, more general fallback
/// for JAR-derived degenerate ranges) picks the symbol named `d` with the
/// SMALLEST line number at or after the container's own line -- without
/// real container-name matching, that fallback would return the decoy
/// (closer to Timber's line) instead of the real Forest.d, so this decoy is
/// what makes the test actually exercise container-based matching rather
/// than passing for the wrong reason.
fn populate_fake_timber_jar(idx: &crate::indexer::Indexer) {
    let jar_path = "/home/test/.gradle/caches/timber-5.0.1.jar";
    let symbols = vec![
        fake_sidecar_symbol("Timber", "class", "", "class Timber", vec![]),
        fake_sidecar_symbol("DecoyOwner", "class", "", "class DecoyOwner", vec![]),
        fake_sidecar_symbol(
            "DecoyForest",
            "object",
            "DecoyOwner",
            "companion object DecoyForest",
            vec![],
        ),
        fake_sidecar_symbol(
            "d",
            "fun",
            "DecoyForest",
            "fun d(): Nothing = TODO(\"decoy\")",
            vec![],
        ),
        fake_sidecar_symbol(
            "Forest",
            "object",
            "Timber",
            "companion object Forest",
            vec!["Tree".to_owned()],
        ),
        fake_sidecar_symbol(
            "d",
            "fun",
            "Forest",
            "fun d(message: String?, args: Array<out Any?>)",
            vec![],
        ),
    ];
    crate::indexer::jar::populate_from_symbols(idx, jar_path.as_ref(), &symbols);
}

#[test]
fn resolve_qualified_finds_a_named_companion_member_on_a_jar_backed_type() {
    let idx = Indexer::new();
    populate_fake_timber_jar(&idx);

    let caller_uri = uri("/Host.kt");
    idx.index_content(
        &caller_uri,
        "package app\nimport timber.log.Timber\nfun use() { Timber.d(\"hi\") }\n",
    );

    let locs = resolve_symbol(&idx, "d", Some("Timber"), &caller_uri);
    assert_eq!(
        locs.len(),
        1,
        "expected Timber.d to resolve through the named Forest companion, got {locs:?}"
    );
    let file_data = idx
        .jar_files
        .get("jar:file:///home/test/.gradle/caches/timber-5.0.1.jar")
        .expect("fake jar must be indexed")
        .clone();
    let resolved_symbol = file_data
        .symbols
        .iter()
        .find(|s| s.selection_range == locs[0].range)
        .expect("resolved location must map to a real symbol");
    assert_eq!(
        resolved_symbol.detail, "fun d(message: String?, args: Array<out Any?>)",
        "expected the REAL Forest.d, not the DecoyForest.d decoy -- got detail: {}",
        resolved_symbol.detail
    );
    assert!(
        locs[0].uri.as_str().starts_with("jar:file://"),
        "expected the JAR-backed declaration, got {:?}",
        locs[0].uri
    );
}

/// Companion to the resolution test above, for dot-completion
/// (`members_for_jar_backed_type`): typing `Timber.` must suggest the real
/// Forest companion's members (`d`, `e`, ...), not the DecoyForest decoy.
#[test]
fn complete_dot_finds_named_companion_members_on_a_jar_backed_type() {
    let idx = Indexer::new();
    populate_fake_timber_jar(&idx);

    let caller_uri = uri("/Host.kt");
    idx.index_content(
        &caller_uri,
        "package app\nimport timber.log.Timber\nfun use() { }\n",
    );

    let items = complete_dot(&idx, "Timber", &caller_uri, false, None);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"d"),
        "expected Timber. to suggest the real Forest.d, got: {labels:?}"
    );
    let d_item = items
        .iter()
        .find(|i| i.label == "d")
        .expect("d must be present");
    assert_eq!(
        d_item.detail.as_deref(),
        Some("fun d(message: String?, args: Array<out Any?>)"),
        "expected the REAL Forest.d, not the DecoyForest.d decoy"
    );
}

// ─── primitive-scalar compiler-intrinsic mapped types ────────────────────────
//
// Kotlin's 8 primitive scalar types (`Int`/`Long`/`Double`/`Float`/`Boolean`/
// `Byte`/`Short`/`Char`) are compiler intrinsics with no compiled `.class`
// file in kotlin-stdlib's JAR at all — the exact same shape as
// `String`/`CharSequence`/the collection interfaces already handled. Real,
// measured gap: `Int.MAX_VALUE` (a real call site in Moneta,
// `core/common/src/main/java/cz/moneta/smartbanka/common/extensions/ListExtensions.kt`)
// resolved to zero candidates.

/// Plain case: the Kotlin name matches the Java platform type's own simple
/// name (`Long` -> `java.lang.Long`).
#[test]
fn resolve_kotlin_builtin_type_platform_equivalent_resolves_long_to_java_lang_long() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fake_android_sdk_source(
        root,
        "java/lang/Long.java",
        "package java.lang;\npublic final class Long {\n    public static final long MAX_VALUE = 0x7fffffffffffffffL;\n}\n",
    );

    let idx = Indexer::new();
    idx.workspace_root.set(root.to_path_buf());

    let locs = resolve_kotlin_builtin_type_platform_equivalent(&idx, "Long");
    assert_eq!(locs.len(), 1, "expected Long to resolve, got {locs:?}");
    assert!(locs[0].uri.path().ends_with("java/lang/Long.java"));
}

/// The one name-mismatch case among the 8: `Char` maps to `java.lang.Character`,
/// not `java.lang.Char` -- mirroring the exact `MutableList` -> `java.util.List`
/// mismatch already handled by looking up the platform type's own simple name
/// rather than the original Kotlin spelling.
#[test]
fn resolve_kotlin_builtin_type_platform_equivalent_resolves_char_to_java_lang_character() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_fake_android_sdk_source(
        root,
        "java/lang/Character.java",
        "package java.lang;\npublic final class Character {\n}\n",
    );

    let idx = Indexer::new();
    idx.workspace_root.set(root.to_path_buf());

    let locs = resolve_kotlin_builtin_type_platform_equivalent(&idx, "Char");
    assert_eq!(
        locs.len(),
        1,
        "expected Char to resolve to java.lang.Character, got {locs:?}"
    );
    assert!(locs[0].uri.path().ends_with("java/lang/Character.java"));
}
