//! End-to-end tests for [`find_references_with_qualifier`].
//!
//! These tests write real `.kt` files to a temp directory so that `rg`
//! can search them, then drive the full `find_references_with_qualifier →
//! rg_scope_for_path → rg_find_references` pipeline against an [`Indexer`]
//! whose `workspace_root` is (or isn't) configured.
//!
//! The scenarios targeted by these tests:
//!
//! - **workspace_root set** — rg searches the workspace dir → cross-file hits.
//! - **workspace_root unset** — `effective_rg_root` falls back to the file's
//!   parent directory → cross-file hits still found (files are co-located).
//! - **workspace_root set to a *different* project** — `effective_rg_root`
//!   walks up from the open file to its git root; `scoped_source_roots` is
//!   cleared because effective != configured root → rg searches the file's
//!   git root without leaking the stale workspace's source-path scoping.

use std::sync::Arc;

use tower_lsp::lsp_types::Url;

use crate::features::references::find_references_with_qualifier;
use crate::indexer::Indexer;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn write(dir: &std::path::Path, name: &str, content: &str) -> (std::path::PathBuf, Url) {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    let uri = Url::from_file_path(&path).unwrap();
    (path, uri)
}

fn hit_files(locs: &[tower_lsp::lsp_types::Location]) -> Vec<String> {
    locs.iter()
        .filter_map(|l| l.uri.to_file_path().ok())
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect()
}

// ─── tests ───────────────────────────────────────────────────────────────────

/// **Core regression**: `find_references` must return cross-file results when
/// `workspace_root` is properly set.
///
/// Layout:
///   Foo.kt — `class MyClass`  (declaration)
///   Bar.kt — `fun use(): MyClass = MyClass()` (usage)
///
/// Calling `find_references("MyClass", foo_uri, …)` must return a hit in
/// `Bar.kt`.  If only `Foo.kt` is returned, `rg_scope_for_path` is not
/// delivering the workspace root to `rg_find_references`.
#[tokio::test]
async fn find_references_cross_file_with_workspace_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let foo_src = "package com.example\nclass MyClass";
    let bar_src = "package com.example\nfun use(): MyClass = MyClass()";

    let (_foo_path, foo_uri) = write(root, "Foo.kt", foo_src);
    let (_bar_path, _bar_uri) = write(root, "Bar.kt", bar_src);

    let idx = Arc::new(Indexer::new());
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&foo_uri, foo_src);

    let locs = find_references_with_qualifier("MyClass", None, &foo_uri, 1, true, &*idx).await;
    let files = hit_files(&locs);

    assert!(
        files.iter().any(|f| f == "Bar.kt"),
        "find_references must include Bar.kt; got files: {:?}",
        files
    );
}

/// `find_references` must still return cross-file results when `workspace_root`
/// is **not** set on the indexer.
///
/// In this case `effective_rg_root` falls back through:
///   1. `walk_to_git_root(open_file)` — tempdir has no `.git`, returns `None`
///   2. `open_file.parent()`           — the tempdir itself ← this must work
///
/// If the fallback resolves to the correct directory, `Bar.kt` is found.
/// If it resolves to the wrong directory (e.g. CWD = the lsp repo), the test
/// catches the broken fallback.
#[tokio::test]
async fn find_references_cross_file_without_workspace_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let foo_src = "package com.example\nclass MyClass";
    let bar_src = "package com.example\nfun use(): MyClass = MyClass()";

    let (_foo_path, foo_uri) = write(root, "Foo.kt", foo_src);
    let (_bar_path, _bar_uri) = write(root, "Bar.kt", bar_src);

    // ← workspace_root intentionally NOT set
    let idx = Arc::new(Indexer::new());
    idx.index_content(&foo_uri, foo_src);

    let locs = find_references_with_qualifier("MyClass", None, &foo_uri, 1, true, &*idx).await;
    let files = hit_files(&locs);

    assert!(
        files.iter().any(|f| f == "Bar.kt"),
        "find_references must include Bar.kt even without workspace_root; \
         effective_rg_root should fall back to the file's parent directory. \
         Got files: {:?}",
        files
    );
}

