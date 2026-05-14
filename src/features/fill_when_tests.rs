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
