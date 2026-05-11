//! CLI `sources` subcommand — list resolved source roots.
//!
//! Shows what the canonical workspace source resolver would contribute as
//! source roots, which paths actually exist, and where each path was
//! found. Useful for verifying project setup without starting the LSP
//! server.

use std::collections::HashSet;
use std::path::Path;

use serde::Serialize;

use crate::workspace::Config;

#[derive(Debug, Serialize)]
pub(crate) struct SourceRoot {
    pub path: String, // lossy-UTF8; always serializable
    pub origin: &'static str,
    pub exists: bool,
}

/// Collect all resolved source roots for the given workspace root.
pub(crate) fn discover(workspace_root: &Path) -> Vec<SourceRoot> {
    let config = Config {
        root: workspace_root.to_path_buf(),
        explicit_source_paths: Vec::new(),
        ignore_patterns: Vec::new(),
        pin_workspace: false,
    };
    let workspace_json_paths: HashSet<String> =
        crate::workspace_json::load_source_paths(workspace_root)
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
    let extract_sources_path = crate::util::home_dir().map(|home| {
        home.join(".kotlin-lsp")
            .join("sources")
            .to_string_lossy()
            .into_owned()
    });

    config
        .resolve_sources()
        .into_iter()
        .map(|path| SourceRoot {
            exists: Path::new(&path).is_dir(),
            origin: source_origin(
                &path,
                &workspace_json_paths,
                extract_sources_path.as_deref(),
            ),
            path,
        })
        .collect()
}

fn source_origin(
    path: &str,
    workspace_json_paths: &HashSet<String>,
    extract_sources_path: Option<&str>,
) -> &'static str {
    if workspace_json_paths.contains(path) {
        return "workspace.json";
    }
    if extract_sources_path == Some(path) {
        return "extract-sources";
    }
    "build-layout"
}

pub(crate) fn run_sources(workspace_root: &Path, json: bool) {
    let roots = discover(workspace_root);

    if roots.is_empty() {
        if !json {
            eprintln!(
                "No source roots found. Add a workspace.json or a build.gradle.kts / pom.xml."
            );
        } else {
            println!("[]");
        }
        std::process::exit(1);
    }

    if json {
        match serde_json::to_string_pretty(&roots) {
            Ok(json_str) => println!("{json_str}"),
            Err(e) => {
                eprintln!("error: failed to serialize sources: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // Text output — group by origin, mark missing paths.
    let mut last_origin = "";
    for root in &roots {
        if root.origin != last_origin {
            println!("\n[{}]", root.origin);
            last_origin = root.origin;
        }
        let marker = if root.exists { "  ✓" } else { "  ✗" };
        println!("{} {}", marker, root.path);
    }

    let missing = roots.iter().filter(|r| !r.exists).count();
    if missing > 0 {
        eprintln!("\n{missing} path(s) marked ✗ do not exist on disk.");
    }

    if roots.is_empty() || roots.iter().any(|r| r.origin == "build-layout") {
        eprintln!("\nTip: run `kotlin-lsp extract-sources` to unpack Gradle *-sources.jar files");
        eprintln!("     so library source code is available for hover and go-to-definition.");
    }
}
