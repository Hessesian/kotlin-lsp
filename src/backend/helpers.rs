use crate::types::SyntaxError;
use tower_lsp::lsp_types::*;

pub(crate) fn syntax_diagnostics(errors: &[SyntaxError]) -> Vec<Diagnostic> {
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
