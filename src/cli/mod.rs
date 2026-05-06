//! Standalone CLI interface for kotlin-lsp.
//!
//! Subcommands:
//!   find  <name>                 — locate declarations for NAME
//!   refs  <name>                 — locate all usages of NAME
//!   hover <file> <line> <col>    — show symbol signature at position
//!   index                        — pre-build the workspace index cache
//!
//! Modes (default: auto):
//!   auto   — load cache if available; fall back to rg/fd when no cache exists
//!   --fast  — always use rg/fd, never load index
//!   --smart — require a pre-built index; exit with error if absent
//!
//! Output:
//!   plain text to stdout (default)
//!   --json — emit a JSON array of result objects
//!
//! Root:
//!   --root <dir> — workspace root (default: nearest .git parent, then cwd)
//!
//! All diagnostics and mode notices go to stderr only.

mod args;
mod hover;
mod output;
mod run;

pub(crate) use args::CliArgs;
pub(crate) use run::run;