/// **Package-scoped regression**: `find_references` for an *uppercase* symbol
/// must use the package-scoped rg path and return cross-file results.
///
/// `resolve_scope` for an uppercase symbol that IS the declaration returns
/// `(parent=None, pkg=Some("com.example"))`.  This triggers
/// `package_scoped_reference_locations` which first scans for
/// candidate files via import/package patterns, then searches those files.
///
/// If `rg_scope_for_path` returns the wrong `search_root`, the import-pattern
/// scan finds no candidates and the function returns empty — showing only the
/// current-file hit injected by `add_current_file_locations`.
#[tokio::test]
async fn find_references_package_scoped_cross_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Foo.kt: declaration of MyClass at line 1 (0-indexed)
    let foo_src = "package com.example\nclass MyClass";
    // Bar.kt: same package → no import needed, but has explicit import for clarity
    let bar_src = "package com.example\nimport com.example.MyClass\nfun use(): MyClass = MyClass()";
    // Baz.kt: different package, imports MyClass explicitly
    let baz_src = "package com.other\nimport com.example.MyClass\nval x: MyClass = MyClass()";

    let (_, foo_uri) = write(root, "Foo.kt", foo_src);
    write(root, "Bar.kt", bar_src);
    write(root, "Baz.kt", baz_src);

    let idx = Arc::new(Indexer::new());
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&foo_uri, foo_src);

    // line=1: declaration of MyClass → resolve_scope returns (None, Some("com.example"))
    // → package_scoped_reference_locations is used
    let locs = find_references_with_qualifier("MyClass", None, &foo_uri, 1, true, &*idx).await;
    let files = hit_files(&locs);

    assert!(
        files.iter().any(|f| f == "Bar.kt"),
        "package-scoped search must find Bar.kt (same package); got files: {:?}",
        files
    );
    assert!(
        files.iter().any(|f| f == "Baz.kt"),
        "package-scoped search must find Baz.kt (imports MyClass); got files: {:?}",
        files
    );
}

