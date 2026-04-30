//! Extension trait adding source-line analysis methods to `[String]`.
//!
//! Each method is a thin façade over the original free function; no logic
//! lives here.  The original free functions are kept intact so existing
//! callers continue to compile during the incremental migration.
use tower_lsp::lsp_types::{Range, TextEdit};
use crate::types::{ImportEntry, Visibility};

pub(crate) trait LinesExt {
    /// Concatenate lines from `start_line..=end_line` into a single detail string.
    #[allow(dead_code)]
    fn extract_detail(&self, start_line: u32, end_line: u32) -> String;

    /// Kotlin/Java visibility of the declaration on `line_no`.
    #[allow(dead_code)]
    fn visibility_at(&self, line_no: usize) -> Visibility;

    /// Swift visibility of the declaration on `line_no`.
    #[allow(dead_code)]
    fn swift_visibility_at(&self, line_no: usize) -> Visibility;

    /// Names declared in these lines (for fast lookup without full parsing).
    #[allow(dead_code)]
    fn declared_names(&self) -> Vec<String>;

    /// All import entries found in these lines.
    fn parse_imports(&self) -> Vec<ImportEntry>;

    /// Collect a multi-line function/class signature starting at `start_line`.
    fn collect_signature(&self, start_line: usize) -> String;

    /// Collect parameters from the function starting at `start_line`.
    #[allow(dead_code)]
    fn collect_params_from_line(&self, start_line: usize) -> Option<String>;

    /// Find the name of the call expression that encloses `(line_no, col)`.
    #[allow(dead_code)]
    fn find_enclosing_call_name(&self, line_no: usize, col: usize) -> Option<String>;

    /// Line number at which a new `import` statement should be inserted.
    fn import_insertion_line(&self) -> u32;

    /// Build a TextEdit that inserts an import for `fqn` at the appropriate line.
    fn make_import_edit(&self, fqn: &str, needs_semicolon: bool) -> TextEdit;

    /// Infer the (stripped) type of `var_name` from a type annotation in these lines.
    fn infer_type(&self, var_name: &str) -> Option<String>;

    /// Like [`infer_type`] but preserves generic parameters.
    fn infer_type_raw(&self, var_name: &str) -> Option<String>;

    /// Find the declaration range of `name` anywhere in these lines.
    fn find_declaration_range(&self, name: &str) -> Option<Range>;

    /// Find the declaration range of `name` starting from `start_line`.
    fn find_declaration_range_after(&self, name: &str, start_line: u32) -> Option<Range>;
}

impl LinesExt for [String] {
    fn extract_detail(&self, start_line: u32, end_line: u32) -> String {
        crate::parser::extract_detail(self, start_line, end_line)
    }

    fn visibility_at(&self, line_no: usize) -> Visibility {
        crate::parser::visibility_at_line(self, line_no)
    }

    fn swift_visibility_at(&self, line_no: usize) -> Visibility {
        crate::parser::swift_visibility_at_line(self, line_no)
    }

    fn declared_names(&self) -> Vec<String> {
        crate::parser::extract_declared_names(self)
    }

    fn parse_imports(&self) -> Vec<ImportEntry> {
        crate::parser::parse_imports_from_lines(self)
    }

    fn collect_signature(&self, start_line: usize) -> String {
        crate::indexer::collect_signature(self, start_line)
    }

    fn collect_params_from_line(&self, start_line: usize) -> Option<String> {
        crate::indexer::collect_params_from_line(self, start_line)
    }

    fn find_enclosing_call_name(&self, line_no: usize, col: usize) -> Option<String> {
        crate::indexer::find_enclosing_call_name(self, line_no, col)
    }

    fn import_insertion_line(&self) -> u32 {
        crate::resolver::import_insertion_line(self)
    }

    fn make_import_edit(&self, fqn: &str, needs_semicolon: bool) -> TextEdit {
        crate::resolver::make_import_edit(fqn, self, needs_semicolon)
    }

    fn infer_type(&self, var_name: &str) -> Option<String> {
        crate::resolver::infer::infer_type_in_lines(self, var_name)
    }

    fn infer_type_raw(&self, var_name: &str) -> Option<String> {
        crate::resolver::infer::infer_type_in_lines_raw(self, var_name)
    }

    fn find_declaration_range(&self, name: &str) -> Option<Range> {
        crate::resolver::infer::find_declaration_range_in_lines(self, name)
    }

    fn find_declaration_range_after(&self, name: &str, start_line: u32) -> Option<Range> {
        crate::resolver::find::find_declaration_range_after_line(self, name, start_line)
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::LinesExt;

    fn lines(src: &str) -> Vec<String> {
        src.lines().map(str::to_owned).collect()
    }

    #[test]
    fn extract_detail_single_line() {
        let ls = lines("fun foo() {}");
        assert!(!ls.extract_detail(0, 0).is_empty());
    }

    #[test]
    fn visibility_at_private() {
        let ls = lines("private fun foo() {}");
        use crate::types::Visibility;
        assert_eq!(ls.visibility_at(0), Visibility::Private);
    }

    #[test]
    fn import_insertion_after_imports() {
        let ls = lines("package com.example\nimport android.os.Bundle\n\nclass Foo");
        assert!(ls.import_insertion_line() > 0);
    }

    #[test]
    fn declared_names_finds_val() {
        let ls = lines("val viewModel: MyViewModel");
        assert!(ls.declared_names().iter().any(|n| n == "viewModel"));
    }

    #[test]
    fn infer_type_finds_annotation() {
        let ls = lines("val items: List<String> = emptyList()");
        assert_eq!(ls.infer_type("items").as_deref(), Some("List"));
    }

    #[test]
    fn infer_type_raw_preserves_generics() {
        let ls = lines("val items: List<String> = emptyList()");
        assert_eq!(ls.infer_type_raw("items").as_deref(), Some("List<String>"));
    }

    #[test]
    fn find_declaration_range_finds_val() {
        let ls = lines("val account: Account = Account()");
        let r = ls.find_declaration_range("account");
        assert!(r.is_some());
    }

    #[test]
    fn collect_signature_single_line() {
        let ls = lines("fun foo(x: Int): Boolean {");
        assert_eq!(ls.collect_signature(0), "fun foo(x: Int): Boolean");
    }

    #[test]
    fn parse_imports_finds_import() {
        let ls = lines("import android.os.Bundle");
        let imports = ls.parse_imports();
        assert!(imports.iter().any(|i| i.full_path.contains("Bundle")));
    }
}
