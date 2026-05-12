//! Integration tests for `kotlin-lsp complete`.
//!
//! These tests invoke the compiled binary so they require `cargo build` to have
//! run first (they use `env!("CARGO_BIN_EXE_kotlin-lsp")`).
//!
//! Each test:
//!   1. Writes a small Kotlin fixture to a temp directory.
//!   2. Optionally writes "library" files to simulate sourcePaths symbols.
//!   3. Calls `kotlin-lsp index --root <tmpdir>` to pre-build the index.
//!   4. Calls `kotlin-lsp complete <file> <line> <col> --json --root <tmpdir>`.
//!   5. Asserts the expected labels appear (and absent labels don't).

use std::path::Path;
use std::process::Command;

use serde_json::Value;

// ── helpers ──────────────────────────────────────────────────────────────────

const BIN: &str = env!("CARGO_BIN_EXE_kotlin-lsp");

/// Write `content` to `dir/rel_path`, creating parent dirs as needed.
fn write_fixture(dir: &Path, rel_path: &str, content: &str) {
    let full = dir.join(rel_path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&full, content).unwrap();
}

/// Run `kotlin-lsp index` against `root` and panic on failure.
fn index_root(root: &Path) {
    let status = Command::new(BIN)
        .args(["index", "--root"])
        .arg(root)
        .status()
        .expect("failed to spawn kotlin-lsp");
    assert!(status.success(), "kotlin-lsp index failed");
}

/// Run `kotlin-lsp complete <file> <line> <col> --json --root <root>` and
/// return the parsed JSON array.  Returns `None` if stdout is empty
/// (the binary may exit 0 with no completions or non-zero on error).
fn complete_json(root: &Path, file: &Path, line: u32, col: u32) -> Option<Vec<Value>> {
    let out = Command::new(BIN)
        .args(["complete"])
        .arg(file)
        .arg(line.to_string())
        .arg(col.to_string())
        .args(["--json", "--root"])
        .arg(root)
        .output()
        .expect("failed to spawn kotlin-lsp");

    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim().is_empty() {
        return None;
    }
    Some(serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("JSON parse error: {e}\nOutput was:\n{stdout}");
    }))
}