/// End-to-end actor test: after a full workspace scan, `find_references` on a
/// symbol declared in one file must find usages in another file.
///
/// This is the canonical regression test for "find refs only returns current
/// file" — it drives the complete path:
///   Helix opens file → actor receives Initialize → scan completes →
///   user calls find_references → cross-file hits returned.
#[tokio::test]
async fn actor_scan_then_find_references_cross_file() {
    use tokio::sync::oneshot;

    use crate::indexer::NoopReporter;
    use crate::workspace::{Actor, Config, Event};

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // workspace.json opts out of external sourcePaths (test isolation)
    std::fs::write(root.join("workspace.json"), r#"{"sourcePaths":[]}"#).unwrap();

    let foo_src = "package com.example\nclass MyClass";
    let bar_src = "package com.example\nfun use(): MyClass = MyClass()";
    let (_, foo_uri) = write(root, "Foo.kt", foo_src);
    write(root, "Bar.kt", bar_src);

    let indexer = Arc::new(Indexer::new());
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let actor = Actor::new(Arc::clone(&indexer), Arc::new(NoopReporter), rx, None);
    tokio::spawn(actor.run());

    let (done_tx, done_rx) = oneshot::channel();
    tx.send(Event::Initialize {
        config: Config {
            root: root.to_path_buf(),
            explicit_source_paths: Vec::new(),
            ignore_patterns: Vec::new(),
            pin_workspace: false,
        },
        completion_tx: Some(done_tx),
    })
    .await
    .unwrap();

    // Wait for the workspace scan to complete before querying.
    tokio::time::timeout(std::time::Duration::from_secs(10), done_rx)
        .await
        .expect("workspace scan must complete within 10s")
        .unwrap();

    let locs = find_references_with_qualifier("MyClass", None, &foo_uri, 1, true, &*indexer).await;
    let files = hit_files(&locs);

    assert!(
        files.iter().any(|f| f == "Bar.kt"),
        "after full scan, find_references must return Bar.kt; got files: {:?}\n\
         workspace_root = {:?}",
        files,
        indexer.workspace_root.get()
    );
}
///
/// Concretely: workspace_root = `/tmp/other_project`, open file = in a
/// different tempdir.  `effective_rg_root` walks up from the file, finds no
/// `.git`, falls back to `file.parent()` (the correct tempdir) → Bar.kt found.
///
/// This catches the case where stale `workspace_source_roots` from the old
/// project "leak" into the search and scope rg to paths that don't contain
/// the current file's siblings.
#[tokio::test]
async fn find_references_stale_workspace_root_does_not_suppress_results() {
    let other_project = tempfile::tempdir().unwrap();
    let current_project = tempfile::tempdir().unwrap();

    let foo_src = "package com.example\nclass MyClass";
    let bar_src = "package com.example\nfun use(): MyClass = MyClass()";

    // Files live in `current_project`, but workspace_root points elsewhere.
    let (_foo_path, foo_uri) = write(current_project.path(), "Foo.kt", foo_src);
    let (_bar_path, _bar_uri) = write(current_project.path(), "Bar.kt", bar_src);

    let idx = Arc::new(Indexer::new());
    // workspace_root → wrong project; scoped_source_roots will be cleared
    // by rg_scope_for_path because effective_root != workspace_root.
    idx.workspace_root.set(other_project.path().to_path_buf());
    idx.index_content(&foo_uri, foo_src);

    let locs = find_references_with_qualifier("MyClass", None, &foo_uri, 1, true, &*idx).await;
    let files = hit_files(&locs);

    assert!(
        files.iter().any(|f| f == "Bar.kt"),
        "find_references must search the file's actual project when workspace_root \
         points to a different directory; got files: {:?}",
        files
    );
}

/// **Regression: nested Factory — declaration-site cursor should scope correctly**
///
/// When the cursor is ON the `class Factory` declaration line (no qualifier in
/// the source text), `on_decl=true` and `enclosing_class_at` must return the
/// parent class (`ReducerA`).  Without this the scope falls back to bare-word
/// search and bleeds across all reducers.
///
/// Also covers the annotation case: `@AssistedFactory\n interface Factory {` —
/// the annotation pushes the tree-sitter `interface_declaration` start row above
/// the `interface` keyword line, tricking `enclosing_class_at` into returning
/// `"Factory"` itself (start_row < cursor_row satisfied by Factory's own node).
/// The fix checks that the cursor is inside the class *body*, not the header.
#[tokio::test]
async fn find_references_nested_factory_from_declaration_site() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // @AssistedFactory is on line 2 (0-based), `interface Factory` on line 3.
    // This triggers the annotation-offset bug in enclosing_class_at.
    let reducer_a = "\
package com.example.a
class ReducerA {
    @SomeAnnotation
    interface Factory {
        fun create(): ReducerA
    }
}
";
    let reducer_b = "\
package com.example.b
class ReducerB {
    interface Factory {
        fun create(): ReducerB
    }
}
";
    let viewmodel = "\
package com.example
import com.example.a.ReducerA
import com.example.b.ReducerB
class ViewModel(
    private val reducerAFactory: ReducerA.Factory,
    private val reducerBFactory: ReducerB.Factory,
)
";
    let other_caller = "\
package com.example
import com.example.b.ReducerB
class OtherCaller(val f: ReducerB.Factory)
";

    write(root, "ReducerA.kt", reducer_a);
    write(root, "ReducerB.kt", reducer_b);
    write(root, "ViewModel.kt", viewmodel);
    write(root, "OtherCaller.kt", other_caller);
    std::fs::write(root.join("workspace.json"), r#"{"sourcePaths":[]}"#).unwrap();

    let (_, ra_uri) = (
        root.join("ReducerA.kt"),
        Url::from_file_path(root.join("ReducerA.kt")).unwrap(),
    );
    let (_, rb_uri) = (
        root.join("ReducerB.kt"),
        Url::from_file_path(root.join("ReducerB.kt")).unwrap(),
    );
    let (_, vm_uri) = (
        root.join("ViewModel.kt"),
        Url::from_file_path(root.join("ViewModel.kt")).unwrap(),
    );
    let (_, oc_uri) = (
        root.join("OtherCaller.kt"),
        Url::from_file_path(root.join("OtherCaller.kt")).unwrap(),
    );

    let idx = Arc::new(Indexer::new());
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&ra_uri, reducer_a);
    idx.index_content(&rb_uri, reducer_b);
    idx.index_content(&vm_uri, viewmodel);
    idx.index_content(&oc_uri, other_caller);

    // Cursor on `Factory` in `    interface Factory {` — line 3 (0-based) in ReducerA.kt
    // (line 2 is `@SomeAnnotation`).  No dot-qualifier → qualifier=None, on_decl=true.
    // enclosing_class_at must return "ReducerA", not "Factory".
    let locs = find_references_with_qualifier("Factory", None, &ra_uri, 3, false, &*idx).await;

    let files = hit_files(&locs);

    assert!(
        files.iter().any(|f| f == "ViewModel.kt"),
        "ReducerA.Factory usage in ViewModel.kt must be found; got: {:?}",
        files
    );
    assert!(
        !files.iter().any(|f| f == "OtherCaller.kt"),
        "OtherCaller.kt uses ReducerB.Factory and must NOT appear; got: {:?}",
        files
    );
}

