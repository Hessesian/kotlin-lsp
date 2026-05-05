# Known Issues, Tech Debt, and Concerns

## Core Sections (Required)

### 1) Production TODOs / FIXMEs / HACKs

The codebase has **zero production TODOs/FIXMEs/HACKs** in source code (excluding test files). 

Search result from scan:
- All `TODO()` occurrences are in stdlib Kotlin function entries or test data, not production logic
- No `// TODO:` or `// FIXME:` comments in src/
- Production code is clean of deliberate tech debt markers

### 2) High-Churn Files (Last 90 Days, Top 20)

Files with highest modification counts are undergoing active refactoring (Phase 12 structural changes).

| File | Commits | Risk Level | Why |
|------|---------|-----------|-----|
| `src/indexer.rs` | 85 | 🔴 **CRITICAL** | Core index interface; undergoing trait refactoring (ProgressReporter, port extraction) |
| `src/backend.rs` | 50 | 🟡 **HIGH** | LSP handler dispatch; frequent LSP feature updates |
| `src/parser.rs` | 47 | 🟡 **HIGH** | Symbol extraction logic; tree-sitter grammar updates |
| `src/resolver.rs` | 41 | 🟡 **HIGH** | Type inference, completion; complex multi-hop logic |
| `Cargo.toml` | 31 | 🟢 **MEDIUM** | Dependency updates and feature toggles |
| `src/resolver/complete.rs` | 26 | 🟡 **HIGH** | Completion scoring and ranking; frequent algorithm tweaks |
| `src/backend/handlers.rs` | 26 | 🟡 **HIGH** | Individual LSP handlers; protocol evolution |
| `src/indexer/lookup.rs` | 23 | 🟡 **HIGH** | Symbol lookup; undergoing consolidation into resolver |
| `src/indexer/resolution.rs` | 23 | 🟡 **HIGH** | Type substitution and symbol enrichment |
| `src/resolver/infer.rs` | 23 | 🟡 **HIGH** | Type inference for `it`, `this`; complex edge cases |

**Implications:**
- **Fragility risk:** High-churn code has hidden complexity; recent refactoring may introduce bugs
- **Code review priority:** Changes to top 5 files warrant extra scrutiny
- **Test coverage:** Top files have good test coverage (resolver 60KB, indexer 75KB test files)

### 3) Large Files (Complexity Signals)

Files over 500 LOC often mix multiple responsibilities:

| File | Size | Responsibility | Complexity |
|------|------|-----------------|-----------|
| `src/indexer_tests.rs` | 75 KB | Unit tests for indexing | Medium (test code is clear) |
| `src/parser.rs` | 61 KB | Symbol extraction + visibility rules | 🔴 **HIGH** — needs extraction |
| `src/resolver/tests.rs` | 60 KB | Resolver unit tests | Medium (test code) |
| `src/indexer/resolution.rs` | 44 KB | Type substitution + symbol enrichment | 🔴 **HIGH** — intertwined concerns |
| `src/indexer/scope.rs` | ~35 KB | Scope chain, variable shadowing | Medium (focused responsibility) |

**Recommendation:** Consider splitting `parser.rs` and `indexer/resolution.rs` into smaller, more focused modules in Phase 13.

### 4) Known Limitations

#### a) No Type Checking
- Tree-sitter parses only **structure**, not types
- Lambda `it` type inference is heuristic (function signature lookup), not type-checked
- False positives possible (e.g., if two lambdas have same name, inference may pick wrong one)

#### b) findReferences Noise
- Cross-file reference search is **name-based** using `rg --word-regexp`
- Common method names (e.g., `getValue`) return many false positives
- **Workaround:** Users can grep manually or refactor code to use unique names

#### c) No Multi-Version Support
- Only one workspace root per LSP instance
- Multi-workspace projects (e.g., Android + iOS in same repo) require separate LSP servers
- Workspace root can be changed via `kotlin-lsp/changeRoot` command (requires LSP restart)

#### d) No Incremental Parse
- File changes require full re-parse on next indexing pass
- 120ms debounce mitigates thrashing, but large edits still cause pause

#### e) Limited Language Coverage
- **Kotlin:** Full support (syntax, imports, hierarchy)
- **Java:** Full support (syntax, imports, hierarchy)
- **Swift:** Partial (hover works; some advanced features untested)
- **Other JVM languages** (Scala, Groovy, Clojure): Not supported

### 5) Security Risks

| Risk | Severity | Mitigation |
|------|----------|-----------|
| **Arbitrary command execution via rg** | 🟡 **MEDIUM** | Workspace root restricted to LSP client's specified folder; rg invocations scoped to workspace |
| **Malicious .gitignore patterns** | 🟡 **MEDIUM** | `ignore` crate sanitizes glob patterns; no shell metacharacter injection |
| **File read during scan** | 🟢 **LOW** | Only indexes source files; non-readable files skip gracefully (logged) |
| **Cache poisoning** | 🟢 **LOW** | Cache file has SHA2 checksum of workspace root; invalid cache discarded |
| **Symlink following** | 🟢 **LOW** | `fd` and `walkdir` have symlink recursion limits (default: none follow) |

