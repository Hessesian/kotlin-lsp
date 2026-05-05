use tree_sitter::{Parser, Query, QueryCursor};

fn parse_swift(code: &str) -> tree_sitter::Tree {
    let lang = tree_sitter_swift_bundled::language();
    let mut parser = Parser::new();
    parser.set_language(&lang).unwrap();
    parser.parse(code, None).unwrap()
}

#[test]
fn swift_definitions_query() {
    let code = r#"
import Foundation
import UIKit

public class ViewController: UIViewController {
    private var count: Int = 0
    let name: String
    
    func viewDidLoad() {
        super.viewDidLoad()
    }
    
    public func increment(by amount: Int) -> Int {
        count += amount
        return count
    }
    
    init(name: String) {
        self.name = name
    }
}

struct Point {
    var x: Double
    var y: Double
}

protocol Drawable {
    func draw()
    var color: String { get }
}

enum Direction {
    case north, south, east, west
}

extension Point: Drawable {
    func draw() {}
    var color: String { return "red" }
}

typealias StringArray = [String]
let globalConst = 42
var globalVar = "hello"

func topLevelFunction(param: String) -> Bool {
    return param.isEmpty
}
"#;

    let tree = parse_swift(code);
    let root = tree.root_node();
    let lang = tree_sitter_swift_bundled::language();
    let bytes = code.as_bytes();

    // Combined query using declaration_kind field to distinguish
    let defs_query = r#"
; 0 — class
(class_declaration "class" name: (type_identifier) @name) @def

; 1 — struct
(class_declaration "struct" name: (type_identifier) @name) @def

; 2 — enum
(class_declaration "enum" name: (type_identifier) @name) @def

; 3 — extension (name is user_type, not type_identifier)
(class_declaration "extension" name: (user_type (type_identifier) @name)) @def

; 4 — protocol
(protocol_declaration name: (type_identifier) @name) @def

; 5 — function
(function_declaration name: (simple_identifier) @name) @def

; 6 — typealias
(typealias_declaration name: (type_identifier) @name) @def

; 7 — protocol function
(protocol_function_declaration name: (simple_identifier) @name) @def

; 8 — init
(init_declaration) @def

; 9 — property (let/var with bound identifier)
(property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @def

; 10 — protocol property
(protocol_property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @def

; 11 — enum entry
(enum_entry name: (simple_identifier) @name) @def
"#;

    let q = Query::new(&lang, defs_query).expect("Query should compile");
    let def_idx = q.capture_index_for_name("def").unwrap();
    let name_idx = q.capture_index_for_name("name").unwrap();

    let mut cur = QueryCursor::new();

    // Use BTreeMap dedup like Kotlin parser
    let mut best: std::collections::BTreeMap<(u32, u32), (usize, String, String)> =
        std::collections::BTreeMap::new();

    for m in cur.matches(&q, root, bytes) {
        let pidx = m.pattern_index;
        let mut def_text = String::new();
        let mut name_text = String::new();
        let mut name_pos = (0u32, 0u32);

        for cap in m.captures {
            if cap.index == def_idx {
                let t = cap.node.utf8_text(bytes).unwrap_or("?");
                def_text = t[..t.len().min(60)].to_string();
            } else if cap.index == name_idx {
                name_text = cap.node.utf8_text(bytes).unwrap_or("?").to_string();
                name_pos = (
                    cap.node.start_position().row as u32,
                    cap.node.start_position().column as u32,
                );
            }
        }

        if !name_text.is_empty() {
            let is_better = best
                .get(&name_pos)
                .map(|(ep, _, _)| pidx < *ep)
                .unwrap_or(true);
            if is_better {
                best.insert(name_pos, (pidx, name_text, def_text));
            }
        } else if pidx == 8 {
            // init has no @name
            println!("  [init] pattern={pidx} def='{def_text}'");
        }
    }

    let kind_names = [
        "class",
        "struct",
        "enum",
        "extension",
        "protocol",
        "func",
        "typealias",
        "proto_func",
        "init",
        "property",
        "proto_property",
        "enum_entry",
    ];

    for ((row, col), (pidx, name, def)) in &best {
        let kind = kind_names.get(*pidx).unwrap_or(&"?");
        println!("  [{kind}] {name} @ {row}:{col}  def='{def}'");
    }

    // Verify expected symbols
    let names: Vec<&str> = best.values().map(|(_, n, _)| n.as_str()).collect();
    assert!(names.contains(&"ViewController"), "missing ViewController");
    assert!(
        names.contains(&"Point"),
        "missing Point (struct or extension)"
    );
    assert!(names.contains(&"Drawable"), "missing Drawable");
    assert!(names.contains(&"Direction"), "missing Direction");
    assert!(names.contains(&"StringArray"), "missing StringArray");
    assert!(
        names.contains(&"topLevelFunction"),
        "missing topLevelFunction"
    );
    assert!(names.contains(&"globalConst"), "missing globalConst");
    assert!(names.contains(&"globalVar"), "missing globalVar");
    assert!(names.contains(&"viewDidLoad"), "missing viewDidLoad");
    assert!(names.contains(&"increment"), "missing increment");
    assert!(names.contains(&"draw"), "missing draw");
    println!("\nAll expected symbols found ✅");
}

#[test]
fn swift_import_query() {
    let code = r#"
import Foundation
import UIKit.UIViewController
import class CoreData.NSManagedObject
"#;

    let tree = parse_swift(code);
    let root = tree.root_node();
    let lang = tree_sitter_swift_bundled::language();
    let bytes = code.as_bytes();

    // Print import node structure
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "import_declaration" {
            let text = child.utf8_text(bytes).unwrap_or("?");
            println!("import_declaration: '{text}'");
            let mut c2 = child.walk();
            for gc in child.children(&mut c2) {
                let gt = gc.utf8_text(bytes).unwrap_or("?");
                println!("  {} (named={}) '{gt}'", gc.kind(), gc.is_named());
            }
        }
    }

    // Test import query
    let import_q = r#"(import_declaration (identifier) @path)"#;
    let q = Query::new(&lang, import_q).unwrap();
    let mut cur = QueryCursor::new();
    for m in cur.matches(&q, root, bytes) {
        for cap in m.captures {
            let text = cap.node.utf8_text(bytes).unwrap_or("?");
            println!("import path: '{text}'");
        }
    }
}
