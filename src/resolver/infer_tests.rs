use super::extract_return_type_from_detail;

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
    assert!(super::has_dot_after_first_call(
        "getList(isRefresh).joinAll()",
        7
    ));
}

#[test]
fn has_dot_after_first_call_standalone() {
    assert!(!super::has_dot_after_first_call(
        "getConnectedAccounts(isRefresh)",
        20
    ));
}

#[test]
fn has_dot_after_first_call_nested_parens() {
    // Nested parens inside arg list must not fool the scanner.
    assert!(super::has_dot_after_first_call("getList(foo(x)).map()", 7));
}
