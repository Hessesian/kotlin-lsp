use super::ImportEntry;

#[test]
fn covers_direct_import() {
    let import = ImportEntry {
        full_path: "com.example.Config".to_owned(),
        local_name: "Config".to_owned(),
        is_star: false,
    };

    assert!(import.covers("com.example", "Config"));
}

#[test]
fn covers_nested_import() {
    let import = ImportEntry {
        full_path: "com.example.Outer.Config".to_owned(),
        local_name: "Config".to_owned(),
        is_star: false,
    };

    assert!(import.covers("com.example", "Config"));
}

#[test]
fn covers_deeply_nested_import() {
    let import = ImportEntry {
        full_path: "com.example.Outer.Inner.Config".to_owned(),
        local_name: "Config".to_owned(),
        is_star: false,
    };

    assert!(import.covers("com.example", "Config"));
}

#[test]
fn covers_star_import_for_package_members() {
    let import = ImportEntry {
        full_path: "com.example".to_owned(),
        local_name: "*".to_owned(),
        is_star: true,
    };

    assert!(import.covers("com.example", "Config"));
}

#[test]
fn does_not_cover_other_package() {
    let import = ImportEntry {
        full_path: "com.other.Outer.Config".to_owned(),
        local_name: "Config".to_owned(),
        is_star: false,
    };

    assert!(!import.covers("com.example", "Config"));
}
