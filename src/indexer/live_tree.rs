use tree_sitter::{Language as TsLanguage, Parser, Tree};

pub(crate) struct LiveDoc {
    pub bytes: Vec<u8>,
    pub tree: Tree,
}

/// Return the tree-sitter `Language` for the given file path, or `None` for
/// unsupported extensions.  This is the extension→language map used for
/// live-tree parsing only; it covers a strict subset of the extensions
/// recognised by `parser.rs`'s `parse_by_extension`.  Unlike that function,
/// `lang_for_path` never falls back to a default language for unknown
/// extensions — it returns `None` so callers can skip live-tree work entirely.
pub(crate) fn lang_for_path(path: &str) -> Option<TsLanguage> {
    match crate::Language::from_path(path) {
        crate::Language::Swift if path.ends_with(".swift") => {
            Some(tree_sitter_swift_bundled::language())
        }
        crate::Language::Java if path.ends_with(".java") => Some(tree_sitter_java::language()),
        crate::Language::Kotlin if path.ends_with(".kt") || path.ends_with(".kts") => {
            Some(tree_sitter_kotlin::language())
        }
        _ => None,
    }
}

/// Convert a UTF-16 column offset (as used in LSP positions) to a byte offset
/// within `line_text`.  Tree-sitter `Point::column` expects byte offsets.
pub(crate) fn utf16_col_to_byte(line_text: &str, utf16_col: usize) -> usize {
    let mut utf16 = 0usize;
    for (bi, ch) in line_text.char_indices() {
        if utf16 >= utf16_col {
            return bi;
        }
        utf16 += ch.len_utf16();
    }
    line_text.len()
}

/// Parse `content` with `lang` and return a `LiveDoc`, or `None` if the
/// parser fails (malformed grammar state — extremely rare).
pub(crate) fn parse_live(content: &str, lang: TsLanguage) -> Option<LiveDoc> {
    let mut parser = Parser::new();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(content, None)?;
    Some(LiveDoc {
        bytes: content.as_bytes().to_vec(),
        tree,
    })
}

#[cfg(test)]
#[path = "live_tree_tests.rs"]
mod tests;
