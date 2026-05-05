//! Tests for the `rg` module — extracted from `indexer.rs` tests.
//!
//! Referenced from `src/rg.rs` via:
//! ```rust
//! #[cfg(test)]
//! #[path = "rg_tests.rs"]
//! mod tests;
//! ```

use tower_lsp::lsp_types::Url;

use crate::rg::{parse_rg_line, rg_find_definition, rg_find_references, IgnoreMatcher};

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
    let locs = rg_find_references(
        "Event",
        Some("ActivateUpdateAppContract"),
        Some("com.example.activate"), // declared_pkg
        Some(root),
        true, // include_declaration
        &activate_uri,
        &[activate_decl],
        None, // no ignore patterns in this test
    );

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

    let locs = rg_find_references(
        "Event",
        Some("OtherContract"),
        Some("com.example.other"),
        Some(root),
        true,
        &other_vm_uri,
        &[other_decl],
        None,
    );

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

    let locs = rg_find_references(
        "Event",
        Some("DashboardContract"),
        Some("com.example.dashboard"),
        Some(root),
        true,
        &dashboard_uri,
        &[dashboard_decl], // NOT including VisitBranchContract.kt
        None,
    );

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
    let locs = rg_find_definition("MyClass", Some(root), Some(&matcher));
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

    let locs = rg_find_references(
        "Event",
        Some("Contract"),
        Some("com.example"),
        Some(root),
        true,
        &uri,
        &[decl],
        Some(&matcher),
    );
    let files: Vec<String> = locs
        .iter()
        .map(|l| l.uri.to_file_path().unwrap().to_string_lossy().into_owned())
        .collect();

    assert!(
        !files.iter().any(|f| f.contains("buildSrc")),
        "must not include buildSrc in references; got: {files:?}"
    );
}