/// **Regression: nested Factory scoped by qualifier**
///
/// Two classes `ReducerA` and `ReducerB` both have a nested `Factory` interface.
/// Class `ViewModel` injects `ReducerA.Factory` in its constructor.
/// The file does NOT import `ReducerA.Factory` directly — only `ReducerA`.
///
/// `find_references("Factory", …, qualifier=Some("ReducerA"))` must return
/// only usages of `ReducerA.Factory`, NOT every use of `ReducerB.Factory`
/// or bare `Factory` in other files.
///
/// Without the fix the qualifier is discarded, `declared_parent_class_of`
/// picks an arbitrary `Factory` definition from the index (non-deterministic
/// when multiple classes define `Factory`), and results bleed across the
/// whole project.
#[tokio::test]
async fn find_references_nested_factory_scoped_by_qualifier() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Two different reducers each with a nested Factory.
    let reducer_a = "\
package com.example.a
class ReducerA {
    interface Factory {
        fun create(): ReducerA
    }
}
";
    let reducer_b = "\
package com.example.b
class ReducerB {
    interface Factory {
        fun create(): ReducerB
    }
}
";
    // ViewModel uses ReducerA.Factory in its constructor.
    // No direct `import com.example.a.ReducerA.Factory` — only `import com.example.a.ReducerA`.
    let viewmodel = "\
package com.example
import com.example.a.ReducerA
import com.example.b.ReducerB
class ViewModel(
    private val reducerAFactory: ReducerA.Factory,
    private val reducerBFactory: ReducerB.Factory,
)
";
    // A second caller that uses ReducerB.Factory only.
    let other_caller = "\
package com.example
import com.example.b.ReducerB
class OtherCaller(val f: ReducerB.Factory)
";

    write(root, "ReducerA.kt", reducer_a);
    write(root, "ReducerB.kt", reducer_b);
    let (_, vm_uri) = write(root, "ViewModel.kt", viewmodel);
    write(root, "OtherCaller.kt", other_caller);

    // Write workspace.json to prevent scanning ~/.kotlin-lsp/sources.
    std::fs::write(root.join("workspace.json"), r#"{"sourcePaths":[]}"#).unwrap();

    let idx = Arc::new(Indexer::new());
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&vm_uri, viewmodel);

    // Index the companion files so `declared_parent_class_of` has both entries.
    let (_, ra_uri) = (
        root.join("ReducerA.kt"),
        Url::from_file_path(root.join("ReducerA.kt")).unwrap(),
    );
    let (_, rb_uri) = (
        root.join("ReducerB.kt"),
        Url::from_file_path(root.join("ReducerB.kt")).unwrap(),
    );
    let (_, oc_uri) = (
        root.join("OtherCaller.kt"),
        Url::from_file_path(root.join("OtherCaller.kt")).unwrap(),
    );
    idx.index_content(&ra_uri, reducer_a);
    idx.index_content(&rb_uri, reducer_b);
    idx.index_content(&oc_uri, other_caller);

    // Cursor is on `Factory` in `private val reducerAFactory: ReducerA.Factory`
    // (line 4, after the dot — qualifier = "ReducerA").
    // Line 4 (0-based) = `    private val reducerAFactory: ReducerA.Factory,`
    let locs =
        find_references_with_qualifier("Factory", Some("ReducerA"), &vm_uri, 4, false, &*idx).await;

    let files = hit_files(&locs);

    // Must find the ViewModel itself (it uses ReducerA.Factory).
    assert!(
        files.iter().any(|f| f == "ViewModel.kt"),
        "ReducerA.Factory usage in ViewModel.kt must be found; got: {:?}",
        files
    );
    // Must NOT bleed into OtherCaller (uses ReducerB.Factory, different class).
    assert!(
        !files.iter().any(|f| f == "OtherCaller.kt"),
        "OtherCaller.kt uses ReducerB.Factory and must NOT appear; got: {:?}",
        files
    );
}

