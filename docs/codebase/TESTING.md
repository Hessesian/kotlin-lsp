# Testing

## Core Sections (Required)

### 1) Test Framework and Infrastructure

| Area | Framework / Tool | Version | Evidence |
|------|------------------|---------|----------|
| Test runner | `cargo test` (built-in) | N/A (Rust std) | Cargo.toml implicit |
| Assertion library | `assert_eq!`, `assert!` (Rust std) | N/A (Rust std) | *_tests.rs files |
| Test fixtures | Manual test data in `tests/fixtures/` | N/A | tests/fixtures/kotlin/ |
| Mocking framework | None (manual test doubles) | N/A | indexer_tests.rs, resolver_tests.rs |

### 2) Test Organization and Location

| Test Type | Location | Pattern | Count |
|-----------|----------|---------|-------|
| **Unit tests** | Inline `#[cfg(test)] mod tests` | Same file as code, at bottom | 664 total |
| **Integration tests** | `tests/` directory | Whole-module test files | 2 (swift_grammar.rs, fixtures) |
| **Fixtures** | `tests/fixtures/kotlin/`, `tests/fixtures/mvi/` | Kotlin source files for parsing tests | ~5 files |

### 3) Running Tests

```bash
# Run all tests (unit + integration)
cargo test

# Run with backtrace on failure
RUST_BACKTRACE=1 cargo test

# Run specific test module
cargo test indexer::

# Run with output (including println!)
cargo test -- --nocapture --test-threads=1

# Run tests matching a pattern
cargo test symbol_resolution
```

### 4) Test Doubles and Mocking

#### Manual Test Indexer

```rust
// indexer_tests.rs
fn uri(path: &str) -> Url {
    Url::parse(&format!("file:///test{path}")).unwrap()
}

fn indexed(path: &str, src: &str) -> (Url, Indexer) {
    let u = uri(path);
    let idx = Indexer::new();
    idx.index_content(&u, src);
    (u, idx)
}
```

No mocking framework; real `Indexer` populated with test code snippets.

#### Test Helpers

**`indexer/test_helpers.rs`:**
- `with_env_var()` — temporary env var override for tests
- `ENV_VAR_LOCK` — Mutex to serialize env var tests (avoid race conditions)
- Cache clearing utilities

### 5) Coverage and Test Gaps

- **No coverage threshold:** No configured coverage enforcement
- **Baseline:** 664 unit + integration tests passing
- **High-churn code well-tested:**
  - `resolver/tests.rs`: 60 KB (comprehensive)
  - `indexer_tests.rs`: 75 KB (workspace indexing, symbol extraction)
  - `parser_tests.rs`: 61 KB (tree-sitter grammar tests)

### 6) Continuous Integration

- **CI/CD:** None currently configured (scan showed no GitHub Actions)
- **Manual testing:** Developers run `cargo test` locally before pushing
- **Clippy linting:** Applied in recent commits; not enforced by CI

### 7) Evidence

- src/indexer_tests.rs (unit tests for indexing)
- src/resolver/tests.rs (resolution and completion tests)
- src/parser_tests.rs (tree-sitter parsing tests)
- tests/ directory (integration tests)
- Recent commit: "refactor(clippy): apply dead code removals and clippy fixes"

## Extended Sections (Optional)

### Unit Test Examples

#### Workspace Indexing

```rust
#[test]
fn test_index_workspace_stores_symbols() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///test/Main.kt").unwrap();
    idx.index_content(&uri, "class Main { fun hello() {} }");
    
    assert!(idx.definitions.contains_key("Main"));
    assert!(idx.definitions.contains_key("hello"));
}
```

#### Symbol Resolution

```rust
#[test]
fn test_goto_definition_follows_imports() {
    let idx = Indexer::new();
    idx.index_content(&uri_a, "package com.example\nclass Foo");
    idx.index_content(&uri_b, "package com.client\nimport com.example.Foo\nval x: Foo = Foo()");
    
    let result = resolver::find_definition(&idx, &uri_b, 4, 18); // at "Foo"
    assert_eq!(result.uri, uri_a);
    assert_eq!(result.range.start.line, 1);
}
```

#### Completion Scoring

```rust
#[test]
fn test_completion_scores_prefix_matches_higher() {
    let items = vec![
        CompletionItem { label: "TestClass" },  // prefix match
        CompletionItem { label: "OtherTest" },  // substring match
    ];
    let sorted = score_and_sort(items, "Test");
    
    assert_eq!(sorted[0].label, "TestClass");  // prefix: score 0
    assert_eq!(sorted[1].label, "OtherTest");  // substring: score 2
}
```

### Integration Tests

**`tests/swift_grammar.rs`:**
Tests that tree-sitter Swift grammar loads and parses correctly.

```rust
#[test]
fn swift_definitions_query() {
    let parser = Parser::new();
    let language = tree_sitter_swift::language();
    parser.set_language(language).unwrap();
    
    let tree = parser.parse("func hello() {}", None);
    // Verify query extracts "hello" as a function definition
}
```

### Test Data Organization

**`tests/fixtures/kotlin/`:**
```
input/
  Main.kt      — simple single-file Kotlin code
  MVI/         — multi-file MVI architecture sample
```

These files are read by tests to verify parsing and symbol extraction across realistic codebases.

### Known Test Limitations

1. **No end-to-end LSP test:** Tests do not exercise full LSP protocol (JSON-RPC encoding/decoding)
   - Tests use Indexer directly; LSP layer tested manually

2. **No performance/stress tests:** No tests for large codebases (e.g., Android 100K+ files)
   - Performance validated on real projects during development

3. **No isolation between test files:** Filesystem and env var state could theoretically leak
   - Mitigated by `ENV_VAR_LOCK` and test data in-memory

### Test-Driven Development (TDD) Status

- **Refactoring heavy:** Latest commits show tests updated alongside refactoring (good sign)
- **Fix-first commits:** Some commits fix bugs without preceding test (reactive, not TDD)
- **Coverage asymmetric:** High-value resolution logic well-tested; formatter/action handlers have less coverage

### Adding New Tests

1. **Inline in module:** Add test to same file, wrapped in `#[cfg(test)] mod tests`
2. **Follow naming:** `#[test] fn test_<behavior>_<condition>()`
3. **Use test helpers:** Import from `indexer::test_helpers`, `resolver::tests` 
4. **Avoid sleep/timing:** No async wait or time-based tests (use mocks or event-based checks)
5. **Isolate state:** Create fresh Indexer per test, avoid shared global state

### Mock Limitations and Workarounds

**Limitation:** No mocking framework (e.g., Mockito) or dependency injection framework.

**Workaround:** 
- Use real `Indexer` with tiny test code snippets
- Test failures are clear (actual vs. expected behavior)
- Keeps test code simple and Rust-idiomatic

**Risk:** Tests could inadvertently depend on implementation details.

**Mitigation:** Tests verify external contracts (LSP response format, symbol names) not internal APIs.

### Pre-Commit Test Recommendations

Before pushing, run:
```bash
cargo test && cargo clippy -- -W clippy::cognitive_complexity -W clippy::too_many_lines
```

Ensures no test failures or major lints before remote CI would catch them.
