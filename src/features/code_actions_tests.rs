use tower_lsp::lsp_types::Url;

use super::{build_add_package_action, find_package_insert_line, resolve_package_from_path};

// ─── missing_package_diagnostic ──────────────────────────────────────────────

#[test]
fn test_diagnostic_fires_for_missing_package() {
    use super::missing_package_diagnostic;
    let lines: Vec<String> = vec!["class Foo".into()];
    let u = uri("/home/dev/MyApp/app/src/main/kotlin/com/example/app/Foo.kt");
    let diag = missing_package_diagnostic(&lines, &u);
    assert!(diag.is_some());
    let d = diag.unwrap();
    assert!(d.message.contains("com.example.app"));
}

#[test]
fn test_diagnostic_absent_when_package_present() {
    use super::missing_package_diagnostic;
    let lines = vec!["package com.example.app".into(), "class Foo".into()];
    let u = uri("/home/dev/MyApp/app/src/main/kotlin/com/example/app/Foo.kt");
    assert!(missing_package_diagnostic(&lines, &u).is_none());
}

#[test]
fn test_diagnostic_absent_for_unknown_path() {
    use super::missing_package_diagnostic;
    let lines: Vec<String> = vec![];
    let u = uri("/some/random/Foo.kt");
    assert!(missing_package_diagnostic(&lines, &u).is_none());
}

#[test]
fn test_diagnostic_range_minimum_width_on_empty_file() {
    use super::missing_package_diagnostic;
    let lines: Vec<String> = vec![];
    let u = uri("/home/dev/MyApp/app/src/main/kotlin/com/example/app/Foo.kt");
    let diag = missing_package_diagnostic(&lines, &u).unwrap();
    // Even for an empty file the range must be at least "package".len() wide
    assert!(diag.range.end.character >= "package".len() as u32);
}

#[test]
fn test_diagnostic_range_after_file_annotation() {
    use super::missing_package_diagnostic;
    use tower_lsp::lsp_types::DiagnosticSeverity;
    let lines = vec![
        "@file:Suppress(\"unused\")".into(),
        "".into(),
        "class Foo".into(),
    ];
    let u = uri("/home/dev/MyApp/app/src/main/kotlin/com/example/app/Foo.kt");
    let diag = missing_package_diagnostic(&lines, &u).unwrap();
    // Should point to line 1 (just after the @file: annotation)
    assert_eq!(diag.range.start.line, 1);
    assert_eq!(diag.severity, Some(DiagnosticSeverity::WARNING));
}

#[test]
fn test_resolve_package_android_kotlin() {
    let path = "/home/dev/MyApp/app/src/main/kotlin/com/example/app/ui/home/HomeViewModel.kt";
    assert_eq!(
        resolve_package_from_path(path),
        Some("com.example.app.ui.home".into())
    );
}

#[test]
fn test_resolve_package_android_java() {
    let path = "/home/dev/MyApp/app/src/main/java/com/example/app/data/Repo.java";
    assert_eq!(
        resolve_package_from_path(path),
        Some("com.example.app.data".into())
    );
}

#[test]
fn test_resolve_package_kmp_common() {
    let path = "/home/dev/shared/src/commonMain/kotlin/com/example/app/utils/StringExt.kt";
    assert_eq!(
        resolve_package_from_path(path),
        Some("com.example.app.utils".into())
    );
}

#[test]
fn test_resolve_package_kmp_android() {
    let path = "/home/dev/shared/src/androidMain/kotlin/com/example/app/platform/AndroidImpl.kt";
    assert_eq!(
        resolve_package_from_path(path),
        Some("com.example.app.platform".into())
    );
}

#[test]
fn test_resolve_package_test_source_set() {
    let path = "/home/dev/MyApp/app/src/test/kotlin/com/example/app/HomeTest.kt";
    assert_eq!(
        resolve_package_from_path(path),
        Some("com.example.app".into())
    );
}

#[test]
fn test_resolve_package_at_source_root_no_subdir() {
    // File directly at the source root — top-level package would be empty
    let path = "/home/dev/MyApp/app/src/main/kotlin/Bar.kt";
    assert_eq!(resolve_package_from_path(path), None);
}

#[test]
fn test_resolve_package_no_marker() {
    let path = "/some/random/path/to/Foo.kt";
    assert_eq!(resolve_package_from_path(path), None);
}

#[test]
fn test_resolve_package_hyphenated_dir_rejected() {
    // Hyphens are not valid in Java/Kotlin identifiers — no silent rewrite
    let path = "/home/dev/MyApp/app/src/main/kotlin/com/my-module/Bar.kt";
    assert_eq!(resolve_package_from_path(path), None);
}

