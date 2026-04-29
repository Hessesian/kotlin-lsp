# Changelog

## 0.9.1

- **CST inlay hints** — inlay hint computation replaced with a tree-sitter preorder walk; no longer scans line-by-line. `line_starts` precomputed for O(1) offset lookups; `hint_property` now uses CST initializer inference for untyped `val`/`var`.
- **Live parse trees** — each open document keeps a live tree-sitter parse tree updated on every `didChange`. CST-first paths in `lambda_params_at_col`, `enclosing_class_at`, and `find_it_element_type_in_lines_impl` use the live tree instead of backward character scans.
- **`it` inside nested lambdas no longer shows `: suspend`** — `find_as_call_arg_type` now tracks brace depth; a cursor inside `setState { it }` no longer walks out through the `{` and mis-infers the outer function's `suspend` parameter type.
- **O(1) line access in CST fast paths** — replaced `from_utf8(&doc.bytes).lines().nth(row)` (O(row)) with `live_lines` map lookups (O(1)) in scope and inference hot paths.

## 0.8.0

- **Completion relevance & ranking** — completions are now scored and sorted by match quality: exact prefix match (score 0) → camelCase acronym match (score 1, e.g. typing `CB` matches `ColumnButton`) → substring (score 2, same-file/package only). Results are capped at 150 items with `isIncomplete: true` so the client re-queries as you type, keeping the list tight. Cross-package (auto-import) symbols require a prefix of ≥ 2 characters and only include prefix/acronym matches (no substring flood). Typing after `@` restricts completions to class/annotation kinds (functions and variables are suppressed).
- **Auto-import completion** — selecting an unimported class/interface/object in completion automatically adds the correct `import` statement. Multiple classes with the same name (from different packages) appear as separate items with the package shown in the detail column. Already-imported, same-package, and star-import-covered symbols are shown without a redundant edit.
- **`sourcePaths` configuration** — index extra directories (library sources, Gradle-unpacked stubs) for hover, go-to-definition and autocomplete, while excluding them from `findReferences` and `rename`. Paths can be absolute (including `~/…`) or relative to the workspace root; no hardcoded directory excludes are applied (the user's intent is trusted). Files inside the workspace root are indexed but not excluded from findReferences.
- **`contrib/extract-sources.py`** — cross-platform Python 3 script that finds `*-sources.jar` files in the Gradle cache, deduplicates by keeping the latest version of each artifact, and extracts `.kt`/`.java` sources to `~/.kotlin-lsp/sources/` for use with `sourcePaths`. Supports substring filters (e.g. `androidx.compose`), `--dry-run`, and custom `--gradle-home`/`--output` paths.

## 0.7.1

- **`ignorePatterns` configuration** — exclude directories/files from indexing via `initializationOptions`. Supports gitignore-style globs: bare patterns (e.g. `bazel-*`) match at any depth; path-scoped patterns (e.g. `third-party/**`) match relative to the workspace root. Absolute paths under the workspace root are also accepted. Applied to both `fd` and the `walkdir` fallback, and to the warm-start cached manifest so newly configured patterns take effect without clearing the cache. See [Configuration](#configuration) in the README.
- **Swift hover keyword fix** — Swift functions now correctly show `func` instead of `fun` in hover code blocks.

## 0.7.0

- **`it`/`this` type-directed inference** — when `it` or `this` is used as a call argument (named or positional), the expected parameter type is inferred from the function signature. E.g. `.send(channel = this)` → `SendChannel`, `process(it)` → `Item`
- **`this` in receiver vs regular lambdas** — `this` inside a regular `(T) -> R` lambda now correctly hints the enclosing class instead of the lambda param; only receiver lambdas `T.() -> R` and scope functions (`run`/`apply`/`also`/`let`/`with`) hint the receiver type
- **`fun interface` recognition** — fix tree-sitter not recognising `fun interface` declarations
- **Suspend lambda type inference** — correct type inference for `suspend` lambda parameters
- **Rename regression tests** — 9 tests covering 2/3/4 occurrences same-line, multi-line, substring false-positive, UTF-16 range correctness
- **Copilot extension** — remove overly restrictive `kotlin_rg` pre-hook; all `rg` queries now pass through unconditionally

## 0.6.1

- **`super.method` go-to-def** — must not fall through to an override in the current file; resolves to the parent class declaration

## 0.6.0

- **`super`/`this` go-to-def** — `super` resolves to the parent class; `this.method` resolves via the enclosing class hierarchy
- **Multi-line constructors** — go-to-def works when the constructor spans multiple lines
- **`typealias` support** — indexed and resolved in go-to-def chains
- **Cross-module resolution** — improved supertype priority indexing for cross-module hierarchies

## 0.5.0

- **Workspace pinning** — workspace set once at `initialize` from env var / `~/.config/kotlin-lsp/workspace` / `rootUri`; never overridden at runtime by `did_open`
- **Removed `changeRoot` command** — one LSP instance per workspace; restart to switch projects
- **Outside-root file isolation** — files opened outside the workspace root are skipped for workspace-wide indexing
- **Tiered root auto-detection** — strong project markers (`settings.gradle.kts`, `Cargo.toml`) > `.git` > `Package.swift`; correctly handles mono-repos
- **Cold-start navigation** — `hover`, `goToDefinition`, `documentSymbol` work immediately on first file open via on-demand `index_content`
- **`rg` fallback at cold start** — `lines_for` reads from disk when file not yet indexed
- **Live indexing progress** — `WorkDoneProgress::Report` notifications every 500 ms with percentage
- **Extension tools** — `kotlin_lsp_status`, `kotlin_lsp_set_workspace`

## 0.4.1

- **SOLID refactoring** — pure functions, coordinator pattern, `WorkspaceIndexResult` pipeline
- **Async indexing** — concurrent file parsing with semaphore-guarded `spawn_blocking`
- **iOS indexing fixes** — non-blocking parse, deadlock prevention
- **Cache versioning** — `CACHE_VERSION` bump invalidates stale on-disk indexes
- **`--index-only` CLI mode** — headless one-shot indexing for CI/tooling

## 0.4.0

- **Swift support** — full structural indexing of `.swift` files with all LSP features; SwiftPM `.build` and Xcode `DerivedData` excluded automatically
- **Centralized parser dispatch** — `parse_by_extension()` routes `.kt`/`.java`/`.swift` to the correct tree-sitter parser
- **Dynamic file discovery** — `fd`/`rg` glob patterns and file watchers include all supported extensions

## 0.3.13

- **Inlay hints** — type hints for lambda `it`, named params, `this`, untyped `val`/`var`
- **Go-to-implementation** — transitive subtype lookup via BFS
- **Syntax diagnostics** — tree-sitter `ERROR`/`MISSING` nodes
- **Cross-file lambda resolution** — named-arg lambdas resolve parameter types from constructor signatures
- **Instant feature availability** — all features work immediately via `rg` fallback
- **Race condition fix** — semaphore permit held through `spawn_blocking`
- **Workspace symbol** — dot-qualified queries for extension functions
