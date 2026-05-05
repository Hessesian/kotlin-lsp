use crate::types::SyntaxError;
use crate::StrExt;
use tower_lsp::lsp_types::*;

/// Determine the `(parent_class, declared_pkg)` scope for a `findReferences` request.
///
/// For uppercase symbols the scope is narrowed via import analysis or declaration
/// site lookup so that `rg_find_references` Pass A/B can restrict results to the
/// specific class variant (e.g. the right `Event` among many sealed interfaces).
///
/// For lowercase symbols (fields, methods) `(None, None)` is returned — an
/// unscoped bare-word search is used.  Injecting a parent class derived from
/// `this`/`it` type inference would narrow rg to `ClassName.fieldName` qualified
/// patterns which almost never appear in real Kotlin code, leaving only in-memory
/// hits in the current file.
pub(super) fn resolve_references_scope(
    idx: &crate::indexer::Indexer,
    uri: &Url,
    line: u32,
    name: &str,
) -> (Option<String>, Option<String>) {
    if !name.starts_with_uppercase() {
        return (None, None);
    }
    let on_decl = idx.is_declared_in(uri, name)
        && idx
            .definitions
            .get(name)
            .map(|locs| {
                locs.iter()
                    .any(|l| l.uri == *uri && l.range.start.line == line)
            })
            .unwrap_or(false);
    if on_decl {
        let parent = idx.enclosing_class_at(uri, line);
        let pkg = idx.package_of(uri);
        return (parent, pkg);
    }
    let (parent, pkg) = idx.resolve_symbol_via_import(uri, name);
    if parent.is_some() || pkg.is_some() {
        return (parent, pkg);
    }
    let parent = idx.declared_parent_class_of(name, uri);
    let pkg = idx.declared_package_of(name);
    (parent, pkg)
}

