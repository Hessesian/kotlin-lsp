//! Workspace feature contract — the complete public surface of the workspace module.
//!
//! Mirrors the `*Contract` interface pattern from Moneta's MVI layer: one file
//! that a contributor can read to understand every input, state, and output of
//! the workspace subsystem.
//!
//! # Layout
//!
//! | Type | Role |
//! |---|---|
//! | [`WorkspaceEvent`] | Inputs — every write to workspace state |
//! | [`WorkspacePhase`] | State — `Uninitialized` or `Ready(WorkspaceData)` |
//! | [`WorkspaceEffect`] | Outputs — one-shot side-effects from the actor |
//!
//! # Compiler enforcement
//!
//! * Adding a [`WorkspaceEvent`] variant → compile error in `WorkspaceActor::run`
//!   until the handler is implemented.
//! * Adding a [`WorkspaceEffect`] variant → compile error in every site that
//!   matches on the effect channel.
//! * Accessing [`WorkspacePhase::Ready`] data without checking for
//!   `Uninitialized` is a type-level error — callers must go through
//!   [`WorkspacePhase::ready`] which returns `Option<&WorkspaceData>`.

// Items re-exported here are the single source of truth for the workspace
// public surface.  Individual types are unused at the re-export site until
// Wave 2/3 wires backend and CLI through this contract.
#[allow(unused_imports)]
pub(crate) use super::event::WorkspaceEvent;
#[allow(unused_imports)]
pub(crate) use super::phase::{WorkspaceData, WorkspacePhase};

// ─── WorkspaceEffect ─────────────────────────────────────────────────────────

/// One-shot effects emitted by the workspace actor.
///
/// Effects are distinct from state: they are not accumulated and are consumed
/// exactly once by a subscriber (CLI wait-for-scan, test poll loop, etc.).
///
/// This mirrors `BusinessEffect` / `UiEffect` from Moneta: the actor sends
/// effects for transient events that don't belong in the persistent state.
#[allow(dead_code)] // consumed by Wave 3 CLI and test subscribers
pub(crate) enum WorkspaceEffect {
    /// Emitted when a workspace scan finishes (initial or reindex).
    ///
    /// CLI mode waits for this before returning to the interactive prompt.
    /// Tests can poll for it instead of sleeping.
    ScanComplete,

    /// The workspace root was switched to a new path.
    RootChanged { new_root: std::path::PathBuf },
}