/// **Regression: sibling qualifier bleed**
///
/// When a single file (`ViewModel.kt`) has BOTH `ReducerA.Factory` AND `ReducerC.Factory`
/// as constructor parameters, searching for references of `ReducerA.Factory` must not
/// include the line that has `ReducerC.Factory`.
///
/// Root cause: the bare-word step in `parent_scoped_reference_locations` searches for
/// `Factory` word-boundary in candidate files without checking whether a specific hit
/// has a *different* qualifier on the same line.
#[tokio::test]
async fn find_references_sibling_qualifier_does_not_bleed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let reducer_a = "\
package com.example.a
class ReducerA {
    interface Factory {
        fun create(): ReducerA
    }
}
";
    let reducer_b = "\
package com.example.b
class ReducerB {
    interface Factory {
        fun create(): ReducerB
    }
}
";
    let reducer_c = "\
package com.example.c
class ReducerC {
    interface Factory {
        fun create(): ReducerC
    }
}
";
    // ViewModel has BOTH ReducerA.Factory AND ReducerC.Factory as params.
    let viewmodel = "\
package com.example
import com.example.a.ReducerA
import com.example.b.ReducerB
import com.example.c.ReducerC
class ViewModel(
    private val reducerAFactory: ReducerA.Factory,
    private val reducerBFactory: ReducerB.Factory,
    private val reducerCFactory: ReducerC.Factory,
)
";

    write(root, "ReducerA.kt", reducer_a);
    write(root, "ReducerB.kt", reducer_b);
    write(root, "ReducerC.kt", reducer_c);
    let (_, vm_uri) = write(root, "ViewModel.kt", viewmodel);
    std::fs::write(root.join("workspace.json"), r#"{"sourcePaths":[]}"#).unwrap();

    let idx = Arc::new(Indexer::new());
    idx.workspace_root.set(root.to_path_buf());
    let ra_uri = Url::from_file_path(root.join("ReducerA.kt")).unwrap();
    let rb_uri = Url::from_file_path(root.join("ReducerB.kt")).unwrap();
    let rc_uri = Url::from_file_path(root.join("ReducerC.kt")).unwrap();
    idx.index_content(&ra_uri, reducer_a);
    idx.index_content(&rb_uri, reducer_b);
    idx.index_content(&rc_uri, reducer_c);
    idx.index_content(&vm_uri, viewmodel);

    // Search refs of ReducerA.Factory (qualifier = "ReducerA").
    // Line 5 = `    private val reducerAFactory: ReducerA.Factory,` (0-based)
    let locs =
        find_references_with_qualifier("Factory", Some("ReducerA"), &vm_uri, 5, false, &*idx).await;

    let lines: Vec<u32> = locs
        .iter()
        .filter(|l| l.uri == vm_uri)
        .map(|l| l.range.start.line)
        .collect();

    // Line 5 (ReducerA.Factory) must appear; lines 6 and 7 (ReducerB/C.Factory) must not.
    assert!(
        lines.contains(&5),
        "ReducerA.Factory line (5) must be found; got lines: {:?}",
        lines
    );
    assert!(
        !lines.contains(&7),
        "ReducerC.Factory line (7) must NOT appear in ReducerA.Factory search; got lines: {:?}",
        lines
    );
}

/// **Regression: lowercase method names at declaration site are scoped to package**
///
/// `fun create()` declared inside a nested `Factory` interface was previously
/// treated as "no scope" (lowercase early-return) and fell through to a
/// codebase-wide rg search, returning every file with `create` in the entire
/// workspace.
///
/// The fix: when cursor is at the declaration site (`on_decl=true`) of a lowercase
/// name, use the declaring file's package as the search scope instead of None.
#[tokio::test]
async fn find_references_lowercase_method_scoped_to_package_on_decl() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let reducer_a = "\
package com.example.a
class ReducerA {
    interface Factory {
        fun create(): ReducerA
    }
}
";
    // A totally unrelated file in a *different* package that also has `fun create`.
    let unrelated = "\
package com.unrelated
class Unrelated {
    fun create(): Unrelated = Unrelated()
}
";
    // Same-package caller that calls reducer factory.
    let caller = "\
package com.example.a
import com.example.a.ReducerA
fun buildReducer(f: ReducerA.Factory): ReducerA = f.create()
";

    let ra_uri = Url::from_file_path(root.join("ReducerA.kt")).unwrap();
    let unrelated_uri = Url::from_file_path(root.join("Unrelated.kt")).unwrap();
    let caller_uri = Url::from_file_path(root.join("Caller.kt")).unwrap();

    write(root, "ReducerA.kt", reducer_a);
    write(root, "Unrelated.kt", unrelated);
    write(root, "Caller.kt", caller);
    std::fs::write(root.join("workspace.json"), r#"{"sourcePaths":[]}"#).unwrap();

    let idx = Arc::new(Indexer::new());
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&ra_uri, reducer_a);
    idx.index_content(&unrelated_uri, unrelated);
    idx.index_content(&caller_uri, caller);

    // Cursor on `create` in `fun create(): ReducerA` — line 3 (0-based).
    let locs = find_references_with_qualifier("create", None, &ra_uri, 3, false, &*idx).await;

    let files = hit_files(&locs);

    // Same-package caller must be found (calls f.create()).
    assert!(
        files.iter().any(|f| f == "Caller.kt"),
        "Caller.kt (same package) must appear; got: {:?}",
        files
    );
    // Unrelated.kt in a different package must NOT be returned.
    assert!(
        !files.iter().any(|f| f == "Unrelated.kt"),
        "Unrelated.kt (different package) must NOT appear; got: {:?}",
        files
    );
}

