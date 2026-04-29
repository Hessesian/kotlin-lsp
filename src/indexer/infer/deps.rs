//! `InferDeps` — a narrow seam that isolates pure lambda/type inference helpers
//! from the full `Indexer` so they can be unit-tested with lightweight stubs.
//!
//! ## What goes in the trait
//!
//! Only the two operations that the pure leaf helpers in `it_this.rs` need:
//! - looking up a function's parameter signature
//! - inferring a variable's declared type
//!
//! The trait intentionally excludes `mem_lines_for` and `live_doc`: those are
//! needed by the higher-level orchestrators (`find_it_element_type_in_lines_impl`
//! etc.) which already take `&Indexer`.  Move them into the trait only if those
//! orchestrators are later pushed behind the seam.
//!
//! ## `fun_params_text` is not cheap
//!
//! The `Indexer` implementation of `find_fun_params_text` delegates to
//! `find_fun_signature_full`, which may perform on-demand rg indexing.
//! Callers should not assume this is a pure in-memory lookup.

use tower_lsp::lsp_types::Url;

/// Minimum dependency surface for pure lambda/type inference leaf functions.
///
/// Two concrete impls exist (Rule of Three satisfied):
/// - `Indexer` — production, full resolution with rg fallback
/// - `TestDeps` — test stub, drives leaf-helper unit tests from plain `HashMap`s
pub trait InferDeps {
    /// Return the raw parameter signature text for a function, e.g.
    /// `"(key: K, flow: Flow<T>, map: (T) -> Model)"`.
    ///
    /// May perform on-demand rg/disk I/O in the `Indexer` implementation.
    /// Returns `None` when the function is not found.
    fn find_fun_params_text(&self, fn_name: &str, uri: &Url) -> Option<String>;

    /// Infer the declared type of a local variable from its annotation in the
    /// file at `uri`, e.g. `"val interactor: OneYearOlderInteractor"` → `"OneYearOlderInteractor"`.
    ///
    /// Returns `None` when the variable has no detectable declaration.
    fn find_var_type(&self, var_name: &str, uri: &Url) -> Option<String>;
}

// ─── Test stub ───────────────────────────────────────────────────────────────

/// Lightweight stub for unit-testing pure inference leaf functions.
///
/// Keyed by `(uri_str, name)` to mirror production `uri`-aware resolution and
/// prevent tests from hiding ambiguity bugs that would surface in real projects.
#[cfg(test)]
pub(crate) struct TestDeps {
    /// `(uri_str, fn_name)` → raw params text
    pub fun_sigs:  std::collections::HashMap<(String, String), String>,
    /// `(uri_str, var_name)` → type name
    pub var_types: std::collections::HashMap<(String, String), String>,
}

#[cfg(test)]
impl TestDeps {
    pub fn new() -> Self {
        TestDeps {
            fun_sigs:  std::collections::HashMap::new(),
            var_types: std::collections::HashMap::new(),
        }
    }

    /// Register `fn_name` → `params_text` for `uri`.
    pub fn with_fun(mut self, uri: &str, fn_name: &str, params: &str) -> Self {
        self.fun_sigs.insert((uri.to_string(), fn_name.to_string()), params.to_string());
        self
    }

    /// Register `var_name` → `type_name` for `uri`.
    pub fn with_var(mut self, uri: &str, var_name: &str, ty: &str) -> Self {
        self.var_types.insert((uri.to_string(), var_name.to_string()), ty.to_string());
        self
    }
}

#[cfg(test)]
impl InferDeps for TestDeps {
    fn find_fun_params_text(&self, fn_name: &str, uri: &Url) -> Option<String> {
        self.fun_sigs.get(&(uri.to_string(), fn_name.to_string())).cloned()
    }
    fn find_var_type(&self, var_name: &str, uri: &Url) -> Option<String> {
        self.var_types.get(&(uri.to_string(), var_name.to_string())).cloned()
    }
}
