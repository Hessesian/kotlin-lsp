use std::path::PathBuf;

/// Returns the current user's home directory.
///
/// Wraps the deprecated `std::env::home_dir` in a single place so callsites
/// don't need to repeat the `#[allow(deprecated)]` annotation.
#[allow(deprecated)]
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

/// Heuristic: a file path likely belongs to a test source set.
// TODO: replace callers with FileData.source_set.
// Retained temporarily for callers outside the FileData.source_set migration.
#[allow(dead_code)]
pub(crate) fn is_test_file(uri_str: &str) -> bool {
    uri_str.contains("/src/test/")
        || uri_str.contains("/src/androidTest/")
        || uri_str.contains("/src/commonTest/")
        || uri_str.contains("/src/iosTest/")
}
