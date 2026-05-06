use super::NodeExt;
use crate::queries::{KIND_CALL_EXPR, KIND_LAMBDA_LIT, KIND_VALUE_ARG, KIND_VALUE_ARGS};

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
    assert_eq!(
        args[0].value_arg_position(),
        0,
        "first arg position should be 0"
    );
    assert_eq!(
        args[1].value_arg_position(),
        1,
        "second arg position should be 1"
    );
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
    assert_eq!(
        call.call_fn_and_qualifier(&bytes),
        Some(("foo".to_string(), None))
    );
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
