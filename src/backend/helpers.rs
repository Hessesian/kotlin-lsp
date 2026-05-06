use crate::types::SyntaxError;
use crate::StrExt;
use tower_lsp::lsp_types::*;

/// Determine the `(parent_class, declared_pkg)` scope for a `findReferences` request.
///
/// For uppercase symbols the scope is narrowed via import analysis or declaration
/// site lookup so that `rg_find_references` Pass A/B can restrict results to the
/// specific class variant (e.g. the right `Event` among many sealed interfaces).
///
/// For lowercase symbols (fields, methods) `(None, None)` is returned — an
/// unscoped bare-word search is used.  Injecting a parent class derived from
/// `this`/`it` type inference would narrow rg to `ClassName.fieldName` qualified
/// patterns which almost never appear in real Kotlin code, leaving only in-memory
/// hits in the current file.
pub(super) fn resolve_references_scope(
    idx: &crate::indexer::Indexer,
    uri: &Url,
    line: u32,
    name: &str,
) -> (Option<String>, Option<String>) {
    if !name.starts_with_uppercase() {
        return (None, None);
    }
    let on_decl = idx.is_declared_in(uri, name)
        && idx
            .definitions
            .get(name)
            .map(|locs| {
                locs.iter()
                    .any(|l| l.uri == *uri && l.range.start.line == line)
            })
            .unwrap_or(false);
    if on_decl {
        let parent = idx.enclosing_class_at(uri, line);
        let pkg = idx.package_of(uri);
        return (parent, pkg);
    }
    let (parent, pkg) = idx.resolve_symbol_via_import(uri, name);
    if parent.is_some() || pkg.is_some() {
        return (parent, pkg);
    }
    let parent = idx.declared_parent_class_of(name, uri);
    let pkg = idx.declared_package_of(name);
    (parent, pkg)
}

pub(super) fn syntax_diagnostics(errors: &[SyntaxError]) -> Vec<Diagnostic> {
    errors
        .iter()
        .map(|e| Diagnostic {
            range: e.range,
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("kotlin-lsp".into()),
            message: e.message.clone(),
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
#[path = "helpers_tests.rs"]
mod tests;
