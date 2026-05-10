//! [`WorkspacePhase`] — lifecycle state of the workspace actor.
//!
//! Mirrors the `StoreState { Uninitialized | Ready<T> }` pattern from Moneta's
//! MVI layer.  Before the first [`WorkspaceEvent::Initialize`] arrives the actor
//! is `Uninitialized`; after it, `Ready(WorkspaceData)`.
//!
//! All code that needs workspace state should call [`WorkspacePhase::ready`]
//! and handle `None` (pre-init) explicitly — the type makes the uninitialized
//! case visible at every call site instead of a silent `Option<PathBuf>` check.

use std::path::PathBuf;

use super::WorkspaceConfig;

// ─── WorkspaceData ────────────────────────────────────────────────────────────

/// Immutable snapshot of workspace state captured at initialisation time.
///
/// Produced from a [`WorkspaceConfig`] the first time the actor processes a
/// [`WorkspaceEvent::Initialize`] event, and updated on every
/// [`WorkspaceEvent::ChangeRoot`].
///
/// Carries the resolved (not raw) source-path list so that no caller ever has
/// to re-run source discovery.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceData {
    /// Absolute path to the workspace root.
    pub root: PathBuf,

    /// Resolved, deduplicated list of source paths (output of
    /// [`WorkspaceConfig::resolve_sources`]).
    pub source_paths: Vec<String>,

    /// Ignore patterns in effect for this workspace.
    // Read by Wave 3 handlers; field exists for completeness of the snapshot.
    #[allow(dead_code)]
    pub ignore_patterns: Vec<String>,
}

impl WorkspaceData {
    pub(crate) fn from_config(config: &WorkspaceConfig) -> Self {
        Self {
            root: config.root.clone(),
            source_paths: config.resolve_sources(),
            ignore_patterns: config.ignore_patterns.clone(),
        }
    }
}

// ─── WorkspacePhase ──────────────────────────────────────────────────────────

/// Lifecycle phase of the workspace actor.
///
/// ```text
/// ┌──────────────┐  Initialize  ┌────────────────────────┐
/// │ Uninitialized│─────────────▶│  Ready(WorkspaceData)  │
/// └──────────────┘              └────────────────────────┘
///                                         │  ChangeRoot
///                                         ▼
///                               ┌────────────────────────┐
///                               │  Ready(WorkspaceData') │
///                               └────────────────────────┘
/// ```
///
/// There is no transition back to `Uninitialized`.  A `ChangeRoot` replaces
/// `Ready` with a fresh `Ready` for the new root.
#[derive(Debug, Clone, Default)]
pub(crate) enum WorkspacePhase {
    #[default]
    Uninitialized,
    Ready(WorkspaceData),
}

impl WorkspacePhase {
    /// Return a reference to the inner [`WorkspaceData`], or `None` if the
    /// workspace has not been initialised yet.
    pub(crate) fn ready(&self) -> Option<&WorkspaceData> {
        match self {
            WorkspacePhase::Ready(data) => Some(data),
            WorkspacePhase::Uninitialized => None,
        }
    }

    /// Apply `f` if the workspace is `Ready`, returning the result.
    /// Returns `None` and does nothing when `Uninitialized`.
    // Used by Wave 3 read handlers; present here for the pattern to be complete.
    #[allow(dead_code)]
    pub(crate) fn with_ready<F, T>(&self, f: F) -> Option<T>
    where
        F: FnOnce(&WorkspaceData) -> T,
    {
        self.ready().map(f)
    }

    /// Transition to `Ready` (or replace the current `Ready`).
    pub(crate) fn set_ready(&mut self, data: WorkspaceData) {
        *self = WorkspacePhase::Ready(data);
    }
}

#[cfg(test)]
#[path = "phase_tests.rs"]
mod tests;
