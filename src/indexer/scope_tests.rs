//! Tests for `indexer::scope` — cursor/scope resolution helpers.

use tower_lsp::lsp_types::*;
use crate::indexer::Indexer;
use crate::indexer::{
    find_it_element_type,
    find_named_lambda_param_type,
    is_lambda_param,
    line_has_lambda_param,
    lambda_brace_pos_for_param,
};
use super::extract_class_decl_name;

fn uri(path: &str) -> Url {
    Url::parse(&format!("file:///test{path}")).unwrap()
}

fn indexed(path: &str, src: &str) -> (Url, Indexer) {
    let u = uri(path);
    let idx = Indexer::new();
    idx.index_content(&u, src);
    (u, idx)
}

// ── word_at ──────────────────────────────────────────────────────────────

#[test]
fn word_at_middle() {
    let (u, idx) = indexed("/t.kt", "val foo = 1");
    assert_eq!(idx.word_at(&u, Position::new(0, 5)), Some("foo".into()));
}

#[test]
fn word_at_end_of_word() {
    let (u, idx) = indexed("/t.kt", "val foo = 1");
    // character=7 is just past the 'o'; should still return "foo"
    assert_eq!(idx.word_at(&u, Position::new(0, 7)), Some("foo".into()));
}

#[test]
fn word_at_operator_returns_none() {
    let (u, idx) = indexed("/t.kt", "val foo = 1");
    assert_eq!(idx.word_at(&u, Position::new(0, 8)), None); // '='
}

#[test]
fn word_at_angle_bracket_steps_back_to_word() {
    let (u, idx) = indexed("/t.kt", "List<String>");
    // '<' at position 4 is not an id char, but 't' at position 3 is.
    // word_at steps back and returns the word ending there.
    assert_eq!(idx.word_at(&u, Position::new(0, 4)), Some("List".into()));
}

#[test]
fn word_at_space_between_operator_and_number_returns_none() {
    // "val foo = 1"
    //  0123456789A
    // pos 9 = ' ' between '=' and '1'; prev char[8]='=' is also non-ident → None
    let (u, idx) = indexed("/t.kt", "val foo = 1");
    assert_eq!(idx.word_at(&u, Position::new(0, 9)), None);
}

// ── word_and_qualifier_at ────────────────────────────────────────────────

#[test]
fn qualifier_none_plain_word() {
    let (u, idx) = indexed("/t.kt", "val x: Bar = y");
    // cursor on 'B' of 'Bar'
    assert_eq!(
        idx.word_and_qualifier_at(&u, Position::new(0, 7)),
        Some(("Bar".into(), None))
    );
}

#[test]
fn qualifier_dot_access() {
    let (u, idx) = indexed("/t.kt", "val x = Outer.Inner");
    // cursor on 'I' of 'Inner'
    assert_eq!(
        idx.word_and_qualifier_at(&u, Position::new(0, 14)),
        Some(("Inner".into(), Some("Outer".into())))
    );
}

#[test]
fn qualifier_in_type_param() {
    let (u, idx) = indexed("/t.kt", "val x: List<Outer.Content>");
    // cursor on 'C' of 'Content' (position 18) — full chain captured
    assert_eq!(
        idx.word_and_qualifier_at(&u, Position::new(0, 18)),
        Some(("Content".into(), Some("Outer".into())))
    );
}

#[test]
fn qualifier_full_chain() {
    // A.B.C with cursor on C → full chain "A.B" not just "B"
    let (u, idx) = indexed("/t.kt", "A.B.C");
    assert_eq!(
        idx.word_and_qualifier_at(&u, Position::new(0, 4)),
        Some(("C".into(), Some("A.B".into())))
    );
}

#[test]
fn qualifier_deep_chain() {
    // A.B.C.D.E cursor on E → qualifier is the full "A.B.C.D"
    let (u, idx) = indexed("/t.kt", "A.B.C.D.E");
    assert_eq!(
        idx.word_and_qualifier_at(&u, Position::new(0, 8)),
        Some(("E".into(), Some("A.B.C.D".into())))
    );
}

// ── named argument detection ──────────────────────────────────────────────

#[test]
fn named_arg_simple_constructor() {
    // `User(name = "Alice")` cursor on `name` → qualifier should be "User"
    let (u, idx) = indexed("/t.kt", "User(name = \"Alice\")");
    assert_eq!(
        idx.word_and_qualifier_at(&u, Position::new(0, 5)),
        Some(("name".into(), Some("User".into())))
    );
}

#[test]
fn named_arg_not_equality() {
    // `if (x == foo)` — `==` is NOT a named arg
    let (u, idx) = indexed("/t.kt", "val r = x == foo");
    assert_eq!(
        idx.word_and_qualifier_at(&u, Position::new(0, 9)),
        Some(("x".into(), None))  // plain word, no qualifier
    );
}

