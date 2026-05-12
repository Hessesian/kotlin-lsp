use crate::language::LanguageParser;
use crate::language::{java::JavaParser, kotlin::KotlinParser, swift::SwiftParser};

#[test]
fn language_ids_are_distinct() {
    let ids: Vec<&str> = vec![
        KotlinParser.language_id(),
        JavaParser.language_id(),
        SwiftParser.language_id(),
    ];
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len(), "language IDs must be distinct");
}

#[test]
fn language_parser_via_enum_dispatches_correctly() {
    use crate::Language;
    assert_eq!(
        Language::from_path("Foo.kt").parser().language_id(),
        "kotlin"
    );
    assert_eq!(
        Language::from_path("Foo.kts").parser().language_id(),
        "kotlin"
    );
    assert_eq!(
        Language::from_path("Bar.java").parser().language_id(),
        "java"
    );
    assert_eq!(
        Language::from_path("Baz.swift").parser().language_id(),
        "swift"
    );
}

#[test]
fn parse_by_extension_routes_through_provider() {
    // Sanity: parse_by_extension now delegates to Language::parser().parse().
    let kotlin_data = crate::parser::parse_by_extension("Foo.kt", "class Foo");
    assert!(kotlin_data.symbols.iter().any(|s| s.name == "Foo"));

    let java_data = crate::parser::parse_by_extension("Bar.java", "public class Bar {}");
    assert!(java_data.symbols.iter().any(|s| s.name == "Bar"));

    let swift_data = crate::parser::parse_by_extension("Baz.swift", "class Baz {}");
    assert!(swift_data.symbols.iter().any(|s| s.name == "Baz"));
}
