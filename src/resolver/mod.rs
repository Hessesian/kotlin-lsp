//! Symbol resolution for Kotlin, Java, and Swift.
//!
//! See [`resolve`] for the resolution chain and strategy documentation.

pub(crate) mod complete;
mod fd;
pub(crate) mod find;
mod hierarchy;
mod import_edit;
pub(crate) mod infer;
pub(crate) mod resolve;
#[cfg(test)]
mod tests;

// ─── re-exports ───────────────────────────────────────────────────────────────

pub(crate) use complete::is_annotation_context;
pub(crate) use complete::{
    complete_symbol, complete_symbol_with_context, symbols_from_uri_as_completions_pub,
};
pub(crate) use hierarchy::walk_hierarchy;
pub(crate) use import_edit::{already_imported, import_insertion_line, make_import_edit};
pub(crate) use infer::{
    extract_collection_element_type, infer_receiver_type, infer_variable_type_raw, ReceiverKind,
    ReceiverType,
};
pub(crate) use resolve::{
    ensure_file_data, fqns_for_name, resolve_symbol_inner, resolve_symbol_no_rg,
};

// Re-exports used only in tests.
#[cfg(test)]
pub(crate) use crate::rg::build_rg_pattern;
#[cfg(test)]
pub(crate) use complete::{
    complete_bare, complete_dot, is_screaming_snake, match_score, COMPLETION_CAP,
};
#[cfg(test)]
use fd::import_file_stems;
#[cfg(test)]
use fd::package_prefix;
#[cfg(test)]
pub(crate) use infer::{
    find_declaration_range_in_lines, infer_type_in_lines, infer_type_in_lines_raw,
    infer_variable_type,
};
#[cfg(test)]
pub(crate) use resolve::{is_stdlib, resolve_symbol};
