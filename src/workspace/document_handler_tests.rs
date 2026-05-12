use std::sync::Arc;

use tower_lsp::lsp_types::Url;

use crate::indexer::Indexer;

use super::DocumentHandler;
use crate::workspace::file_change_handler::FileChangeHandler;

#[tokio::test]
async fn handle_file_closed_clears_live_document_state() {
    let indexer = Arc::new(Indexer::new());
    let handler = DocumentHandler::new(Arc::clone(&indexer), None);
    let mut file_change_handler = FileChangeHandler::new(Arc::clone(&indexer), None);
    let uri = Url::parse("file:///workspace/Main.kt").unwrap();
    let content = "fun main() = Unit";

    indexer.set_live_lines(&uri, content);
    indexer.store_live_tree(&uri, content);

    handler
        .handle_file_closed(&mut file_change_handler, uri.clone())
        .await;

    assert!(!indexer.live_lines.contains_key(uri.as_str()));
    assert!(indexer.live_doc(&uri).is_none());
}
