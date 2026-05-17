//! Tests for the `rg` module — extracted from `indexer.rs` tests.
//!
//! Referenced from `src/rg.rs` via:
//! ```rust
//! #[cfg(test)]
//! #[path = "rg_tests.rs"]
//! mod tests;
//! ```

use tower_lsp::lsp_types::Url;

use crate::rg::{
    is_declaration_of, parse_rg_line, rg_find_definition, rg_find_references, IgnoreMatcher,
    RgSearchRequest,
};

// ─── parse_rg_line ────────────────────────────────────────────────────────────

#[test]
fn rg_line_absolute_path_parsed() {
    let line = "/home/user/project/Foo.kt:10:5:class Foo {";
    let loc = parse_rg_line(line).unwrap();
    assert_eq!(loc.range.start.line, 9); // 1-indexed → 0-indexed
    assert_eq!(loc.range.start.character, 4);
    assert_eq!(loc.uri.path(), "/home/user/project/Foo.kt");
}

#[test]
fn rg_line_relative_path_ignored() {
    // Before the fix this would panic / produce a wrong URI
    let line = "src/Foo.kt:10:5:class Foo {";
    assert!(
        parse_rg_line(line).is_none(),
        "relative paths must be ignored"
    );
}

// ─── rg_find_references scoping ──────────────────────────────────────────────

/// Write `content` to `dir/rel_path` and return the absolute path as String.
fn write_temp(dir: &std::path::Path, rel_path: &str, content: &str) -> String {
    let p = dir.join(rel_path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&p, content).unwrap();
    p.to_str().unwrap().to_owned()
}