/// **Regression: multi-segment qualifier is matched against the full extracted chain**
///
/// `word_and_qualifier_at` returns the full dot-chain, so for cursor on
/// `Factory` in `Outer.Inner.Factory` the qualifier is `"Outer.Inner"`, not
/// just `"Inner"`.  The old (whole-line) qualifier check extracted only the
/// *single* token immediately before the dot in each line, so
/// `"Inner" != "Outer.Inner"` caused every valid reference to be dropped (false
/// negatives).
///
/// The fix: `has_wrong_qualifier_at_col` walks backward over `[A-Za-z0-9_.]`
/// to extract the full dot-chain from the specific column of each hit, so
/// `"Outer.Inner" == "Outer.Inner"` matches correctly and valid references are
/// preserved.
#[tokio::test]
async fn find_references_multi_segment_qualifier_normalised() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Three-level nesting: Outer → Inner → Factory.
    let outer = "\
package com.example
class Outer {
    class Inner {
        interface Factory {
            fun create(): Inner
        }
    }
}
";
    // Another class has its own nested Factory that must NOT appear.
    let other = "\
package com.example
class Other {
    class Inner {
        interface Factory {
            fun create(): Other.Inner
        }
    }
}
";
    // Caller uses Outer.Inner.Factory — multi-segment qualifier.
    let caller = "\
package com.example
class Caller(val f: Outer.Inner.Factory)
";

    let outer_uri = Url::from_file_path(root.join("Outer.kt")).unwrap();
    let other_uri = Url::from_file_path(root.join("Other.kt")).unwrap();
    let caller_uri = Url::from_file_path(root.join("Caller.kt")).unwrap();

    write(root, "Outer.kt", outer);
    write(root, "Other.kt", other);
    write(root, "Caller.kt", caller);
    std::fs::write(root.join("workspace.json"), r#"{"sourcePaths":[]}"#).unwrap();

    let idx = Arc::new(Indexer::new());
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&outer_uri, outer);
    idx.index_content(&other_uri, other);
    idx.index_content(&caller_uri, caller);

    // Simulate what word_and_qualifier_at returns for cursor on `Factory`
    // in `class Caller(val f: Outer.Inner.Factory)`: qualifier = "Outer.Inner".
    let locs = find_references_with_qualifier(
        "Factory",
        Some("Outer.Inner"),
        &caller_uri,
        1, // line 1 (0-based): `class Caller(val f: Outer.Inner.Factory)`
        false,
        &*idx,
    )
    .await;

    let files = hit_files(&locs);

    // Caller.kt uses Outer.Inner.Factory — must be found.
    assert!(
        files.iter().any(|f| f == "Caller.kt"),
        "Caller.kt (uses Outer.Inner.Factory) must be found; got: {:?}",
        files
    );
    // Other.kt uses Other.Inner.Factory — must NOT appear (different qualifier).
    assert!(
        !files.iter().any(|f| f == "Other.kt"),
        "Other.kt (Other.Inner.Factory) must NOT appear; got: {:?}",
        files
    );
}

