//! Tree-sitter S-expression queries for Kotlin.
//!
//! Grammar facts (tree-sitter-kotlin 0.3, confirmed by probing the parse tree):
//!
//! • No field names on children — `child_by_field_name("name")` always returns None.
//! • `class`, `interface`, `data class`, `sealed class`, `enum class` all parse as
//!   `class_declaration`.  The keyword (`"class"`, `"interface"`, `"enum"`) is an
//!   anonymous node child, which query patterns can match literally.
//! • `object` → `object_declaration` with `type_identifier` (not `simple_identifier`).
//! • `companion object` → `companion_object` node (named child of `class_body`).
//! • `val`/`var` keywords live inside a `binding_pattern_kind` named node.
//! • Function names are `simple_identifier`; class/type names are `type_identifier`.
//! • `identifier` (dotted path) in imports/packages: `utf8_text()` gives full text.
//! • Top-level scope is `source_file`.

// ────────────────────────────────────────────────────────────────────────────
// DEFINITIONS QUERY
//
// One combined query; patterns are ordered and their indices map to KOTLIN_DEF_KINDS.
// Every pattern emits exactly two captures:
//   @def  — the full declaration node  (→ SymbolEntry::range)
//   @name — the identifier node        (→ SymbolEntry::selection_range + text)
// ────────────────────────────────────────────────────────────────────────────

/// Pattern indices → SymbolKind mapping lives in `parser.rs`.
pub const KOTLIN_DEFINITIONS: &str = r#"
; 0 — enum class  MUST be before plain "class" pattern (both have "class" keyword).
;     enum_class_body is unique to enum classes — no ambiguity.
(class_declaration
  (type_identifier) @name
  (enum_class_body)) @def

; 1 — data class  MUST be before plain "class" (subset of pattern 2).
(class_declaration
  (modifiers (class_modifier "data"))
  (type_identifier) @name) @def

; 2 — plain class  (sealed/abstract/open/inner all land here)
(class_declaration
  "class"
  (type_identifier) @name) @def

; 3 — interface  (including sealed interface)
(class_declaration
  "interface"
  (type_identifier) @name) @def

; 4 — object declaration
(object_declaration
  (type_identifier) @name) @def

; 5 — companion object  (named)
(companion_object
  (type_identifier) @name) @def

; 6 — typealias
(type_alias
  (type_identifier) @name) @def

; 7 — operator fun, top-level  MUST be before plain fun patterns.
(source_file
  (function_declaration
    (modifiers (function_modifier "operator"))
    (simple_identifier) @name) @def)

; 8 — operator fun, method / nested  MUST be before plain fun patterns.
(function_declaration
  (modifiers (function_modifier "operator"))
  (simple_identifier) @name) @def

; 9 — top-level fun only  (direct child of source_file)
(source_file
  (function_declaration
    (simple_identifier) @name) @def)

; 10 — method / nested fun  (any function_declaration NOT direct child of source_file)
(function_declaration
  (simple_identifier) @name) @def

; 11 — const val (single variable)  MUST be before plain val patterns.
;     property_modifier is a named leaf whose kind IS "const" (no anonymous child).
(property_declaration
  (modifiers (property_modifier))
  (binding_pattern_kind "val")
  (variable_declaration
    (simple_identifier) @name)) @def

; 12 — val (single variable)
(property_declaration
  (binding_pattern_kind "val")
  (variable_declaration
    (simple_identifier) @name)) @def

; 13 — var (single variable)
(property_declaration
  (binding_pattern_kind "var")
  (variable_declaration
    (simple_identifier) @name)) @def

; 14 — const val (destructuring)
(property_declaration
  (modifiers (property_modifier))
  (binding_pattern_kind "val")
  (multi_variable_declaration
    (variable_declaration
      (simple_identifier) @name))) @def

; 15 — val (destructuring)
(property_declaration
  (binding_pattern_kind "val")
  (multi_variable_declaration
    (variable_declaration
      (simple_identifier) @name))) @def

; 16 — var (destructuring)
(property_declaration
  (binding_pattern_kind "var")
  (multi_variable_declaration
    (variable_declaration
      (simple_identifier) @name))) @def

; 17 — enum entry  (DETAIL, LIST, etc. inside enum class bodies)
(enum_class_body
  (enum_entry
    (simple_identifier) @name) @def)
"#;