/// `rg_find_references` must not bleed references across sealed interfaces
/// that share the same inner name (`Event`) but belong to different contracts.
///
/// Layout:
///   activate_contract.kt   — declares interface ActivateUpdateAppContract { sealed interface Event }
///   other_contract.kt      — declares interface OtherContract             { sealed interface Event }
///   activate_vm.kt         — imports ActivateUpdateAppContract.Event, uses bare `Event`
///   other_vm.kt            — imports OtherContract.Event,             uses bare `Event`
///
/// Finding refs for ActivateUpdateAppContract.Event must return hits in
/// activate_contract.kt and activate_vm.kt ONLY — not other_vm.kt.
#[test]
fn refs_inner_class_does_not_bleed_across_contracts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    write_temp(
        root,
        "activate_contract.kt",
        concat!(
            "package com.example.activate\n",
            "interface ActivateUpdateAppContract {\n",
            "  sealed interface Event\n",
            "}\n",
        ),
    );
    write_temp(
        root,
        "other_contract.kt",
        concat!(
            "package com.example.other\n",
            "interface OtherContract {\n",
            "  sealed interface Event\n",
            "}\n",
        ),
    );
    write_temp(
        root,
        "activate_vm.kt",
        concat!(
            "package com.example.activate\n",
            "import com.example.activate.ActivateUpdateAppContract.Event\n",
            "class ActivateViewModel {\n",
            "  fun handle(e: Event) {}\n",
            "}\n",
        ),
    );
    write_temp(
        root,
        "other_vm.kt",
        concat!(
            "package com.example.other\n",
            "import com.example.other.OtherContract.Event\n",
            "class OtherViewModel {\n",
            "  fun handle(e: Event) {}\n",
            "}\n",
        ),
    );

    let activate_uri = Url::from_file_path(root.join("activate_contract.kt")).unwrap();
    let activate_decl = root
        .join("activate_contract.kt")
        .to_str()
        .unwrap()
        .to_owned();

    // Simulate: cursor on declaration of Event inside ActivateUpdateAppContract.
    // parent_class = "ActivateUpdateAppContract", declared_pkg = "com.example.activate"
    let decl_files = [activate_decl];
    let request = RgSearchRequest::new(
        "Event",
        Some("ActivateUpdateAppContract"),
        Some("com.example.activate"), // declared_pkg
        Some(root),
        true, // include_declaration
        &activate_uri,
        &decl_files,
    );
    let locs = rg_find_references(&request, None);

    let hit_files: std::collections::HashSet<String> = locs
        .iter()
        .map(|l| {
            l.uri
                .to_file_path()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();

    assert!(
        hit_files.contains("activate_contract.kt"),
        "should include declaration file; got: {hit_files:?}"
    );
    assert!(
        hit_files.contains("activate_vm.kt"),
        "should include file that imports ActivateUpdateAppContract.Event; got: {hit_files:?}"
    );
    assert!(
        !hit_files.contains("other_vm.kt"),
        "must NOT include file that only imports OtherContract.Event; got: {hit_files:?}"
    );
    assert!(
        !hit_files.contains("other_contract.kt"),
        "must NOT include OtherContract declaration; got: {hit_files:?}"
    );
}

/// When cursor is on `Event` inside a file that imports `OtherContract.Event`,
/// refs must not include files that only import `ActivateUpdateAppContract.Event`.
#[test]
fn refs_inner_class_resolved_from_import_in_reference_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    write_temp(
        root,
        "activate_contract.kt",
        concat!(
            "package com.example.activate\n",
            "interface ActivateUpdateAppContract {\n",
            "  sealed interface Event\n",
            "}\n",
        ),
    );
    write_temp(
        root,
        "other_contract.kt",
        concat!(
            "package com.example.other\n",
            "interface OtherContract {\n",
            "  sealed interface Event\n",
            "}\n",
        ),
    );
    write_temp(
        root,
        "activate_vm.kt",
        concat!(
            "package com.example.activate\n",
            "import com.example.activate.ActivateUpdateAppContract.Event\n",
            "class ActivateViewModel {\n",
            "  fun handle(e: Event) {}\n",
            "}\n",
        ),
    );
    write_temp(
        root,
        "other_vm.kt",
        concat!(
            "package com.example.other\n",
            "import com.example.other.OtherContract.Event\n",
            "class OtherViewModel {\n",
            "  fun handle(e: Event) {}\n",
            "}\n",
        ),
    );

    // Simulate: cursor on `Event` inside other_vm.kt (a reference, not declaration).
    // resolve_symbol_via_import on other_vm.kt → parent=OtherContract, pkg=com.example.other
    let other_vm_uri = Url::from_file_path(root.join("other_vm.kt")).unwrap();
    let other_decl = root.join("other_contract.kt").to_str().unwrap().to_owned();

    let decl_files = [other_decl];
    let request = RgSearchRequest::new(
        "Event",
        Some("OtherContract"),
        Some("com.example.other"),
        Some(root),
        true,
        &other_vm_uri,
        &decl_files,
    );
    let locs = rg_find_references(&request, None);

    let hit_files: std::collections::HashSet<String> = locs
        .iter()
        .map(|l| {
            l.uri
                .to_file_path()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();

    assert!(
        hit_files.contains("other_contract.kt"),
        "should include OtherContract declaration; got: {hit_files:?}"
    );
    assert!(
        hit_files.contains("other_vm.kt"),
        "should include file importing OtherContract.Event; got: {hit_files:?}"
    );
    assert!(
        !hit_files.contains("activate_vm.kt"),
        "must NOT include file importing ActivateUpdateAppContract.Event; got: {hit_files:?}"
    );
}

/// Regression: when `decl_files` is unfiltered it includes ALL contracts that
/// declare a `sealed interface Event`, causing every consumer ViewModel to appear
/// in results for an unrelated contract's Event.
///
/// Layout: two contracts each with `sealed interface Event`, two ViewModels each
/// importing their own contract's Event.  Finding refs for DashboardContract.Event
/// must NOT return VisitBranchViewModel even though both are in `decl_files` when
/// unfiltered by enclosing-class.
#[test]
fn refs_decl_files_filtered_by_enclosing_class() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    write_temp(
        root,
        "DashboardContract.kt",
        concat!(
            "package com.example.dashboard\n",
            "interface DashboardContract {\n",
            "  sealed interface Event\n",
            "}\n",
        ),
    );
    write_temp(
        root,
        "VisitBranchContract.kt",
        concat!(
            "package com.example.visitbranch\n",
            "interface VisitBranchContract {\n",
            "  sealed interface Event\n",
            "}\n",
        ),
    );
    write_temp(
        root,
        "DashboardViewModel.kt",
        concat!(
            "package com.example.dashboard\n",
            "import com.example.dashboard.DashboardContract.Event\n",
            "class DashboardViewModel {\n",
            "  fun handle(e: Event) {}\n",
            "}\n",
        ),
    );
    write_temp(
        root,
        "VisitBranchViewModel.kt",
        concat!(
            "package com.example.visitbranch\n",
            "import com.example.visitbranch.VisitBranchContract.Event\n",
            "class VisitBranchViewModel {\n",
            "  fun handle(e: Event) {}\n",
            "}\n",
        ),
    );

    let dashboard_uri = Url::from_file_path(root.join("DashboardContract.kt")).unwrap();
    // decl_files filtered to only DashboardContract.kt (enclosing = DashboardContract)
    let dashboard_decl = root
        .join("DashboardContract.kt")
        .to_str()
        .unwrap()
        .to_owned();

    let decl_files = [dashboard_decl];
    let request = RgSearchRequest::new(
        "Event",
        Some("DashboardContract"),
        Some("com.example.dashboard"),
        Some(root),
        true,
        &dashboard_uri,
        &decl_files, // NOT including VisitBranchContract.kt
    );
    let locs = rg_find_references(&request, None);

    let hit_files: std::collections::HashSet<String> = locs
        .iter()
        .map(|l| {
            l.uri
                .to_file_path()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();

    assert!(
        hit_files.contains("DashboardContract.kt"),
        "should include DashboardContract declaration; got: {hit_files:?}"
    );
    assert!(
        hit_files.contains("DashboardViewModel.kt"),
        "should include DashboardViewModel; got: {hit_files:?}"
    );
    assert!(
        !hit_files.contains("VisitBranchViewModel.kt"),
        "must NOT include VisitBranchViewModel; got: {hit_files:?}"
    );
    assert!(
        !hit_files.contains("VisitBranchContract.kt"),
        "must NOT include VisitBranchContract; got: {hit_files:?}"
    );
}

// ─── rg_find_definition / rg_find_references ignore-pattern filtering ─────────

/// `rg_find_definition` must not return results from ignored directories.
#[test]
fn rg_find_definition_filters_ignored_dirs() {
    let dir = tempfile::TempDir::new().expect("create tempdir");
    let root = dir.path();

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/Real.kt"),
        "package com.example\nclass MyClass\n",
    )
    .unwrap();

    std::fs::create_dir_all(root.join("buildSrc/generated")).unwrap();
    std::fs::write(
        root.join("buildSrc/generated/MyClass.kt"),
        "package com.example\nclass MyClass\n",
    )
    .unwrap();

    let matcher = IgnoreMatcher::new(vec!["buildSrc".to_owned()], root);
    let locs = rg_find_definition("MyClass", Some(root), &[], Some(&matcher));
    let files: Vec<String> = locs
        .iter()
        .map(|l| l.uri.to_file_path().unwrap().to_string_lossy().into_owned())
        .collect();

    assert!(
        files.iter().any(|f| f.contains("src/Real.kt")),
        "must include real source; got: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.contains("buildSrc")),
        "must not include buildSrc results; got: {files:?}"
    );
}

/// `rg_find_references` must exclude candidate files from ignored directories.
#[test]
fn rg_find_references_filters_ignored_dirs() {
    let dir = tempfile::TempDir::new().expect("create tempdir");
    let root = dir.path();

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/Contract.kt"),
        "package com.example\nclass Contract {\n  class Event\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/User.kt"),
        "package com.example\nimport com.example.Contract.Event\nfun use(e: Event) {}\n",
    )
    .unwrap();

    std::fs::create_dir_all(root.join("buildSrc")).unwrap();
    std::fs::write(
        root.join("buildSrc/Contract.kt"),
        "package com.example\nclass Contract {\n  class Event\n}\n",
    )
    .unwrap();

    let uri = Url::from_file_path(root.join("src/Contract.kt")).unwrap();
    let decl = root.join("src/Contract.kt").to_str().unwrap().to_owned();
    let matcher = IgnoreMatcher::new(vec!["buildSrc".to_owned()], root);

    let decl_files = [decl];
    let request = RgSearchRequest::new(
        "Event",
        Some("Contract"),
        Some("com.example"),
        Some(root),
        true,
        &uri,
        &decl_files,
    );
    let locs = rg_find_references(&request, Some(&matcher));
    let files: Vec<String> = locs
        .iter()
        .map(|l| l.uri.to_file_path().unwrap().to_string_lossy().into_owned())
        .collect();

    assert!(
        !files.iter().any(|f| f.contains("buildSrc")),
        "must not include buildSrc in references; got: {files:?}"
    );
}

