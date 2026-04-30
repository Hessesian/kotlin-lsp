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
    let hover = idx.hover_info("Account").unwrap();
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
    let hover = idx.hover_info_at_location(locs.first().unwrap(), "repo");
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
    let hover = idx.hover_info_at_location(locs.first().unwrap(), "repo");
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

    let result = idx.completion_docs_for(u.as_str(), line, col);
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
    assert!(idx.completion_docs_for(u.as_str(), line, col).is_none());
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
    let (doc_md, _detail) = idx.completion_docs_for(u.as_str(), line, col).unwrap();
    assert!(doc_md.contains("Configure something"));
}
