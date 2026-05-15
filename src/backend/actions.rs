use super::Backend;
use crate::features;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;

impl Backend {
    pub(super) async fn completion_impl(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let pp = params.text_document_position;
        let uri = &pp.text_document.uri;
        let position = pp.position;
        let snippets = self
            .snippet_support
            .load(std::sync::atomic::Ordering::Relaxed);

        Ok(crate::features::completion::compute_completions(
            uri,
            position,
            snippets,
            self.indexer.as_ref(),
        ))
    }

    pub(super) async fn code_action_impl(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<Vec<CodeActionOrCommand>>> {
        let uri = &params.text_document.uri;
        let range = params.range;

        let lines = self.indexer.mem_lines_for(uri.as_str());
        let ln = range.start.line as usize;
        let line_text: String = lines
            .as_ref()
            .and_then(|l| l.get(ln).cloned())
            .unwrap_or_default();
        let all_lines: Vec<String> = lines
            .as_ref()
            .map(|l| l.as_ref().clone())
            .unwrap_or_default();
        let is_kotlin = crate::Language::from_path(uri.path()) == crate::Language::Kotlin;

        let mut actions = features::code_actions::compute_code_actions(
            &line_text, &all_lines, uri, range, is_kotlin,
        );

        // Indexed code actions (require live tree + symbol index)
        if is_kotlin {
            if let Some(action) =
                features::fill_when::build_fill_when_action(self.indexer.as_ref(), uri, range)
            {
                actions.push(action);
            }
        }

        Ok(if actions.is_empty() {
            None
        } else {
            Some(actions)
        })
    }
}
