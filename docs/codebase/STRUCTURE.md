# Codebase Structure

## Core Sections (Required)

### 1) Top-Level Map

| Path | Purpose | Evidence |
|------|---------|----------|
| `src/` | All production source code | Cargo.toml `[[bin]] path = "src/main.rs"` |
| `src/main.rs` | Tokio runtime entry point, CLI dispatch, TCP transport | Binary entrypoint |
| `src/backend/` | LSP protocol handlers (inbound adapters) | Handlers for LanguageServer trait |
| `src/cli/` | CLI subcommand implementation (find, refs, hover, etc.) | Non-LSP usage of the same indexer |
| `src/indexer/` | File discovery, parsing, in-memory index, cache | Core workspace indexing logic |
| `src/parser.rs` | tree-sitter query execution, symbol extraction | Parser for Kotlin/Java/Swift |
| `src/resolver/` | Definition resolution, completion scoring | Cross-file symbol resolution |
| `src/semantic_tokens.rs` | Semantic token provider for highlight coloring | CST-based token classification |
| `src/workspace_json.rs` | JetBrains workspace.json parser + build-layout auto-discovery | Source path discovery |
| `src/rg.rs` | ripgrep wrapper for fallback text search | External CLI integration |
| `src/types.rs` | Core types: `SymbolEntry`, `FileData`, `Visibility` | Domain model |
| `src/stdlib.rs` / `src/stdlib_tail.rs` | Embedded Kotlin stdlib symbol table | Built-in completions |
| `src/inlay_hints.rs` | Inlay hint generation (type hints for inferred variables) | Inlay hints support |
| `src/queries.rs` | Named constants for all tree-sitter node kind strings | Node kind constants |
| `src/task_runner.rs` | Background task orchestration utilities | Async task helpers |
| `tests/` | Integration tests (e.g., tree-sitter grammar tests) | `tests/swift_grammar.rs` |
| `tests/fixtures/` | Test data (Kotlin sources for fixtures) | Sample code for testing |
| `contrib/` | Legacy scripts (Python extract-sources is now built-in CLI) | Historical reference |
| `docs/` | Architecture, editor setup, feature docs | Manual documentation |
| `.github/` | GitHub Actions workflows, Copilot instructions, extension | CI/CD and tooling config |
| `CHANGELOG.md` | Release notes and feature history | Release documentation |
| `README.md` | Project overview, install, features, config | Public documentation |
| `Cargo.toml` / `Cargo.lock` | Dependency manifest and lock | Build configuration |

### 2) Entry Points

- **Main runtime entry:** `src/main.rs`
  - Builds tokio runtime with 4 workers, 512 blocking threads
  - In `async_main()`, dispatch order:
    1. **CLI mode** — `cli::CliArgs::parse()` matches subcommands (`find`, `refs`, `hover`, `index`, `tokens`, `tree`, `sources`, `extract-sources`); if matched, runs and exits
    2. **`--index-only <path>`** — build cache and exit (legacy flag, still supported)
    3. **TCP transport** — `--port <N>` serves one LSP client over TCP (loopback only; useful for Android Studio / Sora Editor)
    4. **Stdio LSP** — default: tower-lsp service on stdin/stdout
  
- **CLI subcommands** (`src/cli/`):
  - `find <name>` — workspace symbol search
  - `refs <name>` — find all references
  - `hover <file> <line> <col>` — get hover info at position
  - `index` — build workspace index and report stats
  - `tokens <file>` — decode semantic tokens with human-readable positions
  - `tree <file>` — dump tree-sitter CST
  - `sources` — list detected source roots for a workspace
  - `extract-sources` — unpack `*-sources.jar` from Gradle cache to `~/.kotlin-lsp/sources/`
  
- **Entrypoint selection:**
  - Hardcoded: `src/main.rs` is the only binary (see `Cargo.toml` `[[bin]]`)

### 3) Module Boundaries

