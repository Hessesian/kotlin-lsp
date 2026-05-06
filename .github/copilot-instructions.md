# kotlin-lsp — Copilot Agent Instructions

## Project overview

`kotlin-lsp` is a lightweight LSP server for Kotlin and Java files, built in Rust using `tower-lsp` and `tree-sitter`. It is designed for **agentic use** (Copilot CLI, Neovim, Helix) where fast symbol lookup matters more than compiler accuracy.

Key design constraints:
- **No JVM/Gradle**: pure Rust, tree-sitter for parsing — startup is instant
- **< 200 MB RAM** target on large Android repos
- **Cross-file indexing** via in-memory HashMap, persisted to disk cache
- **Workspace root override** — reads `~/.config/kotlin-lsp/workspace` at startup

## Build & install

```sh
cargo build --release
cargo install --path .    # installs to ~/.cargo/bin/kotlin-lsp
```

Tests:
```sh
cargo test
```

## Source layout

| File | Purpose |
|---|---|
| `src/main.rs` | Entry point, stdio transport setup |
| `src/backend.rs` | LSP request handlers (`initialize`, `hover`, `definition`, `references`, `document_symbol`, `workspace_symbol`, `execute_command`) |
| `src/indexer.rs` | File discovery (`fd`), tree-sitter parsing, in-memory index, disk cache |
| `src/parser.rs` | Tree-sitter query execution, `SymbolEntry` extraction, `extract_detail()` |
| `src/resolver.rs` | Cross-file resolution, import handling, `rg` fallback |
| `src/types.rs` | `SymbolEntry`, `Location`, shared types |
| `contrib/copilot-extension/extension.mjs` | Copilot CLI skill extension — copy to `~/.copilot/extensions/kotlin-lsp/` |

## Key types

```rust
// types.rs
pub struct SymbolEntry {
    pub name: String,
    pub kind: String,       // "class", "fun", "val", "var", "interface", "object", "typealias"
    pub file: String,
    pub range: Range,
    pub container: String,  // enclosing class/object name, empty if top-level
    pub detail: String,     // truncated declaration signature, e.g. "fun foo(x: Int): String"
}
```

## Workspace root priority (backend.rs `initialize`)

1. `KOTLIN_LSP_WORKSPACE_ROOT` env var (if set and is a valid dir)
2. `~/.config/kotlin-lsp/workspace` plain-text file (trimmed)
3. LSP client `rootUri` / first `workspaceFolder`

To switch workspace at runtime:
```sh
echo /path/to/project > ~/.config/kotlin-lsp/workspace
# then kill & restart the LSP process
```

Or use the `kotlin-lsp/changeRoot` workspace command (programmatic).

## Custom LSP commands

Both registered in `execute_command_provider` capabilities:

| Command | Args | Effect |
|---|---|---|
| `kotlin-lsp/reindex` | none | Clear cache, re-scan all files |
| `kotlin-lsp/changeRoot` | `["/abs/path"]` | Swap workspace root, clear index, reindex |

Note: the built-in Copilot `lsp` tool does not support `executeCommand`. Use the skill extension's `kotlin_lsp_set_workspace` tool (writes config file + kills process) instead.

## LSP-first workflow for agentic code investigation

Prefer LSP over `grep`/`rg` in this order:

1. **`workspaceSymbol "Name"`** — find class/fun/val by name across all files; returns name + signature + location
2. **`documentSymbol file.kt`** — list all symbols in a file (methods, fields, nested classes)
3. **`hover file.kt line col`** — get declaration signature and type info at a position
4. **`goToDefinition`** — jump to declaration
5. **`findReferences`** — find all usages (warning: common method names return noise — see below)
6. **`rg` / `grep`** — last resort, or when method names are too common for findReferences

### findReferences noise mitigation

`findReferences` is name-based (no type resolution). For common method names:
- Use `rg` with a qualified pattern: `rg "ReceiverClass\.methodName\("` 
- Or scope to the declaring class's package directory

Planned improvement: import-aware filtering — only return refs from files that import the declaring class.

## Disk cache

Cache stored in `~/.cache/kotlin-lsp/index-<hash>.bin` (bincode format).  
Current `CACHE_VERSION = 2` — bump this in `indexer.rs` when `SymbolEntry` schema changes.

