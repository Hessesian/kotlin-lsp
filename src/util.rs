use std::path::PathBuf;

/// Returns the current user's home directory.
///
/// Wraps the deprecated `std::env::home_dir` in a single place so callsites
/// don't need to repeat the `#[allow(deprecated)]` annotation.
#[allow(deprecated)]
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}