#[test]
fn named_arg_assignment_not_arg() {
    // `val x = y` — `=` after a `val` binding is NOT a named arg (not inside a call)
    let (u, idx) = indexed("/t.kt", "val x = someValue");
    assert_eq!(
        idx.word_and_qualifier_at(&u, Position::new(0, 4)),
        Some(("x".into(), None))  // no enclosing `(` → no qualifier
    );
}

#[test]
fn named_arg_multiline_ctor() {
    // Constructor split across lines:
    //   User(
    //       name = "Alice",  ← cursor on name (col 4)
    //   )
    let src = "User(\n    name = \"Alice\",\n)";
    let (u, idx) = indexed("/t.kt", src);
    assert_eq!(
        idx.word_and_qualifier_at(&u, Position::new(1, 4)),
        Some(("name".into(), Some("User".into())))
    );
}

#[test]
fn named_arg_method_with_uppercase_receiver() {
    // BottomSheetState.empty(onBottomSheetClose = handler)
    // → qualifier should be "BottomSheetState" (the receiver type)
    let src = "BottomSheetState.empty(onBottomSheetClose = handler)";
    let (u, idx) = indexed("/t.kt", src);
    assert_eq!(
        idx.word_and_qualifier_at(&u, Position::new(0, 23)),
        Some(("onBottomSheetClose".into(), Some("BottomSheetState".into())))
    );
}

#[test]
fn named_arg_fully_qualified_ctor() {
    // com.example.User(name = "Alice") → qualifier "User" (last uppercase segment)
    let src = "com.example.User(name = \"Alice\")";
    let (u, idx) = indexed("/t.kt", src);
    assert_eq!(
        idx.word_and_qualifier_at(&u, Position::new(0, 17)),
        Some(("name".into(), Some("User".into())))
    );
}

#[test]
fn named_arg_lowercase_method_no_receiver() {
    // someFunction(param = value) — pure lowercase, no type info → None qualifier
    let src = "someFunction(param = value)";
    let (u, idx) = indexed("/t.kt", src);
    // qualifier should be None (we can't resolve this without type inference)
    let result = idx.word_and_qualifier_at(&u, Position::new(0, 13));
    assert_eq!(result.as_ref().map(|(w, _)| w.as_str()), Some("param"));
    assert_eq!(result.as_ref().and_then(|(_, q)| q.as_deref()), None);
}

#[test]
fn named_arg_state_multiline_with_method_receiver() {
    // Simulates the real-world pattern:
    //   State(
    //     sheetState = BottomSheetState.empty(SheetType.Empty, onBottomSheetClose = cb),
    //     ...                                                   ^^^^^^^^^^^^^^^^^^
    let src = "State(\n  sheetState = BottomSheetState.empty(SheetType.Empty, onBottomSheetClose = cb),\n)";
    let (u, idx) = indexed("/t.kt", src);
    // cursor on onBottomSheetClose (inside the inner .empty() call on line 1)
    let line1 = &src.lines().collect::<Vec<_>>()[1];
    let col = line1.find("onBottomSheetClose").unwrap() as u32;
    let result = idx.word_and_qualifier_at(&u, Position::new(1, col));
    assert_eq!(result.as_ref().map(|(w, _)| w.as_str()), Some("onBottomSheetClose"));
    assert_eq!(result.as_ref().and_then(|(_, q)| q.as_deref()), Some("BottomSheetState"));
}

// ── it-completion ─────────────────────────────────────────────────────────

#[test]
fn it_element_type_list() {
    // val items: List<Product>
    // items.forEach { it.  ← element type should be "Product"
    let src = "val items: List<Product> = emptyList()";
    let (u, idx) = indexed("/t.kt", src);
    let before = "items.forEach { it.";
    let result = find_it_element_type(before, &idx, &u);
    assert_eq!(result.as_deref(), Some("Product"));
}

#[test]
fn it_element_type_flow() {
    let src = "val events: Flow<Event> = emptyFlow()";
    let (u, idx) = indexed("/t.kt", src);
    let before = "events.collect { it.";
    assert_eq!(find_it_element_type(before, &idx, &u).as_deref(), Some("Event"));
}

#[test]
fn it_element_type_state_flow() {
    let src = "    private val _state: StateFlow<UiState>";
    let (u, idx) = indexed("/t.kt", src);
    let before = "_state.value.let { it."; // `value` is lowercase → chain, falls back
    // _state itself is StateFlow, but we ask about `value` which isn't typed here.
    // Just ensure no panic.
    let _ = find_it_element_type(before, &idx, &u);
}

