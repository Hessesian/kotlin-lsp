//! Unit tests for the doc-comment extraction functions in `doc.rs`.
use super::extract_doc_comment;

fn lines(src: &str) -> Vec<String> {
    src.lines().map(String::from).collect()
}

#[test]
fn kdoc_simple_block_comment() {
    let src = r#"
/**
 * Does something useful.
 */
fun doThing() {}"#;
    let ls = lines(src);
    let decl = ls.iter().position(|l| l.contains("fun doThing")).unwrap();
    let doc = extract_doc_comment(&ls, decl).unwrap();
    assert!(doc.contains("Does something useful"), "got: {doc}");
    // extract_doc_comment returns plain text; no code block here
    assert!(!doc.contains("```"), "got: {doc}");
}

#[test]
fn kdoc_with_params_and_return() {
    let src = r#"
/**
 * Fetches the widget.
 *
 * @param id The widget identifier.
 * @param flag Whether to refresh.
 * @return The widget or null.
 */
fun getWidget(id: Int, flag: Boolean): Widget? = null"#;
    let ls = lines(src);
    let decl = ls.iter().position(|l| l.contains("fun getWidget")).unwrap();
    let doc = extract_doc_comment(&ls, decl).unwrap();
    assert!(doc.contains("Fetches the widget"), "got: {doc}");
    assert!(doc.contains("**Parameters**"), "got: {doc}");
    assert!(doc.contains("`id`"), "got: {doc}");
    assert!(doc.contains("`flag`"), "got: {doc}");
    assert!(doc.contains("**Returns**"), "got: {doc}");
}

#[test]
fn kdoc_skips_annotations() {
    let src = r#"
/**
 * Annotated function.
 */
@Suppress("unused")
@JvmStatic
fun annotated() {}"#;
    let ls = lines(src);
    let decl = ls.iter().position(|l| l.contains("fun annotated")).unwrap();
    let doc = extract_doc_comment(&ls, decl).unwrap();
    assert!(doc.contains("Annotated function"), "got: {doc}");
}

#[test]
fn kdoc_no_comment_returns_none() {
    let src = "fun plain() {}";
    let ls = lines(src);
    assert!(extract_doc_comment(&ls, 0).is_none());
}

#[test]
fn kdoc_line_comments() {
    let src = r#"// Short description.
// More detail.
fun withLineDoc() {}"#;
    let ls = lines(src);
    let decl = 2;
    let doc = extract_doc_comment(&ls, decl).unwrap();
    assert!(doc.contains("Short description"), "got: {doc}");
    assert!(doc.contains("More detail"), "got: {doc}");
}

#[test]
fn kdoc_inline_code_and_links() {
    let src = r#"
/**
 * Use {@code Foo.bar()} or [Baz] to achieve this.
 */
fun example() {}"#;
    let ls = lines(src);
    let decl = ls.iter().position(|l| l.contains("fun example")).unwrap();
    let doc = extract_doc_comment(&ls, decl).unwrap();
    assert!(doc.contains("`Foo.bar()`"), "got: {doc}");
    assert!(doc.contains("`Baz`"), "got: {doc}");
}
