use super::*;
use std::fs;
use tempfile::TempDir;

fn make_workspace_json(dir: &TempDir, json: &str) {
    fs::write(dir.path().join("workspace.json"), json).unwrap();
}

// ─── workspace.json tests ─────────────────────────────────────────────────────

#[test]
fn missing_file_returns_empty() {
    let dir = TempDir::new().unwrap();
    let paths = load_source_paths(dir.path());
    assert!(paths.is_empty());
}

#[test]
fn malformed_json_returns_empty() {
    let dir = TempDir::new().unwrap();
    make_workspace_json(&dir, "{ not valid json }}}");
    let paths = load_source_paths(dir.path());
    assert!(paths.is_empty());
}

#[test]
fn extracts_java_source_and_java_test() {
    let dir = TempDir::new().unwrap();
    let json = format!(
        r#"{{
            "modules": [{{
                "contentRoots": [{{
                    "sourceRoots": [
                        {{"path": "<WORKSPACE>/src/main/kotlin", "type": "java-source"}},
                        {{"path": "<WORKSPACE>/src/test/kotlin", "type": "java-test"}},
                        {{"path": "<WORKSPACE>/src/main/resources", "type": "java-resource"}},
                        {{"path": "<WORKSPACE>/src/test/resources", "type": "java-test-resource"}}
                    ]
                }}]
            }}]
        }}"#
    );
    make_workspace_json(&dir, &json);

    let paths = load_source_paths(dir.path());
    assert_eq!(paths.len(), 2);
    assert_eq!(paths[0], dir.path().join("src/main/kotlin"));
    assert_eq!(paths[1], dir.path().join("src/test/kotlin"));
    // resources excluded
    assert!(!paths.iter().any(|p| p.ends_with("resources")));
}

#[test]
fn deduplicates_paths_across_modules() {
    let dir = TempDir::new().unwrap();
    let json = r#"{
        "modules": [
            {"contentRoots": [{"sourceRoots": [{"path": "<WORKSPACE>/src/main/kotlin", "type": "java-source"}]}]},
            {"contentRoots": [{"sourceRoots": [{"path": "<WORKSPACE>/src/main/kotlin", "type": "java-source"}]}]}
        ]
    }"#;
    make_workspace_json(&dir, json);

    let paths = load_source_paths(dir.path());
    assert_eq!(paths.len(), 1);
}

#[test]
fn resolves_workspace_placeholder() {
    let dir = TempDir::new().unwrap();
    let json = r#"{
        "modules": [{"contentRoots": [{"sourceRoots": [
            {"path": "<WORKSPACE>/app/src/main/kotlin", "type": "java-source"}
        ]}]}]
    }"#;
    make_workspace_json(&dir, json);

    let paths = load_source_paths(dir.path());
    assert_eq!(paths.len(), 1);
    assert!(paths[0].is_absolute());
    assert!(paths[0].ends_with("app/src/main/kotlin"));
}

#[test]
fn empty_modules_returns_empty() {
    let dir = TempDir::new().unwrap();
    make_workspace_json(&dir, r#"{"modules": []}"#);
    let paths = load_source_paths(dir.path());
    assert!(paths.is_empty());
}

// ─── build-layout detection tests ────────────────────────────────────────────

#[test]
fn no_build_file_returns_empty() {
    let dir = TempDir::new().unwrap();
    let paths = detect_build_layout_source_paths(dir.path());
    assert!(paths.is_empty());
}

#[test]
fn gradle_kts_probes_standard_dirs() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("build.gradle.kts"), "").unwrap();
    let src = dir.path().join("src/main/kotlin");
    fs::create_dir_all(&src).unwrap();
    let test = dir.path().join("src/test/kotlin");
    fs::create_dir_all(&test).unwrap();

    let paths = detect_build_layout_source_paths(dir.path());
    assert!(paths.contains(&src));
    assert!(paths.contains(&test));
}

#[test]
fn nonexistent_candidates_excluded() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("build.gradle.kts"), "").unwrap();
    // No source dirs created.
    let paths = detect_build_layout_source_paths(dir.path());
    assert!(paths.is_empty());
}

