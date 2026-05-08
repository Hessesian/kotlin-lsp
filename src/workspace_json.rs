//! Auto-discovery of source roots from `workspace.json`.
//!
//! `workspace.json` is produced by JetBrains Gradle/Maven plugins and describes
//! project structure (modules, content roots, source directories). When the file
//! exists at the workspace root we extract every non-resource source root so the
//! indexer covers them without manual `sourcePaths` configuration.
//!
//! Placeholder substitution:
//! - `<WORKSPACE>` → absolute workspace root path
//! - `<MAVEN_REPO>` → skipped (library jars are not indexed)
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

/// Detects standard Maven/Gradle source layouts without requiring `workspace.json`.
///
/// Activates when a build file (`build.gradle.kts`, `build.gradle`, `pom.xml`, …) exists
/// at the workspace root. Probes well-known source directories; returns only those that
/// actually exist on disk so the indexer never spins on empty paths.
///
/// Multi-module Gradle: `settings.gradle(.kts)` is parsed for `include(":module")` calls;
/// each listed module is treated as a subproject and its standard source dirs are probed.
///
/// These paths are typically already covered by the workspace root scan, but listing them
/// explicitly ensures consistent indexing when the workspace root is set to a parent dir.
pub(crate) fn detect_build_layout_source_paths(workspace_root: &Path) -> Vec<PathBuf> {
    let has_gradle = ["build.gradle.kts", "build.gradle"]
        .iter()
        .any(|f| workspace_root.join(f).exists());
    let has_settings = ["settings.gradle.kts", "settings.gradle"]
        .iter()
        .any(|f| workspace_root.join(f).exists());
    let has_maven = workspace_root.join("pom.xml").exists();

    if !has_gradle && !has_settings && !has_maven {
        return Vec::new();
    }

    let mut roots: Vec<PathBuf> = Vec::new();

    // Subproject dirs from settings.gradle(.kts)
    let subprojects = settings_subprojects(workspace_root);

    // Probe candidates for each directory scope.
    let scan_dirs: Vec<PathBuf> = if subprojects.is_empty() {
        vec![workspace_root.to_owned()]
    } else {
        subprojects
            .iter()
            .map(|s| workspace_root.join(s))
            .collect()
    };

    // Always include the root itself (root build.gradle may have sources too).
    let mut all_dirs = vec![workspace_root.to_owned()];
    for d in &scan_dirs {
        if d != workspace_root && !all_dirs.contains(d) {
            all_dirs.push(d.clone());
        }
    }

    let source_candidates = [
        "src/main/kotlin",
        "src/main/java",
        "src/test/kotlin",
        "src/test/java",
    ];

    for dir in &all_dirs {
        for candidate in &source_candidates {
            let path = dir.join(candidate);
            if path.is_dir() && !roots.contains(&path) {
                roots.push(path);
            }
        }
    }

    if !roots.is_empty() {
        log::info!(
            "build-layout: auto-discovered {} source roots",
            roots.len()
        );
    }
    roots
}

/// Extracts subproject directory names from `settings.gradle` / `settings.gradle.kts`.
///
/// Handles both forms:
/// - `include(":app", ":core")` — Gradle convention (colon prefix)
/// - `include("app", "core")` — variant without colon
/// - Nested: `include(":feature:login")` → maps to `feature/login`
fn settings_subprojects(workspace_root: &Path) -> Vec<String> {
    for filename in &["settings.gradle.kts", "settings.gradle"] {
        let path = workspace_root.join(filename);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        return parse_include_calls(&content);
    }
    Vec::new()
}

/// Parses `include("...", "...")` calls and returns directory paths.
///
/// Handles both double- and single-quoted project names, and both Kotlin DSL
/// (`include(":app")`) and Groovy (`include ':app'`) styles. Lines starting
/// with `includeBuild` or `includeFlat` are intentionally ignored.
fn parse_include_calls(content: &str) -> Vec<String> {
    let mut result = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        // Only match `include(` — reject `includeBuild(`, `includeFlat(`, etc.
        if !trimmed.starts_with("include(") {
            continue;
        }
        // Extract all single- or double-quoted strings on this line.
        let mut chars = trimmed.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '"' || c == '\'' {
                let quote = c;
                let token: String = chars.by_ref().take_while(|&d| d != quote).collect();
                // ":app" → "app", ":feature:login" → "feature/login"
                let dir = token
                    .trim_start_matches(':')
                    .replace(':', std::path::MAIN_SEPARATOR_STR);
                if !dir.is_empty() && !result.contains(&dir) {
                    result.push(dir);
                }
            }
        }
    }
    result
}

#[cfg(test)]
#[path = "workspace_json_tests.rs"]
mod tests;
