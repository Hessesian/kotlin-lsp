//! Signature help feature — extracts the active call site via CST and returns parameter info.
//!
//! The CST (`cst_call_info`) is authoritative: it walks up the live tree-sitter parse tree to
//! find the enclosing `call_expression`, counts `value_argument` children for `active_param`,
//! and handles multiline calls naturally. No text-scan fallback is needed or used.

use tower_lsp::lsp_types::{
    ParameterInformation, ParameterLabel, Position, SignatureHelp, SignatureInformation, Url,
};

use super::traits::{LiveTreeAccess, SignatureIndex};

/// Compute signature help for the call under the cursor at `pos` in `uri`.
///
/// Returns `None` when:
/// - no live parse tree exists for the file (not yet opened/edited),
/// - the cursor is not inside a `call_expression` (e.g. inside a trailing lambda body), or
/// - the function name cannot be resolved to a known signature.
pub(crate) fn compute_signature_help(
    uri: &Url,
    pos: Position,
    index: &(impl SignatureIndex + LiveTreeAccess),
) -> Option<SignatureHelp> {
    let ci = index.call_info_at(pos, uri)?;

    let params_text =
        index.find_fun_signature_with_receiver(uri, &ci.fn_name, ci.qualifier.as_deref())?;

    build_signature_help(&ci.fn_name, &params_text, ci.active_param)
}

fn build_signature_help(
    fn_name: &str,
    params_text: &str,
    active_param: u32,
) -> Option<SignatureHelp> {
    let raw = params_text.trim_matches(|c| c == '(' || c == ')');
    let param_parts: Vec<&str> = raw
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let parameters: Vec<ParameterInformation> = param_parts
        .iter()
        .map(|p| ParameterInformation {
            label: ParameterLabel::Simple(p.to_string()),
            documentation: None,
        })
        .collect();
    let label = format!("{}({})", fn_name, param_parts.join(", "));
    let active_param = active_param.min(parameters.len().saturating_sub(1) as u32);
    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation: None,
            parameters: Some(parameters),
            active_parameter: Some(active_param),
        }],
        active_signature: Some(0),
        active_parameter: Some(active_param),
    })
}
