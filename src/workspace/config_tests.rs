//! Unit tests for [`Config::resolve_sources`].

use std::fs;
use std::path::Path;

use super::Config;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn config(root: &Path) -> Config {
    Config {
        root: root.to_path_buf(),
        explicit_source_paths: Vec::new(),
        ignore_patterns: Vec::new(),
    }
}

fn config_with_explicit(root: &Path, explicit: &[&str]) -> Config {
    Config {
        root: root.to_path_buf(),
        explicit_source_paths: explicit.iter().map(|s| s.to_string()).collect(),
        ignore_patterns: Vec::new(),
    }
}

/// Write a `build.gradle.kts` so that `detect_build_layout_source_paths` triggers.
fn write_gradle(root: &Path) {
    fs::write(root.join("build.gradle.kts"), "").unwrap();
}

// ─── resolve_sources tests ───────────────────────────────────────────────────

#[test]
fn no_workspace_json_no_build_file_no_layout_paths() {
    let dir = tempfile::tempdir().unwrap();
    let sources = config(dir.path()).resolve_sources();
    // No workspace.json, no build file → no layout paths from this dir
    // (we accept that ~/.kotlin-lsp/sources may appear if it exists on the host)
    let has_temp_dir_paths = sources
        .iter()
        .any(|s| s.starts_with(dir.path().to_str().unwrap_or("")));
    assert!(
        !has_temp_dir_paths,
        "No paths from temp dir expected, got: {sources:?}"
    );
}

#[test]
fn explicit_paths_are_included() {
    let dir = tempfile::tempdir().unwrap();
    let sources = config_with_explicit(dir.path(), &["/some/external/lib", "/another/lib"])
        .resolve_sources();
    assert!(sources.contains(&"/some/external/lib".to_string()));
    assert!(sources.contains(&"/another/lib".to_string()));
}

#[test]
fn explicit_paths_come_first() {
    let dir = tempfile::tempdir().unwrap();
    // Create a Gradle build file + standard layout so auto-detection fires.
    write_gradle(dir.path());
    let src_main = dir.path().join("src").join("main").join("kotlin");
    fs::create_dir_all(&src_main).unwrap();

    let cfg = config_with_explicit(dir.path(), &["/explicit"]);
    let sources = cfg.resolve_sources();

    let explicit_pos = sources
        .iter()
        .position(|s| s == "/explicit")
        .expect("explicit path missing");
    let layout_pos = sources
        .iter()
        .position(|s| {
            std::path::Path::new(s).ends_with(std::path::Path::new("src/main/kotlin"))
        })
        .expect("layout path missing");

    assert!(
        explicit_pos < layout_pos,
        "Explicit path should appear before auto-discovered paths"
    );
}

#[test]
fn no_duplicate_paths() {
    let dir = tempfile::tempdir().unwrap();
    write_gradle(dir.path());
    let src_main = dir.path().join("src").join("main").join("kotlin");
    fs::create_dir_all(&src_main).unwrap();

    let src_str = src_main.to_string_lossy().into_owned();
    // Pass the auto-discovered path as explicit too — should appear exactly once.
    let cfg = config_with_explicit(dir.path(), &[&src_str]);
    let sources = cfg.resolve_sources();

    let count = sources.iter().filter(|s| s.as_str() == src_str).count();
    assert_eq!(count, 1, "Expected exactly one occurrence of {src_str}");
}

#[test]
fn workspace_json_paths_are_included() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().to_string_lossy();

    let src_dir = dir.path().join("src").join("main").join("kotlin");
    fs::create_dir_all(&src_dir).unwrap();

    // Format expected by workspace_json::load_source_paths.
    let json = format!(
        r#"{{
            "modules": [{{
                "contentRoots": [{{
                    "sourceRoots": [
                        {{"path": "<WORKSPACE>/src/main/kotlin", "type": "java-source"}}
                    ]
                }}]
            }}]
        }}"#
    );
    // load_source_paths replaces <WORKSPACE> with the root, so write at workspace root.
    let _ = ws; // used in format only via the path
    fs::write(dir.path().join("workspace.json"), json).unwrap();

    let sources = config(dir.path()).resolve_sources();
    let src_str = src_dir.to_string_lossy().into_owned();
    assert!(
        sources.contains(&src_str),
        "Expected workspace.json path in sources, got: {sources:?}"
    );
}

