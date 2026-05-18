# Changelog

## 0.15.0

### Features

- **`textDocument/implementation`** — `go to implementation` now works for interface methods and abstract functions. Finds all concrete override sites across the workspace, handling Kotlin `override fun`, Java `@Override`, and abstract class methods. Scoped by declaring class to avoid false positives from same-name methods in unrelated classes.
- **Field and property reference scoping** — `find references` for `val`/`var`/Java fields is now scoped to files that reference the declaring class, eliminating false positives from same-named fields in unrelated classes. Declaration lines in other files are filtered out; override declarations in subtypes are kept.
- **`ThisContext` enum** — `this` type inference in receiver lambdas now returns a tri-state (`Resolved`, `InsideReceiver`, `NotFound`) instead of `Option<String>`. Callers can distinguish "inside an `apply`/`run`/`with` lambda with unknown receiver type" from "not in any receiver lambda", preventing incorrect fallback to `enclosing_class_at`.
- **Generic type substitution in lambda `it`** — `it` now resolves to the concrete element type when the receiver is a generic container (e.g. `result.getOrNull()?.also { it.field }` → `it: FamilyAccount` when `result: Result<FamilyAccount>`). Works for extension functions and multi-hop chains.
- **CST-first lambda parameter resolution** — lambda parameter type inference uses the live tree-sitter CST as the primary path. Falls back to text-scan only when no live document is available.
- **`fill_when` diagnostics and code action** — detects missing branches in `when` expressions over sealed classes and enums, and offers a "Fill missing branches" code action. Handles `is` branches, object branches, Boolean exhaustion, and smart-cast type narrowing in `when`/`is` branches.
- **Missing argument diagnostics** — call expressions with too few or mismatched arguments are flagged. Handles default parameters, varargs, `@JvmOverloads`, and Java constructor overloads.
- **Synthetic enum members** — `.entries`, `.values()`, `.valueOf()`, `.name`, `.ordinal` resolve correctly in go-to-definition, hover, and completion.
- **`infer_expr_type` extended** — expression type inference now covers boolean operations, if-expressions, range literals, and single-expression function return types. Powers inlay hints for return types.
- **Async rg enrichment with debounced inlay hint refresh** — inlay hints trigger a background `rg` pass to enrich unresolved types; results are pushed to the client via debounced refresh rather than blocking the initial response.
- **Panic-safe LSP handlers** — every LSP request handler is wrapped in a `catch_unwind` boundary. Panics produce a structured crash report (file, line, backtrace fragment) logged via `RUST_LOG` instead of crashing the server.
- **`params` field on `SymbolEntry`** — function/method symbols now carry their parameter list (extracted from CST at index time), enabling accurate call-site arity checks without an rg round-trip.
- **`container` field on `SymbolEntry`** — every symbol now records its enclosing class/object name, enabling tighter scoping in cross-file resolution.

### Performance

- **Chunked library cache** — library index (`~/.kotlin-lsp/sources`, Android SDK) is saved as 20 MB chunks instead of one large file, eliminating the end-of-indexing memory spike and enabling streaming load on startup.
- **Streaming library cache load** — chunks are deserialised and applied incrementally; peak RSS during warm start is now proportional to one chunk rather than the full library index.
- **jemalloc allocator** — switched to `tikv-jemallocator` on Linux/macOS for lower fragmentation on the DashMap-heavy workload; ~15–20% RSS reduction on large Android projects.
- **Signature lookup cache** — repeated `rg` calls to resolve the same function signature are deduplicated via an in-memory cache; measurable speedup on files with many call expressions.
- **Worker thread scaling** — Tokio worker threads now scale to available CPU cores.
- **fill_when subtype scan dedup** — sealed-class subtype discovery is cached for the duration of a single diagnostics pass.

### Bug fixes