/// `rg_find_definition` with non-empty `source_paths` must only return results
/// from within those directories, not from the full workspace root.
#[test]
fn rg_find_definition_scoped_to_source_paths() {
    let dir = tempfile::TempDir::new().expect("create tempdir");
    let root = dir.path();

    std::fs::create_dir_all(root.join("app/src/main/kotlin")).unwrap();
    std::fs::write(root.join("app/src/main/kotlin/Foo.kt"), "class Foo\n").unwrap();

    // A second directory that should NOT be searched when source_paths is set.
    std::fs::create_dir_all(root.join("generated")).unwrap();
    std::fs::write(root.join("generated/Foo.kt"), "class Foo\n").unwrap();

    let source_path = root
        .join("app/src/main/kotlin")
        .to_string_lossy()
        .into_owned();
    let source_paths = vec![source_path.clone()];

    let locs = rg_find_definition("Foo", Some(root), &source_paths, None);
    let files: Vec<String> = locs
        .iter()
        .map(|l| l.uri.to_file_path().unwrap().to_string_lossy().into_owned())
        .collect();

    assert!(
        !files.is_empty(),
        "must find Foo inside the configured source_path; got nothing"
    );
    assert!(
        files.iter().all(|f| f.contains("app/src/main/kotlin")),
        "must only return results from source_paths; got: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.contains("generated")),
        "must not include files outside source_paths; got: {files:?}"
    );
}

