/// Shared test helpers for indexer sub-modules.
///
/// Compiled only in `#[cfg(test)]` mode.

/// Global mutex serialising tests that mutate `XDG_CACHE_HOME`.
/// All tests in any `indexer::*` sub-module that touch this env var
/// must acquire this lock to avoid races when tests run in parallel.
pub(crate) static XDG_CACHE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` with `XDG_CACHE_HOME` temporarily pointing at `dir`.
/// Restores the original value (or removes the var) on exit, even on panic.
pub(crate) fn with_xdg_cache<F: FnOnce()>(dir: &std::path::Path, f: F) {
    let _guard = XDG_CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("XDG_CACHE_HOME").ok();
    std::env::set_var("XDG_CACHE_HOME", dir);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match prev {
        Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
        None    => std::env::remove_var("XDG_CACHE_HOME"),
    }
    if let Err(e) = result { std::panic::resume_unwind(e); }
}
