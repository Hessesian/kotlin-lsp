//! `InferDeps` — a narrow seam that isolates pure lambda/type inference helpers
//! from the full `Indexer` so they can be unit-tested with lightweight stubs.
//!
//! ## What goes in the trait
//!
//! Only the two operations that the pure leaf helpers in `it_this.rs` need:
//! - looking up a function's parameter signature
//! - inferring a variable's declared type
//!
//! The trait intentionally excludes `mem_lines_for`; higher-level orchestrators
//! already take the caller-provided lines directly. `live_doc` is included only as
//! an optional CST hook for orchestration helpers; pure leaf helpers should keep
//! working against the narrower lookup methods below.
//!
//! ## `fun_params_text` is not cheap
//!
//! The `Indexer` implementation of `find_fun_params_text` delegates to
//! `find_fun_signature_full`, which may perform on-demand rg indexing.
//! Callers should not assume this is a pure in-memory lookup.

use std::sync::Arc;

use tower_lsp::lsp_types::Url;

use crate::indexer::LiveDoc;

/// Minimum dependency surface for pure lambda/type inference leaf functions.
///
/// Two concrete implementations:
/// - `Indexer` — production, full resolution with rg fallback
/// - `TestDeps` — test stub, drives leaf-helper unit tests from plain `HashMap`s
pub(crate) trait InferDeps {
    /// Return the raw parameter text inside a function's outer `()`, e.g.
    /// `"key: K, flow: Flow<T>, map: (T) -> Model"` (no surrounding parens).
    ///
    /// May perform on-demand rg/disk I/O in the `Indexer` implementation.
    /// Returns `None` when the function is not found.
    fn find_fun_params_text(&self, fn_name: &str, uri: &Url) -> Option<String>;

    /// Infer the declared type of a local variable from its annotation in the
    /// file at `uri`, e.g. `"val interactor: OneYearOlderInteractor"` → `"OneYearOlderInteractor"`.
    ///
    /// Returns `None` when the variable has no detectable declaration.
    fn find_var_type(&self, var_name: &str, uri: &Url) -> Option<String>;

    /// Look up the return type of a function by name, without needing to know the
    /// receiver type.  Used for method-chain receivers like
    /// `getList().joinAll().firstOrNull { it }` where `receiver_var = "joinAll"`.
    ///
    /// Returns `None` by default; overridden by `Indexer`.
    fn find_fun_return_type(&self, _fn_name: &str) -> Option<String> {
        None
    }

    /// Look up the raw declared type of `field_name` inside class `class_name`,
    /// searching across indexed files.  Preserves generic parameters so that
    /// `extract_collection_element_type` can extract the element type.
    ///
    /// Example: `class_name = "ResponseBody"`, `field_name = "availableBanks"` →
    /// `Some("MutableList<MultibankingBank>")`.
    ///
    /// Returns `None` when the class or field is not found.
    /// Default implementation returns `None`; overridden by `Indexer`.
    fn find_field_type(&self, _class_name: &str, _field_name: &str) -> Option<String> {
        None
    }

    /// Return the live CST document for `uri` when the file is currently open.
    /// Higher-level orchestration helpers may use this to walk the tree and then
    /// feed extracted context into the pure string helpers.
    fn live_doc(&self, _uri: &Url) -> Option<Arc<LiveDoc>> {
        None
    }
}

// ─── Test stub ───────────────────────────────────────────────────────────────

/// Lightweight stub for unit-testing pure inference leaf functions.
///
/// Keyed by `(uri_str, name)` to mirror production `uri`-aware resolution and
/// prevent tests from hiding ambiguity bugs that would surface in real projects.
#[cfg(test)]
pub(crate) struct TestDeps {
    /// `(uri_str, fn_name)` → raw params text
    pub fun_sigs: std::collections::HashMap<(String, String), String>,
    /// `(uri_str, var_name)` → type name
    pub var_types: std::collections::HashMap<(String, String), String>,
    /// `(class_name, field_name)` → raw type (with generics)
    pub field_types: std::collections::HashMap<(String, String), String>,
    /// `fn_name` → raw return type (with generics)
    pub return_types: std::collections::HashMap<String, String>,
}

#[cfg(test)]
impl TestDeps {
    pub(crate) fn new() -> Self {
        TestDeps {
            fun_sigs: std::collections::HashMap::new(),
            var_types: std::collections::HashMap::new(),
            field_types: std::collections::HashMap::new(),
            return_types: std::collections::HashMap::new(),
        }
    }

    /// Register `fn_name` → `params_text` for `uri`.
    pub(crate) fn with_fun(mut self, uri: &str, fn_name: &str, params: &str) -> Self {
        self.fun_sigs
            .insert((uri.to_string(), fn_name.to_string()), params.to_string());
        self
    }

    /// Register `var_name` → `type_name` for `uri`.
    pub(crate) fn with_var(mut self, uri: &str, var_name: &str, ty: &str) -> Self {
        self.var_types
            .insert((uri.to_string(), var_name.to_string()), ty.to_string());
        self
    }

    /// Register `field_name` in `class_name` → raw type (with generics).
    pub(crate) fn with_field(mut self, class_name: &str, field_name: &str, ty: &str) -> Self {
        self.field_types.insert(
            (class_name.to_string(), field_name.to_string()),
            ty.to_string(),
        );
        self
    }

    /// Register `fn_name` → raw return type (with generics), for method-chain tests.
    pub(crate) fn with_return(mut self, fn_name: &str, ty: &str) -> Self {
        self.return_types
            .insert(fn_name.to_string(), ty.to_string());
        self
    }
}

#[cfg(test)]
impl InferDeps for TestDeps {
    fn find_fun_params_text(&self, fn_name: &str, uri: &Url) -> Option<String> {
        self.fun_sigs
            .get(&(uri.to_string(), fn_name.to_string()))
            .cloned()
    }
    fn find_var_type(&self, var_name: &str, uri: &Url) -> Option<String> {
        self.var_types
            .get(&(uri.to_string(), var_name.to_string()))
            .cloned()
    }
    fn find_field_type(&self, class_name: &str, field_name: &str) -> Option<String> {
        self.field_types
            .get(&(class_name.to_string(), field_name.to_string()))
            .cloned()
    }
    fn find_fun_return_type(&self, fn_name: &str) -> Option<String> {
        self.return_types.get(fn_name).cloned()
    }
}
