use super::{any_local_var_decl_in_scope, cst_cursor_is_local_var, cst_cursor_is_method};
use crate::indexer::Indexer;
use tower_lsp::lsp_types::{Position, Url};

fn make_indexer_with(src: &str) -> (Indexer, Url) {
    let idx = Indexer::new();
    let uri = Url::parse("file:///tmp/test.kt").unwrap();
    idx.store_live_tree(&uri, src);
    (idx, uri)
}

fn make_indexed(src: &str) -> (Indexer, Url) {
    let idx = Indexer::new();
    let uri = Url::parse("file:///tmp/test.kt").unwrap();
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);
    (idx, uri)
}

#[test]
fn cst_var_declaration_is_not_method() {
    let src = "val syncWith = repo.syncWith(Arg())\n";
    let (idx, uri) = make_indexer_with(src);
    // Cursor on `syncWith` at col 4 (the variable declaration)
    let pos = Position {
        line: 0,
        character: 4,
    };
    assert!(
        !cst_cursor_is_method(&idx, &uri, pos),
        "val decl should not be method"
    );
}

#[test]
fn cst_nav_expr_member_access_is_method() {
    let src = "val syncWith = repo.syncWith(Arg())\n";
    let (idx, uri) = make_indexer_with(src);
    // Cursor on `syncWith` in `.syncWith(` — after 'repo.' → col 20
    let pos = Position {
        line: 0,
        character: 20,
    };
    assert!(
        cst_cursor_is_method(&idx, &uri, pos),
        "navigation_expression should be method"
    );
}

#[test]
fn cst_function_declaration_is_method() {
    let src = "fun syncWith(arg: Arg): Result {\n    return Result()\n}\n";
    let (idx, uri) = make_indexer_with(src);
    // Cursor on `syncWith` at col 4
    let pos = Position {
        line: 0,
        character: 4,
    };
    assert!(
        cst_cursor_is_method(&idx, &uri, pos),
        "fun declaration should be method"
    );
}

#[test]
fn cst_var_reference_not_method() {
    let src = "fun go() {\n    val x = 1\n    x.toString()\n}\n";
    let (idx, uri) = make_indexer_with(src);
    // Cursor on `x` at line 1, col 8 (in `val x = 1`)
    let pos = Position {
        line: 1,
        character: 8,
    };
    assert!(
        !cst_cursor_is_method(&idx, &uri, pos),
        "var decl should not be method"
    );
}

/// Local variable inside a function body → is local var (skip_dotted = true).
#[test]
fn local_var_inside_function_is_local() {
    let src = "fun go() {\n    val localFoo = 1\n}\n";
    let (idx, uri) = make_indexed(src);
    // Cursor on `localFoo` at line 1, col 8
    let pos = Position {
        line: 1,
        character: 8,
    };
    assert!(
        cst_cursor_is_local_var(&idx, &uri, pos),
        "val inside fun should be local var"
    );
}

/// Class property → not a local var, dotted accesses should be renamed.
#[test]
fn class_property_is_not_local_var() {
    let src = "class Foo {\n    val myProp: String = \"\"\n}\n";
    let (idx, uri) = make_indexed(src);
    // Cursor on `myProp` at line 1, col 8
    let pos = Position {
        line: 1,
        character: 8,
    };
    assert!(
        !cst_cursor_is_local_var(&idx, &uri, pos),
        "class property should not be local var"
    );
}

/// Variable declared inside a lambda literal → is local var (skip_dotted = true).
#[test]
fn local_var_inside_lambda_is_local() {
    let src = "val f = { val x = 1\n    x\n}\n";
    let (idx, uri) = make_indexed(src);
    // Cursor on `x` at line 0, col 14 (the `val x` declaration inside the lambda)
    let pos = Position {
        line: 0,
        character: 14,
    };
    assert!(
        cst_cursor_is_local_var(&idx, &uri, pos),
        "val inside lambda should be local var"
    );
}

/// Cursor on a *reference* to a local variable — any_local_var_decl_in_scope
/// should find the declaration in the index and confirm it is local.
#[test]
fn reference_to_local_var_is_detected_via_index() {
    // line 0: fun go() {
    // line 1:     val syncWith = 1
    // line 2:     syncWith   ← cursor here (reference, not declaration)
    // line 3: }
    let src = "fun go() {\n    val syncWith = 1\n    syncWith\n}\n";
    let (idx, uri) = make_indexed(src);
    // Scope covers the whole function body.
    let scope = (0, 3);
    assert!(
        any_local_var_decl_in_scope(&idx, &uri, "syncWith", scope),
        "reference to a local var inside a function should be detected as local"
    );
}

/// Class-level property — any_local_var_decl_in_scope must return false even when
/// the scope covers the entire class, because the declaration's CST ancestor is
/// class_body (not a function body).
#[test]
fn class_property_not_detected_as_local_via_index() {
    let src = "class Foo {\n    val myProp: String = \"\"\n}\n";
    let (idx, uri) = make_indexed(src);
    // Even searching the full file range, myProp is a class-level property.
    let scope = (0, 2);
    assert!(
        !any_local_var_decl_in_scope(&idx, &uri, "myProp", scope),
        "class property should not be detected as local var"
    );
}
