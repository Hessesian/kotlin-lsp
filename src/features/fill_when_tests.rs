use tower_lsp::lsp_types::*;

use crate::features::fill_when::build_fill_when_action;
use crate::indexer::Indexer;

fn uri(path: &str) -> Url {
    Url::parse(&format!("file:///test{path}")).unwrap()
}

fn setup(files: &[(&str, &str)]) -> Indexer {
    let idx = Indexer::new();
    for (path, src) in files {
        let u = uri(path);
        idx.index_content(&u, src);
        idx.set_live_lines(&u, src);
        idx.store_live_tree(&u, src);
    }
    idx
}

fn cursor_at(line: u32, col: u32) -> Range {
    Range::new(Position::new(line, col), Position::new(line, col))
}

// ─── Enum tests ───────────────────────────────────────────────────────────────

const ENUM_SRC: &str = "\
enum class Color {
    RED, GREEN, BLUE
}
";

const ENUM_WHEN_PARTIAL: &str = "\
fun handle(c: Color) {
    when (c) {
        Color.RED -> println(\"red\")
    }
}
";

#[test]
fn enum_missing_branches_offered() {
    let idx = setup(&[("/Color.kt", ENUM_SRC), ("/main.kt", ENUM_WHEN_PARTIAL)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(1, 6));
    assert!(action.is_some(), "expected fill-when action");
    let action = action.unwrap();
    match action {
        CodeActionOrCommand::CodeAction(ca) => {
            assert!(ca.title.contains("Color"), "title: {}", ca.title);
            let edit = ca.edit.unwrap();
            let changes = edit.changes.unwrap();
            let edits = changes.get(&u).unwrap();
            let text = &edits[0].new_text;
            assert!(text.contains("Color.GREEN"), "missing GREEN: {text}");
            assert!(text.contains("Color.BLUE"), "missing BLUE: {text}");
            assert!(
                !text.contains("Color.RED"),
                "RED should not be re-added: {text}"
            );
        }
        _ => panic!("expected CodeAction"),
    }
}

#[test]
fn enum_all_branches_covered_no_action() {
    let src = "\
fun handle(c: Color) {
    when (c) {
        Color.RED -> println(\"r\")
        Color.GREEN -> println(\"g\")
        Color.BLUE -> println(\"b\")
    }
}
";
    let idx = setup(&[("/Color.kt", ENUM_SRC), ("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(1, 6));
    assert!(action.is_none(), "no action when all branches covered");
}

#[test]
fn enum_else_branch_suppresses_action() {
    let src = "\
fun handle(c: Color) {
    when (c) {
        Color.RED -> println(\"r\")
        else -> println(\"other\")
    }
}
";
    let idx = setup(&[("/Color.kt", ENUM_SRC), ("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(1, 6));
    assert!(action.is_none(), "else branch should suppress action");
}

// ─── Sealed class tests ──────────────────────────────────────────────────────

const SEALED_SRC: &str = "\
sealed class Effect {
    data class ShowToast(val message: String) : Effect()
    object Loading : Effect()
    data object Done : Effect()
}
";

const SEALED_WHEN_PARTIAL: &str = "\
fun handle(e: Effect) {
    when (e) {
        is Effect.ShowToast -> println(\"toast\")
    }
}
";

#[test]
fn sealed_missing_branches_offered() {
    let idx = setup(&[
        ("/Effect.kt", SEALED_SRC),
        ("/main.kt", SEALED_WHEN_PARTIAL),
    ]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(1, 6));
    assert!(
        action.is_some(),
        "expected fill-when action for sealed class"
    );
    let action = action.unwrap();
    match action {
        CodeActionOrCommand::CodeAction(ca) => {
            let edit = ca.edit.unwrap();
            let changes = edit.changes.unwrap();
            let edits = changes.get(&u).unwrap();
            let text = &edits[0].new_text;
            // Loading and Done are objects — should NOT use `is`
            assert!(text.contains("Effect.Loading"), "missing Loading: {text}");
            assert!(text.contains("Effect.Done"), "missing Done: {text}");
            assert!(
                !text.contains("Effect.ShowToast"),
                "ShowToast should not be re-added: {text}"
            );
        }
        _ => panic!("expected CodeAction"),
    }
}

#[test]
fn sealed_all_covered_no_action() {
    let src = "\
fun handle(e: Effect) {
    when (e) {
        is Effect.ShowToast -> println(\"t\")
        Effect.Loading -> println(\"l\")
        Effect.Done -> println(\"d\")
    }
}
";
    let idx = setup(&[("/Effect.kt", SEALED_SRC), ("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(1, 6));
    assert!(
        action.is_none(),
        "no action when all sealed branches covered"
    );
}

// ─── Edge cases ──────────────────────────────────────────────────────────────

#[test]
fn cursor_outside_when_no_action() {
    let src = "\
fun handle(c: Color) {
    println(\"hello\")
}
";
    let idx = setup(&[("/Color.kt", ENUM_SRC), ("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(1, 4));
    assert!(action.is_none(), "no action outside when expression");
}

#[test]
fn when_without_subject_no_action() {
    let src = "\
fun handle() {
    when {
        true -> println(\"yes\")
    }
}
";
    let idx = setup(&[("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(1, 6));
    assert!(action.is_none(), "no action for when without subject");
}

#[test]
fn non_enum_non_sealed_no_action() {
    let type_src = "class Foo(val x: Int)";
    let src = "\
fun handle(f: Foo) {
    when (f) {
    }
}
";
    let idx = setup(&[("/Foo.kt", type_src), ("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(1, 6));
    assert!(action.is_none(), "no action for regular class");
}

#[test]
fn local_val_with_space_before_colon() {
    let src = "\
fun test() {
    val tip : Color = Color.RED
    when(tip){
    }
}
";
    let idx = setup(&[("/Color.kt", ENUM_SRC), ("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(2, 6));
    assert!(
        action.is_some(),
        "should resolve type from CST even with space before colon"
    );
    match action.unwrap() {
        CodeActionOrCommand::CodeAction(ca) => {
            let edit = ca.edit.unwrap();
            let changes = edit.changes.unwrap();
            let edits = changes.get(&u).unwrap();
            let text = &edits[0].new_text;
            assert!(text.contains("Color.GREEN"), "missing GREEN: {text}");
            assert!(text.contains("Color.BLUE"), "missing BLUE: {text}");
            // RED is already assigned but not in the when — all 3 should be generated
            assert!(text.contains("Color.RED"), "missing RED: {text}");
        }
        _ => panic!("expected CodeAction"),
    }
}

#[test]
fn function_parameter_type_resolution() {
    let src = "\
fun handle(c: Color) {
    when (c) {
    }
}
";
    let idx = setup(&[("/Color.kt", ENUM_SRC), ("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(1, 6));
    assert!(
        action.is_some(),
        "should resolve type from function parameter"
    );
}

#[test]
fn empty_when_block_formatting() {
    let src = "\
fun test() {
    val c: Color = Color.RED
    when(c){
      
    }
}
";
    let idx = setup(&[("/Color.kt", ENUM_SRC), ("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(3, 0));
    assert!(action.is_some(), "expected action for empty when block");
    match action.unwrap() {
        CodeActionOrCommand::CodeAction(ca) => {
            let edit = ca.edit.unwrap();
            let changes = edit.changes.unwrap();
            let edits = changes.get(&u).unwrap();
            let text = &edits[0].new_text;
            // Verify proper formatting: branches indented, closing brace on own line
            assert!(
                text.ends_with('}'),
                "should end with closing brace: {text:?}"
            );
            assert!(
                text.contains("Color.RED -> TODO()\n"),
                "branches should have newline: {text:?}"
            );
            // Closing brace should be at same indent as when keyword (4 spaces)
            assert!(
                text.ends_with("    }"),
                "closing brace should be indented to match when: {text:?}"
            );
        }
        _ => panic!("expected CodeAction"),
    }
}

#[test]
fn when_block_with_newline_between_braces() {
    let src = "\
fun test() {
    val c: Color = Color.RED
    when(c) {

    }
}
";
    let idx = setup(&[("/Color.kt", ENUM_SRC), ("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(3, 0));
    assert!(action.is_some(), "expected action");
    match action.unwrap() {
        CodeActionOrCommand::CodeAction(ca) => {
            let edit = ca.edit.unwrap();
            let changes = edit.changes.unwrap();
            let edits = changes.get(&u).unwrap();
            let range = &edits[0].range;
            let text = &edits[0].new_text;
            // Should replace from line 3 (after `{` on line 2) through line 4 (`}`)
            assert_eq!(range.start.line, 3, "replace start line");
            assert_eq!(range.end.line, 4, "replace end line");
            // No leading blank line in output
            assert!(
                !text.starts_with('\n'),
                "should not start with blank line: {text:?}"
            );
            assert!(text.ends_with('}'), "should end with brace: {text:?}");
        }
        _ => panic!("expected CodeAction"),
    }
}

// ─── Boolean tests ────────────────────────────────────────────────────────────

#[test]
fn boolean_inferred_from_literal() {
    let src = "\
fun test() {
    val bool = false
    when(bool) {

    }
}
";
    let idx = setup(&[("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(3, 0));
    assert!(action.is_some(), "expected action for inferred Boolean");
    match action.unwrap() {
        CodeActionOrCommand::CodeAction(ca) => {
            let edit = ca.edit.unwrap();
            let changes = edit.changes.unwrap();
            let edits = changes.get(&u).unwrap();
            let text = &edits[0].new_text;
            assert!(
                text.contains("true -> TODO()"),
                "should have true: {text:?}"
            );
            assert!(
                text.contains("false -> TODO()"),
                "should have false: {text:?}"
            );
        }
        _ => panic!("expected CodeAction"),
    }
}

#[test]
fn boolean_fill_all_branches() {
    let src = "\
fun test(flag: Boolean) {
    when(flag) {

    }
}
";
    let idx = setup(&[("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(2, 0));
    assert!(action.is_some(), "expected action for Boolean when");
    match action.unwrap() {
        CodeActionOrCommand::CodeAction(ca) => {
            let edit = ca.edit.unwrap();
            let changes = edit.changes.unwrap();
            let edits = changes.get(&u).unwrap();
            let text = &edits[0].new_text;
            assert!(
                text.contains("true -> TODO()"),
                "should have true: {text:?}"
            );
            assert!(
                text.contains("false -> TODO()"),
                "should have false: {text:?}"
            );
            assert!(
                !text.contains("Boolean."),
                "should be bare true/false: {text:?}"
            );
        }
        _ => panic!("expected CodeAction"),
    }
}

#[test]
fn boolean_partial_branch() {
    let src = "\
fun test(flag: Boolean) {
    when(flag) {
        true -> println(\"yes\")
    }
}
";
    let idx = setup(&[("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(2, 0));
    assert!(action.is_some(), "expected action");
    match action.unwrap() {
        CodeActionOrCommand::CodeAction(ca) => {
            let edit = ca.edit.unwrap();
            let changes = edit.changes.unwrap();
            let edits = changes.get(&u).unwrap();
            let text = &edits[0].new_text;
            assert!(
                text.contains("false -> TODO()"),
                "should have false: {text:?}"
            );
            assert!(
                !text.contains("true -> TODO()"),
                "should not have true (already present): {text:?}"
            );
        }
        _ => panic!("expected CodeAction"),
    }
}

#[test]
fn boolean_complete_no_action() {
    let src = "\
fun test(flag: Boolean) {
    when(flag) {
        true -> println(\"yes\")
        false -> println(\"no\")
    }
}
";
    let idx = setup(&[("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(2, 0));
    assert!(
        action.is_none(),
        "should have no action when all Boolean branches present"
    );
}

#[test]
fn boolean_nullable_type_resolution() {
    let src = "\
fun test(flag: Boolean?) {
    when(flag) {
        true -> println(\"yes\")
    }
}
";
    let idx = setup(&[("/main.kt", src)]);
    let u = uri("/main.kt");
    let action = build_fill_when_action(&idx, &u, cursor_at(2, 0));
    assert!(
        action.is_some(),
        "expected action for nullable Boolean when"
    );
    match action.unwrap() {
        CodeActionOrCommand::CodeAction(ca) => {
            let edit = ca.edit.unwrap();
            let changes = edit.changes.unwrap();
            let edits = changes.get(&u).unwrap();
            let text = &edits[0].new_text;
            assert!(
                text.contains("false -> TODO()"),
                "should have false: {text:?}"
            );
        }
        _ => panic!("expected CodeAction"),
    }
}

// ─── Diagnostics tests ───────────────────────────────────────────────────────

use crate::features::fill_when::when_diagnostics;

#[test]
fn diagnostics_reports_missing_enum_branches() {
    let src = "\
fun test(c: Color) {
    when (c) {
        Color.RED -> println(\"red\")
    }
}
";
    let idx = setup(&[("/Color.kt", ENUM_SRC), ("/main.kt", src)]);
    let u = uri("/main.kt");
    let diags = when_diagnostics(&idx, &u);
    assert_eq!(diags.len(), 1, "should have 1 diagnostic");
    assert!(
        diags[0].message.contains("GREEN"),
        "message: {}",
        diags[0].message
    );
    assert!(
        diags[0].message.contains("BLUE"),
        "message: {}",
        diags[0].message
    );
    assert_eq!(diags[0].severity, Some(DiagnosticSeverity::WARNING));
    assert_eq!(diags[0].source.as_deref(), Some("kotlin-lsp"));
}

#[test]
fn diagnostics_no_report_when_complete() {
    let src = "\
fun test(c: Color) {
    when (c) {
        Color.RED -> println(\"red\")
        Color.GREEN -> println(\"green\")
        Color.BLUE -> println(\"blue\")
    }
}
";
    let idx = setup(&[("/Color.kt", ENUM_SRC), ("/main.kt", src)]);
    let u = uri("/main.kt");
    let diags = when_diagnostics(&idx, &u);
    assert!(diags.is_empty(), "no diagnostic when all branches covered");
}

#[test]
fn diagnostics_no_report_with_else() {
    let src = "\
fun test(c: Color) {
    when (c) {
        Color.RED -> println(\"red\")
        else -> println(\"other\")
    }
}
";
    let idx = setup(&[("/Color.kt", ENUM_SRC), ("/main.kt", src)]);
    let u = uri("/main.kt");
    let diags = when_diagnostics(&idx, &u);
    assert!(diags.is_empty(), "no diagnostic when else present");
}

#[test]
fn diagnostics_boolean_missing() {
    let src = "\
fun test(flag: Boolean) {
    when(flag) {
        true -> println(\"yes\")
    }
}
";
    let idx = setup(&[("/main.kt", src)]);
    let u = uri("/main.kt");
    let diags = when_diagnostics(&idx, &u);
    assert_eq!(diags.len(), 1);
    assert!(
        diags[0].message.contains("false"),
        "message: {}",
        diags[0].message
    );
}
