# Codebase Structure

## Core Sections (Required)

### 1) Top-Level Map

| Path | Purpose | Evidence |
|------|---------|----------|
| `src/` | All production source code | Cargo.toml `[[bin]] path = "src/main.rs"` |
| `src/main.rs` | Tokio runtime entry point, stdio setup | Binary entrypoint |
| `src/backend/` | LSP protocol handlers (inbound adapters) | Handlers for LanguageServer trait |
| `src/indexer/` | File discovery, parsing, in-memory index, cache | Core workspace indexing logic |
| `src/parser.rs` | tree-sitter query execution, symbol extraction | Parser for Kotlin/Java/Swift |
| `src/resolver/` | Definition resolution, completion scoring | Cross-file symbol resolution |
| `src/rg.rs` | ripgrep wrapper for fallback text search | External CLI integration |
| `src/types.rs` | Core types: `SymbolEntry`, `FileData`, `Visibility` | Domain model |
| `tests/` | Integration tests (e.g., tree-sitter grammar tests) | `tests/swift_grammar.rs` |
| `tests/fixtures/` | Test data (Kotlin sources for fixtures) | Sample code for testing |
| `contrib/` | External scripts (Python extract-sources utility) | Helper tools, not shipped |
| `docs/` | Architecture, editor setup, feature docs | Manual documentation |
| `.github/` | Copilot CLI extension and instructions | Integration config |
| `CHANGELOG.md` | Release notes and feature history | Release documentation |
| `README.md` | Project overview, install, features, config | Public documentation |
| `Cargo.toml` / `Cargo.lock` | Dependency manifest and lock | Build configuration |

### 2) Entry Points

- **Main runtime entry:** `src/main.rs`
  - Builds tokio runtime with custom blocking pool
  - Calls `async_main()` which sets up tower-lsp service on stdin/stdout
  - Single LSP binary, no CLI subcommands (though supports `--index-only` for initial indexing)
  
- **Secondary entry points:**
  - None (single-purpose LSP server)
  
- **Entrypoint selection:**
  - Hardcoded: `src/main.rs` is the only binary entry (see `Cargo.toml` `[[bin]]`)

### 3) Module Boundaries

| Module | What belongs here | What must not be here |
|--------|-------------------|------------------------|
| `backend/` | LSP protocol adapters (LanguageServer impl, request handlers) | Domain business logic, parsing |
| `indexer/` | Index maintenance, file discovery, concurrent parsing, workspace state | LSP protocol types, external CLI execution |
| `parser.rs` | tree-sitter grammar loading, CST traversal, symbol extraction | Index state, file I/O orchestration |
| `resolver/` | Multi-hop resolution, type inference, symbol lookup by position | LSP handler code, index mutation |
| `rg.rs` | ripgrep CLI invocation, output parsing | Domain logic, index updates |
| `types.rs` | Core domain types (SymbolEntry, FileData, Visibility, Language) | Framework-specific types (except lsp_types) |
| `main.rs` | Tokio runtime setup, LSP service wiring | Business logic, index operations |

### 4) Naming and Organization Rules

- **File naming pattern:** `snake_case.rs` (e.g., `backend.rs`, `parser.rs`, `inlay_hints.rs`)
  - Modules within directories also use `snake_case` (e.g., `indexer/infer.rs`, `resolver/complete.rs`)
  - Test files follow pattern: `*_tests.rs` or `tests.rs` at end of module
  
- **Directory organization pattern:** **Layer-based** (not feature-based)
  - `backend/` = inbound adapters (LSP protocol layer)
  - `indexer/` = application layer (orchestration, indexing)
  - `resolver/` = domain layer (definition resolution, type inference)
  - Root-level files (`parser.rs`, `types.rs`, `rg.rs`) = cross-cutting concerns

- **Import aliasing / path conventions:**
  - No `tsconfig` aliases; relative and crate-rooted imports only
  - Private modules use `mod module_name;` (non-public, re-exports via `pub(crate) use` when needed)
  - Example: `indexer::scan` is private; re-exported as `pub(crate) use self::scan::{ProgressReporter, NoopReporter}` in `indexer.rs`

### 5) Evidence

- Cargo.toml (entry point `[[bin]] path = "src/main.rs"`)
- src/main.rs (tokio runtime setup)
- src/backend/mod.rs (LSP LanguageServer trait impl)
- src/indexer.rs (module definitions: `pub mod scan`, `pub mod parser`, etc.)
- src/types.rs (SymbolEntry, FileData, Visibility)

## Extended Sections (Optional)

### Indexer Submodule Map

The `src/indexer/` directory contains:

| Submodule | Purpose |
|-----------|---------|
| `scan.rs` | Workspace scanning orchestration, entry points: `index_workspace*()` |
| `discover.rs` | File discovery via `fd` or `walkdir` fallback |
| `cache.rs` | Disk cache serialization/deserialization (bincode + SHA2) |
| `parser.rs` | tree-sitter parsing and CST traversal (moved to top-level, symlink?) |
| `live_tree.rs` / `live_tree_impl.rs` | Live parse tree maintenance for open documents |
| `scope.rs` | Scope chain and variable shadowing analysis |
| `lookup.rs` | Symbol lookup by position (used mainly in tests; production uses resolver) |
| `resolution.rs` | Type substitution and symbol enrichment at positions |
| `node_ext.rs` | tree-sitter node helper methods |
| `apply.rs` | Apply workspace edits (rename implementation) |
| `doc.rs` | Extract documentation from comments |
| `infer/` | Type inference submodule for lambda params, `it`, `this` |
| `test_helpers.rs` | Test utility functions |
| `*_tests.rs` | Unit tests accompanying each module |

### Backend Submodule Map

The `src/backend/` directory contains:

| Submodule | Purpose |
|-----------|---------|
| `mod.rs` | LSP LanguageServer trait impl, progress types, adapter impl |
| `handlers.rs` | Individual LSP request handlers (hover, definition, completion, etc.) |
| `actions.rs` | Workspace actions (rename, document edit assembly) |
| `cursor.rs` | Cursor position utilities and conversions |
| `nav.rs` | Navigation utilities (jump to definition, find references) |
| `format.rs` | Formatting and signature helpers |
| `rename.rs` | Rename handler implementation |
| `helpers.rs` | Helper functions for protocol conversion |

### Resolver Submodule Map

The `src/resolver/` directory contains:

| Submodule | Purpose |
|-----------|---------|
| `mod.rs` | Public API and cross-module orchestration |
| `find.rs` | Symbol lookup by name (workspace symbol, definition resolution) |
| `complete.rs` | Completion item scoring, ranking, and auto-import generation |
| `infer.rs` | Type inference for lambda parameters and `it`/`this` |
| `tests.rs` | Comprehensive resolver unit tests |

### Hexagonal Architecture Alignment

The codebase follows **Ports & Adapters** principles (as of Phase 12 refactor):

- **Inbound adapters:** `backend/` (receives LSP protocol messages, converts to domain calls)
- **Outbound ports:** `ProgressReporter` trait (defined in `indexer/scan.rs`)
- **Outbound adapters:** `LspProgressReporter` in `backend/mod.rs` (sends `$/progress` notifications); `rg.rs` wraps ripgrep CLI
- **Domain layer:** `resolver/`, `parser.rs`, `types.rs` (pure symbol resolution, no framework imports)
- **Application layer:** `indexer/` (orchestrates discovery, parsing, caching; avoids framework types except via ports)

No violations: `lsp_types::*` is acceptable (LSP types ARE the system's domain vocabulary).
