use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use super::Backend;
use crate::resolver::{ReceiverKind, infer_receiver_type};

fn locs_to_response(locs: Vec<Location>) -> GotoDefinitionResponse {
    match locs.len() {
        1 => GotoDefinitionResponse::Scalar(locs.into_iter().next().unwrap()),
        _ => GotoDefinitionResponse::Array(locs),
    }
}

impl Backend {
    pub(super) async fn goto_definition_impl(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pp       = params.text_document_position_params;
        let uri      = &pp.text_document.uri;
        let position = pp.position;

        let Some((word, qualifier)) = self.indexer.word_and_qualifier_at(uri, position) else {
            return Ok(None);
        };

        // Special case: `this` keyword — navigate to the enclosing class definition.
        if qualifier.is_none() && word == "this" {
            if let Some(class_name) = self.indexer.enclosing_class_at(uri, position.line) {
                let locs = self.indexer.find_definition_qualified(&class_name, None, uri);
                if !locs.is_empty() {
                    return Ok(Some(locs_to_response(locs)));
                }
            }
            return Ok(None);
        }

        // Special case: `super` keyword — navigate to the enclosing class's first supertype.
        if qualifier.is_none() && word == "super" {
            if let Some(result) = self.goto_super_class(uri, position.line).await {
                return Ok(Some(result));
            }
            return Ok(None);
        }

        // Special case: `super.method(...)` — resolve `method` in the parent class.
        if qualifier.as_deref() == Some("super") {
            if let Some(result) = self.goto_super_method(uri, position.line, &word).await {
                return Ok(Some(result));
            }
            return Ok(None);
        }

        // Special case: `it` or a named lambda parameter — resolve to the
        // inferred element/receiver type class instead of trying a text search.
        if qualifier.is_none() && (word == "it" || word.chars().next().map(|c| c.is_lowercase()).unwrap_or(true)) {
            if let Some(type_name) = self.indexer.infer_lambda_param_type_at(&word, uri, position) {
                // For qualified names (e.g. `Outer.Inner`) try the full name first,
                // then fall back to the last segment which is what the index stores.
                let lookup = type_name.rsplit('.').next().unwrap_or(&type_name);
                let locs = self.indexer.find_definition_qualified(lookup, None, uri);
                if !locs.is_empty() {
                    return Ok(match locs.len() {
                        1 => Some(GotoDefinitionResponse::Scalar(locs.into_iter().next().unwrap())),
                        _ => Some(GotoDefinitionResponse::Array(locs)),
                    });
                }
            }
            // If the word is a lambda parameter (type resolution failed), jump to
            // the `{ name ->` declaration line in the current file.
            let lambda_params = self.indexer.lambda_params_at_col(uri, position.line as usize, position.character as usize);
            if lambda_params.contains(&word) {
                if let Some(loc) = self.indexer.find_lambda_param_decl(uri, &word, position.line as usize) {
                    return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
                }
                return Ok(None);
            }
        }

        // `this.field` / `it.field` — resolve the implicit receiver/lambda type and
        // use it as the real qualifier so definition lookup finds the correct file.
        if let Some(qual) = qualifier.as_deref() {
            if qual == "this" || qual == "it" {
                let kind = ReceiverKind::Contextual { name: qual, position };
                if let Some(rt) = infer_receiver_type(&self.indexer, kind, uri) {
                    // Try full qualified type first (e.g. `Outer.Inner`), then leaf segment.
                    let locs = self.indexer.find_definition_qualified(&word, Some(&rt.qualified), uri);
                    let locs = if locs.is_empty() && rt.leaf != rt.qualified {
                        self.indexer.find_definition_qualified(&word, Some(&rt.leaf), uri)
                    } else { locs };
                    if !locs.is_empty() {
                        return Ok(Some(locs_to_response(locs)));
                    }
                }
            }
        }

        let locs = self.indexer.find_definition_qualified(&word, qualifier.as_deref(), uri);
        if !locs.is_empty() {
            return Ok(match locs.len() {
                1 => Some(GotoDefinitionResponse::Scalar(locs.into_iter().next().unwrap())),
                _ => Some(GotoDefinitionResponse::Array(locs)),
            });
        }

        // Index miss (symbol not indexed or indexing in progress) → rg fallback.
        // Use effective_rg_root so searches use the open file's project root
        // when workspace_root points to a different project (e.g. android vs ios).
        let file_path = uri.to_file_path().ok();
        let (root_opt, matcher) = {
            let wr = self.indexer.workspace_root.read().unwrap().clone();
            let m = self.indexer.ignore_matcher.read().unwrap().clone();
            (crate::rg::effective_rg_root(wr.as_deref(), file_path.as_deref()), m)
        };
        let name_clone = word.clone();
        let rg_locs = tokio::task::spawn_blocking(move || {
            crate::rg::rg_find_definition(&name_clone, root_opt.as_deref(), matcher.as_deref())
        }).await.unwrap_or_default();
        Ok(match rg_locs.len() {
            0 => None,
            1 => Some(GotoDefinitionResponse::Scalar(rg_locs.into_iter().next().unwrap())),
            _ => Some(GotoDefinitionResponse::Array(rg_locs)),
        })
    }

