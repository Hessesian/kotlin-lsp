//! Extension trait adding Kotlin/Java CST helper methods to `tree_sitter::Node`.
//!
//! These methods are lightweight convenience wrappers around tree-sitter node
//! traversal; their bodies were extracted from the free functions they replace.
use tree_sitter::Node;
use crate::StrExt;
use crate::queries::{
    KIND_SIMPLE_IDENT, KIND_TYPE_IDENT, KIND_VALUE_ARG, KIND_VALUE_ARGS,
    KIND_LAMBDA_PARAMS,
};

pub(crate) trait NodeExt<'a>: Sized + Copy {
    /// Extract the node's text as an owned `String`.  Returns `None` if the bytes
    /// are not valid UTF-8 (should never happen in practice for Kotlin/Java source).
    fn utf8_text_owned(self, bytes: &[u8]) -> Option<String>;

    /// Find the first direct child whose `kind()` equals `kind`.
    fn first_child_of_kind(self, kind: &str) -> Option<Node<'a>>;

    /// Collect all direct children whose `kind()` equals `kind`.
    /// Allocates a `Vec`; child counts are typically small (< 20), so this is acceptable
    /// for indexing paths. For very hot loops, prefer `for_each_child_of_kind`.
    fn children_of_kind(self, kind: &str) -> Vec<Node<'a>>;

    /// Call `f` for each direct child whose `kind()` equals `kind`.
    /// Non-allocating alternative to `children_of_kind` for performance-sensitive code.
    fn for_each_child_of_kind(self, kind: &str, f: impl FnMut(Node<'a>));

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

    /// For a `call_expression` node, returns `(fn_name, qualifier)`.
    /// - Simple call `foo(...)` → `("foo", None)`
    /// - Navigation call `obj.bar(...)` → `("bar", Some("obj"))`
    /// - Returns `None` if the callee kind is not recognized.
    fn call_fn_and_qualifier(self, bytes: &[u8]) -> Option<(String, Option<String>)>;

    /// Extract the user-type name from a `user_type` node (Kotlin/Java).
    fn user_type_name(self, bytes: &[u8]) -> Option<String>;

    /// Extract the first type name from a Java type node.
    fn java_first_type_name(self, bytes: &[u8]) -> Option<String>;

    /// Returns the line number (0-based) of the first named identifier child,
    /// or the node's own start line if no named child is found.
    fn name_line(self) -> u32;
}

impl<'a> NodeExt<'a> for Node<'a> {
    fn utf8_text_owned(self, bytes: &[u8]) -> Option<String> {
        self.utf8_text(bytes).ok().map(|s| s.to_owned())
    }

    fn first_child_of_kind(self, kind: &str) -> Option<Node<'a>> {
        (0..self.child_count())
            .filter_map(|i| self.child(i))
            .find(|c| c.kind() == kind)
    }

    fn children_of_kind(self, kind: &str) -> Vec<Node<'a>> {
        (0..self.child_count())
            .filter_map(|i| self.child(i))
            .filter(|c| c.kind() == kind)
            .collect()
    }

    fn for_each_child_of_kind(self, kind: &str, mut f: impl FnMut(Node<'a>)) {
        for i in 0..self.child_count() {
            if let Some(c) = self.child(i) {
                if c.kind() == kind { f(c); }
            }
        }
    }

    fn call_fn_name(self, bytes: &[u8]) -> Option<String> {
        self.call_fn_and_qualifier(bytes).map(|(name, _)| name)
    }

    fn named_arg_label(self, bytes: &[u8]) -> Option<String> {
        let count = self.child_count();
        for i in 0..count.saturating_sub(1) {
            let (c, next) = (self.child(i)?, self.child(i + 1)?);
            if c.kind() == KIND_SIMPLE_IDENT && next.kind() == "=" {
                return c.utf8_text_owned(bytes);
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
            if child.kind() == KIND_VALUE_ARG {
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
            if child.kind() == KIND_VALUE_ARGS {
                return Some(child);
            }
            if child.kind() == "call_suffix" {
                let mut w2 = child.walk();
                for gc in child.children(&mut w2) {
                    if gc.kind() == KIND_VALUE_ARGS {
                        return Some(gc);
                    }
                }
            }
        }
        None
    }

    fn has_lambda_named_params(self, bytes: &[u8]) -> bool {
        let Some(lp) = self.first_child_of_kind(KIND_LAMBDA_PARAMS)
        else {
            return false;
        };

        (0..lp.child_count())
            .filter_map(|i| lp.child(i))
            .filter(|c| c.kind() == "variable_declaration")
            .any(|vd| {
                let Some(si) = vd.child(0).filter(|n| n.kind() == KIND_SIMPLE_IDENT) else {
                    return false;
                };
                let Ok(name) = std::str::from_utf8(&bytes[si.byte_range()]) else {
                    return false;
                };
                name != "it" && name != "_"
            })
    }

    fn collect_lambda_param_names(self, bytes: &[u8], existing: &[String]) -> Vec<String> {
        let Some(lp) = self.first_child_of_kind(KIND_LAMBDA_PARAMS)
        else {
            return Vec::new();
        };

        (0..lp.child_count())
            .filter_map(|i| lp.child(i))
            .filter(|c| c.kind() == "variable_declaration")
            .filter_map(|vd| {
                let si = vd.child(0).filter(|n| n.kind() == KIND_SIMPLE_IDENT)?;
                si.utf8_text_owned(bytes)
            })
            .filter(|name| {
                name != "it"
                    && name != "_"
                    && name.starts_with_lowercase()
                    && !existing.contains(name)
            })
            .collect()
    }

    fn extract_type_name(self, bytes: &[u8]) -> Option<String> {
        if let Some(n) = self.child_by_field_name("name") {
            if let Some(s) = n.utf8_text_owned(bytes) {
                if s.starts_with_uppercase() {
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
                    if let Some(s) = child.utf8_text_owned(bytes) {
                        if s.starts_with_uppercase() {
                            return Some(s);
                        }
                    }
                }
            }
        }
        None
    }

    fn call_fn_and_qualifier(self, bytes: &[u8]) -> Option<(String, Option<String>)> {
        let callee = self.child(0)?;
        match callee.kind() {
            "simple_identifier" | "type_identifier" => {
                let name = callee.utf8_text_owned(bytes)?;
                Some((name, None))
            }
            "navigation_expression" => {
                let mut walker = callee.walk();
                let mut qualifier_opt: Option<String> = None;
                let mut fn_name_opt: Option<String> = None;
                for child in callee.children(&mut walker) {
                    match child.kind() {
                        "simple_identifier" | "type_identifier" => {
                            qualifier_opt = child.utf8_text_owned(bytes);
                        }
                        "navigation_suffix" => {
                            fn_name_opt = (0..child.child_count())
                                .filter_map(|i| child.child(i))
                                .find(|c| c.kind() == KIND_SIMPLE_IDENT || c.kind() == KIND_TYPE_IDENT)
                                .and_then(|c| c.utf8_text_owned(bytes));
                        }
                        _ => {}
                    }
                }
                Some((fn_name_opt?, qualifier_opt))
            }
            _ => None,
        }
    }

    fn user_type_name(self, bytes: &[u8]) -> Option<String> {
        let mut segments = Vec::new();
        collect_user_type_segments(self, bytes, &mut segments);
        if segments.is_empty() { None } else { Some(segments.join(".")) }
    }

    fn java_first_type_name(self, bytes: &[u8]) -> Option<String> {
        let mut stack = vec![self];
        while let Some(n) = stack.pop() {
            match n.kind() {
                "type_identifier" => {
                    return n.utf8_text_owned(bytes);
                }
                "scoped_type_identifier" => {
                    let text = n.utf8_text(bytes).ok()?;
                    let name = text.split('<').next().unwrap_or(text).trim();
                    return if name.is_empty() { None } else { Some(name.to_owned()) };
                }
                "type_arguments" => continue,
                _ => {}
            }
            let mut cur = n.walk();
            for child in n.children(&mut cur) {
                if child.is_named() { stack.push(child); }
            }
        }
        None
    }

    fn name_line(self) -> u32 {
        // Java uses field "name"; Kotlin has type_identifier as a direct child.
        if let Some(n) = self.child_by_field_name("name") {
            return n.start_position().row as u32;
        }
        let mut cur = self.walk();
        for child in self.children(&mut cur) {
            if matches!(child.kind(), "type_identifier" | "simple_identifier" | "identifier") {
                return child.start_position().row as u32;
            }
        }
        self.start_position().row as u32
    }
}

// ─── Private helpers ──────────────────────────────────────────────────────────

fn collect_user_type_segments(node: Node<'_>, bytes: &[u8], segments: &mut Vec<String>) {
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        match child.kind() {
            "type_arguments" => {}  // skip generic parameters entirely
            "simple_identifier" | "type_identifier" | "identifier" => {
                if let Ok(text) = child.utf8_text(bytes) {
                    let text = text.trim();
                    if !text.is_empty() { segments.push(text.to_owned()); }
                }
            }
            _ if child.is_named() => collect_user_type_segments(child, bytes, segments),
            _ => {}
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::NodeExt;
    use crate::queries::{KIND_CALL_EXPR, KIND_VALUE_ARGS, KIND_VALUE_ARG, KIND_LAMBDA_LIT};

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
        let call = find_node_kind(tree.root_node(), KIND_CALL_EXPR).unwrap();
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
        let call = find_node_kind(tree.root_node(), KIND_CALL_EXPR).unwrap();
        assert_eq!(call.call_fn_name(&bytes), Some("bar".to_string()));
    }

    #[test]
    fn value_arg_position_first_and_second() {
        let (tree, bytes) = parse_kotlin("foo(a, b)");
        let _ = bytes; // not needed for position
        let call = find_node_kind(tree.root_node(), KIND_CALL_EXPR).unwrap();
        let value_args_node = find_node_kind(call, KIND_VALUE_ARGS).unwrap();
        let mut args = vec![];
        for i in 0..value_args_node.child_count() {
            if let Some(c) = value_args_node.child(i) {
                if c.kind() == KIND_VALUE_ARG {
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
        let lambda = find_node_kind(tree.root_node(), KIND_LAMBDA_LIT).unwrap();
        assert!(
            lambda.has_lambda_named_params(&bytes),
            "param named `item` should yield true"
        );
    }

    #[test]
    fn has_lambda_named_params_false_for_no_params() {
        let (tree, bytes) = parse_kotlin("val x = items.map { it.name }");
        let lambda = find_node_kind(tree.root_node(), KIND_LAMBDA_LIT).unwrap();
        assert!(
            !lambda.has_lambda_named_params(&bytes),
            "no lambda_parameters child should yield false"
        );
    }

    #[test]
    fn collect_lambda_param_names_collects_named() {
        let (tree, bytes) = parse_kotlin("val x = items.map { item -> item.foo }");
        let lambda = find_node_kind(tree.root_node(), KIND_LAMBDA_LIT).unwrap();
        let names = lambda.collect_lambda_param_names(&bytes, &[]);
        assert_eq!(names, vec!["item".to_string()]);
    }

    #[test]
    fn named_arg_label_present() {
        let (tree, bytes) = parse_kotlin("foo(bar = 1)");
        let call = find_node_kind(tree.root_node(), KIND_CALL_EXPR).unwrap();
        let va = find_node_kind(call, KIND_VALUE_ARG).unwrap();
        assert_eq!(va.named_arg_label(&bytes), Some("bar".to_string()));
    }

    #[test]
    fn named_arg_label_absent() {
        let (tree, bytes) = parse_kotlin("foo(1)");
        let call = find_node_kind(tree.root_node(), KIND_CALL_EXPR).unwrap();
        let va = find_node_kind(call, KIND_VALUE_ARG).unwrap();
        assert_eq!(va.named_arg_label(&bytes), None);
    }

    #[test]
    fn call_fn_and_qualifier_simple_call() {
        let (tree, bytes) = parse_kotlin("val x = foo(1)");
        let call = find_node_kind(tree.root_node(), KIND_CALL_EXPR).unwrap();
        assert_eq!(call.call_fn_and_qualifier(&bytes), Some(("foo".to_string(), None)));
    }

    #[test]
    fn call_fn_and_qualifier_navigation_call() {
        let (tree, bytes) = parse_kotlin("val x = obj.bar(1)");
        let call = find_node_kind(tree.root_node(), KIND_CALL_EXPR).unwrap();
        assert_eq!(
            call.call_fn_and_qualifier(&bytes),
            Some(("bar".to_string(), Some("obj".to_string())))
        );
    }

    #[test]
    fn call_fn_name_delegates_to_and_qualifier() {
        // call_fn_name is now implemented via call_fn_and_qualifier —
        // verify both return the same name for navigation and simple calls.
        let (tree, bytes) = parse_kotlin("val x = obj.bar(1)");
        let call = find_node_kind(tree.root_node(), KIND_CALL_EXPR).unwrap();
        let via_and_qualifier = call.call_fn_and_qualifier(&bytes).map(|(n, _)| n);
        let via_name = call.call_fn_name(&bytes);
        assert_eq!(via_name, via_and_qualifier);
        assert_eq!(via_name, Some("bar".to_string()));
    }
}