#[test]
fn test_resolve_package_digit_leading_segment_rejected() {
    let path = "/home/dev/MyApp/app/src/main/kotlin/com/123invalid/Bar.kt";
    assert_eq!(resolve_package_from_path(path), None);
}

#[test]
fn test_resolve_package_common_test() {
    let path = "/home/dev/shared/src/commonTest/kotlin/com/example/app/UtilsTest.kt";
    assert_eq!(
        resolve_package_from_path(path),
        Some("com.example.app".into())
    );
}

// ─── find_package_insert_line ─────────────────────────────────────────────────

#[test]
fn test_insert_line_empty_file() {
    let lines: Vec<String> = vec![];
    assert_eq!(find_package_insert_line(&lines), 0);
}

#[test]
fn test_insert_line_no_file_annotations() {
    let lines = vec!["class Foo".into()];
    assert_eq!(find_package_insert_line(&lines), 0);
}

#[test]
fn test_insert_line_after_file_annotation() {
    let lines = vec![
        "@file:Suppress(\"unused\")".into(),
        "".into(),
        "class Foo".into(),
    ];
    // Insert at line 1 (just after the @file: annotation)
    assert_eq!(find_package_insert_line(&lines), 1);
}

#[test]
fn test_insert_line_after_multiple_file_annotations() {
    let lines = vec![
        "@file:Suppress(\"unused\")".into(),
        "@file:JvmName(\"FooKt\")".into(),
        "".into(),
        "class Foo".into(),
    ];
    assert_eq!(find_package_insert_line(&lines), 2);
}

#[test]
fn test_insert_line_file_annotation_after_comment() {
    // Comment → @file: → code
    let lines = vec![
        "// Copyright 2024".into(),
        "@file:Suppress(\"unused\")".into(),
        "class Foo".into(),
    ];
    assert_eq!(find_package_insert_line(&lines), 2);
}

// ─── build_add_package_action ─────────────────────────────────────────────────

fn uri(path: &str) -> Url {
    Url::parse(&format!("file://{path}")).unwrap()
}

#[test]
fn test_action_fires_for_empty_kotlin_file() {
    let lines: Vec<String> = vec![];
    let u = uri("/home/dev/MyApp/app/src/main/kotlin/com/example/app/Foo.kt");
    assert!(build_add_package_action(&lines, &u).is_some());
}

#[test]
fn test_action_skips_when_package_already_present() {
    let lines = vec![
        "package com.example.app".into(),
        "".into(),
        "class Foo".into(),
    ];
    let u = uri("/home/dev/MyApp/app/src/main/kotlin/com/example/app/Foo.kt");
    assert!(build_add_package_action(&lines, &u).is_none());
}

#[test]
fn test_action_skips_unknown_path() {
    let lines: Vec<String> = vec![];
    let u = uri("/some/random/path/Foo.kt");
    assert!(build_add_package_action(&lines, &u).is_none());
}

#[test]
fn test_action_insert_text_correct() {
    use tower_lsp::lsp_types::{CodeActionOrCommand, WorkspaceEdit};

    let lines: Vec<String> = vec![];
    let u = uri("/home/dev/MyApp/app/src/main/kotlin/com/example/app/Foo.kt");
    let action = build_add_package_action(&lines, &u).unwrap();
    let CodeActionOrCommand::CodeAction(ca) = action else {
        panic!("expected CodeAction");
    };
    let edit: &WorkspaceEdit = ca.edit.as_ref().unwrap();
    let changes = edit.changes.as_ref().unwrap();
    let edits = changes.get(&u).unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].new_text, "package com.example.app\n\n");
    assert_eq!(edits[0].range.start.line, 0);
}

#[test]
fn test_action_inserts_after_file_annotation() {
    use tower_lsp::lsp_types::CodeActionOrCommand;

    let lines = vec![
        "@file:Suppress(\"unused\")".into(),
        "".into(),
        "class Foo".into(),
    ];
    let u = uri("/home/dev/MyApp/app/src/main/kotlin/com/example/app/Foo.kt");
    let action = build_add_package_action(&lines, &u).unwrap();
    let CodeActionOrCommand::CodeAction(ca) = action else {
        panic!("expected CodeAction");
    };
    let edit = ca.edit.unwrap();
    let edits = edit.changes.unwrap();
    let edits = edits.get(&u).unwrap();
    assert_eq!(edits[0].range.start.line, 1); // after line 0 (@file:)
}

#[test]
fn test_action_fires_for_java_file() {
    let lines: Vec<String> = vec![];
    let u = uri("/home/dev/MyApp/app/src/main/java/com/example/app/Repo.java");
    assert!(build_add_package_action(&lines, &u).is_some());
}
