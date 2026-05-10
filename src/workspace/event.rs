//! [`WorkspaceEvent`] — the sealed set of workspace-level state mutations.
//!
//! Every write to workspace state goes through one of these variants.
//! Adding a new variant produces a compile error in [`WorkspaceActor::run`]
//! until the handler is implemented — this is the key correctness invariant.
// Items unused until Wave 2 wires this into backend/CLI (ws-backend, ws-cli, ws-main).
#![allow(dead_code)]

use std::path::PathBuf;

use super::WorkspaceConfig;

/// All workspace-level mutations, serialised through [`WorkspaceActor`].
///
/// File-level events (textDocument/didOpen, didChange, etc.) are handled
/// directly by the LSP backend for now; they will migrate here in a later phase.
pub(crate) enum WorkspaceEvent {
    /// Configure the workspace and start an initial scan.
    ///
    /// Must be the first event sent to a fresh actor.  Subsequent `Initialize`
    /// events switch the root and restart the scan, discarding old source paths.
    Initialize { config: WorkspaceConfig },

    /// Re-scan the current workspace from scratch.
    ///
    /// Equivalent to the `kotlin-lsp/reindex` execute-command.  Keeps the
    /// long-lived `Indexer` so live-document state is preserved.
    Reindex,

    /// Switch to a new workspace root and restart the scan.
    ///
    /// Source paths are re-resolved via `WorkspaceConfig::resolve_sources` for
    /// the new root. Existing explicit `source_paths_raw` are discarded because
    /// they were relative to the old root.
    ChangeRoot { root: PathBuf },
}
