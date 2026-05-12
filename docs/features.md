# Features

## LSP capabilities

| LSP capability | Notes |
|---|---|
| `textDocument/definition` | Index lookup → superclass hierarchy → `rg` fallback |
| `textDocument/hover` | Declaration kind, source line, lambda param types, Kotlin stdlib signatures |
| `textDocument/documentSymbol` | All symbols in the current file (outline view) |
| `textDocument/completion` | Dot-completion (`it.`, `this.`, named params), bare-word, stdlib entries |
| `completionItem/resolve` | Lazy KDoc/Javadoc + signature on item selection; keeps initial list fast |
| `textDocument/references` | Project-wide `rg --word-regexp` + in-memory scan of open buffers |
| `textDocument/signatureHelp` | Active function signature + highlighted parameter as you type |
| `textDocument/rename` | Renames symbol across all files via `WorkspaceEdit`; index updated via file watcher |
| `textDocument/foldingRange` | Brace-based region folds + consecutive comment block folds |
| `textDocument/inlayHint` | Type hints for lambda `it`, named lambda params, `this`, untyped `val`/`var` |
| `textDocument/semanticTokens/full` | Two-phase: Phase 1 CST classification + Phase 2 cross-file resolution. Kotlin, Java, Swift |
| `textDocument/publishDiagnostics` | Syntax errors from tree-sitter (ERROR/MISSING nodes) — not type checking |
| `textDocument/implementation` | Transitive subtype lookup (interface → all implementing classes, BFS) |
| `textDocument/documentHighlight` | Highlights all in-file occurrences; declaration sites marked WRITE, usages READ |
| `workspace/symbol` | Fuzzy substring search; supports dot-qualified queries for extension functions |
| `$/progress` | Spinner while workspace is indexed; non-blocking |
| `textDocument/didSave` | Re-indexes the saved file so external formatters/codegen are picked up |

## Not yet implemented

The following LSP features are commonly supported by full language servers and
are candidates for future work. Rough effort estimates assume tree-sitter
parsing only (no type resolution):

| LSP capability | Effort | Notes |
|---|---|---|
| ~~`textDocument/semanticTokens/full`~~ | ~~High~~ | ✅ **Implemented in 0.11.0.** Two-phase pipeline: Phase 1 (CST classification) + Phase 2 (cross-file index resolution). Kotlin, Java, Swift. |
| `textDocument/prepareCallHierarchy` + `callHierarchy/incomingCalls` + `callHierarchy/outgoingCalls` | Medium | Call tree viewer. Would need `rg`-based caller search similar to `references`. |
| `textDocument/selectionRange` | Medium | Smart expand-selection by CST node boundaries. tree-sitter has the structure. |
| `completionItem` — `deprecated` tag (`CompletionItemTag::DEPRECATED`) | Medium | Strikethrough for `@Deprecated`/`@deprecated` symbols. Requires detecting the annotation at index time and storing a flag on `SymbolEntry`. |
| `completionItem` — `label_details` | Medium | Inline param list + right-aligned return type in the completion list (RA-style). Would require splitting `SymbolEntry.detail` into params + return type at parse time. |
| `textDocument/gotoDeclaration` | Trivial | Identical to `goto_definition` for our use-case (no separate declaration/definition concept in Kotlin/Java). |
| `textDocument/typeDefinition` | Medium | Jump to the type of a variable. Requires type inference beyond what tree-sitter provides without the compiler. |
| `textDocument/codeAction` — quick-fixes | Medium | Currently only "introduce local variable" and "add import alias" are implemented. Missing: add missing import, generate override stubs, suppress warning. |
| `textDocument/onTypeFormatting` | Low | Auto-indent / brace matching as you type. |
| `textDocument/formatting` | Low | Delegate to `ktfmt` / `google-java-format` subprocess if available on `$PATH`. |

## Known UX gaps (scouted from JetBrains kotlin-lsp, 2026-05)

Small improvements identified by comparing against the JetBrains reference implementation. None require type resolution.