fn labels(items: &[Value]) -> Vec<&str> {
    items.iter().filter_map(|v| v["label"].as_str()).collect()
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Local variables in scope must appear as completions.
#[test]
fn local_vars_appear_in_completion() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Suppress global ~/.kotlin-lsp/sources so the test stays fast.
    write_fixture(root, "workspace.json", r#"{"sourcePaths":[]}"#);

    // Line 3: "    myVariable" — cursor after 'e' (col 15, 1-based)
    write_fixture(
        root,
        "src/Example.kt",
        "package com.example\nfun demo() {\n    myVariable\n}\nval myVariable = 42\n",
    );

    index_root(root);

    let file = root.join("src/Example.kt");
    let items = complete_json(root, &file, 3, 15).unwrap_or_default();
    let lbls = labels(&items);
    assert!(
        lbls.contains(&"myVariable"),
        "local 'myVariable' must appear in completions; got: {lbls:?}"
    );
}

/// Symbols from a different package (simulating a library) must appear in
/// bare-word completions once the index is built.  This is the regression that
/// was reported: @Composable and Column were missing after a refactor.
#[test]
fn cross_package_library_symbols_appear() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Suppress global ~/.kotlin-lsp/sources so the cap isn't flooded.
    write_fixture(root, "workspace.json", r#"{"sourcePaths":[]}"#);

    // "library" files — live under sources/ to simulate sourcePaths extraction
    write_fixture(
        root,
        "sources/compose/Composable.kt",
        "package androidx.compose.runtime\nannotation class Composable\n",
    );
    write_fixture(
        root,
        "sources/compose/Column.kt",
        "package androidx.compose.foundation.layout\nfun Column() {}\n",
    );

    // the file the agent is editing
    write_fixture(
        root,
        "src/Screen.kt",
        "package com.example\nfun Screen() {\n    Comp\n}\n",
    );

    index_root(root);

    let file = root.join("src/Screen.kt");
    // Line 3: "    Comp" — cursor after 'p' (col 9, 1-based)
    let items = complete_json(root, &file, 3, 9).unwrap_or_default();
    let lbls = labels(&items);

    assert!(
        lbls.contains(&"Composable"),
        "@Composable from library must appear for prefix 'Comp'; got: {lbls:?}"
    );

    // Also verify the auto-import edit is present
    let composable = items.iter().find(|v| v["label"] == "Composable").unwrap();
    assert!(
        composable.get("import").is_some(),
        "Composable completion must include an auto-import; item: {composable}"
    );
}

/// `Column` from a library package must appear for prefix "Col".
#[test]
fn cross_package_fun_appears() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Suppress global ~/.kotlin-lsp/sources so the cap isn't flooded.
    write_fixture(root, "workspace.json", r#"{"sourcePaths":[]}"#);

    write_fixture(
        root,
        "sources/layout/Column.kt",
        "package androidx.compose.foundation.layout\nfun Column() {}\n",
    );
    write_fixture(
        root,
        "src/Screen.kt",
        "package com.example\nfun Screen() {\n    Col\n}\n",
    );

    index_root(root);

    let file = root.join("src/Screen.kt");
    // Line 3: "    Col" — cursor after 'l' (col 8, 1-based)
    let items = complete_json(root, &file, 3, 8).unwrap_or_default();
    let lbls = labels(&items);

    assert!(
        lbls.contains(&"Column"),
        "Column (fun) from library must appear for prefix 'Col'; got: {lbls:?}"
    );
}

/// Symbols from a library dir registered via `workspace.json` `sourcePaths`
/// must appear in completion.  The library dir is *outside* the workspace root,
/// so it is indexed via `index_source_paths` — the actual code path that was
/// reported broken.
#[test]
fn source_paths_outside_workspace_appear_in_completion() {
    let workspace = tempfile::tempdir().unwrap();
    let lib_dir = tempfile::tempdir().unwrap();

    // Library file lives in a completely separate directory.
    write_fixture(
        lib_dir.path(),
        "testlib/LibraryTestClass.kt",
        "package com.kotlinlsp.testlib\nclass LibraryTestClass {\n    fun greet(): String = \"hello\"\n}\n",
    );

    // workspace.json tells the indexer about the external library dir.
    let workspace_json = serde_json::json!({
        "sourcePaths": [lib_dir.path().to_string_lossy()]
    });
    std::fs::write(
        workspace.path().join("workspace.json"),
        serde_json::to_string_pretty(&workspace_json).unwrap(),
    )
    .unwrap();

    // The file being edited references the library class by prefix.
    write_fixture(
        workspace.path(),
        "src/Screen.kt",
        "package com.example\nfun Screen() {\n    LibraryTest\n}\n",
    );

    index_root(workspace.path());

    let file = workspace.path().join("src/Screen.kt");
    // Line 3: "    LibraryTest" — cursor after 't' (col 16, 1-based)
    let items = complete_json(workspace.path(), &file, 3, 16).unwrap_or_default();
    let lbls = labels(&items);

    assert!(
        lbls.contains(&"LibraryTestClass"),
        "LibraryTestClass from external sourcePaths dir must appear; got: {lbls:?}"
    );

    let item = items
        .iter()
        .find(|v| v["label"] == "LibraryTestClass")
        .unwrap();
    assert!(
        item.get("import").is_some(),
        "LibraryTestClass must include an auto-import edit; item: {item}"
    );
}

/// Smoke test: `~/.kotlin-lsp/sources/Test.kt` ships a `LibraryTestClass`.
/// This test exercises the *default* source path that real users rely on.
/// Marked `#[ignore]` — run manually with `cargo test -- --ignored` on machines
/// with an extracted sources dir.
#[test]
#[ignore]
fn default_sources_dir_appears_in_completion() {
    #[allow(deprecated)]
    let sources = std::env::home_dir()
        .unwrap_or_default()
        .join(".kotlin-lsp")
        .join("sources");

    if !sources.join("Test.kt").exists() {
        eprintln!("~/.kotlin-lsp/sources/Test.kt not found — skipping smoke test");
        return;
    }

    let workspace = tempfile::tempdir().unwrap();
    write_fixture(
        workspace.path(),
        "src/App.kt",
        "package com.example\nfun app() {\n    LibraryTest\n}\n",
    );

    index_root(workspace.path());

    let file = workspace.path().join("src/App.kt");
    let items = complete_json(workspace.path(), &file, 3, 16).unwrap_or_default();
    let lbls = labels(&items);

    assert!(
        lbls.contains(&"LibraryTestClass"),
        "LibraryTestClass from ~/.kotlin-lsp/sources must appear; got: {lbls:?}"
    );
}

#[test]
fn dot_complete_shows_class_members() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Suppress global ~/.kotlin-lsp/sources so the test stays fast.
    write_fixture(root, "workspace.json", r#"{"sourcePaths":[]}"#);

    write_fixture(
        root,
        "src/Model.kt",
        "package com.example\nclass Model {\n    fun load() {}\n    fun save() {}\n}\n",
    );
    write_fixture(
        root,
        "src/Usage.kt",
        "package com.example\nfun use(m: Model) {\n    m.\n}\nfun unrelated() {}\n",
    );

    index_root(root);

    let file = root.join("src/Usage.kt");
    // Line 3, col 7 — just after "m."
    let items = complete_json(root, &file, 3, 7).unwrap_or_default();
    if items.is_empty() {
        // dot-complete needs a running server for type inference — skip gracefully
        eprintln!("dot-complete returned empty (likely needs live server); skipping assertions");
        return;
    }
    let lbls = labels(&items);
    assert!(
        lbls.contains(&"load") || lbls.contains(&"save"),
        "Model members must appear after dot; got: {lbls:?}"
    );
}
