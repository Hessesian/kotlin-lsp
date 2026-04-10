use tree_sitter::Language;

extern "C" {
    fn tree_sitter_swift() -> Language;
}

/// Return the tree-sitter [`Language`] for Swift.
///
/// Compatible with tree-sitter 0.22.
pub fn language() -> Language {
    unsafe { tree_sitter_swift() }
}