    pub(super) async fn goto_implementation_impl(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pp       = params.text_document_position_params;
        let uri      = &pp.text_document.uri;
        let position = pp.position;

        let Some((word, _qualifier)) = self.indexer.word_and_qualifier_at(uri, position) else {
            return Ok(None);
        };

        // Direct subtypes from the index.
        let mut locs: Vec<Location> = self.indexer.subtypes
            .get(&word)
            .map(|v| v.clone())
            .unwrap_or_default();

        // If index is empty for this symbol (cold start), try rg-based heuristic
        // to find implementors quickly to avoid client timeouts in large projects.
        if locs.is_empty() {
            let file_path = uri.to_file_path().ok();
            let (root_opt, matcher) = {
                let wr = self.indexer.workspace_root.read().unwrap().clone();
                let m = self.indexer.ignore_matcher.read().unwrap().clone();
                (crate::rg::effective_rg_root(wr.as_deref(), file_path.as_deref()), m)
            };
            let word_clone = word.clone();
            let rg_impls = tokio::task::spawn_blocking(move || {
                crate::rg::rg_find_implementors(&word_clone, root_opt.as_deref(), matcher.as_deref())
            }).await.unwrap_or_default();
            if !rg_impls.is_empty() {
                // Return early with rg results.
                return Ok(match rg_impls.len() {
                    1 => Some(GotoDefinitionResponse::Scalar(rg_impls.into_iter().next().unwrap())),
                    _ => Some(GotoDefinitionResponse::Array(rg_impls)),
                });
            }
        }

        // Also collect transitive subtypes (BFS, depth-limited).
        let mut queue: Vec<String> = locs.iter()
            .filter_map(|loc| {
                let data = self.indexer.files.get(loc.uri.as_str())?;
                data.symbols.iter()
                    .find(|s| s.selection_range == loc.range)
                    .map(|s| s.name.clone())
            })
            .collect();
        let mut visited = vec![word.clone()];
        while let Some(name) = queue.pop() {
            if visited.contains(&name) { continue; }
            visited.push(name.clone());
            if let Some(sub_locs) = self.indexer.subtypes.get(&name) {
                for loc in sub_locs.iter() {
                    if !locs.iter().any(|l| l.uri == loc.uri && l.range == loc.range) {
                        locs.push(loc.clone());
                        if let Some(data) = self.indexer.files.get(loc.uri.as_str()) {
                            if let Some(sym) = data.symbols.iter().find(|s| s.selection_range == loc.range) {
                                queue.push(sym.name.clone());
                            }
                        }
                    }
                }
            }
        }

        Ok(match locs.len() {
            0 => None,
            1 => Some(GotoDefinitionResponse::Scalar(locs.into_iter().next().unwrap())),
            _ => Some(GotoDefinitionResponse::Array(locs)),
        })
    }

    /// Collect the parent class names for the class enclosing `row` in `uri`.
    pub(super) fn super_names_at(&self, uri: &Url, row: u32) -> Vec<String> {
        let class_name = match self.indexer.enclosing_class_at(uri, row) {
            Some(n) => n,
            None => return vec![],
        };
        let locs = self.indexer.definitions
            .get(&class_name)
            .map(|v| v.clone())
            .unwrap_or_default();
        for loc in &locs {
            if let Some(file) = self.indexer.files.get(loc.uri.as_str()) {
                let names: Vec<String> = file.supers.iter()
                    .filter(|(l, _)| *l == loc.range.start.line)
                    .map(|(_, n)| n.clone())
                    .collect();
                if !names.is_empty() { return names; }
            }
        }
        // Fallback: parse live_lines for the open file itself.
        if let Some(lines) = self.indexer.live_lines.get(uri.as_str()) {
            let content = lines.join("\n");
            let names: Vec<String> = crate::parser::parse_by_extension(uri.path(), &content)
                .supers.into_iter().map(|(_, n)| n).collect();
            if !names.is_empty() { return names; }
        }
        vec![]
    }

    pub(super) async fn rg_resolve(&self, uri: &Url, name: &str) -> Vec<Location> {
        let name_clone = name.to_string();
        let file_path = uri.to_file_path().ok();
        let (root_opt, matcher) = {
            let wr = self.indexer.workspace_root.read().unwrap().clone();
            let m = self.indexer.ignore_matcher.read().unwrap().clone();
            (crate::rg::effective_rg_root(wr.as_deref(), file_path.as_deref()), m)
        };
        tokio::task::spawn_blocking(move || {
            crate::rg::rg_find_definition(&name_clone, root_opt.as_deref(), matcher.as_deref())
        }).await.unwrap_or_default()
    }

    pub(super) async fn goto_super_class(&self, uri: &Url, row: u32) -> Option<GotoDefinitionResponse> {
        for super_name in &self.super_names_at(uri, row) {
            let locs = self.indexer.find_definition_qualified(super_name, None, uri);
            if !locs.is_empty() {
                return Some(locs_to_response(locs));
            }
            let rg_locs = self.rg_resolve(uri, super_name).await;
            if !rg_locs.is_empty() {
                return Some(locs_to_response(rg_locs));
            }
        }
        None
    }

    pub(super) async fn goto_super_method(&self, uri: &Url, row: u32, method: &str) -> Option<GotoDefinitionResponse> {
        // resolve_qualified already handles root=="super" via resolve_from_class_hierarchy.
        let locs = self.indexer.find_definition_qualified(method, Some("super"), uri);
        if !locs.is_empty() {
            return Some(locs_to_response(locs));
        }
        // Method not found in indexed hierarchy (e.g. Android SDK parent).
        // Fall back to navigating to the parent class itself.
        self.goto_super_class(uri, row).await
    }
}
