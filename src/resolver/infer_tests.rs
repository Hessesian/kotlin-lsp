use super::super::infer_lines::{extract_return_type_from_detail, has_dot_after_first_call};

#[test]
fn return_type_simple() {
    assert_eq!(
        extract_return_type_from_detail("fun getDetail(req: Req): AccountDetail"),
        Some("AccountDetail".into()),
    );
}

#[test]
fn return_type_generic() {
    assert_eq!(
        extract_return_type_from_detail(
            "fun getAccountDetail(body: Body): Response<AccountDetail>"
        ),
        Some("Response<AccountDetail>".into()),
    );
}

#[test]
fn return_type_unit_returns_none() {
    assert_eq!(
        extract_return_type_from_detail("fun doSomething(x: Int)"),
        None
    );
}

#[test]
fn return_type_primitive_returns_none() {
    assert_eq!(extract_return_type_from_detail("fun count(): int"), None);
}

#[test]
fn return_type_nullable_stripped() {
    assert_eq!(
        extract_return_type_from_detail("fun find(): User?"),
        Some("User".into()),
    );
}

#[test]
fn has_dot_after_first_call_chained() {
    // paren_pos=7: "getList" is 7 chars, then "("
    assert!(has_dot_after_first_call("getList(isRefresh).joinAll()", 7));
}

#[test]
fn has_dot_after_first_call_standalone() {
    assert!(!has_dot_after_first_call(
        "getConnectedAccounts(isRefresh)",
        20
    ));
}

#[test]
fn has_dot_after_first_call_nested_parens() {
    // Nested parens inside arg list must not fool the scanner.
    assert!(has_dot_after_first_call("getList(foo(x)).map()", 7));
}

// ─── type_annotations (CST annotated property path) ──────────────────────────

#[test]
fn infer_annotated_property_from_cst() {
    use crate::indexer::Indexer;
    use crate::resolver::infer::{infer_variable_type, infer_variable_type_raw};
    use tower_lsp::lsp_types::Url;

    fn uri(p: &str) -> Url {
        Url::parse(&format!("file://{p}")).unwrap()
    }

    let file_uri = uri("/Foo.kt");
    let idx = Indexer::new();
    idx.index_content(
        &file_uri,
        "package com.example\nclass Foo {\n    val repo: UserRepository = inject()\n    val items: List<Product> = emptyList()\n    val state: StateFlow<UiState>? = null\n}",
    );

    // Non-raw: strips generics and nullability.
    assert_eq!(
        infer_variable_type(&idx, "repo", &file_uri),
        Some("UserRepository".into()),
        "simple annotated property"
    );
    assert_eq!(
        infer_variable_type(&idx, "items", &file_uri),
        Some("List".into()),
        "generic annotated property: non-raw strips generics"
    );
    assert_eq!(
        infer_variable_type(&idx, "state", &file_uri),
        Some("StateFlow".into()),
        "nullable annotated property: non-raw strips nullability"
    );

    // Raw: preserves generics and outer `?` (nullable flows through to ReceiverType).
    assert_eq!(
        infer_variable_type_raw(&idx, "items", &file_uri),
        Some("List<Product>".into()),
        "generic annotated property: raw preserves generics"
    );
    assert_eq!(
        infer_variable_type_raw(&idx, "state", &file_uri),
        Some("StateFlow<UiState>?".into()),
        "nullable annotated property: raw preserves ? (stripped in ReceiverType::from_raw)"
    );

    // Non-generic nullable: raw preserves ? too.
    let idx2 = Indexer::new();
    idx2.index_content(
        &file_uri,
        "package com.example\nclass Bar {\n    val user: User? = null\n}",
    );
    assert_eq!(
        infer_variable_type_raw(&idx2, "user", &file_uri),
        Some("User?".into()),
        "non-generic nullable: raw preserves ?"
    );
}

// ─── field_access_rhs: val x = recv.field preserves generics ─────────────────