The `#[serde(default)]` attribute on new `SymbolEntry` fields allows old cache entries to deserialize without error (new field gets its default value).

## Release process

1. Bump version in `Cargo.toml`
2. Update `README.md` changelog / feature notes
3. `cargo build --release && cargo test`
4. `git tag v0.x.y && git push --tags`
5. `cargo publish`

## Copilot CLI integration

Install the skill extension:
```sh
mkdir -p ~/.copilot/extensions/kotlin-lsp
cp contrib/copilot-extension/extension.mjs ~/.copilot/extensions/kotlin-lsp/
```

LSP config (`~/.copilot/lsp-config.json`):
```json
{
  "servers": [
    {
      "name": "kotlin-lsp",
      "command": ["kotlin-lsp"],
      "languages": ["kotlin", "java"],
      "fileExtensions": [".kt", ".kts", ".java"]
    }
  ]
}
```

The skill extension provides:
- `kotlin_lsp_status` — check indexing progress
- `kotlin_lsp_set_workspace` — write config file and restart LSP for a new project
- `kotlin_lsp_info` — capabilities and known limitations

## Rust coding guidelines

These rules are distilled from the actionbook/rust-skills layer framework and leonardomso/rust-skills,
cherry-picked for relevance to kotlin-lsp's architecture.

### Design tracing (actionbook layer model)

Before making a design decision, trace through three layers top-down:

1. **WHY (Domain)** — What constraint does this solve? (e.g. "infer functions are pure reads over a snapshot")
2. **WHAT (Design)** — What pattern fits? (e.g. `InferDeps` trait, `CursorPos` newtype)
3. **HOW (Mechanics)** — Which Rust feature? (e.g. generic bound, struct, method)

Never jump straight to HOW. A misdiagnosed WHY produces technically correct but wrong abstractions.

### Newtypes for semantic safety

Adjacent `usize` params like `(cursor_line, cursor_col)` are a transposition bug waiting to happen.
Wrap them in a named struct with documented units:

```rust
/// Cursor position in a document. `col` is UTF-16 code units (LSP protocol).
pub struct CursorPos { pub line: usize, pub col: usize }
```

Apply when: two same-type params appear together in ≥2 function signatures with swappable semantics.

### Rule of Three before abstracting

Don't introduce a generic bound until you have ≥2 distinct concrete implementations that actually
differ. For the `InferDeps` trait pattern: the rule is met — `Indexer` (production) and `TestDeps`
(test stub) are genuinely different. If only one concrete type exists, keep the function concrete.

### Prefer generics over `Box<dyn Trait>`

Use `impl Trait` or `<T: Trait>` for infer functions (static dispatch, zero cost, no heap).
Reserve `Box<dyn Trait>` only for heterogeneous runtime collections or plugin-style registries.

```rust
// Good: infer function with generic bound
fn infer_it_type<D: InferDeps>(deps: &D, pos: CursorPos) -> Option<String> { ... }

// Avoid: dyn Trait adds vtable overhead and heap allocation for no benefit here
fn infer_it_type(deps: &dyn InferDeps, pos: CursorPos) -> Option<String> { ... }
```

### Traits for testability

Extract snapshot access behind a trait so infer functions can be tested without a full `Indexer`:

```rust
pub trait InferDeps {
    fn lines(&self, uri: &str) -> Option<Arc<Vec<String>>>;
    fn live_doc(&self, uri: &str) -> Option<Arc<LiveDoc>>;
    fn symbol_detail(&self, name: &str, container: &str) -> Option<String>;
}
```

Unit tests implement `TestDeps` as a simple struct — no DashMap, no disk, fast.

### Purity in infer functions

Functions that read doc/index data and return inference results are pure: `(inputs) -> output`.
Do not let them mutate index state. Mutation (on-demand indexing, cache fills) belongs on `Indexer`,
not inside the infer call graph.

### Dedup before abstracting

Before introducing a new utility function, check if it already exists:
- `utf16_col_to_byte` — in `src/indexer/live_tree.rs`; don't inline the loop
- `lines_for(uri)` — in `src/indexer/scope.rs` (moving to `indexer.rs`); don't duplicate the pattern

## Pre-commit checklist

Run these checks before every commit. A commit that fails any of them should not be pushed.

### 1. Build and tests

