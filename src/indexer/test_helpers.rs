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

/// Global mutex serialising tests that mutate arbitrary env vars.
/// Each unique env var should use its own lock to avoid unnecessary serialisation.
pub(crate) static ENV_VAR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` with `var` temporarily set to `value`.
/// Saves the previous value and restores it (or removes the var) on exit, even on panic.
pub(crate) fn with_env_var<F: FnOnce()>(var: &str, value: &str, lock: &std::sync::Mutex<()>, f: F) {
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var(var).ok();
    std::env::set_var(var, value);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match prev {
        Some(v) => std::env::set_var(var, v),
        None    => std::env::remove_var(var),
    }
    if let Err(e) = result { std::panic::resume_unwind(e); }
}
