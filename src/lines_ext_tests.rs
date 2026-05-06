use super::LinesExt;

fn lines(src: &str) -> Vec<String> {
    src.lines().map(str::to_owned).collect()
}

#[test]
fn extract_detail_single_line() {
    let ls = lines("fun foo() {}");
    assert!(!ls.extract_detail(0, 0).is_empty());
}

#[test]
fn visibility_at_private() {
    let ls = lines("private fun foo() {}");
    use crate::types::Visibility;
    assert_eq!(ls.visibility_at(0), Visibility::Private);
}

#[test]
fn import_insertion_after_imports() {
    let ls = lines("package com.example\nimport android.os.Bundle\n\nclass Foo");
    assert!(ls.import_insertion_line() > 0);
}

#[test]
fn declared_names_finds_val() {
    let ls = lines("val viewModel: MyViewModel");
    assert!(ls.declared_names().iter().any(|n| n == "viewModel"));
}

#[test]
fn infer_type_finds_annotation() {
    let ls = lines("val items: List<String> = emptyList()");
    assert_eq!(ls.infer_type("items").as_deref(), Some("List"));
}

#[test]
fn infer_type_raw_preserves_generics() {
    let ls = lines("val items: List<String> = emptyList()");
    assert_eq!(ls.infer_type_raw("items").as_deref(), Some("List<String>"));
}

#[test]
fn find_declaration_range_finds_val() {
    let ls = lines("val account: Account = Account()");
    let r = ls.find_declaration_range("account");
    assert!(r.is_some());
}

#[test]
fn collect_signature_single_line() {
    let ls = lines("fun foo(x: Int): Boolean {");
    assert_eq!(ls.collect_signature(0), "fun foo(x: Int): Boolean");
}

#[test]
fn parse_imports_finds_import() {
    let ls = lines("import android.os.Bundle");
    let imports = ls.parse_imports();
    assert!(imports.iter().any(|i| i.full_path.contains("Bundle")));
}
