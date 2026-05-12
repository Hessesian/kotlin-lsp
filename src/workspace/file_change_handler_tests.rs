use std::sync::Arc;

use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, Url};

use crate::indexer::Indexer;

use super::FileChangeHandler;

#[tokio::test]
async fn handle_file_changed_stores_live_lines() {
    let indexer = Arc::new(Indexer::new());
    let mut handler = FileChangeHandler::new(Arc::clone(&indexer), None);
    let uri = Url::parse("file:///workspace/Main.kt").unwrap();

    handler
        .handle_file_changed(
            uri.clone(),
            vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "fun main() = Unit".to_string(),
            }],
        )
        .await;

    let live_lines = indexer.live_lines.get(uri.as_str()).unwrap();
    assert_eq!(live_lines.as_ref(), &vec!["fun main() = Unit".to_string()]);

    handler.cancel_pending_reindex(&uri);
}