```sh
cargo test
cargo clippy -- -D warnings -W clippy::cognitive_complexity -W clippy::too_many_lines
```

Zero warnings required. Fix, don't suppress with `#[allow]` unless the suppression is
accompanied by a comment explaining why the lint is inapplicable.

### 2. No hardcoded tree-sitter node kind strings

All node kind comparisons must use named constants from `src/queries.rs`.

- **Wrong:** `node.kind() == "function_declaration"`
- **Right:** `node.kind() == KIND_FUN_DECL`

If a constant doesn't exist yet, add it to the appropriate section in `src/queries.rs` before
using it. Same rule applies to modifier keywords (`"static"`, `"final"`) and Java node kinds.

### 3. Prefer trait bounds over concrete types

When a function only touches a subset of a struct's API, ask: is that subset already a trait?
If not, and if ≥2 distinct implementations could exist (production + test stub), extract one.

- Use `impl Trait` / `<T: Trait>` — static dispatch, zero heap cost.
- Reserve `Box<dyn Trait>` only for heterogeneous runtime collections or plugin registries.
- Apply the Rule of Three: wait for the second concrete implementation before abstracting.

### 4. No deep nesting

Functions should have at most 3 levels of indentation in their body.

Flatten with:
- Early `return` / `return None` guards at the top
- The `?` operator for error propagation
- `let … else` for mandatory destructuring
- Extracted helper functions for inner loops or match arms

A `match` nested inside another `match` inside an `if` is a signal to extract.

### 5. Section comments inside a function body signal a split

If you feel the need to write a `// --- Step 1: …` or `// Build the result` comment to
separate logical phases inside a function, that's a signal the function should be split.

- Each logical phase becomes a named helper function — the name replaces the comment.
- The top-level function becomes a readable sequence of helper calls.
- Exception: a single clarifying comment on a non-obvious line is fine; what's banned is
  using comments as section dividers to compensate for a function doing too many things.

### 6. Long names signal missing structs or traits; avoid abbreviations

**No abbreviations.** `sym` → `symbol`, `idx` → `index`, `uri_str` → `uri` (or a newtype).
Short names save keystrokes and lose meaning. The compiler remembers the type; the reader
does not.

**A long function name signals a missing struct.** If you find yourself writing
`resolve_symbol_with_fallback_and_type_args(uri, name, container, type_args, fallback)`,
the parameters want to be a struct:

```rust
struct SymbolQuery { uri: Url, name: String, container: String, type_args: Vec<String> }
fn resolve_symbol(query: &SymbolQuery) -> Option<SymbolEntry> { … }
```

The function name shrinks because the struct name carries the context.

**A confusing function signature signals a missing trait.** If callers must read the body
to understand what a function does, extract the behaviour into a named trait. The trait name
and method name together should make the call site self-documenting:

```rust
// Unclear: what does `index` do here?
fn enrich(index: &Indexer, pos: CursorPos) -> Option<String>

// Clear: the trait name declares the contract
fn enrich<R: SymbolResolver>(resolver: &R, pos: CursorPos) -> Option<String>
```

Use traits to clarify *what role* a parameter plays, not just *what type* it is.

### 7. No `unwrap()` or `expect()` in production code

Use `?`, `if let`, `match`, or log-and-return patterns. Exception: `#[cfg(test)]` code may
use `unwrap()` / `expect()`.

### 8. Test-only code must be gated

Functions, imports, or types that are only used in tests must be annotated `#[cfg(test)]`.
Do not leave production code with dead-code warnings suppressed via `#[allow(dead_code)]`
without a comment explaining why the gate can't be used instead.

### 9. Cache version bump on schema changes

If `SymbolEntry` gains or loses fields, bump `CACHE_VERSION` in `src/indexer/cache.rs`.
New fields must carry `#[serde(default)]` so old cache files still deserialize.

### 10. Minimal visibility

Default to module-private (`fn`, `struct`). Widen to `pub(crate)` only when a sibling module
requires it; widen to `pub` only for items that form part of the external API surface.

## Known limitations

- **No type resolution** — tree-sitter gives structure, not type-checked references
- **`findReferences` on common names** — returns all files with that identifier, not just typed callers
- **No incremental parse** — file changes require reindex (or manual `reindex` command)
- **Java support** — indexed but less thoroughly tested than Kotlin
- **No completion** — textDocument/completion not implemented