// ────────────────────────────────────────────────────────────────────────────
// IMPORTS QUERY
//
// Captures:
//   @path  — full dotted path, e.g. "com.example.Foo"  (always present)
//   @alias — local alias after `as`, e.g. "F"          (only for aliased imports)
//
// For wildcard imports (import com.example.*) the @path will end with ".*"
// because the identifier text includes all named children but NOT the
// wildcard_import node.  Detect wildcard by checking for (wildcard_import)
// child or by checking whether @path ends with ".*".
// ────────────────────────────────────────────────────────────────────────────
#[allow(dead_code)]
pub const KOTLIN_IMPORTS: &str = r#"
; plain import
(import_header
  (identifier) @path)

; aliased import — also emits @alias
(import_header
  (identifier) @path
  (import_alias
    (type_identifier) @alias))
"#;

// ────────────────────────────────────────────────────────────────────────────
// PACKAGE QUERY
//
// Captures:
//   @name — full dotted package name, e.g. "com.example.app"
// ────────────────────────────────────────────────────────────────────────────
#[allow(dead_code)]
pub const KOTLIN_PACKAGE: &str = r#"
(package_header
  (identifier) @name)
"#;

// ────────────────────────────────────────────────────────────────────────────
// REFERENCES QUERY
//
// Returns ALL simple_identifier and type_identifier nodes in a file.
// The caller must filter by name (compare node text to target).
//
// Why not embed the name via `#eq?`:
//   tree-sitter evaluates `#eq?` predicates automatically in matches(),
//   but it compares the *node text* to the string — which is correct.
//   We expose both variants so callers can choose:
//     • KOTLIN_REFS_ALL     — every identifier (caller filters)
//     • kotlin_refs_for()   — builds a query with the name baked in
//
// Captures:
//   @ref — every occurrence of any identifier in the file
//
// Node types included:
//   simple_identifier  — values, function calls, parameters, local vars
//   type_identifier    — type annotations, super-types, generic args
// ────────────────────────────────────────────────────────────────────────────
#[allow(dead_code)]
pub const KOTLIN_REFS_ALL: &str = r#"
[
  (simple_identifier) @ref
  (type_identifier)   @ref
]
"#;

/// Build a references query that pre-filters to `name` via `#eq?`.
/// Using this avoids iterating every identifier when the target is known.
///
/// ```
/// let q = kotlin_refs_for("MyClass");
/// // → r#"[(simple_identifier) @ref (type_identifier) @ref] (#eq? @ref "MyClass")"#
/// ```
#[allow(dead_code)]
pub fn kotlin_refs_for(name: &str) -> String {
    // Escape any double-quotes in the name (identifiers normally can't have them,
    // but be defensive).
    let safe = name.replace('\\', r"\\").replace('"', r#"\""#);
    format!(
        r#"[
  (simple_identifier) @ref
  (type_identifier)   @ref
]
(#eq? @ref "{safe}")"#
    )
}

// ────────────────────────────────────────────────────────────────────────────
// PATTERN INDEX → SYMBOL METADATA
// ────────────────────────────────────────────────────────────────────────────

use tower_lsp::lsp_types::SymbolKind;

/// Maps a pattern index from `KOTLIN_DEFINITIONS` to `(SymbolKind, detail_label)`.
///
/// `detail_label` is shown as `DocumentSymbol::detail` (e.g. "data class").
pub fn def_pattern_meta(pattern_index: usize) -> (SymbolKind, Option<&'static str>) {
    match pattern_index {
        0  => (SymbolKind::ENUM,            None),              // enum class
        1  => (SymbolKind::STRUCT,          Some("data class")),// data class
        2  => (SymbolKind::CLASS,           None),              // plain class (sealed/abstract/…)
        3  => (SymbolKind::INTERFACE,       None),              // interface
        4  => (SymbolKind::OBJECT,          None),              // object
        5  => (SymbolKind::OBJECT,          Some("companion object")),
        6  => (SymbolKind::CLASS,           Some("typealias")), // typealias
        7  => (SymbolKind::OPERATOR,        None),              // operator fun (top-level)
        8  => (SymbolKind::OPERATOR,        None),              // operator fun (method)
        9  => (SymbolKind::FUNCTION,        None),              // top-level fun
        10 => (SymbolKind::METHOD,          None),              // method / nested fun
        11 => (SymbolKind::CONSTANT,        Some("const val")), // const val
        12 => (SymbolKind::PROPERTY,        None),              // val property
        13 => (SymbolKind::VARIABLE,        None),              // var
        14 => (SymbolKind::CONSTANT,        Some("const val (destructure)")),
        15 => (SymbolKind::PROPERTY,        Some("val (destructure)")),
        16 => (SymbolKind::VARIABLE,        Some("var (destructure)")),
        17 => (SymbolKind::ENUM_MEMBER,     None),              // enum entry
        _  => (SymbolKind::NULL,            None),
    }
}