**No secrets exposure:** LSP never logs credentials, auth tokens, or user data.

### 6) Performance Bottlenecks

| Bottleneck | Symptom | Mitigation |
|------------|---------|-----------|
| **Large iOS projects (50K+ files)** | Initial indexing can take 30-60s | Unlimited file indexing v0.9.3; query caching; concurrent parse workers |
| **Hover on common symbols** | Type inference searches all supertypes (BFS) | Cache completion results per type; early termination on match |
| **References on common names** | `rg --word-regexp MethodName` returns entire codebase | Educate users; recommend renaming to domain-specific names |
| **Live tree updates on huge files** | `didChange` event can delay if file has 10K+ lines | Background task + debounce (120ms) mitigates |
| **On-demand index builds** | First reference search without pre-indexed project can be slow | Pre-index workspace on initialize (v0.9.3 does this) |

### 7) Evidence

- src/indexer.rs (85 commits, trait refactoring in progress)
- src/parser.rs (61 KB; mix of symbol extraction + visibility rules)
- src/indexer/resolution.rs (44 KB; type substitution + enrichment intertwined)
- Scan output: HIGH-CHURN FILES section, CODE METRICS section
- Recent refactor commits: "Phase 12 structural refactoring", "ProgressReporter port refactor"
- README.md (Features section; Known Limitations implicit)

## Extended Sections (Optional)

### Phase 12 Refactoring Status

**Objective:** Decouple application layer from LSP framework (Ports & Adapters pattern).

**Work in Progress:**
- ✅ Replace `Option<tower_lsp::Client>` with `ProgressReporter` trait (v0.9.3+)
- ✅ Move framework types to adapter layer (KotlinProgress in backend/mod.rs)
- ✅ Downgrade `pub` to `pub(crate)` across codebase (visibility cleanup)
- ✅ Fix anti-patterns (bare unwrap, double dereferences, blocking I/O)
- ⏳ Split `parser.rs` into smaller modules (planned Phase 13)
- ⏳ Consolidate `indexer/lookup.rs` into `resolver/` (already migrated in recent commits)

**Impact:** This refactoring enables future language server implementations (e.g., Swift LSP) without LSP framework coupling.

### Dependency Risk Assessment

| Dependency | Churn | Risk | Recommendation |
|------------|-------|------|-----------------|
| tree-sitter-kotlin | Moderate | Upstream grammar changes; re-export needed if fwcd changes | Monitor upstream; pin to working versions (currently 0.3) |
| tower-lsp | Low | Async runtime is stable; unlikely breaking changes | Keep up-to-date with security patches |
| serde | Low | Mature, widely used serialization; stable API | Update cautiously; test cache compatibility |

### Memory Footprint Concerns

**Baseline:** ~30 MB for small projects; up to 200 MB for large Android repos.

- **DashMap overhead:** Concurrent HashMap ~10-20% overhead vs. standard HashMap
- **String duplication:** Source lines stored in Arc<Vec<String>>; ~15 KB per file (750 bytes/line × 20 lines typical)
- **Symbol entries:** ~200 bytes each (name, kind, range, visibility, detail)
- **Index growth:** Unbounded (as of v0.9.3); no automatic pruning

**Risk:** Memory could exceed 500 MB on very large projects (100K+ files). No current mitigation.

**Recommendation:** Monitor on real projects; consider LRU cache or symbol pruning if needed.

### Test Coverage Gaps

**Untested areas:**
1. Full LSP protocol integration (JSON-RPC, stdio transport) — tested manually in editors
2. Rename on cross-file definitions — tested minimally
3. Inlay hints on complex nested generics — edge cases not exhaustively covered
4. Performance regression — no benchmarks configured
5. Formatter / code actions — minimal test coverage

**Recommendation:** Add integration test suite with full LSP protocol simulation (e.g., tower-lsp's own test setup).

### Known Workarounds

| Issue | Workaround |
|-------|-----------|
| Need to index extra library sources | Use `sourcePaths` configuration + contrib/extract-sources.py |
| LSP doesn't find symbol in huge project | Restart LSP or manually clear cache (`~/.cache/kotlin-lsp/`) |
| Hover shows wrong type for overloaded method | LSP resolution may pick first candidate; user can disambiguate via go-to-definition |

### Future Debt

**Likely future work:**
- Type-aware reference resolution (currently name-based)
- Multi-workspace support (currently single root)
- Incremental parsing (currently full re-parse on change)
- Language Server Protocol v3.18 features (streaming diagnostics, semantic tokens)
- Performance benchmarking and regression testing
- Improved error recovery (parser should not panic on malformed input)

### Copilot CLI Integration Notes

The `.github/extensions/kotlin-lsp/extension.mjs` provides custom commands for Copilot:
- `kotlin_lsp_status` — reads `~/.cache/kotlin-lsp/status.json`
- `kotlin_lsp_set_workspace` — writes to `~/.config/kotlin-lsp/workspace`

**Risk:** If cache file format changes, Copilot extension will break. Coordinate updates carefully.
