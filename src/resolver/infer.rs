use tower_lsp::lsp_types::{Position, SymbolKind, Url};

use crate::indexer::Indexer;
use crate::LinesExt;
use crate::StrExt;

use super::ensure_file_data;
use super::infer_lines::{extract_return_type_from_detail, find_rhs_str, has_dot_after_first_call};

// ─── InferenceChain trait ─────────────────────────────────────────────────────

/// Capability trait for type-inference queries over an indexed workspace.
///
/// Implemented by [`Indexer`] in production.  Mirrors the shape of
/// [`ResolutionChain`](super::resolve::ResolutionChain) — all methods
/// delegate to the free functions in this module so the trait is a zero-cost
/// façade.
///
/// `#[allow(dead_code)]` is retained until this trait is wired through the
/// resolution pipeline in a future pass (G4).
// TODO(G4): wire trait bound through resolution pipeline to enable test stubs
#[allow(dead_code)]
pub(crate) trait InferenceChain {
    fn infer_variable_type(&self, var_name: &str, uri: &Url) -> Option<String>;
    fn infer_variable_type_raw(&self, var_name: &str, uri: &Url) -> Option<String>;
    fn infer_field_type(&self, file_uri: &str, field_name: &str) -> Option<String>;
    fn find_field_type_in_class(&self, class_name: &str, field_name: &str) -> Option<String>;
    fn find_fun_return_type_by_name(&self, fn_name: &str) -> Option<String>;
    fn find_method_return_type(&self, type_name: &str, method_name: &str) -> Option<String>;
    fn infer_receiver_type(&self, kind: ReceiverKind<'_>, uri: &Url) -> Option<ReceiverType>;
}

impl InferenceChain for Indexer {
    fn infer_variable_type(&self, var_name: &str, uri: &Url) -> Option<String> {
        infer_variable_type(self, var_name, uri)
    }
    fn infer_variable_type_raw(&self, var_name: &str, uri: &Url) -> Option<String> {
        infer_variable_type_raw(self, var_name, uri)
    }
    fn infer_field_type(&self, file_uri: &str, field_name: &str) -> Option<String> {
        infer_field_type(self, file_uri, field_name)
    }
    fn find_field_type_in_class(&self, class_name: &str, field_name: &str) -> Option<String> {
        find_field_type_in_class(self, class_name, field_name)
    }
    fn find_fun_return_type_by_name(&self, fn_name: &str) -> Option<String> {
        find_fun_return_type_by_name(self, fn_name)
    }
    fn find_method_return_type(&self, type_name: &str, method_name: &str) -> Option<String> {
        find_method_return_type(self, type_name, method_name)
    }
    fn infer_receiver_type(&self, kind: ReceiverKind<'_>, uri: &Url) -> Option<ReceiverType> {
        infer_receiver_type(self, kind, uri)
    }
}

// ─── Type-string helpers ──────────────────────────────────────────────────────

/// Strip generic parameters and nullability markers from a type string.
///
/// `"List<Product>"` → `"List"`, `"String?"` → `"String"`, `"Outer.Inner<T>"` → `"Outer.Inner"`
///
/// Mirrors the stripping done by [`infer_type_in_lines`](super::infer_lines::infer_type_in_lines)
/// so that `type_annotations` lookups return the same shape as line-scan results.
fn strip_generics(ty: &str) -> String {
    let stripped: String = ty
        .chars()
        .take_while(|&c| c.is_alphanumeric() || c == '_' || c == '.')
        .collect();
    stripped.trim_end_matches('.').to_owned()
}

// ─── Receiver type resolution ─────────────────────────────────────────────────

/// How the receiver expression should be resolved.
///
/// - `Variable`: a named val/var (e.g. `interactor`, `viewModel`).
///   Resolved via line-scan type annotation (`val name: Type`).
/// - `Contextual`: `it`, `this`, or a named lambda parameter.
///   Requires cursor `position` for scope analysis; falls back to
///   `infer_variable_type_raw` only if scope analysis returns nothing.
pub(crate) enum ReceiverKind<'a> {
    Variable(&'a str),
    Contextual { name: &'a str, position: Position },
}

/// A fully-normalised receiver type with multiple access forms.
///
/// All forms are derived from a single raw string (e.g. `"Outer.Inner<Param>"`):
/// - `raw`       — original with generics: `"Outer.Inner<Param>"`
/// - `qualified` — no generics, dots preserved: `"Outer.Inner"`
/// - `outer`     — first dot-segment: `"Outer"`  (used for file lookup)
/// - `leaf`      — last dot-segment: `"Inner"`   (used for fallback member lookup)
pub(crate) struct ReceiverType {
    /// Full raw type string as inferred, e.g. `"StateFlow<UiState>?"`.
    pub raw: String,
    /// Type name with no generics and no `?`, e.g. `"StateFlow"` or `"Outer.Inner"`.
    pub qualified: String,
    /// Outermost segment of `qualified`, e.g. `"Outer"`.
    pub outer: String,
    /// Innermost segment of `qualified`, e.g. `"Inner"`.
    pub leaf: String,
    /// Whether the type was annotated as nullable (`?`), e.g. `val x: User?`.
    /// Available for hover/completion display; lookup sites use `qualified`.
    #[allow(dead_code)]
    pub nullable: bool,
}

impl ReceiverType {
    pub(crate) fn from_raw(raw: String) -> Self {
        // Strip generics and outer `?` — stop at first `<` or `?`.
        let qualified: String = raw.chars().take_while(|&c| c != '<' && c != '?').collect();
        let nullable = raw.contains('?');
        let outer = qualified
            .split('.')
            .next()
            .unwrap_or(&qualified)
            .to_string();
        let leaf = qualified
            .rsplit('.')
            .next()
            .unwrap_or(&qualified)
            .to_string();
        ReceiverType {
            raw,
            qualified,
            outer,
            leaf,
            nullable,
        }
    }
}

/// Infer the type of a receiver expression and normalise it into a
/// [`ReceiverType`].
///
/// Returns `None` when type inference fails (no annotation, unindexed file,
/// or lambda scope not resolvable).  Call sites then decide whether to skip
/// or fall back; this function never performs a global rg scan.
pub(crate) fn infer_receiver_type(
    idx: &Indexer,
    kind: ReceiverKind<'_>,
    uri: &Url,
) -> Option<ReceiverType> {
    let raw = match kind {
        ReceiverKind::Variable(name) => infer_variable_type_raw(idx, name, uri)?,
        ReceiverKind::Contextual { name, position } => {
            // Lambda / implicit-receiver path.
            if let Some(ty) = idx.infer_lambda_param_type_at(name, uri, position) {
                ty
            } else {
                // Contextual fallback: ordinary annotated var that happens to
                // appear in a lambda context (e.g. captured val with explicit type).
                infer_variable_type_raw(idx, name, uri)?
            }
        }
    };
    Some(ReceiverType::from_raw(raw))
}

/// Like [`infer_receiver_type`] but checks smart-cast narrowing at the given
/// position first.  If the variable is inside a `when (var) { is Type -> }`
/// branch or an `if (var is Type)` block, returns the narrowed type.
pub(crate) fn infer_receiver_type_at(
    idx: &Indexer,
    name: &str,
    uri: &Url,
    position: Position,
) -> Option<ReceiverType> {
    // Try smart cast narrowing first when lines are available.
    let lines = idx
        .live_lines
        .get(uri.as_str())
        .map(|ll| (*ll).clone())
        .or_else(|| idx.files.get(uri.as_str()).map(|d| d.lines.clone()));
    if let Some(lines) = lines {
        if let Some(narrowed) =
            super::infer_lines::smart_cast_type_at_line(&lines, name, position.line)
        {
            return Some(ReceiverType::from_raw(narrowed));
        }
    }
    // Fallback to normal inference
    infer_receiver_type(idx, ReceiverKind::Variable(name), uri)
}

/// Scan the current file's lines for a type annotation on `var_name` and return
/// the declared type name if found.  Delegates to [`infer_type_in_lines`] and
/// falls back to method return-type inference for `val x = receiver.method(...)`.
pub(crate) fn infer_variable_type(idx: &Indexer, var_name: &str, uri: &Url) -> Option<String> {
    infer_variable_type_impl(idx, var_name, uri, 4)
}

/// Like [`infer_variable_type`] but preserves generic parameters in the returned
/// type string.  e.g. `val items: List<Product>` → `"List<Product>"`.
///
/// Used by the `it`-completion path to extract the collection element type.
pub(crate) fn infer_variable_type_raw(idx: &Indexer, var_name: &str, uri: &Url) -> Option<String> {
    infer_variable_type_raw_impl(idx, var_name, uri, 4)
}

fn infer_variable_type_impl(idx: &Indexer, var_name: &str, uri: &Url, depth: u8) -> Option<String> {
    if depth == 0 {
        return None;
    }
    // Scope block: all DashMap guards are dropped before method-return inference,
    // which may call this function recursively and must not deadlock.
    let lines = {
        if let Some(ll) = idx.live_lines.get(uri.as_str()) {
            if let result @ Some(_) = ll.infer_type(var_name) {
                return result;
            }
            (*ll).clone()
        } else if let Some(data) = idx.files.get(uri.as_str()) {
            // CST explicit type annotation — highest priority for indexed files.
            // Covers `val x: Type` and `val x: Type?` without line scanning.
            if let Some(ann) = data.type_annotations.iter().find(|(_, n, _)| n == var_name) {
                return Some(strip_generics(&ann.2));
            }
            // Line scan — fallback for constructor parameters and edge cases not
            // captured by the property_declaration CST walk (e.g. `class Foo(val x: T)`).
            if let result @ Some(_) = data.lines.infer_type(var_name) {
                return result;
            }
            // CST-indexed RHS types (unannotated properties) and method-call RHS.
            let rhs_match = data
                .rhs_types
                .iter()
                .find(|(_, n, _)| n == var_name)
                .map(|(_, _, ty)| ty.clone());
            let method_match = data
                .method_call_rhs
                .iter()
                .find(|(_, n, _, _)| n == var_name)
                .map(|(_, _, recv, method)| (recv.clone(), method.clone()));
            let field_match = data
                .field_access_rhs
                .iter()
                .find(|(_, n, _, _)| n == var_name)
                .map(|(_, _, recv, field)| (recv.clone(), field.clone()));
            let lines = data.lines.clone();
            // Drop DashMap guard before any potential recursive call.
            drop(data);
            if let Some(ty) = rhs_match {
                return Some(ty);
            }
            if let Some((recv, method)) = method_match {
                if let Some(recv_type) = infer_variable_type_impl(idx, &recv, uri, depth - 1) {
                    if let Some(ret) = find_method_return_type(idx, &recv_type, &method) {
                        return Some(ret);
                    }
                }
            }
            if let Some((recv, field)) = field_match {
                if let Some(recv_type) = infer_variable_type_impl(idx, &recv, uri, depth - 1) {
                    let recv_stripped = recv_type.split('<').next().unwrap_or(&recv_type);
                    let recv_base = recv_stripped.rsplit('.').next().unwrap_or(recv_stripped);
                    if let Some(field_type) = find_field_type_in_class(idx, recv_base, &field) {
                        return Some(field_type);
                    }
                }
            }
            // Fallback: line scan for function parameters and unindexed edge cases.
            return infer_method_return_type(idx, var_name, &lines, uri, depth - 1);
        } else {
            // File not indexed yet — read from disk; skip method inference.
            let path = uri.to_file_path().ok()?;
            let content = std::fs::read_to_string(&path).ok()?;
            let lines: Vec<String> = content.lines().map(String::from).collect();
            return lines.infer_type(var_name);
        }
    };
    // All DashMap guards are dropped here.  Safe to recurse.
    infer_method_return_type(idx, var_name, &lines, uri, depth - 1)
}

fn infer_variable_type_raw_impl(
    idx: &Indexer,
    var_name: &str,
    uri: &Url,
    depth: u8,
) -> Option<String> {
    if depth == 0 {
        return None;
    }
    let lines = {
        if let Some(ll) = idx.live_lines.get(uri.as_str()) {
            if let result @ Some(_) = ll.infer_type_raw(var_name) {
                return result;
            }
            (*ll).clone()
        } else if let Some(data) = idx.files.get(uri.as_str()) {
            // CST explicit type annotation — return verbatim (includes `?` for nullable).
            // `ReceiverType::from_raw` strips `?` from qualified/outer/leaf for lookups.
            if let Some(ann) = data.type_annotations.iter().find(|(_, n, _)| n == var_name) {
                return Some(ann.2.clone());
            }
            // Line scan — fallback for constructor parameters and edge cases not
            // captured by the property_declaration CST walk (e.g. `class Foo(val x: T)`).
            if let result @ Some(_) = data.lines.infer_type_raw(var_name) {
                return result;
            }
            let rhs_match = data
                .rhs_types
                .iter()
                .find(|(_, n, _)| n == var_name)
                .map(|(_, _, ty)| ty.clone());
            let method_match = data
                .method_call_rhs
                .iter()
                .find(|(_, n, _, _)| n == var_name)
                .map(|(_, _, recv, method)| (recv.clone(), method.clone()));
            let field_match = data
                .field_access_rhs
                .iter()
                .find(|(_, n, _, _)| n == var_name)
                .map(|(_, _, recv, field)| (recv.clone(), field.clone()));
            let lines = data.lines.clone();
            drop(data);
            if let Some(ty) = rhs_match {
                return Some(ty);
            }
            if let Some((recv, method)) = method_match {
                if let Some(recv_type) = infer_variable_type_raw_impl(idx, &recv, uri, depth - 1) {
                    if let Some(ret) = find_method_return_type(idx, &recv_type, &method) {
                        return Some(ret);
                    }
                }
            }
            if let Some((recv, field)) = field_match {
                if let Some(recv_type) = infer_variable_type_raw_impl(idx, &recv, uri, depth - 1) {
                    let recv_stripped = recv_type.split('<').next().unwrap_or(&recv_type);
                    let recv_base = recv_stripped.rsplit('.').next().unwrap_or(recv_stripped);
                    if let Some(field_type) = find_field_type_in_class(idx, recv_base, &field) {
                        return Some(field_type);
                    }
                }
            }
            return infer_method_return_type(idx, var_name, &lines, uri, depth - 1);
        } else {
            let path = uri.to_file_path().ok()?;
            let content = std::fs::read_to_string(&path).ok()?;
            let lines: Vec<String> = content.lines().map(String::from).collect();
            return lines.infer_type_raw(var_name);
        }
    };
    infer_method_return_type(idx, var_name, &lines, uri, depth - 1)
}

/// Scan a specific (possibly un-indexed) file for the declared type of `field_name`.
///
/// Checks CST type annotations first (indexed files), then falls back to line
/// scanning, then reads from disk for un-indexed files.
pub(crate) fn infer_field_type(idx: &Indexer, file_uri: &str, field_name: &str) -> Option<String> {
    let uri = tower_lsp::lsp_types::Url::parse(file_uri).ok()?;
    let file_data = ensure_file_data(idx, &uri)?;
    if let Some(ann) = file_data
        .type_annotations
        .iter()
        .find(|(_, n, _)| n == field_name)
    {
        return Some(strip_generics(&ann.2));
    }
    file_data.lines.infer_type(field_name)
}

/// Like `infer_field_type` but preserves generic parameters in the result.
///
/// Returns `"MutableList<MbAccount>"` rather than `"MutableList"`, which is
/// needed for collection element type extraction via `extract_collection_element_type`.
/// Checks live editor lines first (most up-to-date), then CST type annotations,
/// then falls back to indexed lines and finally to a disk read for un-indexed files.
pub(crate) fn infer_field_type_raw(
    idx: &Indexer,
    file_uri: &str,
    field_name: &str,
) -> Option<String> {
    if let Some(live) = idx.live_lines.get(file_uri) {
        return live.infer_type_raw(field_name);
    }
    if let Some(data) = idx.files.get(file_uri) {
        if let Some(ann) = data
            .type_annotations
            .iter()
            .find(|(_, n, _)| n == field_name)
        {
            return Some(ann.2.clone());
        }
        return data.lines.infer_type_raw(field_name);
    }
    let path = tower_lsp::lsp_types::Url::parse(file_uri)
        .ok()?
        .to_file_path()
        .ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let lines: Vec<String> = content.lines().map(String::from).collect();
    lines.infer_type_raw(field_name)
}

/// Look up the raw type of `field_name` declared inside class `class_name`,
/// resolving across files via the definitions index.
///
/// Used for multi-segment receiver chains like `result.availableBanks.map { it }`:
/// resolves `result` → `ResponseBody`, then looks up `availableBanks` in `ResponseBody`.
pub(crate) fn find_field_type_in_class(
    idx: &Indexer,
    class_name: &str,
    field_name: &str,
) -> Option<String> {
    let locs = idx.definitions.get(class_name)?;
    for loc in locs.iter() {
        if let Some(ty) = infer_field_type_raw(idx, loc.uri.as_str(), field_name) {
            return Some(ty);
        }
    }
    // Fallback: full variable inference including CST-indexed field_access_rhs
    // and method_call_rhs data (handles unannotated `val x = recv.field`).
    let locs = idx.definitions.get(class_name)?;
    for loc in locs.iter() {
        if let Some(ty) = infer_variable_type_raw(idx, field_name, &loc.uri) {
            return Some(ty);
        }
    }
    None
}

// ─── Method return-type inference ─────────────────────────────────────────────

fn infer_method_return_type(
    idx: &Indexer,
    var_name: &str,
    lines: &[String],
    uri: &Url,
    depth: u8,
) -> Option<String> {
    let mut plain_fn_candidates: Vec<String> = Vec::new();

    for line in lines {
        let rhs = match find_rhs_str(line, var_name) {
            Some(r) => r,
            None => continue,
        };

        // Match `receiver.method(` where receiver is a simple identifier.
        let paren_pos = match rhs.find('(') {
            Some(p) => p,
            None => continue,
        };
        let before_paren = &rhs[..paren_pos];
        match before_paren.rfind('.') {
            Some(dot_pos) => {
                let receiver = before_paren[..dot_pos].trim();
                let method = before_paren[dot_pos + 1..].trim();

                if receiver.is_empty() || method.is_empty() {
                    continue;
                }
                // Skip `this`/`super` and multi-segment receivers.
                if receiver == "this" || receiver == "super" || receiver.contains('.') {
                    continue;
                }
                if !method.starts_with_lowercase() {
                    continue;
                }

                // Recursively infer the receiver type (DashMap guards already dropped).
                if let Some(receiver_type) = infer_variable_type_impl(idx, receiver, uri, depth) {
                    if let Some(ret) = find_method_return_type(idx, &receiver_type, method) {
                        return Some(ret);
                    }
                }
            }
            None => {
                // Plain function call: `val result = getFoo(args)` — no dot-receiver.
                // Guard: skip when the first call is part of a chain (`getFoo(...).bar()`).
                // In that case `paren_pos` is inside the first segment only; the overall
                // expression has chaining we can't track with a single name lookup.
                let fn_name = before_paren.trim();
                if !fn_name.is_empty()
                    && fn_name.starts_with_lowercase()
                    && !has_dot_after_first_call(rhs, paren_pos)
                {
                    plain_fn_candidates.push(fn_name.to_owned());
                }
            }
        }
    }

    // Secondary pass: plain function calls whose return type is in the definitions index.
    // Handles `val result = getConnectedAccounts(isRefresh)` → look up `getConnectedAccounts`.
    for fn_name in &plain_fn_candidates {
        if let Some(ret) = find_fun_return_type_by_name(idx, fn_name) {
            return Some(ret);
        }
    }

    None
}

/// Look up `method_name` in the symbol index for `type_name` and return its
/// return type, extracted from `SymbolEntry.detail`.
/// Look up the return type of a function by name, searching across all indexed files.
///
/// Unlike `find_method_return_type` this requires no receiver type — useful when
/// the caller is a method chain expression and the receiver type is unknown.
/// Returns the raw return type string (with generics preserved), e.g. `"List<Account>"`.
pub(crate) fn find_fun_return_type_by_name(idx: &Indexer, fn_name: &str) -> Option<String> {
    let locations = idx.definitions.get(fn_name)?;
    for loc in locations.iter() {
        if let Some(file_data) = idx.files.get(loc.uri.as_str()) {
            for sym in &file_data.symbols {
                if sym.name != fn_name {
                    continue;
                }
                if !matches!(
                    sym.kind,
                    SymbolKind::FUNCTION | SymbolKind::METHOD | SymbolKind::OPERATOR
                ) {
                    continue;
                }
                if let Some(ret) = extract_return_type_from_detail(&sym.detail) {
                    return Some(ret);
                }
                let start_line = sym.selection_start() as usize;
                let full_sig = file_data.lines.collect_signature(start_line);
                if let Some(ret) = extract_return_type_from_detail(&full_sig) {
                    return Some(ret);
                }
            }
        }
    }
    None
}

pub(crate) fn find_method_return_type(
    idx: &Indexer,
    type_name: &str,
    method_name: &str,
) -> Option<String> {
    let type_base = type_name.split('.').next_back().unwrap_or(type_name);
    let locations = idx.definitions.get(type_base)?;
    for loc in locations.iter() {
        if let Some(file_data) = idx.files.get(loc.uri.as_str()) {
            // Find the class entry for type_base so we can do range containment
            // filtering — avoids picking a same-named method from an unrelated class
            // in the same file.
            let class_range = file_data
                .symbols
                .iter()
                .find(|s| s.name == type_base)
                .map(|s| s.range);

            for sym in &file_data.symbols {
                if sym.name != method_name {
                    continue;
                }
                if !matches!(
                    sym.kind,
                    SymbolKind::FUNCTION | SymbolKind::METHOD | SymbolKind::OPERATOR
                ) {
                    continue;
                }
                // When we know the class range, skip methods outside it.
                if let Some(cr) = class_range {
                    if sym.range.start.line < cr.start.line || sym.range.end.line > cr.end.line {
                        continue;
                    }
                }
                // Try detail first; fall back to source lines when detail is truncated.
                if let Some(ret) = extract_return_type_from_detail(&sym.detail) {
                    return Some(ret);
                }
                // detail may be truncated (120 char limit) — try the source lines.
                let start_line = sym.selection_start() as usize;
                let full_sig = file_data.lines.collect_signature(start_line);
                if let Some(ret) = extract_return_type_from_detail(&full_sig) {
                    return Some(ret);
                }
            }
        }
    }
    None
}

#[cfg(test)]
#[path = "infer_tests.rs"]
mod infer_tests;
