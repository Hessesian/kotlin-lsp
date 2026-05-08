//! CLI `sources` subcommand — list auto-discovered source roots.
//!
//! Shows what `workspace.json` and/or standard build layout detection
//! would contribute as source roots, which paths actually exist, and
//! where each path was found. Useful for verifying project setup without
//! starting the LSP server.

use std::path::Path;

use serde::Serialize;

#[derive(Debug, Serialize)]
pub(crate) struct SourceRoot {
    pub path: String, // lossy-UTF8; always serializable
    pub origin: &'static str, // "workspace.json" | "build-layout"
    pub exists: bool,
}

/// Collect all auto-discovered source roots for the given workspace root.
pub(crate) fn discover(workspace_root: &Path) -> Vec<SourceRoot> {
    let mut roots: Vec<SourceRoot> = Vec::new();

    let json_paths = crate::workspace_json::load_source_paths(workspace_root);
    for path in &json_paths {
        roots.push(SourceRoot {
            exists: path.is_dir(),
            path: path.to_string_lossy().into_owned(),
            origin: "workspace.json",
        });
    }

    if json_paths.is_empty() {
        for path in crate::workspace_json::detect_build_layout_source_paths(workspace_root) {
            roots.push(SourceRoot {
                exists: path.is_dir(),
                path: path.to_string_lossy().into_owned(),
                origin: "build-layout",
            });
        }
    }

    roots
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
}
