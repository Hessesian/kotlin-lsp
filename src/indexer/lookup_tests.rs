//! Tests for `indexer::lookup` — the "read path" of the index.
//!
//! `super` = `crate::indexer::lookup`
//! `crate::indexer` = the parent `indexer` module.

use tower_lsp::lsp_types::*;

use crate::indexer::Indexer;

// ── Test helpers ──────────────────────────────────────────────────────────────

fn uri(path: &str) -> Url {
    Url::parse(&format!("file:///test{path}")).unwrap()
}

fn indexed(path: &str, src: &str) -> (Url, Indexer) {
    let u = uri(path);
    let idx = Indexer::new();
    idx.index_content(&u, src);
    (u, idx)
}

// ── Swift hover uses "func" not "fun" ────────────────────────────────────────

#[test]
fn swift_hover_uses_func_keyword() {
    // Swift function with no signature detail should show "func", not "fun".
    let src = "func greet() {}";
    let (u, idx) = indexed("/Greeting.swift", src);
    let hover = idx.hover_info_at_location(
        &Location { uri: u.clone(), range: Default::default() },
        "greet",
        None,
        None,
    ).unwrap_or_default();
    assert!(
        hover.contains("func"),
        "Swift hover should say 'func', got: {hover}"
    );
    assert!(
        !hover.contains("```kotlin\nfun ") && !hover.contains("```swift\nfun "),
        "Swift hover must not emit 'fun', got: {hover}"
    );
}

#[test]
fn kotlin_hover_still_uses_fun_keyword() {
    let src = "fun greet() {}";
    let (u, idx) = indexed("/Greeting.kt", src);
    let hover = idx.hover_info_at_location(
        &Location { uri: u.clone(), range: Default::default() },
        "greet",
        None,
        None,
    ).unwrap_or_default();
    assert!(
        hover.contains("fun"),
        "Kotlin hover should say 'fun', got: {hover}"
    );
}

#[test]
fn hover_includes_kdoc() {
    let src = r#"package com.example

/**
 * Represents a user account.
 */
class Account(val name: String)"#;
    let (u, idx) = indexed("/Account.kt", src);
    let hover = idx.hover_info("Account", None).unwrap();
    assert!(hover.contains("Represents a user account"), "got: {hover}");
    assert!(hover.contains("```kotlin"), "got: {hover}");
    assert!(hover.contains("---"), "separator missing: {hover}");
}

// ── hover on val bindings ────────────────────────────────────────────────────

#[test]
fn hover_val_binding_constructor_param() {
    // Constructor parameter: `private val repo: IGoldConversionRepository`
    let idx = Indexer::new();
    let u = uri("/Foo.kt");
    idx.index_content(&u, "\
class Foo(
    private val repo: IGoldConversionRepository
) {
    fun doStuff() {}
}");
    // 1. repo should be captured as a symbol
    let data = idx.files.get(u.as_str()).unwrap();
    let repo_sym = data.symbols.iter().find(|s| s.name == "repo");
    assert!(repo_sym.is_some(), "repo should be in symbols; got: {:?}",
        data.symbols.iter().map(|s| &s.name).collect::<Vec<_>>());

    // 2. find_definition_qualified should find it
    let locs = idx.find_definition_qualified("repo", None, &u);
    assert!(!locs.is_empty(), "repo should be found via find_definition_qualified");

    // 3. hover_info_at_location should return something
    let hover = idx.hover_info_at_location(locs.first().unwrap(), "repo", None, None);
    assert!(hover.is_some(), "hover on val repo should produce result");
    let md = hover.unwrap();
    assert!(md.contains("repo"), "hover should mention 'repo', got: {md}");
}

// ── real-world patterns ──────────────────────────────────────────────────────

