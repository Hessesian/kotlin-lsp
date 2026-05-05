# Coding Conventions

## Core Sections (Required)

### 1) Naming Conventions

| Element | Pattern | Examples | Evidence |
|---------|---------|----------|----------|
| Files | `snake_case.rs` | `parser.rs`, `backend.rs`, `inlay_hints.rs` | src/ directory |
| Modules | `snake_case` | `mod scan`, `mod infer`, `mod complete` | src/indexer/*, src/resolver/* |
| Types (structs, enums) | `PascalCase` | `SymbolEntry`, `FileData`, `Visibility`, `ProgressReporter` | src/types.rs |
| Constants | `SCREAMING_SNAKE_CASE` | `MAX_FILES_UNLIMITED`, `MAX_READ_FAILURES_LOGGED`, `CACHE_VERSION` | src/indexer/scan.rs |
| Functions | `snake_case` | `find_definition`, `index_workspace`, `extract_detail` | throughout src/ |
| Private functions | `snake_case` (no prefix; use `pub` vs private) | `index_workspace_impl` (private) | src/indexer/scan.rs |
| Test functions | `snake_case_test` or `test_*` convention | `test_index_workspace`, `symbol_resolution_test` | *_tests.rs files |

### 2) Rust-Specific Standards

#### Visibility and Access

- **Private by default:** `mod scan;` (not `pub mod scan;`)
- **Re-exports for APIs:** `pub(crate) use self::scan::{NoopReporter, ProgressReporter};` in module boundary
- **Sealed internals:** Modules like `indexer::scan` are private to indexer; only public re-exports are exposed
- **No `pub` on struct fields in external API:** Fields accessed via methods or getters

#### Error Handling

- **Result-based returns:** All fallible operations return `Result<T, E>` or `Option<T>`
- **Panic policy:**
  - `.unwrap()` permitted for non-recoverable assertion failures (e.g., file permission denied in CLI mode)
  - `.expect(msg)` preferred over `.unwrap()` to provide context
  - Logging before `.unwrap()` is standard in error paths
- **No silent failures:** All Err/None branches either propagated (`?`), logged, or handled explicitly

#### Async/Await

- Entry points for concurrent work use `tokio::spawn(async move { ... })`
- All background tasks capture required state via `Arc<T>` clones
- `'static` bound propagates through generic types for spawned tasks
- No blocking I/O in async contexts; use `tokio::fs`, `tokio::io` equivalents

#### Generics and Traits

- Trait bounds in where clauses, not in angle brackets (when possible)
- Outbound ports use generics: `fn index_workspace<R: ProgressReporter>(reporter: Arc<R>)`
- No `Box<dyn Trait>` for inferred types; prefer `impl Trait` or generic bounds
- `'static` bound used for spawned task compatibility

#### Immutability

- Prefer `&self` methods that return new values over `&mut self` mutation
- `Arc<T>` for shared, concurrent access (replaces `Box<T>` when multiple tasks read same data)
- DashMap preferred over `Mutex<HashMap>` (concurrent reads without blocking)

### 3) Formatting and Style

- **Auto-format:** `cargo fmt` applied project-wide (commit: `chore(fmt): apply cargo fmt`)
- **Line length:** Cargo.toml defines no explicit limit; idiomatic Rust style (typically ~100 chars)
- **Imports:** 
  - Organize by: std library → external crates → internal modules
  - No barrel exports (star imports) except in test helper modules
  - Example: `use std::sync::Arc; use tower_lsp::lsp_types::*; use crate::types::SymbolEntry;`
- **Module ordering:** public APIs first, private helpers last
- **Comments:** Doc comments (`///`) on public items, inline comments (`//`) for non-obvious logic

### 4) Error Messages and Logging

- **Log levels:**
  - `error!()` — unrecoverable failures (file permission denied, out of disk)
  - `warn!()` — recoverable but significant (file read failed, falling back)
  - `info!()` — progress (indexing started, completed)
  - `debug!()` — detailed flow (symbol lookup steps, cache hits/misses)
- **Log format:** Uses `env_logger`; controlled by `RUST_LOG` env var (e.g., `RUST_LOG=debug`)
- **Error transparency:** Client-facing errors include context (file name, line number)

### 5) Testing Patterns

#### Unit Test Organization

- Co-located: `#[cfg(test)] mod tests` at end of same file
- Naming: `#[test] fn test_<behavior>()` or `#[test] fn <behavior>_test()`
- Test helpers in `indexer/test_helpers.rs` and `resolver/tests.rs`

#### Test Doubles

- **Fake indexer:** Manual test struct, not mocked
- Example: `Indexer::new()` for unit tests; real index populated with test data
- No proc-macro mocking (e.g., Mockito) — manual fakes preferred for clarity

#### Fixtures

- Test data in `tests/fixtures/kotlin/`, `tests/fixtures/mvi/`
- Kotlin sample files used as input for tree-sitter parsing tests

#### Coverage

- No coverage threshold configured
- High-churn areas have comprehensive tests (e.g., `resolver/tests.rs`: 60 KB, ~1000 test cases)
- Integration tests in `tests/` directory run tree-sitter grammar tests

### 6) Documentation

- **Doc comments:** Public structs, traits, functions have `///` doc comments
- **Example:** `/// Outbound port for LSP progress notifications.` (in `ProgressReporter` trait)
- **Module-level docs:** `//!` at top of file (e.g., `src/indexer/scan.rs` describes workspace scanning)
- **No boilerplate:** docs only when non-obvious from name

### 7) Evidence

- src/ directory (file/module naming)
- src/types.rs (type naming: SymbolEntry, FileData)
- src/indexer/scan.rs (module organization, doc comments, constants)
- Recent commits: `chore(fmt)`, `clippy` fixes
- README.md (environment variables: RUST_LOG, KOTLIN_LSP_MAX_FILES)
- Cargo.toml ([profile.release] settings)

## Extended Sections (Optional)

### Memory and Performance Conventions

- **Arc vs Box:** `Arc<T>` for multi-task shared state; `Box<T>` for heap allocation without sharing
- **String handling:** `&str` for borrowed strings; `String` for owned; avoid unnecessary `to_string()` allocations
- **Clone avoidance:** Verify clone is needed (especially in tight loops); use references when possible

### Dependency Injection

All major components accept dependencies as constructor/argument parameters:
- Indexer receives scanner config, parser, cache paths
- Resolver receives Indexer (immutable ref)
- Handlers receive Indexer (shared Arc)

No global singletons or service locators.

### Macro Usage

- No custom procedural macros in production code
- tree-sitter grammar macros handled by dependencies (tree-sitter-java, etc.)
- `#[derive(...)]` used for Serde, Clone, etc.

### Clippy Lints Enabled

- `clippy::cognitive_complexity` — function complexity cap
- `clippy::too_many_lines` — function size cap (default ~200 lines)
- Regular clippy runs to catch common mistakes (see commits: "refactor(clippy): apply...")

### Phase 12 Refactoring Standards (In-Progress)

The codebase is undergoing structural refactoring (Phase 12) to:
- Replace `Option<T>` with trait-based ports (e.g., `ProgressReporter`)
- Enforce hexagonal architecture boundaries
- Remove anti-patterns (bare `unwrap`, double dereferences)
- Replace blocking I/O with tokio equivalents

Standard for all new code:
- Use traits for outbound dependencies
- Return named structs instead of bool/tuple for domain logic
- Apply visibility downgrade (`pub` → `pub(crate)`) unless external API
