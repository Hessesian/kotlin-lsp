//! Auto-discovery of source roots from `workspace.json`.
//!
//! `workspace.json` is produced by JetBrains Gradle/Maven plugins and describes
//! project structure (modules, content roots, source directories). When the file
//! exists at the workspace root we extract every non-resource source root so the
//! indexer covers them without manual `sourcePaths` configuration.
//!
//! Placeholder substitution:
//! - `<WORKSPACE>` → absolute workspace root path
//! - `<MAVEN_REPO>` → `~/.m2/repository` (library jars, currently skipped)
//!
//! Source root types we index:
//! - `"java-source"` — production Kotlin/Java sources
//! - `"java-test"` — test Kotlin/Java sources

use serde::Deserialize;
use std::path::{Path, PathBuf};

const SOURCE_TYPES: &[&str] = &["java-source", "java-test"];
const WORKSPACE_PLACEHOLDER: &str = "<WORKSPACE>";

#[derive(Deserialize)]
struct WorkspaceData {
    #[serde(default)]
    modules: Vec<ModuleData>,
}

#[derive(Deserialize)]
struct ModuleData {
    #[serde(default, rename = "contentRoots")]
    content_roots: Vec<ContentRootData>,
}

#[derive(Deserialize)]
struct ContentRootData {
    #[serde(default, rename = "sourceRoots")]
    source_roots: Vec<SourceRootData>,
}

#[derive(Deserialize)]
struct SourceRootData {
    path: String,
    #[serde(rename = "type", default)]
    root_type: String,
}

/// Reads `<workspace_root>/workspace.json` and returns source root paths.
///
/// Returns an empty `Vec` (with a log warning) if the file is missing, malformed,
/// or contains no eligible source roots — never panics.
pub(crate) fn load_source_paths(workspace_root: &Path) -> Vec<PathBuf> {
    let json_path = workspace_root.join("workspace.json");
    if !json_path.exists() {
        return Vec::new();
    }

    let content = match std::fs::read_to_string(&json_path) {
        Ok(c) => c,
        Err(error) => {
            log::warn!("workspace.json: failed to read: {error}");
            return Vec::new();
        }
    };

    let data: WorkspaceData = match serde_json::from_str(&content) {
        Ok(d) => d,
        Err(error) => {
            log::warn!("workspace.json: failed to parse: {error}");
            return Vec::new();
        }
    };

    let workspace_str = workspace_root.to_string_lossy();
    let mut paths: Vec<PathBuf> = Vec::new();

    for module in &data.modules {
        for content_root in &module.content_roots {
            for source_root in &content_root.source_roots {
                if !SOURCE_TYPES.contains(&source_root.root_type.as_str()) {
                    continue;
                }
                let resolved = source_root
                    .path
                    .replace(WORKSPACE_PLACEHOLDER, &workspace_str);
                let path = PathBuf::from(resolved);
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }

    log::info!(
        "workspace.json: auto-discovered {} source roots",
        paths.len()
    );
    paths
}

#[cfg(test)]
#[path = "workspace_json_tests.rs"]
mod tests;