/// **Regression: `create()` inside nested Factory finds callers in parent package**
///
/// `fun create()` declared inside `ReducerA.Factory` must:
///   1. Return callers in a *parent* package that use variable-name syntax
///      (`reducerAFactory.create()`), and
///   2. NOT return `fun create` declarations in sibling factories that live in
///      the same package as `ReducerA`.
///
/// Root cause: package-scoped search (patterns matching `package com.example.a`)
/// finds all sibling factories in the same package → FPs, while callers in
/// `com.example` (parent) are outside the package scope → FNs.
///
/// Fix: `outer_class_for_decl_site` walks the CST chain to find that `create`
/// is inside `Factory` inside `ReducerA`; the outer class `ReducerA` is used for
/// file discovery so only files that reference `ReducerA` are searched.
#[tokio::test]
async fn find_references_nested_factory_create_finds_callers_not_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // ReducerA in package a with a nested Factory.
    let reducer_a = "\
package com.example.a
class ReducerA {
    interface Factory {
        fun create(): ReducerA
    }
}
";
    // Sibling in same package — also has Factory.create (must NOT appear).
    let reducer_b = "\
package com.example.a
class ReducerB {
    interface Factory {
        fun create(): ReducerB
    }
}
";
    // Caller in PARENT package that references ReducerA.Factory via a variable.
    let dashboard = "\
package com.example
import com.example.a.ReducerA
class Dashboard(private val reducerAFactory: ReducerA.Factory) {
    fun build() = reducerAFactory.create()
}
";
    // A file that imports ReducerA (so it appears in owner-class candidate files)
    // AND declares its own unrelated `fun create()` — must NOT appear as an FP.
    let reducer_c = "\
package com.example.a
import com.example.a.ReducerA
class ReducerC {
    interface Factory {
        fun create(): ReducerC
    }
    fun useA(f: ReducerA.Factory) = Unit
}
";

    let ra_uri = Url::from_file_path(root.join("ReducerA.kt")).unwrap();
    let rb_uri = Url::from_file_path(root.join("ReducerB.kt")).unwrap();
    let dash_uri = Url::from_file_path(root.join("Dashboard.kt")).unwrap();
    let rc_uri = Url::from_file_path(root.join("ReducerC.kt")).unwrap();

    write(root, "ReducerA.kt", reducer_a);
    write(root, "ReducerB.kt", reducer_b);
    write(root, "Dashboard.kt", dashboard);
    write(root, "ReducerC.kt", reducer_c);
    std::fs::write(root.join("workspace.json"), r#"{"sourcePaths":[]}"#).unwrap();

    let idx = Arc::new(Indexer::new());
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&ra_uri, reducer_a);
    idx.index_content(&rb_uri, reducer_b);
    idx.index_content(&dash_uri, dashboard);
    idx.index_content(&rc_uri, reducer_c);

    // Cursor on `create` in `fun create(): ReducerA` — line 3 (0-based).
    let locs = find_references_with_qualifier("create", None, &ra_uri, 3, false, &*idx).await;

    let files = hit_files(&locs);

    // Caller in parent package must be found.
    assert!(
        files.iter().any(|f| f == "Dashboard.kt"),
        "Dashboard.kt (parent-package caller) must appear; got: {:?}",
        files
    );
    // Sibling factory in same package — must NOT appear.
    assert!(
        !files.iter().any(|f| f == "ReducerB.kt"),
        "ReducerB.kt (sibling factory, same pkg) must NOT appear; got: {:?}",
        files
    );
    // File that imports ReducerA but declares its own create() — must NOT appear.
    assert!(
        !files.iter().any(|f| f == "ReducerC.kt"),
        "ReducerC.kt (imports ReducerA but declares own create) must NOT appear; got: {:?}",
        files
    );

    // Same assertions must hold when include_decl=true (LSP default).
    let locs_incl = find_references_with_qualifier("create", None, &ra_uri, 3, true, &*idx).await;
    let files_incl = hit_files(&locs_incl);
    assert!(
        files_incl.iter().any(|f| f == "ReducerA.kt"),
        "ReducerA.kt (declaration) must appear with include_decl=true; got: {:?}",
        files_incl
    );
    assert!(
        files_incl.iter().any(|f| f == "Dashboard.kt"),
        "Dashboard.kt must appear with include_decl=true; got: {:?}",
        files_incl
    );
    assert!(
        !files_incl.iter().any(|f| f == "ReducerC.kt"),
        "ReducerC.kt must NOT appear even with include_decl=true; got: {:?}",
        files_incl
    );
}