- **CLI warm-start only seeing 56 files** — `resolve_root` was returning a relative path (`"."`) when invoked from the workspace directory, causing all workspace source files to be misclassified as library URIs and omitted from the on-disk cache. Warm starts now correctly see all indexed files.
- **Memory regression after cache fix** — after the root canonicalization fix, the 107 build-layout source roots (inside the workspace) were being re-indexed as library sources, doubling memory use. They are now correctly skipped when already covered by the workspace scan.
- **Named lambda parameter with receiver on previous line** — resolved to `:T` when the receiver type and the lambda were on different lines. Fixed by threading the correct UTF-16 column to the CST lookup.
- **CST lambda snapshot race** — stale live-doc snapshots could cause position mismatches in named-lambda-param CST lookup. Fixed by snapshotting `live_doc` once before position derivation.
- **`collect_signature` panic** — panicked when `start_line >= lines.len()`. Now returns `None` gracefully.
- **`forward_resolve_segments` dedup regression** — failed suffix incorrectly suppressed future resolution of the same suffix in a different chain context. Dedup gate now correctly keys on `(segment, resolved_prefix)`.
- **CST chain root type stripping** — fully qualified dotted types (e.g. `androidx.fragment.app.Fragment`) were stripped to `Fragment` too aggressively. The CST root now preserves the full type until the final resolution step.
- **Semantic diagnostics during scan** — diagnostics were published mid-scan, causing transient false positives. Diagnostics are now suppressed until the workspace reaches ready state, then republished.
- **`enclosing_class_at` false positives in receiver lambdas** — `this` inside an `apply`/`run` lambda with an unresolvable receiver type was incorrectly resolved to the enclosing class. The new `ThisContext::InsideReceiver` variant prevents this fallback.
- **Sibling-qualifier bleed in field references** — references with a dot-qualifier matching a sibling field (e.g. `account.value` picked up `value` from unrelated class) are now filtered by checking that the qualifier contains the declaring class name.

### Architecture (internal)

- **MVI workspace actor** (`src/workspace/`) — all mutable workspace state (index, live docs, workspace root, scan phase) is owned by a single `WorkspaceActor` driven by an event loop. Backend and CLI communicate via `WorkspaceHandle`. Eliminates a class of write-order races.
- **Features module** (`src/features/`) — LSP feature implementations (references, hover, definition, completion, rename, go-to-implementation, fill_when, signature_help, …) extracted from the 2000-line `backend/mod.rs` into focused per-feature modules.
- **Language abstraction** (`src/language/`) — per-language keyword sets and override-declaration detection extracted from scattered `if lang == Kotlin` blocks into a `Language` enum with per-variant impls.
- **Infer module split** (`src/indexer/infer/`) — the 1900-line `it_this.rs` split into `chain.rs` (navigation chain resolution), `cst_lambda.rs` (CST-backed lambda context), `receiver.rs` (receiver type inference), and `type_subst.rs` (generic type substitution).

## 0.14.0