| Module | What belongs here | What must not be here |
|--------|-------------------|------------------------|
| `backend/` | LSP protocol adapters (LanguageServer impl, request handlers) | Domain business logic, parsing |
| `cli/` | CLI subcommand handlers, argument parsing, human-readable output | LSP protocol types, background indexing |
| `indexer/` | Index maintenance, file discovery, concurrent parsing, workspace state | LSP protocol types, external CLI execution |
| `parser.rs` | tree-sitter grammar loading, CST traversal, symbol extraction | Index state, file I/O orchestration |
| `resolver/` | Multi-hop resolution, type inference, symbol lookup by position | LSP handler code, index mutation |
| `semantic_tokens.rs` | CST → semantic token conversion | LSP transport, index mutation |
| `workspace_json.rs` | workspace.json parsing, build-layout detection | Index state, LSP types |
| `rg.rs` | ripgrep CLI invocation, output parsing | Domain logic, index updates |
| `types.rs` | Core domain types (SymbolEntry, FileData, Visibility, Language) | Framework-specific types (except lsp_types) |
| `main.rs` | Tokio runtime setup, transport selection (stdio/TCP), CLI dispatch | Business logic, index operations |

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
- src/main.rs (tokio runtime, CLI dispatch, TCP transport at lines 83-105)
- src/cli/mod.rs (subcommand list: find, refs, hover, index, tokens, tree, sources, extract-sources)
- src/cli/run.rs (CLI index bootstrap, collect_cli_source_paths)
- src/backend/mod.rs (LSP LanguageServer trait impl)
- src/indexer.rs (module definitions: `pub mod scan`, `pub mod parser`, etc.)
- src/types.rs (SymbolEntry, FileData, Visibility)
- src/workspace_json.rs (load_source_paths, detect_build_layout_source_paths)
- src/semantic_tokens.rs (semantic token provider)

## Extended Sections (Optional)

### CLI Submodule Map

The `src/cli/` directory contains:

| Submodule | Purpose |
|-----------|---------|
| `mod.rs` | Subcommand enum, CliArgs parser, public `run()` dispatch |
| `args.rs` | Argument parsing (lexopt-based) |
| `run.rs` | `build_index()` bootstrap, `collect_cli_source_paths()` auto-discovery |
| `hover.rs` | `hover` subcommand handler |
| `sources.rs` | `sources` subcommand — list detected source roots |
| `tokens.rs` | `tokens` and `tree` subcommands — decode semantic tokens, dump CST |
| `extract_sources.rs` | `extract-sources` — walk Gradle cache, extract `*-sources.jar` |
| `output.rs` | Shared output formatting utilities |

### Indexer Submodule Map

The `src/indexer/` directory contains:

| Submodule | Purpose |
|-----------|---------|
| `scan.rs` | Workspace scanning orchestration, entry points: `index_workspace*()` |
| `discover.rs` | File discovery via `fd` or `walkdir` fallback |
| `cache.rs` | Disk cache serialization/deserialization (bincode + SHA2) |
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
| `mod.rs` | LSP LanguageServer trait impl, progress types, workspace root resolution |
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

The codebase follows **Ports & Adapters** principles:

- **Inbound adapters:** `backend/` (receives LSP protocol messages, converts to domain calls); `cli/` (receives CLI args, converts to index queries)
- **Outbound ports:** `ProgressReporter` trait (defined in `indexer/scan.rs`)
- **Outbound adapters:** `LspProgressReporter` in `backend/mod.rs` (sends `$/progress` notifications); `rg.rs` wraps ripgrep CLI
- **Domain layer:** `resolver/`, `parser.rs`, `types.rs` (pure symbol resolution, no framework imports)
- **Application layer:** `indexer/` (orchestrates discovery, parsing, caching; avoids framework types except via ports)

No violations: `lsp_types::*` is acceptable (LSP types ARE the system's domain vocabulary).