#[test]
fn field_access_rhs_preserves_generics() {
    use crate::indexer::Indexer;
    use crate::resolver::infer::{infer_variable_type, infer_variable_type_raw};
    use tower_lsp::lsp_types::Url;

    fn uri(p: &str) -> Url {
        Url::parse(&format!("file://{p}")).unwrap()
    }

    let helper_uri = uri("/DashboardTriggersHelper.kt");
    let interactor_uri = uri("/RefreshDashboardInteractor.kt");

    let idx = Indexer::new();

    // Index the helper class with a Flow<DashboardTrigger> field.
    idx.index_content(
        &helper_uri,
        "package com.example\nclass DashboardTriggersHelper {\n    val triggersFlow: Flow<DashboardTrigger> = MutableStateFlow(emptyList())\n}",
    );

    // Index the interactor: constructor param (no val) + unannotated val with field access RHS.
    idx.index_content(
        &interactor_uri,
        "package com.example\nclass RefreshDashboardInteractor(\n    dashboardTriggersHelper: DashboardTriggersHelper\n) {\n    val triggers = dashboardTriggersHelper.triggersFlow\n}",
    );

    // Raw path should preserve generics: Flow<DashboardTrigger>
    assert_eq!(
        infer_variable_type_raw(&idx, "triggers", &interactor_uri),
        Some("Flow<DashboardTrigger>".into()),
        "field_access_rhs raw should preserve generics"
    );

    // Non-raw path should also preserve generics for display (matching method_call_rhs behavior)
    assert_eq!(
        infer_variable_type(&idx, "triggers", &interactor_uri),
        Some("Flow<DashboardTrigger>".into()),
        "field_access_rhs non-raw should preserve generics (like method_call_rhs does)"
    );
}

/// Cross-file resolution: `find_field_type_in_class` should resolve unannotated
/// `val x = recv.field` properties by falling back to `infer_variable_type_raw`.
#[test]
fn find_field_type_in_class_resolves_unannotated_field_access() {
    use crate::indexer::Indexer;
    use crate::resolver::infer::find_field_type_in_class;
    use tower_lsp::lsp_types::Url;

    fn uri(p: &str) -> Url {
        Url::parse(&format!("file://{p}")).unwrap()
    }

    let helper_uri = uri("/DashboardTriggersHelper.kt");
    let interactor_uri = uri("/RefreshDashboardInteractor.kt");

    let idx = Indexer::new();

    idx.index_content(
        &helper_uri,
        "package com.example\nclass DashboardTriggersHelper {\n    val triggersFlow: Flow<DashboardTrigger> = MutableStateFlow(emptyList())\n}",
    );
    idx.index_content(
        &interactor_uri,
        "package com.example\nclass RefreshDashboardInteractor(\n    dashboardTriggersHelper: DashboardTriggersHelper\n) {\n    val triggers = dashboardTriggersHelper.triggersFlow\n}",
    );

    // find_field_type_in_class should resolve through field_access_rhs fallback.
    assert_eq!(
        find_field_type_in_class(&idx, "RefreshDashboardInteractor", "triggers"),
        Some("Flow<DashboardTrigger>".into()),
        "find_field_type_in_class should resolve unannotated val with field_access_rhs"
    );
}

#[test]
fn supertype_subst_replaces_generic_params() {
    let raw = "Flow<ReducedResult<EffectType, StateType>>";
    let params = vec!["EventType".into(), "EffectType".into(), "StateType".into()];
    let args = vec![
        "BuildingSavingsInputEvent".into(),
        "BuildingSavingsEffect".into(),
        "Sheet".into(),
    ];
    assert_eq!(
        super::apply_supertype_subst(raw, &params, &args),
        "Flow<ReducedResult<BuildingSavingsEffect, Sheet>>"
    );
}

#[test]
fn supertype_subst_whole_word_only() {
    let raw = "EventTypeHandler<EventType>";
    let params = vec!["EventType".into()];
    let args = vec!["Click".into()];
    // "EventType" inside "EventTypeHandler" should NOT be replaced
    assert_eq!(
        super::apply_supertype_subst(raw, &params, &args),
        "EventTypeHandler<Click>"
    );
}