#[test]
fn it_scope_fn_let() {
    // val user: User — `user.let { it.` — it IS the User type
    let src = "val user: User = User()";
    let (u, idx) = indexed("/t.kt", src);
    let before = "user.let { it.";
    // User is not a collection, so returns the base type directly
    assert_eq!(find_it_element_type(before, &idx, &u).as_deref(), Some("User"));
}

#[test]
fn it_element_type_nullable_call() {
    // val user: User? — `user?.let { it.`
    let src = "val user: User? = null";
    let (u, idx) = indexed("/t.kt", src);
    let before = "user?.let { it.";
    // `?` in `?.` is normalised away — should still find "User"
    // `infer_type_in_lines_raw` for `user: User?` → "User" (? stripped at type boundary)
    let result = find_it_element_type(before, &idx, &u);
    assert_eq!(result.as_deref(), Some("User"));
}

#[test]
fn it_element_type_with_call_args() {
    // items.map(transform) { it.  → strip `(transform)` first
    let src = "val items: List<Order> = emptyList()";
    let (u, idx) = indexed("/t.kt", src);
    let before = "items.mapNotNull(::transform) { it.";
    // strip `(::transform)` → callee = `items.mapNotNull` → receiver = `items` → List<Order>
    assert_eq!(find_it_element_type(before, &idx, &u).as_deref(), Some("Order"));
}

#[test]
fn it_unknown_var_returns_none() {
    let (u, idx) = indexed("/t.kt", "");
    assert_eq!(find_it_element_type("unknown.forEach { it.", &idx, &u), None);
}

// ── named lambda parameter type inference ─────────────────────────────────

#[test]
fn named_lambda_param_same_line() {
    // items.forEach { item -> item.  ← same line
    let src = "val items: List<Product> = emptyList()";
    let (u, idx) = indexed("/t.kt", src);
    let before = "items.forEach { item -> item.";
    let result = find_named_lambda_param_type(before, "item", &idx, &u, 0);
    assert_eq!(result.as_deref(), Some("Product"));
}

#[test]
fn named_lambda_param_multiline() {
    // items.forEach { item ->
    //     item.  ← cursor here
    let src = "val items: List<Order> = emptyList()\nitems.forEach { order ->\n    order.x\n}";
    let (u, idx) = indexed("/t.kt", src);
    // cursor on line 2 ("    order.x"), scanning back to line 1 for `{ order ->`
    let result = find_named_lambda_param_type("    order.", "order", &idx, &u, 2);
    assert_eq!(result.as_deref(), Some("Order"));
}

#[test]
fn named_lambda_param_scope_fn() {
    // val user: User — `user.also { u -> u.` — `u` is User itself
    let src = "val user: User = User()";
    let (u, idx) = indexed("/t.kt", src);
    let before = "user.also { u -> u.";
    let result = find_named_lambda_param_type(before, "u", &idx, &u, 0);
    assert_eq!(result.as_deref(), Some("User"));
}

#[test]
fn is_lambda_param_detects_same_line() {
    let src = "";
    let (u, idx) = indexed("/t.kt", src);
    assert!(is_lambda_param("item", "items.forEach { item -> item.", &idx, &u, 0));
    assert!(!is_lambda_param("item", "val item = something()", &idx, &u, 0));
}

// ── enclosing_class_at ───────────────────────────────────────────────────

#[test]
fn enclosing_class_simple() {
    let src = "\
sealed interface NewsFeedUiState {
    data object Loading : NewsFeedUiState
    data class Success(val items: List<String>) : NewsFeedUiState
}";
    let (u, idx) = indexed("/NewsFeed.kt", src);
    // Line 1 = "    data object Loading ..."  → enclosing = NewsFeedUiState
    assert_eq!(
        idx.enclosing_class_at(&u, 1),
        Some("NewsFeedUiState".into()),
    );
}

#[test]
fn enclosing_class_top_level_returns_none() {
    let src = "sealed interface NewsFeedUiState {\n    data object Loading : NewsFeedUiState\n}";
    let (u, idx) = indexed("/NewsFeed.kt", src);
    // Line 0 = the sealed interface itself — no enclosure
    assert_eq!(idx.enclosing_class_at(&u, 0), None);
}

#[test]
fn enclosing_class_nested_two_levels() {
    let src = "\
class Outer {
    sealed class Inner {
        data object Loading : Inner
    }
}";
    let (u, idx) = indexed("/Outer.kt", src);
    // Line 2 = "        data object Loading..." → enclosing = Inner (closer one)
    assert_eq!(idx.enclosing_class_at(&u, 2), Some("Inner".into()));
}

#[test]
fn enclosing_class_multiline_constructor() {
    // Simulates DashboardProductsViewModel: `class` keyword on line 0,
    // closing `)` + `{` on line 3. Cursor at line 5 (inside the body).
    let src = "\
class Foo @Inject constructor(
  private val a: A,
  private val b: B,
) : Bar() {
  override fun doIt() {
    super.doIt()
  }
}";
    let (u, idx) = indexed("/Foo.kt", src);
    // Line 5 = "    super.doIt()" → enclosing class = Foo
    assert_eq!(idx.enclosing_class_at(&u, 5), Some("Foo".into()));
}

