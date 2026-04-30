//! Extension trait adding Kotlin/Java CST helper methods to `tree_sitter::Node`.
//!
//! These methods are lightweight convenience wrappers around tree-sitter node
//! traversal; their bodies were extracted from the free functions they replace.
use tree_sitter::Node;

pub(crate) trait NodeExt<'a>: Sized + Copy {
    /// Extract the function name from a `call_expression` node.
    /// Handles simple calls `foo(...)` and navigation chains `foo.bar(...)`.
    fn call_fn_name(self, bytes: &[u8]) -> Option<String>;

    /// If `value_argument` has a named-arg label (`simple_identifier "="` prefix),
    /// return the label text; otherwise `None`.
    fn named_arg_label(self, bytes: &[u8]) -> Option<String>;

    /// Count how many `value_argument` siblings precede `self` in its parent.
    fn value_arg_position(self) -> usize;

    /// Find the `value_arguments` node within a `call_expression`, searching
    /// through the optional `call_suffix` intermediate node.
    fn find_value_arguments(self) -> Option<Node<'a>>;

    /// Returns `true` if `self` (a `lambda_literal` CST node) has a
    /// `lambda_parameters` child containing at least one named parameter
    /// that is neither `it` nor `_`.
    fn has_lambda_named_params(self, bytes: &[u8]) -> bool;

    /// Collect named lambda parameter identifiers from a `lambda_literal` CST node.
    /// Skips `it`, `_`, uppercase-first (type refs), and deduplicates against `existing`.
    fn collect_lambda_param_names(self, bytes: &[u8], existing: &[String]) -> Vec<String>;

    /// Extract the type/class name from a CST class/interface/object/companion_object node.
    fn extract_type_name(self, bytes: &[u8]) -> Option<String>;
}

