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

/// Metadata about a resolved callable (function or method) used for generic
/// type substitution in lambda parameter inference.
#[derive(Clone, Debug, Default)]
pub(crate) struct CallableInfo {
    /// Declared type parameter names (e.g. `["EffectType", "StateType"]`).
    pub type_params: Vec<String>,
    /// Full extension receiver type with generics (e.g. `"Flow<ReducedResult<E, S>>"`).
    /// Empty for non-extension functions.
    pub extension_receiver_type: String,
}

/// Minimum dependency surface for pure inference helpers and their lightweight
/// orchestration layers.
///
/// Two concrete implementations:
/// - `Indexer` — production, full resolution with rg fallback
/// - `TestDeps` — test stub, drives inference unit tests from plain `HashMap`s
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

    /// Return the declared type parameter names for a class (e.g. `["T"]` for
    /// `class Result<T>`, `["K", "V"]` for `class Map<K, V>`).
    ///
    /// Used to build a substitution map when the class is instantiated with
    /// concrete type arguments (e.g. `Result<FamilyAccount>` → `{"T": "FamilyAccount"}`).
    ///
    /// Returns an empty vec when the class is not found or has no type params.
    fn find_class_type_params(&self, _class_name: &str) -> Vec<String> {
        Vec::new()
    }

    /// Return the raw return type of `method_name` declared inside `class_name`,
    /// preserving generic type parameters (e.g. `"T?"` for `Result.getOrNull()`).
    ///
    /// Used together with `find_class_type_params` + `apply_type_subst` to
    /// resolve generic return types to concrete types at a call site.
    ///
    /// Returns `None` when the method is not found.
    fn find_method_return_type_for_type(
        &self,
        _class_name: &str,
        _method_name: &str,
    ) -> Option<String> {
        None
    }

    /// Return the raw parameter text for a method declared inside `class_name`.
    ///
    /// Used for receiver-aware positional param lookup in inline lambdas:
    /// `factory.create(arg, { it })` → resolve `factory` type → look up `create`
    /// on that type specifically (avoids ambiguity with other classes' `create`).
    ///
    /// Returns `None` when the class or method is not found.
    fn find_method_params_text(&self, _class_name: &str, _method_name: &str) -> Option<String> {
        None
    }

    /// Return callable metadata (type params + extension receiver type) for a
    /// function resolved by name.  Used to apply generic type substitution when
    /// a lambda parameter type is a generic type parameter.
    ///
    /// The default implementation returns `None`; overridden by `Indexer`.
    fn find_fun_callable_info(&self, _fn_name: &str, _uri: &Url) -> Option<CallableInfo> {
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
    /// `class_name` → list of type parameter names, e.g. `"Result"` → `["T"]`
    pub class_params: std::collections::HashMap<String, Vec<String>>,
    /// `(class_name, method_name)` → raw return type (with generics)
    pub method_return_types: std::collections::HashMap<(String, String), String>,
    /// `(class_name, method_name)` → raw params text
    pub method_params: std::collections::HashMap<(String, String), String>,
    /// `fn_name` → callable info (type params + extension receiver type)
    pub callable_infos: std::collections::HashMap<String, CallableInfo>,
}

#[cfg(test)]
impl TestDeps {
    pub(crate) fn new() -> Self {
        TestDeps {
            fun_sigs: std::collections::HashMap::new(),
            var_types: std::collections::HashMap::new(),
            field_types: std::collections::HashMap::new(),
            return_types: std::collections::HashMap::new(),
            class_params: std::collections::HashMap::new(),
            method_return_types: std::collections::HashMap::new(),
            method_params: std::collections::HashMap::new(),
            callable_infos: std::collections::HashMap::new(),
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

    /// Register `class_name` → type parameter names (e.g. `"Result"` → `["T"]`).
    pub(crate) fn with_class_params(mut self, class_name: &str, params: &[&str]) -> Self {
        self.class_params.insert(
            class_name.to_string(),
            params.iter().map(|s| s.to_string()).collect(),
        );
        self
    }

    /// Register `(class_name, method_name)` → raw return type (with generics).
    pub(crate) fn with_method_return_for_type(
        mut self,
        class_name: &str,
        method_name: &str,
        ty: &str,
    ) -> Self {
        self.method_return_types.insert(
            (class_name.to_string(), method_name.to_string()),
            ty.to_string(),
        );
        self
    }

    /// Register `(class_name, method_name)` → raw params text.
    #[allow(dead_code)]
    pub(crate) fn with_method_params(
        mut self,
        class_name: &str,
        method_name: &str,
        params: &str,
    ) -> Self {
        self.method_params.insert(
            (class_name.to_string(), method_name.to_string()),
            params.to_string(),
        );
        self
    }

    /// Register `fn_name` → callable info for generic extension function tests.
    #[allow(dead_code)]
    pub(crate) fn with_callable_info(
        mut self,
        fn_name: &str,
        type_params: &[&str],
        extension_receiver_type: &str,
    ) -> Self {
        self.callable_infos.insert(
            fn_name.to_string(),
            CallableInfo {
                type_params: type_params.iter().map(|s| s.to_string()).collect(),
                extension_receiver_type: extension_receiver_type.to_string(),
            },
        );
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
    fn find_class_type_params(&self, class_name: &str) -> Vec<String> {
        self.class_params
            .get(class_name)
            .cloned()
            .unwrap_or_default()
    }
    fn find_method_return_type_for_type(
        &self,
        class_name: &str,
        method_name: &str,
    ) -> Option<String> {
        self.method_return_types
            .get(&(class_name.to_string(), method_name.to_string()))
            .cloned()
    }
    fn find_method_params_text(&self, class_name: &str, method_name: &str) -> Option<String> {
        self.method_params
            .get(&(class_name.to_string(), method_name.to_string()))
            .cloned()
    }
    fn find_fun_callable_info(&self, fn_name: &str, _uri: &Url) -> Option<CallableInfo> {
        self.callable_infos.get(fn_name).cloned()
    }
}
