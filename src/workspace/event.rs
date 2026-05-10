//! [`Event`] — the sealed set of workspace-level state mutations.
//!
//! Every write to workspace state goes through one of these variants.
//! Adding a new variant produces a compile error in [`super::Actor::run`]
//! until the handler is implemented — this is the key correctness invariant.
// Items unused until Wave 2 wires this into backend/CLI (ws-backend, ws-cli, ws-main).
#![allow(dead_code)]

use std::path::PathBuf;

use tokio::sync::oneshot;
use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, Url};

use super::Config;

/// All workspace-level mutations, serialised through [`Actor`].
pub(crate) enum Event {
    /// Configure the workspace and start an initial scan.
    ///
    /// Must be the first event sent to a fresh actor. Subsequent `Initialize`
    /// events switch the root and restart the scan, discarding old source paths.
    Initialize {
        config: Config,
        completion_tx: Option<oneshot::Sender<()>>,
    },

    /// Re-scan the current workspace from scratch.
    ///
    /// Equivalent to the `kotlin-lsp/reindex` execute-command. Keeps the
    /// long-lived `Indexer` so live-document state is preserved.
    Reindex,

    /// Switch to a new workspace root and restart the scan.
    ///
    /// Source paths are re-resolved via `Config::resolve_sources` for
    /// the new root. Existing explicit `source_paths_raw` are discarded because
    /// they were relative to the old root.
    ChangeRoot { root: PathBuf },

    /// Store live document state and schedule indexing for a newly opened file.
    FileOpened {
        uri: Url,
        language_id: String,
        content: String,
    },

    /// Update live document state and debounce re-indexing after edits.
    FileChanged {
        uri: Url,
        changes: Vec<TextDocumentContentChangeEvent>,
    },

    /// Re-index the current on-disk content for a saved file.
    FileSaved { uri: Url },

    /// Drop live document state for a closed file.
    FileClosed { uri: Url },
}
