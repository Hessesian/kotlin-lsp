use std::sync::Arc;
use tower_lsp::lsp_types::Url;

use super::live_tree::{lang_for_path, parse_live, LiveDoc};
use super::Indexer;

impl Indexer {
    /// Parse `content` and store the resulting `LiveDoc` for `uri`.
    ///
    /// If the file extension is unsupported or parsing fails, any previously
    /// stored tree for `uri` is removed so consumers never read a stale doc.
    pub fn store_live_tree(&self, uri: &Url, content: &str) {
        let path = uri.path();
        match lang_for_path(path).and_then(|lang| parse_live(content, lang)) {
            Some(doc) => {
                self.live_trees.insert(uri.to_string(), Arc::new(doc));
            }
            None => {
                self.live_trees.remove(uri.as_str());
            }
        }
    }

    /// Return the `LiveDoc` for `uri`, or `None` if the file is not open.
    pub fn live_doc(&self, uri: &Url) -> Option<Arc<LiveDoc>> {
        self.live_trees.get(uri.as_str()).map(|r| Arc::clone(&*r))
    }

    /// Remove the live parse tree for `uri` (called on `textDocument/didClose`).
    pub fn remove_live_tree(&self, uri: &Url) {
        self.live_trees.remove(uri.as_str());
    }
}
