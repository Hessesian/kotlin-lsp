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
| `textDocument/semanticTokens/full` | High | Full syntax-highlight override via tree-sitter token types. Data is available; mapping to `SemanticTokenTypes` is tedious. |
| `textDocument/prepareCallHierarchy` + `callHierarchy/incomingCalls` + `callHierarchy/outgoingCalls` | Medium | Call tree viewer. Would need `rg`-based caller search similar to `references`. |
| `textDocument/selectionRange` | Medium | Smart expand-selection by CST node boundaries. tree-sitter has the structure. |
| `completionItem` — `deprecated` tag (`CompletionItemTag::DEPRECATED`) | Medium | Strikethrough for `@Deprecated`/`@deprecated` symbols. Requires detecting the annotation at index time and storing a flag on `SymbolEntry`. |
| `completionItem` — `label_details` | Medium | Inline param list + right-aligned return type in the completion list (RA-style). Would require splitting `SymbolEntry.detail` into params + return type at parse time. |
| `textDocument/gotoDeclaration` | Trivial | Identical to `goto_definition` for our use-case (no separate declaration/definition concept in Kotlin/Java). |
| `textDocument/typeDefinition` | Medium | Jump to the type of a variable. Requires type inference beyond what tree-sitter provides without the compiler. |
| `textDocument/codeAction` — quick-fixes | Medium | Currently only "introduce local variable" and "add import alias" are implemented. Missing: add missing import, generate override stubs, suppress warning. |
| `textDocument/onTypeFormatting` | Low | Auto-indent / brace matching as you type. |
| `textDocument/formatting` | Low | Delegate to `ktfmt` / `google-java-format` subprocess if available on `$PATH`. |



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

## Definition / Go-to

- Single-hop: `ClassName`, `functionName`, `CONSTANT`
- Multi-hop field chains: `account.profile.email`
- Constructor parameter declarations (without `val`/`var`)
- Lambda parameters: `{ account -> account.name }` jumps to the `account ->` binding
- `this.method()` and `super.method()` qualifier handling
- Precise `fd --full-path` search uses the full package path from the import, not just the filename — dramatically faster in multi-module projects
- Cross-file fallback via `rg` for symbols not yet in the index
