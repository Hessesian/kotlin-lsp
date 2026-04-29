use tree_sitter::{Language, Parser, Tree};

pub struct LiveDoc {
    pub bytes: Vec<u8>,
    pub tree:  Tree,
}

/// Return the tree-sitter `Language` for the given file path, or `None` for
/// unsupported extensions.  This is the canonical extension→language map for
/// live-tree parsing; `parser.rs`'s `parse_by_extension` uses the same order.
pub fn lang_for_path(path: &str) -> Option<Language> {
    if path.ends_with(".swift") {
        Some(tree_sitter_swift_bundled::language())
    } else if path.ends_with(".java") {
        Some(tree_sitter_java::language())
    } else if path.ends_with(".kt") || path.ends_with(".kts") {
        Some(tree_sitter_kotlin::language())
    } else {
        None
    }
}

/// Parse `content` with `lang` and return a `LiveDoc`, or `None` if the
/// parser fails (malformed grammar state — extremely rare).
pub fn parse_live(content: &str, lang: Language) -> Option<LiveDoc> {
    let mut parser = Parser::new();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(content, None)?;
    Some(LiveDoc { bytes: content.as_bytes().to_vec(), tree })
}

#[cfg(test)]
#[path = "live_tree_tests.rs"]
mod tests;
