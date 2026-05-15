use std::sync::Arc;

use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, Url};

use crate::features::call_arg_diagnostics::call_arg_diagnostics;
use crate::indexer::live_tree::parse_live;
use crate::indexer::Indexer;

use super::FileChangeHandler;

fn change(text: &str) -> Vec<TextDocumentContentChangeEvent> {
    vec![TextDocumentContentChangeEvent {
        range: None,
        range_length: None,
        text: text.to_string(),
    }]
}

#[tokio::test]
async fn handle_file_changed_stores_live_lines() {
    let indexer = Arc::new(Indexer::new());
    let mut handler = FileChangeHandler::new(Arc::clone(&indexer), None);
    let uri = Url::parse("file:///workspace/Main.kt").unwrap();

    handler
        .handle_file_changed(uri.clone(), change("fun main() = Unit"))
        .await;

    let live_lines = indexer.live_lines.get(uri.as_str()).unwrap();
    assert_eq!(live_lines.as_ref(), &vec!["fun main() = Unit".to_string()]);

    handler.cancel_pending_reindex(&uri);
}

/// Regression: type loadData() → delete → retype should leave the indexer in a
/// state where call_arg_diagnostics fires on the final content.
///
/// This tests the FULL ASYNC PIPELINE (FileChangeHandler debounce + index_content)
/// rather than the unit-level logic already covered in call_arg_diagnostics_tests.rs.
/// It catches bugs where the debounce task or generation counter leaves indexer.files
/// in a stale state after the retype cycle.
#[tokio::test]
async fn retype_cycle_indexer_state_supports_diagnostics() {
    // Use a temp workspace dir so workspace.json scan doesn't pick up host sources.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("workspace.json"), r#"{"sourcePaths":[]}"#).unwrap();
    let file_path = tmp.path().join("Foo.kt");

    let indexer = Arc::new(Indexer::new());
    // Set workspace root so the file is treated as a workspace file.
    indexer.workspace_root.set(tmp.path().to_path_buf());

    let mut handler = FileChangeHandler::new(Arc::clone(&indexer), None);
    let uri = Url::parse(&format!("file://{}", file_path.display())).unwrap();

    let with_call = concat!(
        "fun loadData(arg: String) {}\n",
        "fun main() {\n",
        "    loadData()\n",
        "}\n",
    );
    let without_call = concat!("fun loadData(arg: String) {}\n", "fun main() {\n", "}\n",);

    // Step 1: type the call.
    handler
        .handle_file_changed(uri.clone(), change(with_call))
        .await;
    tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;

    // Indexer should have the file with `loadData` defined.
    assert!(
        indexer.files.contains_key(uri.as_str()),
        "step1: indexer.files should have uri after debounce"
    );
    let diags1 = {
        let doc = parse_live(with_call, tree_sitter_kotlin::language()).unwrap();
        call_arg_diagnostics(&indexer, &uri, &doc)
    };
    assert!(
        !diags1.is_empty(),
        "step1: expected diagnostic from indexer state"
    );

    // Step 2: delete the call.
    handler
        .handle_file_changed(uri.clone(), change(without_call))
        .await;
    tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;

    let diags2 = {
        let doc = parse_live(without_call, tree_sitter_kotlin::language()).unwrap();
        call_arg_diagnostics(&indexer, &uri, &doc)
    };
    assert!(
        diags2.is_empty(),
        "step2: expected no diagnostic after delete"
    );

    // Step 3: retype the call.
    handler
        .handle_file_changed(uri.clone(), change(with_call))
        .await;
    tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;

    // The indexer state must support diagnostics on the retyped content.
    let diags3 = {
        let doc = parse_live(with_call, tree_sitter_kotlin::language()).unwrap();
        call_arg_diagnostics(&indexer, &uri, &doc)
    };
    assert!(
        !diags3.is_empty(),
        "step3: expected diagnostic after retype — indexer state is stale"
    );
}