impl<'a> NodeExt<'a> for Node<'a> {
    fn call_fn_name(self, bytes: &[u8]) -> Option<String> {
        let callee = self.child(0)?;
        let name_node = match callee.kind() {
            "simple_identifier" | "type_identifier" => callee,
            "navigation_expression" => {
                // Single-pass scan: track the last direct identifier and the last
                // identifier inside a navigation_suffix. Prefer the suffix identifier
                // (member name, e.g. "bar" in `obj.bar(…)`); fall back to the direct
                // identifier for bare qualified names with no suffix.
                let mut walker = callee.walk();
                let mut last_identifier = None;
                let mut last_suffix_identifier = None;
                for child in callee.children(&mut walker) {
                    match child.kind() {
                        "simple_identifier" | "type_identifier" => {
                            last_identifier = Some(child);
                        }
                        "navigation_suffix" => {
                            let suffix_id = (0..child.child_count())
                                .filter_map(|i| child.child(i))
                                .find(|c| {
                                    c.kind() == "simple_identifier"
                                        || c.kind() == "type_identifier"
                                });
                            if let Some(id) = suffix_id {
                                last_suffix_identifier = Some(id);
                            }
                        }
                        _ => {}
                    }
                }
                last_suffix_identifier.or(last_identifier)?
            }
            _ => return None,
        };
        std::str::from_utf8(&bytes[name_node.byte_range()])
            .ok()
            .map(|s| s.to_string())
    }

    fn named_arg_label(self, bytes: &[u8]) -> Option<String> {
        let count = self.child_count();
        for i in 0..count.saturating_sub(1) {
            let (c, next) = (self.child(i)?, self.child(i + 1)?);
            if c.kind() == "simple_identifier" && next.kind() == "=" {
                return std::str::from_utf8(&bytes[c.byte_range()])
                    .ok()
                    .map(|s| s.to_string());
            }
        }
        None
    }

    fn value_arg_position(self) -> usize {
        let parent = match self.parent() {
            Some(p) => p,
            None => return 0,
        };
        let target_id = self.id();
        let mut pos = 0usize;
        let mut cursor = parent.walk();
        for child in parent.children(&mut cursor) {
            if child.kind() == "value_argument" {
                if child.id() == target_id {
                    break;
                }
                pos += 1;
            }
        }
        pos
    }

    fn find_value_arguments(self) -> Option<Node<'a>> {
        let mut walker = self.walk();
        for child in self.children(&mut walker) {
            if child.kind() == "value_arguments" {
                return Some(child);
            }
            if child.kind() == "call_suffix" {
                let mut w2 = child.walk();
                for gc in child.children(&mut w2) {
                    if gc.kind() == "value_arguments" {
                        return Some(gc);
                    }
                }
            }
        }
        None
    }

    fn has_lambda_named_params(self, bytes: &[u8]) -> bool {
        let Some(lp) = (0..self.child_count())
            .filter_map(|i| self.child(i))
            .find(|c| c.kind() == "lambda_parameters")
        else {
            return false;
        };

        (0..lp.child_count())
            .filter_map(|i| lp.child(i))
            .filter(|c| c.kind() == "variable_declaration")
            .any(|vd| {
                let Some(si) = vd.child(0).filter(|n| n.kind() == "simple_identifier") else {
                    return false;
                };
                let Ok(name) = std::str::from_utf8(&bytes[si.byte_range()]) else {
                    return false;
                };
                name != "it" && name != "_"
            })
    }

    fn collect_lambda_param_names(self, bytes: &[u8], existing: &[String]) -> Vec<String> {
        let Some(lp) = (0..self.child_count())
            .filter_map(|i| self.child(i))
            .find(|c| c.kind() == "lambda_parameters")
        else {
            return Vec::new();
        };

        (0..lp.child_count())
            .filter_map(|i| lp.child(i))
            .filter(|c| c.kind() == "variable_declaration")
            .filter_map(|vd| {
                let si = vd.child(0).filter(|n| n.kind() == "simple_identifier")?;
                std::str::from_utf8(&bytes[si.byte_range()])
                    .ok()
                    .map(|s| s.to_string())
            })
            .filter(|name| {
                name != "it"
                    && name != "_"
                    && name.chars().next().map(|c| c.is_lowercase()).unwrap_or(false)
                    && !existing.contains(name)
            })
            .collect()
    }

    fn extract_type_name(self, bytes: &[u8]) -> Option<String> {
        if let Some(n) = self.child_by_field_name("name") {
            if let Ok(s) = std::str::from_utf8(&bytes[n.byte_range()]) {
                let s = s.to_string();
                if s.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                    return Some(s);
                }
            }
        }
        for i in 0..self.child_count() {
            if let Some(child) = self.child(i) {
                if matches!(
                    child.kind(),
                    "type_identifier" | "simple_identifier" | "identifier"
                ) {
                    if let Ok(s) = std::str::from_utf8(&bytes[child.byte_range()]) {
                        if s.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                            return Some(s.to_string());
                        }
                    }
                }
            }
        }
        None
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::NodeExt;

    fn parse_kotlin(src: &str) -> (tree_sitter::Tree, Vec<u8>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin::language())
            .unwrap();
        let bytes = src.as_bytes().to_vec();
        let tree = parser.parse(src, None).unwrap();
        (tree, bytes)
    }

    fn find_node_kind<'a>(
        node: tree_sitter::Node<'a>,
        kind: &str,
    ) -> Option<tree_sitter::Node<'a>> {
        if node.kind() == kind {
            return Some(node);
        }
        for i in 0..node.child_count() {
            if let Some(n) = node.child(i).and_then(|c| find_node_kind(c, kind)) {
                return Some(n);
            }
        }
        None
    }

    #[test]
    fn call_fn_name_simple() {
        let (tree, bytes) = parse_kotlin("val x = foo(1)");
        let call = find_node_kind(tree.root_node(), "call_expression").unwrap();
        assert_eq!(call.call_fn_name(&bytes), Some("foo".to_string()));
    }

    #[test]
    fn call_fn_name_navigation() {
        // In tree-sitter-kotlin, `obj.bar` is:
        //   navigation_expression
        //     simple_identifier: obj
        //     navigation_suffix: .bar  ← `bar` is nested here
        // `call_fn_name` should return the member name "bar", not the receiver "obj".
        let (tree, bytes) = parse_kotlin("val x = obj.bar(1)");
        let call = find_node_kind(tree.root_node(), "call_expression").unwrap();
        assert_eq!(call.call_fn_name(&bytes), Some("bar".to_string()));
    }

    #[test]
    fn value_arg_position_first_and_second() {
        let (tree, bytes) = parse_kotlin("foo(a, b)");
        let _ = bytes; // not needed for position
        let call = find_node_kind(tree.root_node(), "call_expression").unwrap();
        let value_args_node = find_node_kind(call, "value_arguments").unwrap();
        let mut args = vec![];
        for i in 0..value_args_node.child_count() {
            if let Some(c) = value_args_node.child(i) {
                if c.kind() == "value_argument" {
                    args.push(c);
                }
            }
        }
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].value_arg_position(), 0, "first arg position should be 0");
        assert_eq!(args[1].value_arg_position(), 1, "second arg position should be 1");
    }

    #[test]
    fn has_lambda_named_params_true_for_named() {
        let (tree, bytes) = parse_kotlin("val x = foo { item -> item }");
        let lambda = find_node_kind(tree.root_node(), "lambda_literal").unwrap();
        assert!(
            lambda.has_lambda_named_params(&bytes),
            "param named `item` should yield true"
        );
    }

    #[test]
    fn has_lambda_named_params_false_for_no_params() {
        let (tree, bytes) = parse_kotlin("val x = items.map { it.name }");
        let lambda = find_node_kind(tree.root_node(), "lambda_literal").unwrap();
        assert!(
            !lambda.has_lambda_named_params(&bytes),
            "no lambda_parameters child should yield false"
        );
    }

    #[test]
    fn collect_lambda_param_names_collects_named() {
        let (tree, bytes) = parse_kotlin("val x = items.map { item -> item.foo }");
        let lambda = find_node_kind(tree.root_node(), "lambda_literal").unwrap();
        let names = lambda.collect_lambda_param_names(&bytes, &[]);
        assert_eq!(names, vec!["item".to_string()]);
    }

    #[test]
    fn named_arg_label_present() {
        let (tree, bytes) = parse_kotlin("foo(bar = 1)");
        let call = find_node_kind(tree.root_node(), "call_expression").unwrap();
        let va = find_node_kind(call, "value_argument").unwrap();
        assert_eq!(va.named_arg_label(&bytes), Some("bar".to_string()));
    }

    #[test]
    fn named_arg_label_absent() {
        let (tree, bytes) = parse_kotlin("foo(1)");
        let call = find_node_kind(tree.root_node(), "call_expression").unwrap();
        let va = find_node_kind(call, "value_argument").unwrap();
        assert_eq!(va.named_arg_label(&bytes), None);
    }
}