#[test]
fn real_hover_constructor_val_binding() {
    // From report: hover on `repo` in constructor param returns nothing
    let idx = Indexer::new();
    let u = uri("/ContactAddressInteractor.kt");
    idx.index_content(&u, "\
package cz.moneta.smartbanka.feature.gold_conversion.model.goldcard
internal class ContactAddressInteractor @Inject constructor(
  private val repo: IGoldConversionRepository,
) : ISimpleLoadDataInteractor<PersonalAddress> {
  override suspend fun loadData(): PersonalAddress =
    requireNotNull(repo.contactAddressSetup().contactAddress)
}");
    // hover on `repo` (line 2, col ~14)
    let locs = idx.find_definition_qualified("repo", None, &u);
    assert!(!locs.is_empty(), "repo should be found");
    let hover = idx.hover_info_at_location(locs.first().unwrap(), "repo", None, None);
    assert!(hover.is_some(), "hover on val repo should work");
    let md = hover.unwrap();
    assert!(md.contains("repo"), "hover should mention repo: {md}");
    assert!(md.contains("IGoldConversionRepository"), "hover should show type: {md}");
}

// ── completion_docs_for ──────────────────────────────────────────────────────

#[test]
fn completion_docs_for_returns_kdoc_and_detail() {
    let idx = Indexer::new();
    let u = uri("/Foo.kt");
    idx.index_content(&u, "\
package com.example

/**
 * Adds two numbers together.
 * @param a first number
 */
fun add(a: Int, b: Int): Int = a + b
");
    // `add` is declared at line 6 (0-based)
    let sym = {
        let f = idx.files.get(u.as_str()).unwrap();
        f.symbols.iter().find(|s| s.name == "add").cloned().unwrap()
    };
    let line = sym.selection_range.start.line;
    let col  = sym.selection_range.start.character;

    let result = idx.completion_docs_for(u.as_str(), line, col, None);
    assert!(result.is_some(), "completion_docs_for should return Some for documented function");
    let (doc_md, detail) = result.unwrap();
    assert!(doc_md.contains("Adds two numbers"), "doc should contain KDoc text, got: {doc_md}");
    assert!(!doc_md.contains("```"), "doc should NOT include a code block (detail carries the sig), got: {doc_md}");
    assert!(!detail.is_empty(), "detail should be non-empty");
}

#[test]
fn completion_docs_for_returns_none_without_kdoc() {
    let idx = Indexer::new();
    let u = uri("/Bar.kt");
    idx.index_content(&u, "fun multiply(x: Int, y: Int): Int = x * y\n");
    let sym = {
        let f = idx.files.get(u.as_str()).unwrap();
        f.symbols.iter().find(|s| s.name == "multiply").cloned().unwrap()
    };
    let line = sym.selection_range.start.line;
    let col  = sym.selection_range.start.character;
    // No KDoc → None (caller skips setting documentation)
    assert!(idx.completion_docs_for(u.as_str(), line, col, None).is_none());
}

#[test]
fn completion_docs_for_kts_uses_kotlin_lang() {
    let idx = Indexer::new();
    let u = uri("/build.gradle.kts");
    idx.index_content(&u, "\
/** Configure something. */
fun configure() {}\n");
    let sym = {
        let f = idx.files.get(u.as_str()).unwrap();
        f.symbols.iter().find(|s| s.name == "configure").cloned().unwrap()
    };
    let line = sym.selection_range.start.line;
    let col  = sym.selection_range.start.character;
    let (doc_md, _detail) = idx.completion_docs_for(u.as_str(), line, col, None).unwrap();
    assert!(doc_md.contains("Configure something"));
}

// ── generic type parameter substitution ─────────────────────────────────────

#[test]
fn hover_generic_type_params_substituted_in_subclass() {
    // FlowReducer is a generic interface in one file.
    // DashboardProductsReducer specialises it in another.
    // Hovering `reduce` from the subclass file should show concrete types.
    let idx = Indexer::new();

    let base_u = uri("/FlowReducer.kt");
    idx.index_content(&base_u, "\
interface FlowReducer<EventType, out EffectType, StateType> {
    fun reduce(state: StateType, event: EventType): StateType
    fun effects(event: EventType): List<EffectType>
}
");

    let sub_u = uri("/DashboardProductsReducer.kt");
    idx.index_content(&sub_u, "\
class DashboardProductsReducer : FlowReducer<Event, Effect, State> {
    override fun reduce(state: State, event: Event): State = state
    override fun effects(event: Event): List<Effect> = emptyList()
}
");

    // `reduce` is declared in FlowReducer — hover from the subclass file
    let base_data = idx.files.get(base_u.as_str()).unwrap();
    let reduce_sym = base_data.symbols.iter().find(|s| s.name == "reduce").cloned()
        .expect("reduce should be indexed in FlowReducer.kt");
    let loc = tower_lsp::lsp_types::Location {
        uri:   base_u.clone(),
        range: reduce_sym.selection_range,
    };

    let hover = idx.hover_info_at_location(&loc, "reduce", Some(sub_u.as_str()), None)
        .expect("hover should return Some");

    // Type params should be substituted: StateType→State, EventType→Event
    assert!(hover.contains("State"), "hover should show 'State', got: {hover}");
    assert!(hover.contains("Event"), "hover should show 'Event', got: {hover}");
    assert!(!hover.contains("StateType"), "hover must NOT show 'StateType', got: {hover}");
    assert!(!hover.contains("EventType"), "hover must NOT show 'EventType', got: {hover}");
}

#[test]
fn hover_generic_no_subst_when_same_file() {
    // Hovering from the same file should NOT substitute (raw type params shown).
    let idx = Indexer::new();
    let u = uri("/FlowReducer.kt");
    idx.index_content(&u, "\
interface FlowReducer<EventType, out EffectType, StateType> {
    fun reduce(state: StateType, event: EventType): StateType
}
");
    let data = idx.files.get(u.as_str()).unwrap();
    let reduce_sym = data.symbols.iter().find(|s| s.name == "reduce").cloned().unwrap();
    let loc = tower_lsp::lsp_types::Location { uri: u.clone(), range: reduce_sym.selection_range };

    // calling_uri == sym_uri → no substitution
    let hover = idx.hover_info_at_location(&loc, "reduce", Some(u.as_str()), None).unwrap();
    assert!(hover.contains("StateType"), "same-file hover should keep raw type params, got: {hover}");
}

// ── Port-regression: property-type harvesting ─────────────────────────────────

/// When the enclosing class's supers contain generic type args, the inlay-hint
/// substitution path (`type_subst_for_enclosing_class`) must return the correct
/// mapping. This tests the "UncC" scenario: build_enclosing_class_subst must
/// perform phase 2 (member property harvesting) so that a property whose type
/// extends a generic base contributes its type-param mappings to the enclosing class.
#[test]
fn inlay_hints_generic_subst_via_property_type_harvesting() {
    let idx = Indexer::new();

    let base_u = uri("/FlowReducer.kt");
    idx.index_content(&base_u, "\
interface FlowReducer<E, S> {
    fun reduce(event: E, state: S): S
}
");

    let reducer_u = uri("/DashReducer.kt");
    idx.index_content(&reducer_u, "\
class DashReducer : FlowReducer<DashEvent, DashState> {
    override fun reduce(event: DashEvent, state: DashState): DashState = state
}
");

    let owner_u = uri("/DashViewModel.kt");
    idx.index_content(&owner_u, "\
class DashViewModel {
    val reducer: DashReducer = DashReducer()
    fun process(event: DashEvent) {}
}
");

    // Inlay hint substitution from inside DashViewModel (line 2 = inside the class):
    // Phase 2 of build_enclosing_class_subst should trace:
    //   property `reducer: DashReducer` → DashReducer supers → FlowReducer<E=DashEvent, S=DashState>
    let subst = idx.type_subst_for_enclosing_class(owner_u.as_str(), 2);
    assert!(subst.contains_key("E") || subst.contains_key("S"),
        "property harvesting should produce substitution for FlowReducer params, got: {subst:?}");
    if let Some(e_val) = subst.get("E") {
        assert_eq!(e_val, "DashEvent", "E should map to DashEvent, got: {e_val}");
    }
    if let Some(s_val) = subst.get("S") {
        assert_eq!(s_val, "DashState", "S should map to DashState, got: {s_val}");
    }
}

// ── Port-regression: two callers in same file ─────────────────────────────────

/// When two classes in the same file extend the same generic base with different
/// type args, hovering inside each class must use the correct substitution.
/// This is the "Unb5/TRjS" regression — fixed by adding cursor_line to
/// build_type_param_subst / hover_info_at_location.
#[test]
fn hover_generic_subst_disambiguates_two_callers_in_same_file() {
    let idx = Indexer::new();

    let base_u = uri("/Processor.kt");
    idx.index_content(&base_u, "\
interface Processor<IN, OUT> {
    fun process(input: IN): OUT
}
");

    let caller_u = uri("/Callers.kt");
    idx.index_content(&caller_u, "\
class StringProcessor : Processor<String, Int> {
    override fun process(input: String): Int = input.length
}

class BoolProcessor : Processor<Boolean, String> {
    override fun process(input: Boolean): String = input.toString()
}
");

    let base_data = idx.files.get(base_u.as_str()).unwrap();
    let process_sym = base_data.symbols.iter().find(|s| s.name == "process").cloned()
        .expect("process not found");
    let loc = tower_lsp::lsp_types::Location {
        uri: base_u.clone(),
        range: process_sym.selection_range,
    };

    // Cursor on line 1 (inside StringProcessor): should use String/Int
    let hover_string = idx.hover_info_at_location(&loc, "process", Some(caller_u.as_str()), Some(1))
        .expect("hover should resolve for StringProcessor");
    assert!(!hover_string.contains("Boolean"),
        "StringProcessor hover (line 1) must not show Boolean, got: {hover_string}");

    // Cursor on line 6 (inside BoolProcessor): should use Boolean/String
    let hover_bool = idx.hover_info_at_location(&loc, "process", Some(caller_u.as_str()), Some(6))
        .expect("hover should resolve for BoolProcessor");
    assert!(hover_bool.contains("Boolean"),
        "BoolProcessor hover (line 6) should show Boolean, got: {hover_bool}");
    assert!(!hover_bool.contains("Int"),
        "BoolProcessor hover (line 6) must not show Int, got: {hover_bool}");
}
