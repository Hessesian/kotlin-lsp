#[test]
fn test_lambda_after_closing_paren() {
    use tree_sitter::{Language, Parser};
    
    // Test the exact pattern from MainActivity
    let code = r#"
fun test() {
    val x = foo(
        a = 1,
        b = 2,
    ) { value -> value }
}
"#;

    let lang = Language::new(tree_sitter_kotlin::language()).unwrap();
    let mut parser = Parser::new();
    parser.set_language(&lang).unwrap();
    
    let tree = parser.parse(code.as_bytes(), None).unwrap();
    let root = tree.root_node();
    
    // Check for errors
    fn has_error(node: tree_sitter::Node) -> bool {
        if node.is_error() || node.is_missing {
            return true;
        }
        for child in node.children(&mut node.walk()) {
            if has_error(child) {
                return true;
            }
        }
        false
    }
    
    if has_error(root) {
        println!("PARSE ERROR: Tree contains ERROR or MISSING nodes");
        
        // Print all error nodes
        fn print_errors(node: tree_sitter::Node, code: &str, depth: usize) {
            if node.is_error() || node.is_missing {
                let start = node.start_byte().min(code.len());
                let end = (node.end_byte()).min(code.len()).min(start + 50);
                let text = code[start..end].replace('\n', "\\n");
                println!("  {}{} at line {}: {}", "  ".repeat(depth), node.kind(), node.start_point().row + 1, text);
            }
            for child in node.children(&mut node.walk()) {
                print_errors(child, code, depth + 1);
            }
        }
        print_errors(root, code, 0);
    } else {
        println!("OK: No parse errors");
    }
}
