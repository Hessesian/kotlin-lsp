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
pub(crate) const KOTLIN_DEFINITIONS: &str = r#"
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

; 18 — primary constructor val parameter  (creates a property / backing field).
;     Uses binding_pattern_kind like property_declaration but inside class_parameter.
(class_parameter
  (binding_pattern_kind "val")
  (simple_identifier) @name) @def

; 19 — primary constructor var parameter
(class_parameter
  (binding_pattern_kind "var")
  (simple_identifier) @name) @def
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
pub(crate) const KOTLIN_IMPORTS: &str = r#"
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
pub(crate) const KOTLIN_PACKAGE: &str = r#"
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
pub(crate) const KOTLIN_REFS_ALL: &str = r#"
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
pub(crate) fn kotlin_refs_for(name: &str) -> String {
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
pub(crate) fn def_pattern_meta(pattern_index: usize) -> (SymbolKind, Option<&'static str>) {
    match pattern_index {
        0 => (SymbolKind::ENUM, None),                 // enum class
        1 => (SymbolKind::STRUCT, Some("data class")), // data class
        2 => (SymbolKind::CLASS, None),                // plain class (sealed/abstract/…)
        3 => (SymbolKind::INTERFACE, None),            // interface
        4 => (SymbolKind::OBJECT, None),               // object
        5 => (SymbolKind::OBJECT, Some("companion object")),
        6 => (SymbolKind::CLASS, Some("typealias")), // typealias
        7 => (SymbolKind::OPERATOR, None),           // operator fun (top-level)
        8 => (SymbolKind::OPERATOR, None),           // operator fun (method)
        9 => (SymbolKind::FUNCTION, None),           // top-level fun
        10 => (SymbolKind::METHOD, None),            // method / nested fun
        11 => (SymbolKind::CONSTANT, Some("const val")), // const val
        12 => (SymbolKind::PROPERTY, None),          // val property
        13 => (SymbolKind::VARIABLE, None),          // var
        14 => (SymbolKind::CONSTANT, Some("const val (destructure)")),
        15 => (SymbolKind::PROPERTY, Some("val (destructure)")),
        16 => (SymbolKind::VARIABLE, Some("var (destructure)")),
        17 => (SymbolKind::ENUM_MEMBER, None), // enum entry
        18 => (SymbolKind::PROPERTY, Some("val param")), // primary ctor val param
        19 => (SymbolKind::VARIABLE, Some("var param")), // primary ctor var param
        _ => (SymbolKind::NULL, None),
    }
}

// ════════════════════════════════════════════════════════════════════════════
// SWIFT
// ════════════════════════════════════════════════════════════════════════════

// ────────────────────────────────────────────────────────────────────────────
// SWIFT DEFINITIONS QUERY
//
// Grammar: alex-pinkus/tree-sitter-swift (devgen fork, 0.21)
//
// Key grammar facts:
// • `class_declaration` is reused for class/struct/enum/extension — distinguished
//   by the `declaration_kind` anonymous keyword child ("class"/"struct"/"enum"/"extension").
// • Extensions have `name: (user_type (type_identifier))` instead of `name: (type_identifier)`.
// • `protocol_declaration` is a separate node type.
// • Properties use `(pattern bound_identifier: (simple_identifier))`.
// • Protocol members: `protocol_function_declaration`, `protocol_property_declaration`.
// • init → `init_declaration` (no name capture; caller can label "init").
// ────────────────────────────────────────────────────────────────────────────

pub(crate) const SWIFT_DEFINITIONS: &str = r#"
; 0 — class
(class_declaration "class" name: (type_identifier) @name) @def

; 1 — struct
(class_declaration "struct" name: (type_identifier) @name) @def

; 2 — enum
(class_declaration "enum" name: (type_identifier) @name) @def

; 3 — extension (name is user_type wrapping type_identifier)
(class_declaration "extension" name: (user_type (type_identifier) @name)) @def

; 4 — protocol
(protocol_declaration name: (type_identifier) @name) @def

; 5 — function
(function_declaration name: (simple_identifier) @name) @def

; 6 — typealias
(typealias_declaration name: (type_identifier) @name) @def

; 7 — protocol function
(protocol_function_declaration name: (simple_identifier) @name) @def

; 8 — init declaration (no @name — caller assigns "init")
(init_declaration) @def

; 9 — property (let/var with bound identifier)
(property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @def

; 10 — protocol property
(protocol_property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @def

; 11 — enum entry
(enum_entry name: (simple_identifier) @name) @def
"#;

/// Maps a pattern index from `SWIFT_DEFINITIONS` to `(SymbolKind, detail_label)`.
pub(crate) fn swift_def_pattern_meta(pattern_index: usize) -> (SymbolKind, Option<&'static str>) {
    match pattern_index {
        0 => (SymbolKind::CLASS, None),                 // class
        1 => (SymbolKind::STRUCT, None),                // struct
        2 => (SymbolKind::ENUM, None),                  // enum
        3 => (SymbolKind::CLASS, Some("extension")),    // extension
        4 => (SymbolKind::INTERFACE, None),             // protocol
        5 => (SymbolKind::FUNCTION, None),              // func (top-level + methods)
        6 => (SymbolKind::CLASS, Some("typealias")),    // typealias
        7 => (SymbolKind::METHOD, None),                // protocol func
        8 => (SymbolKind::CONSTRUCTOR, Some("init")),   // init
        9 => (SymbolKind::PROPERTY, None),              // let/var property
        10 => (SymbolKind::PROPERTY, Some("protocol")), // protocol property
        11 => (SymbolKind::ENUM_MEMBER, None),          // case entry
        _ => (SymbolKind::NULL, None),
    }
}

/// Pattern index of `init_declaration` in `SWIFT_DEFINITIONS`.
/// Pattern 8 has no `@name` capture — the parser synthesises `"init"` instead.
pub(crate) const SWIFT_INIT_PATTERN_IDX: usize = 8;

/// Synthesised name for Swift init declarations.
pub(crate) const SWIFT_INIT_NAME: &str = "init";

// ─── tree-sitter node kind constants ─────────────────────────────────────────
// These match the `node.kind()` strings produced by tree-sitter-kotlin,
// tree-sitter-java, and tree-sitter-swift grammars.

pub(crate) const KIND_SIMPLE_IDENT: &str = "simple_identifier";
pub(crate) const KIND_TYPE_IDENT: &str = "type_identifier";
pub(crate) const KIND_IDENTIFIER: &str = "identifier";
pub(crate) const KIND_SCOPED_IDENT: &str = "scoped_identifier";
pub(crate) const KIND_CALL_EXPR: &str = "call_expression";
pub(crate) const KIND_LAMBDA_LIT: &str = "lambda_literal";
pub(crate) const KIND_LAMBDA_PARAMS: &str = "lambda_parameters";
pub(crate) const KIND_VALUE_ARG: &str = "value_argument";
pub(crate) const KIND_VALUE_ARGS: &str = "value_arguments";
pub(crate) const KIND_USER_TYPE: &str = "user_type";
pub(crate) const KIND_FUN_DECL: &str = "function_declaration";

// ─── Declaration node kinds (shared or language-specific) ──────────────────
pub(crate) const KIND_CLASS_DECL: &str = "class_declaration";
pub(crate) const KIND_ENUM_DECL: &str = "enum_declaration";
pub(crate) const KIND_INTERFACE_DECL: &str = "interface_declaration";

// Kotlin-specific
pub(crate) const KIND_OBJECT_DECL: &str = "object_declaration";
pub(crate) const KIND_DELEGATION_SPEC: &str = "delegation_specifier";

// Java-specific
pub(crate) const KIND_RECORD_DECL: &str = "record_declaration";
pub(crate) const KIND_METHOD_DECL: &str = "method_declaration";
pub(crate) const KIND_CTOR_DECL: &str = "constructor_declaration";
pub(crate) const KIND_FIELD_DECL: &str = "field_declaration";
pub(crate) const KIND_IMPORT_DECL: &str = "import_declaration";
pub(crate) const KIND_PACKAGE_DECL: &str = "package_declaration";
pub(crate) const KIND_SUPERCLASS: &str = "superclass";
pub(crate) const KIND_SUPER_INTERFACES: &str = "super_interfaces";
pub(crate) const KIND_EXTENDS_INTERFACES: &str = "extends_interfaces";
pub(crate) const KIND_TYPE_LIST: &str = "type_list";

// Swift-specific
pub(crate) const KIND_PROTOCOL_DECL: &str = "protocol_declaration";
pub(crate) const KIND_INHERITANCE_SPEC: &str = "inheritance_specifier";

// ─── Generic / type parameter node kinds (shared across Kotlin, Java, Swift) ─
pub(crate) const KIND_TYPE_PARAMS: &str = "type_parameters";
pub(crate) const KIND_TYPE_PARAM: &str = "type_parameter";
pub(crate) const KIND_TYPE_ARGS: &str = "type_arguments";

// ─── Kotlin property / navigation / call node kinds ──────────────────────────
pub(crate) const KIND_PROP_DECL: &str = "property_declaration";
pub(crate) const KIND_PROP_DELEGATE: &str = "property_delegate";
pub(crate) const KIND_VAR_DECL: &str = "variable_declaration";
pub(crate) const KIND_NAV_EXPR: &str = "navigation_expression";
pub(crate) const KIND_CALL_SUFFIX: &str = "call_suffix";
