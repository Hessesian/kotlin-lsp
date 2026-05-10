//! Workspace lifecycle management — configuration, events, and the MVI actor.
//!
//! # Architecture
//!
//! All workspace-level state mutations (root, source paths, ignore patterns, scans)
//! flow through [`WorkspaceActor`] via [`WorkspaceEvent`]s sent on an `mpsc` channel.
//! This serialises writes and gives a single, exhaustive `match` as the authority on
//! what can happen to the workspace.
//!
//! Read-path handlers receive `Arc<Indexer>` directly and operate concurrently.
//!
//! # Source discovery
//!
//! [`WorkspaceConfig::resolve_sources`] is the canonical source-path resolver.
//! Only `WorkspaceActor` event handlers may call it — no other code should write
//! `Indexer::source_paths_raw`.
//!
//! # Wiring status
//!
//! Wave 1 (this PR) establishes the infrastructure.
//! Wave 2 (todos: `ws-backend`, `ws-cli`, `ws-main`) wires the actor into the
//! LSP backend and CLI runner.  Until then the re-exports below are intentionally
//! unreachable from `main()`.

pub(crate) mod actor;
pub(crate) mod contract;
pub(crate) mod event;
pub(crate) mod phase;

// Re-exports are unused until Wave 2 wires this module in (ws-backend, ws-cli, ws-main).
#[allow(unused_imports)]
pub(crate) use actor::WorkspaceActor;
#[allow(unused_imports)]
pub(crate) use contract::{WorkspaceData, WorkspaceEffect, WorkspacePhase};
#[allow(unused_imports)]
pub(crate) use event::WorkspaceEvent;

use std::path::PathBuf;

// ─── WorkspaceConfig ─────────────────────────────────────────────────────────

// WorkspaceConfig is unused until Wave 2 wires this module in (ws-backend, ws-cli, ws-main).
#[allow(dead_code)]
/// Immutable snapshot of workspace configuration collected at startup.
///
/// Passed inside [`WorkspaceEvent::Initialize`]; not mutated after construction.
pub(crate) struct WorkspaceConfig {
    /// Absolute path to the workspace root (nearest `.git` ancestor of the opened file,
    /// or an explicit `--root` flag in CLI mode, or the LSP `rootUri`).
    pub root: PathBuf,

    /// Source paths explicitly configured by the caller (e.g. LSP
    /// `initializationOptions.indexingOptions.sourcePaths`).
    /// These are merged with auto-discovered paths by [`resolve_sources`].
    pub explicit_source_paths: Vec<String>,

    /// Glob-style ignore patterns from LSP `initializationOptions.indexingOptions.ignorePatterns`.
    pub ignore_patterns: Vec<String>,
}

impl WorkspaceConfig {
    /// Return the deduplicated, ordered list of source paths to index.
    ///
    /// Discovery priority (first win for deduplication):
    /// 1. `explicit_source_paths` from LSP `initializationOptions`
    /// 2. Paths from `workspace.json` (JetBrains Gradle/Maven format)
    /// 3. Build-layout auto-detection (standard Maven/Gradle `src/` dirs) —
    ///    only attempted when `workspace.json` is absent
    /// 4. `~/.kotlin-lsp/sources` (default `extract-sources` output dir)
    ///
    /// Called only from `WorkspaceActor` event handlers (`handle_initialize`,
    /// `handle_change_root`).  No other code should call this method.
    pub(crate) fn resolve_sources(&self) -> Vec<String> {
        use std::collections::HashSet;

        let mut seen: HashSet<String> = HashSet::new();
        let mut paths: Vec<String> = Vec::new();

        let mut push = |s: String| {
            if seen.insert(s.clone()) {
                paths.push(s);
            }
        };

        for s in &self.explicit_source_paths {
            push(s.clone());
        }

        let json_paths = crate::workspace_json::load_source_paths(&self.root);
        for p in &json_paths {
            push(p.to_string_lossy().into_owned());
        }

        if json_paths.is_empty() {
            for p in crate::workspace_json::detect_build_layout_source_paths(&self.root) {
                push(p.to_string_lossy().into_owned());
            }
        }

        // Auto-include the well-known `extract-sources` output directory if present.
        // Skip entirely when HOME is unknown to avoid accidentally indexing the
        // current working directory (matches existing backend behaviour).
        #[allow(deprecated)]
        if let Some(home) = std::env::home_dir() {
            let default_sources = home.join(".kotlin-lsp").join("sources");
            if default_sources.is_dir() {
                push(default_sources.to_string_lossy().into_owned());
            }
        }

        paths
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
