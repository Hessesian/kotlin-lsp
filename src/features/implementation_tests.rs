//! End-to-end tests for [`find_implementation`].
//!
//! These tests write real `.kt` files to a temp directory so that `rg` can
//! search them, then drive the full `find_implementation` pipeline against a
//! real [`Indexer`] instance.
//!
//! Scenarios covered:
//! - **Interface method** — `goToImplementation` on `fun someMethod()` inside
//!   an interface must return the `override fun someMethod()` location in each
//!   implementing class, even in different packages.
//! - **Abstract class method** — same expectation for `abstract fun`.
//! - **Interface type** (regression guard) — the existing class-level behaviour
//!   must remain unbroken.

use std::sync::Arc;

use tower_lsp::lsp_types::{GotoDefinitionResponse, Location, Url};

use crate::features::implementation::find_implementation;
use crate::indexer::Indexer;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn write(dir: &std::path::Path, name: &str, content: &str) -> (std::path::PathBuf, Url) {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    let uri = Url::from_file_path(&path).unwrap();
    (path, uri)
}

fn response_files(resp: Option<GotoDefinitionResponse>) -> Vec<String> {
    let locs: Vec<Location> = match resp {
        Some(GotoDefinitionResponse::Scalar(l)) => vec![l],
        Some(GotoDefinitionResponse::Array(ls)) => ls,
        Some(GotoDefinitionResponse::Link(ls)) => ls
            .into_iter()
            .map(|l| Location {
                uri: l.target_uri,
                range: l.target_range,
            })
            .collect(),
        None => vec![],
    };
    locs.iter()
        .filter_map(|l| l.uri.to_file_path().ok())
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect()
}

// ─── tests ───────────────────────────────────────────────────────────────────

/// `goToImplementation` on a method declared in an interface must return the
/// `override fun` sites in all implementing classes, including those in
/// different packages.
///
/// Layout:
///   IRepo.kt  — `interface IRepo { fun load(): String }`
///   ImplA.kt  — `class ImplA : IRepo { override fun load() = "a" }`
///   ImplB.kt  — `class ImplB : IRepo { override fun load() = "b" }`
///   Unrelated.kt — `class Unrelated { fun load() = "x" }` (must NOT appear)
#[tokio::test]
async fn goto_implementation_interface_method_returns_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let irepo_src = "package com.example\ninterface IRepo {\n    fun load(): String\n}";
    let impl_a_src = "package com.example.a\nimport com.example.IRepo\nclass ImplA : IRepo {\n    override fun load() = \"a\"\n}";
    let impl_b_src = "package com.example.b\nimport com.example.IRepo\nclass ImplB : IRepo {\n    override fun load() = \"b\"\n}";
    let unrelated_src = "package com.other\nclass Unrelated {\n    fun load() = \"x\"\n}";

    let (_, irepo_uri) = write(root, "IRepo.kt", irepo_src);
    write(root, "ImplA.kt", impl_a_src);
    write(root, "ImplB.kt", impl_b_src);
    write(root, "Unrelated.kt", unrelated_src);

    let idx = Arc::new(Indexer::new());
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&irepo_uri, irepo_src);

    // "load" is the method name at cursor; declared inside IRepo
    let resp = find_implementation("load", &*idx, &irepo_uri, 2).await;
    let files = response_files(resp);

    assert!(
        files.iter().any(|f| f == "ImplA.kt"),
        "ImplA.kt must appear (implements IRepo.load); got: {:?}",
        files
    );
    assert!(
        files.iter().any(|f| f == "ImplB.kt"),
        "ImplB.kt must appear (implements IRepo.load); got: {:?}",
        files
    );
    assert!(
        !files.iter().any(|f| f == "Unrelated.kt"),
        "Unrelated.kt must NOT appear (does not implement IRepo); got: {:?}",
        files
    );
}

/// `goToImplementation` on an abstract method must return the concrete
/// `override fun` sites in subclasses.
///
/// Layout:
///   Base.kt   — `abstract class Base { abstract fun compute(): Int }`
///   Child.kt  — `class Child : Base() { override fun compute() = 42 }`
#[tokio::test]
async fn goto_implementation_abstract_method_returns_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let base_src = "package com.example\nabstract class Base {\n    abstract fun compute(): Int\n}";
    let child_src = "package com.example\nimport com.example.Base\nclass Child : Base() {\n    override fun compute() = 42\n}";

    let (_, base_uri) = write(root, "Base.kt", base_src);
    write(root, "Child.kt", child_src);

    let idx = Arc::new(Indexer::new());
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&base_uri, base_src);

    let resp = find_implementation("compute", &*idx, &base_uri, 2).await;
    let files = response_files(resp);

    assert!(
        files.iter().any(|f| f == "Child.kt"),
        "Child.kt must appear (overrides Base.compute); got: {:?}",
        files
    );
}

/// Regression guard: `goToImplementation` on the interface **type itself** (not
/// a method) must still return the implementing class locations as before.
///
/// Layout:
///   IService.kt — `interface IService`
///   Impl.kt     — `class Impl : IService`
#[tokio::test]
async fn goto_implementation_interface_type_unbroken() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let iservice_src = "package com.example\ninterface IService";
    let impl_src = "package com.example\nclass Impl : IService";

    let (_, iservice_uri) = write(root, "IService.kt", iservice_src);
    write(root, "Impl.kt", impl_src);

    let idx = Arc::new(Indexer::new());
    idx.workspace_root.set(root.to_path_buf());
    idx.index_content(&iservice_uri, iservice_src);

    // Index the implementor so the subtype graph is populated
    let impl_uri = Url::from_file_path(root.join("Impl.kt")).unwrap();
    idx.index_content(&impl_uri, impl_src);

    // "IService" is a type (not a method); line is the interface declaration line.
    let resp = find_implementation("IService", &*idx, &iservice_uri, 1).await;
    let files = response_files(resp);

    assert!(
        files.iter().any(|f| f == "Impl.kt"),
        "Impl.kt must appear (implements IService); got: {:?}",
        files
    );
}
