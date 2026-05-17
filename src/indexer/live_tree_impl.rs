use std::sync::Arc;
use tower_lsp::lsp_types::Url;

use super::live_tree::{lang_for_path, parse_live, LiveDoc};
use super::Indexer;

impl Indexer {
    /// Parse `content` and store the resulting `LiveDoc` for `uri`.
    ///
    /// If the file extension is unsupported or parsing fails, any previously
    /// stored tree for `uri` is removed so consumers never read a stale doc.
    pub(crate) fn store_live_tree(&self, uri: &Url, content: &str) {
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
    pub(crate) fn live_doc(&self, uri: &Url) -> Option<Arc<LiveDoc>> {
        self.live_trees.get(uri.as_str()).map(|r| Arc::clone(&*r))
    }

    /// Return the `LiveDoc` for `uri`, parsing on-demand if not already cached.
    ///
    /// Used by CST-based queries (e.g. `enclosing_class_at`) that are called
    /// before `textDocument/didOpen` has been fully processed — the actor
    /// receives `did_open` asynchronously so the live tree may not exist yet
    /// when a navigation request arrives.  Content is taken from `live_lines`
    /// (if present), the indexed file, or disk (cold-start fallback); the
    /// result is stored into `live_trees` **only when the file is currently open**
    /// (i.e. `live_lines` is present) so that `live_trees` does not accumulate
    /// parse trees for files that were never opened by the editor.
    pub(crate) fn live_doc_or_parse(&self, uri: &Url) -> Option<Arc<LiveDoc>> {
        if let Some(doc) = self.live_doc(uri) {
            return Some(doc);
        }

        let is_open = self.live_lines.contains_key(uri.as_str());

        let content: String = if let Some(ll) = self.live_lines.get(uri.as_str()) {
            ll.join("\n")
        } else if let Some(fd) = self.files.get(uri.as_str()) {
            fd.lines.join("\n")
        } else if let Ok(path) = uri.to_file_path() {
            std::fs::read_to_string(path).ok()?
        } else {
            return None;
        };

        let lang = lang_for_path(uri.path())?;
        let doc = parse_live(&content, lang)?;
        let doc = Arc::new(doc);
        if is_open {
            self.live_trees.insert(uri.to_string(), Arc::clone(&doc));
        }
        Some(doc)
    }

    /// Remove the live parse tree for `uri` (called on `textDocument/didClose`).
    pub(crate) fn remove_live_tree(&self, uri: &Url) {
        self.live_trees.remove(uri.as_str());
    }
}