pub(super) fn syntax_diagnostics(errors: &[SyntaxError]) -> Vec<Diagnostic> {
    errors
        .iter()
        .map(|e| Diagnostic {
            range: e.range,
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("kotlin-lsp".into()),
            message: e.message.clone(),
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::rename::{enclosing_scope, rename_in_scope};
    use super::resolve_references_scope;
    use tower_lsp::lsp_types::TextEdit;

    fn lines(src: &str) -> Vec<String> {
        src.lines().map(|l| l.to_owned()).collect()
    }

    fn col(edits: &[TextEdit], i: usize) -> (u32, u32) {
        (edits[i].range.start.character, edits[i].range.end.character)
    }

    // ── rename_in_scope ───────────────────────────────────────────────────────

    #[test]
    fn rename_two_occurrences_same_line() {
        let src = "val x = foo + foo\n";
        let ls = lines(src);
        let edits = rename_in_scope(&ls, "foo", "bar", (0, 0));
        assert_eq!(edits.len(), 2, "expected 2 edits, got: {edits:?}");
        // Sorted descending: second occurrence first
        assert!(
            edits[0].range.start.character > edits[1].range.start.character,
            "edits not in descending order: {edits:?}"
        );
        assert_eq!(edits[0].new_text, "bar");
        assert_eq!(edits[1].new_text, "bar");
    }

    #[test]
    fn rename_three_occurrences_same_line() {
        let src = "foo(foo, foo)\n";
        let ls = lines(src);
        let edits = rename_in_scope(&ls, "foo", "baz", (0, 0));
        assert_eq!(edits.len(), 3, "expected 3 edits, got: {edits:?}");
        // Strictly descending columns
        assert!(col(&edits, 0).0 > col(&edits, 1).0);
        assert!(col(&edits, 1).0 > col(&edits, 2).0);
        for e in &edits {
            assert_eq!(e.new_text, "baz");
        }
    }

    #[test]
    fn rename_three_occurrences_across_lines() {
        let src = "fun go() {\n    val a = foo\n    foo.bar()\n    return foo\n}\n";
        let ls = lines(src);
        let scope = (0, ls.len().saturating_sub(1));
        let edits = rename_in_scope(&ls, "foo", "qux", scope);
        assert_eq!(edits.len(), 3, "expected 3 edits, got: {edits:?}");
        // Sorted descending: last line first
        assert!(edits[0].range.start.line > edits[1].range.start.line);
        assert!(edits[1].range.start.line > edits[2].range.start.line);
    }

    #[test]
    fn rename_four_occurrences_mixed() {
        // Two on line 1, one on line 2, one on line 3
        let src = "fun go() {\n    foo(foo)\n    foo.x\n    y(foo)\n}\n";
        let ls = lines(src);
        let scope = (0, ls.len().saturating_sub(1));
        let edits = rename_in_scope(&ls, "foo", "replaced", scope);
        assert_eq!(edits.len(), 4, "expected 4 edits, got: {edits:?}");
        // All replaced correctly
        for e in &edits {
            assert_eq!(e.new_text, "replaced");
        }
        // All edits are within the original positions (no position drift)
        // Line 3: y(foo) — foo starts at col 6
        assert_eq!(edits[0].range.start.line, 3);
        assert_eq!(edits[0].range.start.character, 6);
    }

    #[test]
    fn rename_no_false_positives_substring() {
        // `fooBar` must NOT be renamed when renaming `foo`
        let src = "val fooBar = foo\n";
        let ls = lines(src);
        let edits = rename_in_scope(&ls, "foo", "bar", (0, 0));
        assert_eq!(
            edits.len(),
            1,
            "substring match must not be renamed: {edits:?}"
        );
        assert_eq!(edits[0].range.start.character, 13); // only trailing `foo`
    }

    #[test]
    fn rename_at_line_start_and_end() {
        let src = "foo val foo\n";
        let ls = lines(src);
        let edits = rename_in_scope(&ls, "foo", "x", (0, 0));
        assert_eq!(edits.len(), 2);
        // end occurrence first (descending)
        assert_eq!(edits[0].range.start.character, 8);
        assert_eq!(edits[1].range.start.character, 0);
    }

    #[test]
    fn rename_edits_cover_correct_utf16_range() {
        // ASCII-only: char index == UTF-16 index
        let src = "val foo = foo\n";
        let ls = lines(src);
        let edits = rename_in_scope(&ls, "foo", "renamed", (0, 0));
        // `val foo` at col 4..7; trailing `foo` at col 10..13
        let cols: Vec<(u32, u32)> = edits
            .iter()
            .map(|e| (e.range.start.character, e.range.end.character))
            .collect();
        assert!(cols.contains(&(10, 13)), "trailing foo not found: {cols:?}");
        assert!(cols.contains(&(4, 7)), "leading foo not found: {cols:?}");
    }

    // ── enclosing_scope ───────────────────────────────────────────────────────

    #[test]
    fn enclosing_scope_simple_function() {
        let src = "fun go() {\n    val x = 1\n    val y = x + x\n}\n";
        let ls = lines(src);
        let (start, end) = enclosing_scope(&ls, 2);
        assert_eq!(start, 0);
        assert_eq!(end, 3);
    }

    #[test]
    fn enclosing_scope_nested_braces() {
        let src = "fun go() {\n    if (true) {\n        foo\n    }\n}\n";
        let ls = lines(src);
        // cursor inside inner block
        let (start, end) = enclosing_scope(&ls, 2);
        assert_eq!(start, 1, "should find the inner {{ at line 1");
        assert_eq!(end, 3, "inner block closes at line 3");
    }

    // ── resolve_references_scope ──────────────────────────────────────────────

    fn make_indexer_with(src: &str, uri: &tower_lsp::lsp_types::Url) -> crate::indexer::Indexer {
        let idx = crate::indexer::Indexer::new();
        idx.index_content(uri, src);
        idx
    }

    /// Lowercase member names must always yield (None, None) regardless of context.
    /// This prevents the caller from injecting a parent_class derived from this/it
    /// type inference, which would scope rg to `ClassName.fieldName` qualified
    /// patterns that almost never appear in real Kotlin code.
    #[test]
    fn scope_lowercase_name_always_none() {
        let uri = tower_lsp::lsp_types::Url::parse("file:///t.kt").unwrap();
        let src = "package demo\nclass Foo { val descriptiveNumber: String = \"\" }";
        let idx = make_indexer_with(src, &uri);
        let (parent, pkg) = resolve_references_scope(&idx, &uri, 1, "descriptiveNumber");
        assert_eq!(parent, None, "lowercase member must not get a parent_class");
        assert_eq!(pkg, None, "lowercase member must not get a declared_pkg");
    }

    /// Uppercase names on the declaration line should use enclosing class + package.
    #[test]
    fn scope_uppercase_on_declaration_uses_enclosing_class() {
        let uri = tower_lsp::lsp_types::Url::parse("file:///t.kt").unwrap();
        let src = "package demo\nclass Outer {\n    class Inner\n}";
        let idx = make_indexer_with(src, &uri);
        // `Inner` is declared on line 2 inside `Outer`
        let (parent, pkg) = resolve_references_scope(&idx, &uri, 2, "Inner");
        assert_eq!(
            parent.as_deref(),
            Some("Outer"),
            "declaration site: parent should be enclosing class"
        );
        assert_eq!(pkg.as_deref(), Some("demo"));
    }
}
