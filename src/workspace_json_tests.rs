use super::*;
use std::fs;
use tempfile::TempDir;

fn make_workspace_json(dir: &TempDir, json: &str) {
    fs::write(dir.path().join("workspace.json"), json).unwrap();
}

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
    let ws = dir.path().to_string_lossy();
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