#[test]
fn extract_class_decl_name_variants() {
    assert_eq!(extract_class_decl_name("sealed interface Foo {"), Some("Foo".into()));
    assert_eq!(extract_class_decl_name("data class Bar(val x: Int)"), Some("Bar".into()));
    assert_eq!(extract_class_decl_name("object Baz"), Some("Baz".into()));
    assert_eq!(extract_class_decl_name("fun doSomething() {}"), None);
    assert_eq!(extract_class_decl_name("val x: Int = 0"), None);
    assert_eq!(extract_class_decl_name("// class NotReal"), None);
}

// ── multi-param lambda detection ─────────────────────────────────────────

#[test]
fn multi_param_lambda_params_at() {
    let src = "items.zip(other) { a, b ->\n    a.id\n}";
    let (u, idx) = indexed("/t.kt", src);
    // Simulate live_lines for lambda_params_at
    idx.set_live_lines(&u, src);
    let params = idx.lambda_params_at(&u, 1);
    assert!(params.contains(&"a".to_string()), "expected a, got: {params:?}");
    assert!(params.contains(&"b".to_string()), "expected b, got: {params:?}");
}

#[test]
fn lambda_params_at_excludes_sibling_lambda() {
    // `isRefresh` is in a CLOSED sibling lambda; cursor is in `resultState` body.
    let src = concat!(
        "reload({ isRefresh -> doSomething(isRefresh) }) { resultState ->\n",
        "    resultState.value\n",
        "}",
    );
    let (u, idx) = indexed("/t.kt", src);
    idx.set_live_lines(&u, src);
    let params = idx.lambda_params_at(&u, 1);
    assert!(params.contains(&"resultState".to_string()),
        "resultState should be in scope, got: {params:?}");
    assert!(!params.contains(&"isRefresh".to_string()),
        "isRefresh is a closed sibling — must NOT appear, got: {params:?}");
}

#[test]
fn lambda_params_at_col_inline_lambda() {
    // Cursor is inside the body of an inline lambda on the SAME line.
    // `lambda_params_at` (without col) would see the closing `}` first and
    // NOT collect the params.  `lambda_params_at_col` limits the scan to
    // the cursor column, so it correctly identifies `loanId` and `isWustenrot`.
    let src = "    loan = { loanId, isWustenrot -> setEvent(OnSecondaryActionClick(loanId, isWustenrot)) },";
    let (u, idx) = indexed("/t.kt", src);
    idx.set_live_lines(&u, src);
    // Cursor on the second `loanId` (inside the setEvent call).
    // Column ~= position of second "loanId".
    let col = src.rfind("loanId").unwrap();  // byte offset ≈ UTF-16 col for ASCII
    let params = idx.lambda_params_at_col(&u, 0, col);
    assert!(params.contains(&"loanId".to_string()),
        "loanId should be in scope (col-aware), got: {params:?}");
    assert!(params.contains(&"isWustenrot".to_string()),
        "isWustenrot should be in scope (col-aware), got: {params:?}");
}

#[test]
fn find_named_param_on_line_with_multiple_arrows() {
    // `resultState` is the SECOND lambda on the same line — the first `->` belongs
    // to `{ isRefresh -> ... }`.  `line_has_lambda_param` must scan all arrows.
    let line = "reloadableProduct(ProductKey.FAMILY, { isRefresh -> getFamilyAccount(isRefresh) }) { resultState ->";
    assert!(line_has_lambda_param(line, "resultState"),
        "must find resultState even when isRefresh arrow comes first");
    assert!(line_has_lambda_param(line, "isRefresh"),
        "must still find isRefresh");
    assert!(!line_has_lambda_param(line, "other"),
        "must NOT find unknown name");

    let brace = lambda_brace_pos_for_param(line, "resultState");
    assert!(brace.is_some(), "must find brace for resultState");
    // The brace for resultState is the LAST `{` on the line.
    let last_brace = line.rfind('{').unwrap();
    assert_eq!(brace.unwrap(), last_brace,
        "brace pos should be the last {{ on the line");
}

#[test]
fn multi_param_lambda_is_detected() {
    let src = "items.zip(other) { a, b ->\n    a.id\n}";
    let (u, idx) = indexed("/t.kt", src);
    idx.set_live_lines(&u, src);
    // Both `a` and `b` should be recognised as lambda params
    assert!(is_lambda_param("a", "items.zip(other) { a, b ->", &idx, &u, 0));
    assert!(is_lambda_param("b", "items.zip(other) { a, b ->", &idx, &u, 0));
}