| Area | What to change | Effort |
|---|---|---|
| **Hover — backtick identifiers** | Change ` ```kotlin ` to ` ````kotlin ` (quadruple backtick fence) in `src/backend/format.rs`. Kotlin identifiers can contain backticks (`` `my fun` ``); a triple-backtick fence breaks the Markdown block. | Trivial |
| **Completion item kind — METHOD** | `symbol_kind_to_completion` in `src/resolver/complete.rs` maps both `SymbolKind::FUNCTION` and `SymbolKind::METHOD` to `CompletionItemKind::FUNCTION`. Should map METHOD → `CompletionItemKind::METHOD` so editors show the correct icon. | Trivial |
| **FoldingRange — import block** | Detect `import` blocks (consecutive lines starting with `import`) and emit `FoldingRangeKind::Imports` instead of `Region`. Currently all folds are `Region`. (`src/backend/handlers.rs`) | Low |
| **FoldingRange — block comments** | Detect `/* … */` multi-line comments and fold with `FoldingRangeKind::Comment`. Currently only `//` line-comment blocks are folded. | Low |
| **FoldingRange — collapsedText** | Set `collapsed_text` on every fold range (e.g. `"..."` for blocks, `"imports"` for import folds). Improves editor display when a region is collapsed. | Trivial |

## CLI subcommands

`kotlin-lsp` ships with a standalone CLI in addition to the LSP server.

```
kotlin-lsp sources [--root <dir>] [--json]
```
Lists every source root that would be auto-discovered for a project (from `workspace.json` or standard Gradle/Maven build layout). Marks which paths exist on disk. Run this to verify the indexer will find your sources without starting the server.

If source roots are missing, the command suggests `extract-sources` as a next step.

```
kotlin-lsp complete <file> <line> [--dot] [--eol] [--no-stdlib] [--json] [--root <dir>]
```
Returns completion candidates for the cursor position without starting the LSP daemon — useful for shell integrations, editor plugins, and testing.

| Flag | Description |
|---|---|
| `--dot` | Auto-place cursor immediately after the last `.` on the line |
| `--eol` | Auto-place cursor at the end of the line |
| `--no-stdlib` | Skip `~/.kotlin-lsp/sources` for project-only results (~5× faster) |
| `--json` | Output as JSON `[{label, kind, detail?, import?}]` |

Output (plain text):
```
Column     (function)  fun Column(modifier: Modifier, content: @Composable () -> Unit)
Row        (function)  fun Row(modifier: Modifier, content: @Composable () -> Unit)
```

```
kotlin-lsp extract-sources [PATTERN…] [OPTIONS]
```
Unpacks `*-sources.jar` files from the Gradle module cache so the LSP server can serve hover docs and go-to-definition for library code.

| Option | Default | Description |
|---|---|---|
| `PATTERN…` | (all) | Substring filter on artifact path, e.g. `androidx.compose` `org.jetbrains.kotlin` |
| `--gradle-home <dir>` | `$GRADLE_USER_HOME` or `~/.gradle` | Gradle home directory |
| `--output <dir>` | `~/.kotlin-lsp/sources` | Extraction root |
| `--dry-run` | off | Print what would be extracted; write nothing |

**Typical workflow:**

```sh
# 1. Check what source roots are auto-detected
kotlin-lsp sources --root ./android

# 2. Extract library sources (first time, or after a Gradle sync)
kotlin-lsp extract-sources androidx.compose org.jetbrains.kotlin

# Android SDK, workspace.json, and ~/.kotlin-lsp/sources are picked up automatically.
# 3. Re-index (or restart the server) to pick up new sources
kotlin-lsp index --root ./android
```

The extractor deduplicates by artifact — when multiple versions are cached, only the latest is extracted.

## What gets indexed

**Kotlin:** `class`, `interface`, `object`, `fun`, `val`, `var`, `typealias`, constructor parameters, enum entries  
**Java:** `class`, `interface`, `enum`, `method`, `field`, `enum_constant`  
**Swift:** `class`, `struct`, `enum`, `protocol`, `func`, `let`, `var`, `typealias`, `extension`, `init`, enum cases

## Resolution chain

Go-to-definition resolves symbols in this order:

1. **Local file** — indexed symbols in the same file
2. **Local variables / parameters** — line-scanned, catches un-annotated `fun` params
3. **Explicit imports** — exact FQN lookup, then package-filtered index, then `fd` on-demand
4. **Same package** — symbols in files sharing the same `package` declaration
5. **Star imports** — `import com.example.*` checked in the package dir
6. **Superclass hierarchy** — inherited methods from `extends`/`implements`/Kotlin delegation specifiers, up to 4 levels deep, cycle-safe
7. **Project-wide `rg`** — last resort; always finds symbols not yet indexed

`this.member` searches the current class + its supers.  
`super.member` skips the current class and walks the hierarchy directly.

## Completion details

- **Dot-completion** (`repo.`) — resolves the variable's declared type, finds the matching file, returns its public members. Private members are hidden.
- **Bare-word completion** — matches symbols from the current file and the workspace index by prefix (case-aware: lowercase prefix → lowercase suggestions first).
- **Kotlin stdlib** — scope functions (`run`, `apply`, `let`, `also`, `with`), collection extensions (`map`, `filter`, `find`, …), string extensions, and nullable helpers all appear in completion with proper signatures. They sort after project symbols.
- **Lazy loading** — files beyond the initial index limit are parsed on-demand the first time you trigger completion on one of their types.
- **Pre-warming** — when you open a file, its injected/constructor types are pre-warmed in the background so the first dot-completion is instant.
- **Live line scanning** — dot-detection uses the current document text (not the debounced index) so typing `.`, deleting it, and re-typing it always works correctly.
- **Visibility filtering** — `private` members are hidden from dot-completion; `protected`/`internal` members are shown.

## Completion ranking

Completions are scored by match quality:

| Score | Match type | Example |
|---|---|---|
| 0 | Exact prefix (case-insensitive) | `Col` → **Col**umn |
| 1 | CamelCase acronym | `CB` → **C**olumn**B**utton |
| 2 | Substring (same-file/package only) | `View` → RecyclerView |

Results are capped at 150 items; `isIncomplete: true` is returned so the client re-queries as you type.

**Context-aware filtering:**
- Lowercase prefix → only functions, vars, params
- Uppercase prefix → only classes, objects, types
- `@` prefix → only annotation/class kinds
- Cross-package symbols require prefix ≥ 2 characters

## Auto-import

When completing an unimported symbol:

- Start typing a class name (uppercase, ≥ 2 chars) → candidates appear from all indexed files including `sourcePaths`
- Select a candidate → symbol inserted **and** `import pkg.ClassName` added at the correct position
- Same-named classes from different packages appear as separate items with the package in the detail column
- Already-imported symbols appear without a duplicate edit
- Star imports (`import pkg.*`) are respected — no redundant explicit import added

## Ignore pattern semantics

| Pattern | Matches |
|---|---|
| `bazel-*` | Any dir/file named `bazel-*` at **any depth** |
| `third-party/**` | Everything inside `third-party/` relative to workspace root |
| `/abs/path/**` | Absolute path — normalized to relative before matching |

Patterns apply to both `fd` (fast path) and the `walkdir` fallback, and filter the warm-start cached manifest so newly added patterns take effect without clearing the cache.

## Source path behaviour

| Behaviour | `sourcePaths` files |
|---|---|
| Hover / go-to-definition | ✓ |
| Autocomplete | ✓ |
| `findReferences` | ✗ (excluded) |
| `rename` | ✗ (excluded) |

Paths can be absolute (including `~/…`) or relative to the workspace root. The full path is trusted — standard excludes (`.gradle`, `build`, `target`) are not applied.

- Single-hop: `ClassName`, `functionName`, `CONSTANT`
- Multi-hop field chains: `account.profile.email`
- Constructor parameter declarations (without `val`/`var`)
- Lambda parameters: `{ account -> account.name }` jumps to the `account ->` binding
- `this.method()` and `super.method()` qualifier handling
- Precise `fd --full-path` search uses the full package path from the import, not just the filename — dramatically faster in multi-module projects
- Cross-file fallback via `rg` for symbols not yet in the index
