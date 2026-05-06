//! Extension trait adding source-line analysis methods to `[String]`.
//!
//! Each method is a thin façade over the original free function; no logic
//! lives here.  The original free functions are kept intact so existing
//! callers continue to compile during the incremental migration.
use crate::types::{ImportEntry, Visibility};
use tower_lsp::lsp_types::{Range, TextEdit};

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
#[path = "lines_ext_tests.rs"]
mod tests;
