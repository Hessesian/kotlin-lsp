//! Feature query handlers — pure reads on capability trait abstractions.
//!
//! Each feature module takes exactly the capability trait bounds it needs.
//! The type system is the navigation graph:
//!
//! ```text
//! LSP request → Backend (adapter)
//!   → ensure_indexed (warm)
//!   → RawCursor::build (DocumentAccess)
//!   → features::X::compute(&cursor, &indexer)   ← this module
//!     → trait method call                        → Indexer (go-to-impl)
//! ```
//!
//! Two jumps from any trait call to the concrete implementation.

pub(crate) mod code_actions;
pub(crate) mod completion;
pub(crate) mod cursor;
pub(crate) mod definition;
pub(crate) mod fill_when;
pub(crate) mod folding;
pub(crate) mod highlight;
pub(crate) mod hover;
pub(crate) mod implementation;
pub(crate) mod references;
pub(crate) mod signature_help;
pub(crate) mod symbols;
pub(crate) mod text_utils;
pub(crate) mod traits;
mod traits_impl;
pub(crate) mod workspace_symbols;
