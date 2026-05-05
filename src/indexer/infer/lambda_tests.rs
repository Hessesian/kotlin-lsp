use super::*;

#[test]
fn lambda_type_first_input_parses_correctly() {
    assert_eq!(
        lambda_type_first_input("(ResultState<T>) -> Model"),
        Some("ResultState".into())
    );
    assert_eq!(
        lambda_type_first_input("(String, Int) -> Unit"),
        Some("String".into())
    );
    assert_eq!(lambda_type_first_input("() -> Unit"), None);
    assert_eq!(
        lambda_type_first_input("(id: String, scan: String) -> Unit"),
        Some("String".into())
    );
    // Double-wrapped parens (Kotlin allows `((T) -> R)` as a type annotation):
    assert_eq!(
        lambda_type_first_input("((T) -> ProductDetailSheetModel)"),
        Some("T".into())
    );
    assert_eq!(
        lambda_type_first_input("((LoanDetail) -> Model)"),
        Some("LoanDetail".into())
    );
    // `->` arrow must not confuse angle-bracket depth tracking:
    assert_eq!(
        lambda_type_first_input("(Flow<T>) -> Unit"),
        Some("Flow".into())
    );
    // `suspend` prefix — Kotlin suspend function types like `suspend (T) -> Unit`:
    assert_eq!(
        lambda_type_first_input("suspend (T) -> Unit"),
        Some("T".into())
    );
    assert_eq!(
        lambda_type_first_input("suspend (value: LoanDetail) -> Unit"),
        Some("LoanDetail".into())
    );
    assert_eq!(
        lambda_type_first_input("suspend (String, Int) -> Unit"),
        Some("String".into())
    );
    assert_eq!(lambda_type_first_input("suspend () -> Unit"), None);
}

#[test]
fn lambda_type_nth_input_test() {
    assert_eq!(
        lambda_type_nth_input("(String, Boolean) -> Unit", 0),
        Some("String".into())
    );
    assert_eq!(
        lambda_type_nth_input("(String, Boolean) -> Unit", 1),
        Some("Boolean".into())
    );
    assert_eq!(lambda_type_nth_input("() -> Unit", 0), None);
    assert_eq!(
        lambda_type_nth_input("(SaveInfo) -> Unit", 0),
        Some("SaveInfo".into())
    );
    // suspend function type as whole outer type:
    assert_eq!(
        lambda_type_nth_input("suspend (T) -> Unit", 0),
        Some("T".into())
    );
    assert_eq!(
        lambda_type_nth_input("suspend (LoanDetail) -> Unit", 0),
        Some("LoanDetail".into())
    );
    assert_eq!(lambda_type_nth_input("suspend () -> Unit", 0), None);
}
