//! [`WorkspaceRoot`] — couples the workspace path and its staleness generation.
//!
//! The invariant: every root change bumps the generation. Enforced here so it
//! cannot be forgotten at call sites (rule 16).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// Workspace root path paired with a monotonic staleness generation.
///
/// The generation is incremented on every successful `set()`. Background tasks
/// capture it at start time and call `is_stale()` to detect superseded runs.
///
/// # Write path
/// Only `set()` may mutate the path. It is the single write site, so the
/// generation bump cannot be missed by a caller.
pub(crate) struct WorkspaceRoot {
    path: RwLock<Option<PathBuf>>,
    generation: AtomicU64,
}

impl WorkspaceRoot {
    pub(crate) fn new() -> Self {
        Self {
            path: RwLock::new(None),
            generation: AtomicU64::new(0),
        }
    }

    /// Set a new root and bump the generation.
    ///
    /// Logs a warning and leaves both path and generation unchanged if the
    /// write lock is poisoned.
    pub(crate) fn set(&self, root: PathBuf) {
        let Ok(mut guard) = self.path.write() else {
            log::warn!("WorkspaceRoot: write lock poisoned, root not updated");
            return;
        };
        *guard = Some(root);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Clone the current root path.
    pub(crate) fn get(&self) -> Option<PathBuf> {
        self.path.read().ok()?.clone()
    }

    /// Current generation value.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Bump the generation without changing the path.
    ///
    /// Use when a new scan request supersedes an in-flight scan — the root has
    /// not changed but any running task should detect staleness and bail out.
    pub(crate) fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Reference to the inner atomic for lifetime-scoped staleness checks
    /// (e.g. `ScanSession` which holds `&AtomicU64` across an async boundary).
    pub(crate) fn generation_atomic(&self) -> &AtomicU64 {
        &self.generation
    }
}
