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

// ── keyword helpers ───────────────────────────────────────────────────────────

#[test]
fn swift_uses_func_keyword() {
    // Swift functions must render as `func`, not `fun`.
    assert_eq!(
        super::symbol_kw_for_lang(SymbolKind::FUNCTION, "swift"),
        "func"
    );
    assert_eq!(
        super::symbol_kw_for_lang(SymbolKind::METHOD, "swift"),
        "func"
    );
}

#[test]
fn kotlin_uses_fun_keyword() {
    assert_eq!(
        super::symbol_kw_for_lang(SymbolKind::FUNCTION, "kotlin"),
        "fun"
    );
    assert_eq!(
        super::symbol_kw_for_lang(SymbolKind::METHOD, "kotlin"),
        "fun"
    );
}

// ── resolve_symbol_info: KDoc inclusion ──────────────────────────────────────

#[test]
fn resolve_includes_kdoc() {
    use crate::indexer::resolution::{resolve_symbol_info, ResolveOptions, SubstitutionContext};

    let src = r#"package com.example

/**
 * Represents a user account.
 */
class Account(val name: String)"#;
    let (u, idx) = indexed("/Account.kt", src);

    let info = resolve_symbol_info(
        &idx,
        "Account",
        None,
        &u,
        SubstitutionContext::None,
        &ResolveOptions {
            allow_rg: false,
            include_doc: true,
            apply_subst: false,
            prefer_cached_detail: false,
        },
    )
    .expect("Account should resolve");

    assert!(
        info.doc.contains("Represents a user account"),
        "KDoc should appear in doc field, got: {:?}",
        info.doc
    );
}

// ── val binding: find_definition_qualified + signature ───────────────────────

#[test]
fn val_binding_found_and_has_type() {
    use crate::indexer::resolution::{resolve_symbol_info, ResolveOptions, SubstitutionContext};

    let (u, idx) = indexed(
        "/Foo.kt",
        "\
class Foo(
    private val repo: IGoldConversionRepository
) {
    fun doStuff() {}
}",
    );
    // 1. find_definition_qualified must find repo
    let locs = idx.find_definition_qualified("repo", None, &u);
    assert!(
        !locs.is_empty(),
        "repo should be found via find_definition_qualified"
    );

    // 2. resolve_symbol_info should return a signature mentioning the type
    let info = resolve_symbol_info(
        &idx,
        "repo",
        None,
        &u,
        SubstitutionContext::None,
        &ResolveOptions {
            allow_rg: false,
            include_doc: false,
            apply_subst: false,
            prefer_cached_detail: false,
        },
    );
    assert!(info.is_some(), "resolve should find repo");
    let sig = info.unwrap().signature;
    assert!(
        sig.contains("repo"),
        "signature should mention 'repo', got: {sig}"
    );
    assert!(
        sig.contains("IGoldConversionRepository"),
        "signature should show type, got: {sig}"
    );
}

#[test]
fn real_val_binding_constructor_param() {
    use crate::indexer::resolution::{resolve_symbol_info, ResolveOptions, SubstitutionContext};

    let (u, idx) = indexed(
        "/ContactAddressInteractor.kt",
        "\
package cz.moneta.smartbanka.feature.gold_conversion.model.goldcard
internal class ContactAddressInteractor @Inject constructor(
  private val repo: IGoldConversionRepository,
) : ISimpleLoadDataInteractor<PersonalAddress> {
  override suspend fun loadData(): PersonalAddress =
    requireNotNull(repo.contactAddressSetup().contactAddress)
}",
    );
    let locs = idx.find_definition_qualified("repo", None, &u);
    assert!(!locs.is_empty(), "repo should be found");

    let info = resolve_symbol_info(
        &idx,
        "repo",
        None,
        &u,
        SubstitutionContext::None,
        &ResolveOptions {
            allow_rg: false,
            include_doc: false,
            apply_subst: false,
            prefer_cached_detail: false,
        },
    )
    .expect("hover on val repo should work");
    assert!(
        info.signature.contains("IGoldConversionRepository"),
        "hover should show type: {}",
        info.signature
    );
}

// ── Port-regression: property-type harvesting ─────────────────────────────────

/// When the enclosing class's supers contain generic type args, the inlay-hint
/// substitution path (`build_subst_map`) must return the correct mapping.
/// This tests the "UncC" scenario: build_enclosing_class_subst must perform
/// phase 2 (member property harvesting) so that a property whose type extends
/// a generic base contributes its type-param mappings to the enclosing class.
#[test]
fn inlay_hints_generic_subst_via_property_type_harvesting() {
    let idx = Indexer::new();

    let base_u = uri("/FlowReducer.kt");
    idx.index_content(
        &base_u,
        "\
interface FlowReducer<E, S> {
    fun reduce(event: E, state: S): S
}
",
    );

    let reducer_u = uri("/DashReducer.kt");
    idx.index_content(
        &reducer_u,
        "\
class DashReducer : FlowReducer<DashEvent, DashState> {
    override fun reduce(event: DashEvent, state: DashState): DashState = state
}
",
    );

    let owner_u = uri("/DashViewModel.kt");
    idx.index_content(
        &owner_u,
        "\
class DashViewModel {
    val reducer: DashReducer = DashReducer()
    fun process(event: DashEvent) {}
}
",
    );

    let subst = crate::indexer::resolution::build_subst_map(&idx, owner_u.as_str(), 2);
    assert!(
        subst.contains_key("E") || subst.contains_key("S"),
        "property harvesting should produce substitution for FlowReducer params, got: {subst:?}"
    );
    if let Some(e_val) = subst.get("E") {
        assert_eq!(
            e_val, "DashEvent",
            "E should map to DashEvent, got: {e_val}"
        );
    }
    if let Some(s_val) = subst.get("S") {
        assert_eq!(
            s_val, "DashState",
            "S should map to DashState, got: {s_val}"
        );
    }
}

// ── completionItem/resolve: enrich_at_line populates documentation ─────────────

/// Regression: removing the old completion-doc tests would leave the
/// `completionItem/resolve` path (enrich_at_line → ResolvedSymbol.doc) uncovered.
/// This test ensures that `enrich_at_line` finds the right symbol and that
/// `resolve_symbol_info` with `include_doc: true` populates a documentation string.
#[test]
fn completion_resolve_populates_documentation() {
    use crate::indexer::resolution::{enrich_at_line, ResolveOptions, SubstitutionContext};

    let (u, idx) = indexed(
        "/Documented.kt",
        "\
/** The answer to everything. */
fun theAnswer(): Int = 42
",
    );

    // `enrich_at_line` is called by completion_resolve with line/col stored in
    // the completion item data.  Line 1 = `fun theAnswer()…`; col 4 = 't' in theAnswer.
    let result = enrich_at_line(
        &idx,
        u.as_str(),
        1,  // identifier line
        4,  // col within "theAnswer"
        SubstitutionContext::None,
        &ResolveOptions {
            allow_rg: false,
            include_doc: true,
            apply_subst: false,
            prefer_cached_detail: false,
        },
    );

    let resolved = result.expect("enrich_at_line should find theAnswer");
    assert!(
        resolved.doc.contains("answer"),
        "documentation should contain KDoc text, got: {:?}",
        resolved.doc
    );
}
