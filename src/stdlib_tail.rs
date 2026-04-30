
use crate::stdlib::dot_completions_for;

/// Language-aware dot completions: only return Kotlin stdlib for .kt files,
/// add Swift-specific templates for .swift files. `from_path` should be the
/// file path portion of the request URI (e.g., "/home/user/project/src/Foo.kt").
pub fn dot_completions_for_lang(from_path: &str, receiver_type: &str, snippets: bool) -> Vec<tower_lsp::lsp_types::CompletionItem> {
    if from_path.ends_with(".kt") {
        dot_completions_for(receiver_type, snippets)
    } else if from_path.ends_with(".swift") {
        // Very small Swift snippet set: common patterns
        swift_dot_completions(snippets)
    } else {
        Vec::new()
    }
}

fn swift_dot_completions(snippets: bool) -> Vec<tower_lsp::lsp_types::CompletionItem> {
    use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat};
    let mut items = Vec::new();
    if snippets {
        // simple swift snippets
        items.push(CompletionItem {
            label: "guard".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some("guard statement".to_string()),
            insert_text: Some("guard ${1:condition} else {\n    ${0}\n}".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        });
        items.push(CompletionItem {
            label: "ifLet".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some("if let optional binding".to_string()),
            insert_text: Some("if let ${1:val} = ${2:optional} {\n    ${0}\n}".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    }
    items
}