/// `rg_find_definition` with multiple source_paths searches ALL of them.
/// Regression test for GitHub issue #78: rg only searched one source root.
#[test]
fn rg_find_definition_searches_all_source_paths() {
    let dir = tempfile::TempDir::new().expect("create tempdir");
    let root = dir.path();

    // Two separate source roots
    std::fs::create_dir_all(root.join("frameworks/base/src")).unwrap();
    std::fs::write(
        root.join("frameworks/base/src/PolicyHandle.kt"),
        "class PolicyHandle\n",
    )
    .unwrap();

    std::fs::create_dir_all(root.join("cts/src")).unwrap();
    std::fs::write(
        root.join("cts/src/PolicyIdentifier.kt"),
        "class PolicyIdentifier\n",
    )
    .unwrap();

    let source_paths = vec![
        root.join("frameworks/base/src")
            .to_string_lossy()
            .into_owned(),
        root.join("cts/src").to_string_lossy().into_owned(),
    ];

    // Search for a symbol in source root #1
    let locs = rg_find_definition("PolicyHandle", Some(root), &source_paths, None);
    assert!(
        !locs.is_empty(),
        "must find PolicyHandle in frameworks/base"
    );

    // Search for a symbol in source root #2
    let locs = rg_find_definition("PolicyIdentifier", Some(root), &source_paths, None);
    assert!(
        !locs.is_empty(),
        "must find PolicyIdentifier in cts (second source root)"
    );
}

