//! [`ScanQueue`] — coalescing queue for full-workspace scans.
//! Wired into [`ScanHandler`] as part of the w7 quiescent-state work.
#![allow(dead_code)]
//!
//! Prevents concurrent duplicate scans from the thundering-herd problem:
//! if `Initialize`, `Reindex`, or `ChangeRoot` events arrive while a scan
//! is in progress, the latest request replaces any earlier pending request
//! (last-write-wins). When the running scan finishes, the pending scan starts.
//!
//! Based on `rust-analyzer`'s `OpQueue` pattern.

use std::path::PathBuf;

use tokio::sync::oneshot;

// ─── ScanKind ────────────────────────────────────────────────────────────────

/// Discriminates between the two scan strategies the actor can trigger.
pub(crate) enum ScanKind {
    /// Index the workspace, prioritising `initial_paths` first.
    Prioritized { initial_paths: Vec<PathBuf> },
    /// Full unconditional re-scan (used by Reindex and ChangeRoot).
    Full,
}

// ─── ScanArgs ────────────────────────────────────────────────────────────────

/// Arguments passed to a single workspace scan.
pub(crate) struct ScanArgs {
    pub(crate) root: PathBuf,
    pub(crate) kind: ScanKind,
    /// Source paths snapshot captured at request time.  Passed to
    /// `index_source_paths` so that each scan uses the paths that were
    /// configured when it was enqueued — not whichever set a later event may
    /// have written by the time the scan actually finishes.
    pub(crate) source_paths: Vec<String>,
    /// Fired when the scan completes. `None` if the caller does not need notification.
    /// Dropped (without signalling) if this request is superseded before it starts.
    pub(crate) completion_tx: Option<oneshot::Sender<()>>,
    /// The value of `Indexer::root_generation` at the moment this scan was
    /// enqueued.  The scan task checks this before writing shared state and
    /// before signalling `completion_tx`; if the current generation no longer
    /// matches, the scan has been superseded and should discard its results.
    pub(crate) expected_generation: u64,
}

// ─── ScanQueue ───────────────────────────────────────────────────────────────

/// Coalescing queue that holds at most one pending scan and one in-progress scan.
pub(crate) struct ScanQueue {
    /// The next scan to run once the current one finishes. Replaced on every
    /// new request — only the most recent one ever runs.
    pending: Option<ScanArgs>,
    /// `true` while a scan task is running.
    in_progress: bool,
}

impl ScanQueue {
    pub(crate) fn new() -> Self {
        Self {
            pending: None,
            in_progress: false,
        }
    }

    /// Returns `true` if a scan is currently running.
    pub(crate) fn is_in_progress(&self) -> bool {
        self.in_progress
    }

    /// Store a new scan request, replacing any previous pending one.
    ///
    /// The superseded request's `completion_tx` is dropped silently.
    /// Callers that need to distinguish cancellation should use a
    /// `oneshot::Sender<Result<(), Cancelled>>` pattern instead.
    pub(crate) fn request(&mut self, args: ScanArgs) {
        self.pending = Some(args);
    }

    /// Returns the next args to execute if no scan is currently in progress,
    /// and marks `in_progress = true`.  Returns `None` if busy.
    pub(crate) fn try_start(&mut self) -> Option<ScanArgs> {
        if self.in_progress {
            return None;
        }
        let args = self.pending.take()?;
        self.in_progress = true;
        Some(args)
    }

    /// Mark the current scan as finished.  Call this before `try_start` to
    /// check for a pending follow-up scan.
    pub(crate) fn completed(&mut self) {
        self.in_progress = false;
    }
}

#[cfg(test)]
#[path = "scan_queue_tests.rs"]
mod tests;