- **`sourceRoots` scoping for rg searches** — `rg`-based references, definitions, and symbol searches are now scoped to the configured `sourceRoots` entries from `workspace.json` (IntelliJ/Android Studio module source roots). Searches no longer scan generated code or build output directories when source roots are configured. All callers (Backend, CLI fast mode, resolver step-5, infer) use a single central `Indexer::rg_scope_for_path` path so scoping is consistent across the board. Fixes [#78](https://github.com/Hessesian/kotlin-lsp/issues/78).

## 0.13.0

- **Zed extension** — `contrib/zed-extension` registers `kotlin-lsp` as a first-class Zed language server for Kotlin, Java, and Swift. Resolves the binary from `$PATH`; no symlinks or `binary.path` overrides required. Install locally with `zed --install-dev-extension contrib/zed-extension` or copy to `~/.config/zed/extensions/kotlin-lsp/`.
- **`complete` CLI subcommand** — `kotlin-lsp complete <file> <line> [col]` returns completion candidates as JSON (`[{label, kind, detail?, import?}]`). Flags: `--dot` (auto-place cursor after last `.` on the line), `--eol` (end of trimmed line), `--no-stdlib` (skip `~/.kotlin-lsp/sources` for ~5× faster project-only completions). Useful for agent/script integration without a running LSP daemon.
- **Library cache** — `sourcePaths`-indexed files are saved to a deterministic on-disk cache (`~/.cache/kotlin-lsp/library-<hash>.bin`). Subsequent restarts skip re-parsing unchanged library sources, making warm startup significantly faster on large projects with many source JARs.
- **Library visibility filtering** — symbols marked `private` or `internal` in library source files are stripped from the index. Only `public` and `protected` symbols are indexed for external libraries (inaccessible members add noise to completions and workspace symbol search).
- **Android SDK auto-detection** — the Android platform sources (`$ANDROID_HOME/sources/android-XX/`) are now indexed automatically. Detection order: `sdk.dir` in `local.properties` → `$ANDROID_HOME` → `$ANDROID_SDK_ROOT`. The highest installed API level is picked. No `sourcePaths` config or `extract-sources` needed for Android SDK classes (`Activity`, `Context`, `View`, etc.).
- **`@` completion trigger** — `@` is now a trigger character so annotation completions (`@Composable`, `@Inject`, `@Override`, …) appear immediately after typing `@`.
- **LSP smoke test suite** — `tests/lsp_smoke.rs` exercises the full server over stdio: initialization, workspace symbol, go-to-definition, hover, and inlay hints. Runs against a temp fixture without a real Android project.
- **Stack overflow fix** — `has_fun_interface_descendant` converted from recursive to iterative to prevent stack overflow on deeply nested class hierarchies.

## 0.12.1

- **Auto-include `~/.kotlin-lsp/sources` in LSP server** — after running `kotlin-lsp extract-sources`, extracted library sources are indexed automatically without any manual `sourcePaths` configuration in the LSP client.
- **Docs overhaul** — README restructured for progressive disclosure (VS Code Quick Start first, condensed config, detailed options moved to `docs/features.md`). `docs/editors.md` reordered with VS Code at the top including platform-specific `.vsix` install commands.

## 0.12.0

- **`extract-sources` CLI** — `kotlin-lsp extract-sources` walks the Gradle cache (`~/.gradle/caches/modules-2/files-2.1`), deduplicates `*-sources.jar` by keeping the latest version per artifact, and extracts `.kt`/`.java` sources to `~/.kotlin-lsp/sources`. Supports `--dry-run`, `--output`, `--gradle-home`, and optional group/artifact filter patterns. CLI commands (`find`, `refs`, `hover`, `index`) now automatically include `~/.kotlin-lsp/sources` so extracted library sources are indexed without any manual configuration.
- **`sources` CLI** — `kotlin-lsp sources` lists auto-discovered source roots and their origin (`workspace.json` or `build-layout`). Prints a tip to run `extract-sources` when build-layout detection is active.
- **Zero-config source root discovery** — the LSP server and CLI now auto-discover source roots from JetBrains `workspace.json` (exported by IntelliJ/Android Studio) and from standard Gradle/Maven build layouts (`src/main/kotlin`, `src/main/java`, per-module subprojects). No manual `sourcePaths` configuration needed for most Android projects.
- **Extension robustness** — fixed hang on large workspaces; `shutdown` is now non-blocking; top-level `object` declarations emit `STATIC` semantic token modifier.

## 0.11.0

- **Semantic tokens** — full `textDocument/semanticTokens/full` implementation with two-phase pipeline: Phase 1 (CST classification via tree-sitter) + Phase 2 (cross-file resolution via index). Supports Kotlin, Java, and Swift.
- **`tokens` CLI command** — `kotlin-lsp tokens <file>` dumps semantic tokens (CST-only by default, 19ms). `--resolve` opts into Phase 2 cross-file resolution.
- **`tree` CLI command** — `kotlin-lsp tree <file>` dumps the tree-sitter parse tree for debugging.
- **VS Code extension** — bundled extension with syntax highlighting, binary auto-discovery, and support for Kotlin, Java, and Swift files. GitHub Actions release workflow builds cross-platform binaries and packages `.vsix`.
- **Performance** — CLI `tokens` defaults to CST-only mode (19ms vs 1.1s with full index). Added `docs/performance.md` with benchmarks and profiling guide.
- **`fd` optional** — file discovery falls back to `walkdir` when `fd` is not installed.

## 0.10.0

- **CLI mode** — `kotlin-lsp find|refs|hover|index` subcommands: use kotlin-lsp as a standalone tool without an editor or daemon
- **Auto mode** — uses cached index when available, falls back to fast rg/fd automatically (no flag needed)
- **`--fast` flag** — pure rg/fd, zero startup cost; useful in scripts and CI
- **`--smart` flag** — builds index if missing, uses full cross-file accuracy
- **`--json` flag** — machine-readable output for piping/scripting
- **`--root` flag** — workspace root override; defaults to nearest `.git` dir or cwd
- **`--help` / `--version`** — standard CLI flags; work before or after subcommand
- **Helpful errors** — `--find` (common mistake) prints `'find' is a subcommand, not a flag`

## 0.9.4

- **Phase 12 refactoring complete** — replaced bool/tuple returns with named `struct`s for clarity (e.g., `ScanResult`, `NamedResult`); downgraded unreachable `pub` to `pub(crate)` across the binary crate; fixed bare `unwrap()` and double-ref anti-patterns; replaced blocking `std::fs::read_to_string` with `tokio::fs` in spawned tasks.
- **Hexagonal architecture cleanup** — replaced `Option<tower_lsp::Client>` in `Indexer` with `ProgressReporter` outbound port trait. `LspProgressReporter` adapter in backend sends `$/progress` notifications; `NoopReporter` used in CLI/tests. Fixes LSP violation where domain layer depended on protocol types.
- **Comprehensive codebase documentation** — 7 new markdown guides in `docs/codebase/` covering architecture, module structure, conventions, integrations, testing, and known concerns. Includes hexagonal layer breakdown, design patterns, concurrency model, and high-churn risk areas.
- **Feature contributor onboarding** — CodeTour (13-step walkthrough) at `.tours/feature-contributor-guide.tour` teaches how to implement a new LSP feature from handler to tests. Covers architecture layers, handler pattern, resolver logic, and test strategy.

## 0.9.3

- **Performance: no more file cap** — the default file limit is now unlimited. Previously the LSP mode only eagerly indexed 2000 files; larger projects (especially iOS) fell back to on-demand `rg` for deeper files. After the query/parser caching fix in 0.9.2, the per-file parse cost is low enough that indexing everything upfront is the right default. Use `KOTLIN_LSP_MAX_FILES` env var to set a custom limit if needed.
- **Performance: cached tree-sitter queries and parsers** — `Query` objects (the compiled S-expression query automaton) are now compiled once per process via `OnceLock` and reused across all file parses. `Parser` objects are reused per worker thread via thread-local storage. Eliminates the dominant CPU cost for large iOS codebases during indexing.

## 0.9.2

- **Generic type parameter substitution** — hover, inlay hints, and completion now resolve generic type parameters to their concrete types when inside a subclass. For example, if `DashboardProductsReducer : FlowReducer<Event, Effect, State>`, then `EffectType` is shown as `Effect` in inlay hints, hover tooltips, and completion detail. Works for:
  - Enclosing class supertypes (e.g. `FlowReducer<Event, Effect, State>`)
  - Member property type hierarchies (e.g. a `val reducer: DashboardProductsReducer` in a ViewModel gives access to `FlowReducer`'s param substitution)
  - Annotated classes where the declaration line is an annotation (scans up to 5 source lines to find the actual `<TypeParams>`)
- **Hover/inlay hint consistency** — `it`/lambda param hover now uses the same import-aware resolution as go-to-definition (`resolve_symbol` → local → imports → same-package → hierarchy), fixing cases where hover showed the wrong type (e.g. a deprecated enum instead of the local data class)
- **Hover applies enclosing-class substitution** — `it`/`this` hover applies the same substitution map as inlay hints (was previously using raw inferred type)
- **`parse_type_params` fix** — now only looks for `<>` before the first `(`, avoiding false matches on constructor parameter generic types

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