/// `rg_find_definition` with empty `source_paths` falls back to searching the
/// entire workspace root (backward-compatible behavior).
#[test]
fn rg_find_definition_empty_source_paths_falls_back_to_root() {
    let dir = tempfile::TempDir::new().expect("create tempdir");
    let root = dir.path();

    std::fs::create_dir_all(root.join("app/src/main/kotlin")).unwrap();
    std::fs::write(root.join("app/src/main/kotlin/Bar.kt"), "class Bar\n").unwrap();

    // With empty source_paths, should find via workspace root scan.
    let locs = rg_find_definition("Bar", Some(root), &[], None);
    assert!(
        !locs.is_empty(),
        "must find Bar when source_paths is empty (fallback to root)"
    );
}

/// `rg_find_references` with `with_source_paths` must limit candidate-file
/// discovery and reference search to the configured source root.
#[test]
fn rg_find_references_scoped_to_source_paths() {
    let dir = tempfile::TempDir::new().expect("create tempdir");
    let root = dir.path();

    // Source root: contains the declaration and a legitimate reference.
    std::fs::create_dir_all(root.join("app/src/main/kotlin/com/example")).unwrap();
    std::fs::write(
        root.join("app/src/main/kotlin/com/example/Contract.kt"),
        "package com.example\nclass Contract {\n  class Event\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/src/main/kotlin/com/example/User.kt"),
        "package com.example\nfun use(e: Contract.Event) {}\n",
    )
    .unwrap();

    // Outside source root: has a usage of Contract.Event — must NOT appear in scoped results.
    // Without scoping, rg_find_references would return this file; with scoping it must be excluded.
    std::fs::create_dir_all(root.join("generated/com/example")).unwrap();
    std::fs::write(
        root.join("generated/com/example/OutsideUser.kt"),
        "package com.example\nfun outsideUse(e: Contract.Event) {}\n",
    )
    .unwrap();

    let source_path = root
        .join("app/src/main/kotlin")
        .to_string_lossy()
        .into_owned();
    let source_paths = vec![source_path];

    let decl_uri =
        Url::from_file_path(root.join("app/src/main/kotlin/com/example/Contract.kt")).unwrap();
    let decl_file = root
        .join("app/src/main/kotlin/com/example/Contract.kt")
        .to_string_lossy()
        .into_owned();
    let decl_files = [decl_file];

    let request = RgSearchRequest::new(
        "Event",
        Some("Contract"),
        Some("com.example"),
        Some(root),
        true,
        &decl_uri,
        &decl_files,
    )
    .with_source_paths(&source_paths);

    let locs = rg_find_references(&request, None);
    let files: Vec<String> = locs
        .iter()
        .map(|l| l.uri.to_file_path().unwrap().to_string_lossy().into_owned())
        .collect();

    assert!(
        !files.is_empty(),
        "must find references inside configured source_paths; got nothing"
    );
    assert!(
        !files.iter().any(|f| f.contains("generated")),
        "must not include files outside source_paths (generated/); got: {files:?}"
    );
}

// ─── is_declaration_of ────────────────────────────────────────────────────────

#[test]
fn is_declaration_of_matches_exact_name() {
    assert!(is_declaration_of("    fun create(): Foo", "create"));
    assert!(is_declaration_of("    fun create(x: Int): Foo", "create"));
    assert!(is_declaration_of("val create: Factory", "create"));
}

#[test]
fn is_declaration_of_rejects_longer_name_with_same_prefix() {
    // "fun createWidget" must NOT be treated as a declaration of "create"
    assert!(!is_declaration_of(
        "    fun createWidget(): Widget",
        "create"
    ));
    assert!(!is_declaration_of(
        "    fun createReducer() = factory.create()",
        "create"
    ));
    assert!(!is_declaration_of(
        "    fun createAccount(name: String): Account",
        "create"
    ));
}

#[test]
fn is_declaration_of_rejects_call_site_in_non_declaration() {
    assert!(!is_declaration_of("    val x = factory.create()", "create"));
    assert!(!is_declaration_of(
        "    fun build() = factory.create()",
        "create"
    ));
}