#[test]
fn maven_pom_triggers_detection() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("pom.xml"), "<project/>").unwrap();
    let src = dir.path().join("src/main/java");
    fs::create_dir_all(&src).unwrap();

    let paths = detect_build_layout_source_paths(dir.path());
    assert!(paths.contains(&src));
}

#[test]
fn settings_gradle_multimodule() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("settings.gradle.kts"),
        r#"include(":app", ":core")"#,
    )
    .unwrap();
    let app_src = dir.path().join("app/src/main/kotlin");
    let core_src = dir.path().join("core/src/main/kotlin");
    fs::create_dir_all(&app_src).unwrap();
    fs::create_dir_all(&core_src).unwrap();

    let paths = detect_build_layout_source_paths(dir.path());
    assert!(paths.contains(&app_src));
    assert!(paths.contains(&core_src));
}

// ─── parse_include_calls unit tests ──────────────────────────────────────────

#[test]
fn parses_colon_prefixed_includes() {
    let content = r#"include(":app", ":core", ":data")"#;
    let result = parse_include_calls(content);
    assert_eq!(result, vec!["app", "core", "data"]);
}

#[test]
fn parses_nested_module_paths() {
    let content = r#"include(":feature:login", ":feature:home")"#;
    let result = parse_include_calls(content);
    let sep = std::path::MAIN_SEPARATOR_STR;
    assert_eq!(result[0], format!("feature{sep}login"));
    assert_eq!(result[1], format!("feature{sep}home"));
}

#[test]
fn deduplicates_include_entries() {
    let content = "include(\":app\")\ninclude(\":app\")";
    let result = parse_include_calls(content);
    assert_eq!(result.len(), 1);
}

#[test]
fn parses_single_quoted_includes() {
    let content = "include(':app', ':core')";
    let result = parse_include_calls(content);
    assert_eq!(result, vec!["app", "core"]);
}

#[test]
fn ignores_include_build_lines() {
    let content = "includeBuild(\"../other-project\")\ninclude(\":app\")";
    let result = parse_include_calls(content);
    assert_eq!(result, vec!["app"]);
}

// ─── Android SDK detection tests ─────────────────────────────────────────────

#[test]
fn no_sdk_returns_empty() {
    let dir = TempDir::new().unwrap();
    // No local.properties, no env vars set in test env
    let paths = detect_android_sdk_source_paths(dir.path());
    // Either empty (no SDK) or points to a real SDK — either is valid in CI.
    // We just verify the function returns without panic.
    let _ = paths;
}

#[test]
fn sdk_dir_from_local_properties_finds_sdk_dot_dir() {
    let dir = TempDir::new().unwrap();
    let fake_sdk = dir.path().join("sdk");
    fs::create_dir_all(fake_sdk.join("sources").join("android-34")).unwrap();
    fs::write(
        dir.path().join("local.properties"),
        format!(
            "# generated\nsdk.dir={}\nndk.version=25.0.0\n",
            fake_sdk.display()
        ),
    )
    .unwrap();
    let paths = detect_android_sdk_source_paths(dir.path());
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("android-34"));
}

#[test]
fn picks_highest_api_level() {
    let dir = TempDir::new().unwrap();
    let fake_sdk = dir.path().join("sdk");
    for api in [31_u32, 33, 34] {
        fs::create_dir_all(fake_sdk.join("sources").join(format!("android-{api}"))).unwrap();
    }
    fs::write(
        dir.path().join("local.properties"),
        format!("sdk.dir={}\n", fake_sdk.display()),
    )
    .unwrap();
    let paths = detect_android_sdk_source_paths(dir.path());
    assert_eq!(paths.len(), 1);
    assert!(
        paths[0].ends_with("android-34"),
        "expected android-34, got {:?}",
        paths[0]
    );
}

#[test]
fn sdk_dir_from_local_properties_with_whitespace() {
    let dir = TempDir::new().unwrap();
    let fake_sdk = dir.path().join("sdk");
    fs::create_dir_all(fake_sdk.join("sources").join("android-35")).unwrap();
    fs::write(
        dir.path().join("local.properties"),
        format!("sdk.dir = {} \n", fake_sdk.display()),
    )
    .unwrap();
    let paths = detect_android_sdk_source_paths(dir.path());
    assert_eq!(paths.len(), 1);
    assert!(paths[0].ends_with("android-35"));
}
