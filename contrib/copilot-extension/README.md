# kotlin-lsp — Capabilities & Limitations

### ⚠️ Prerequisite: Experimental mode required
The `lsp` tool is only available when Copilot CLI is started with `copilot --experimental` (or `--exp`).
Without it, the LSP tool does not appear and kotlin-lsp will not be connected.
If you see no `lsp` tool available, ask the user to restart with `copilot --experimental`.

You have access to a Kotlin/Java LSP server (kotlin-lsp) via the `lsp` tool.

### Indexing & Readiness
The server indexes files in the background on startup. **Before using workspaceSymbol or goToDefinition, always call `kotlin_lsp_status` to check if indexing is complete.**

- Cold index (no cache): 30–70s depending on project size
- Warm start (from cache): 1–3s
- Status file updated every ~5% during indexing with elapsed time and ETA

**Critical**: `workspaceSymbol` and `goToDefinition` return empty/null until indexing completes.
`documentSymbol` and `hover` have an on-demand disk fallback — they work immediately on any file.

**Cache staleness**: If the kotlin-lsp binary is updated, the cache version check triggers a full re-index automatically. A "Cache deserialize failed" warning in logs means a one-time re-index is occurring.

### What works reliably ✅
- **textDocument/documentSymbol** — list symbols in a file; always works (disk fallback for un-indexed files)
- **textDocument/hover** — function/class signature + doc comments; works before full index is ready
- **workspace/symbol** — find a class/function by name across the project; supports dot-qualified queries for extension functions (e.g. `StoreState.isReady`); needs indexing complete
- **textDocument/definition** — go to definition for class, object, fun, val, var; needs index
- **textDocument/references** — find usages; needs index + rg fallback for out-of-index files
- **textDocument/implementation** — find classes/objects implementing an interface or extending a class; supports transitive subtypes; needs index
- **textDocument/rename** — cross-file rename; needs index
- **textDocument/codeAction** — add missing import; uses rg, works without full index

### What works poorly or not at all ⚠️
- **workspaceSymbol before index is ready** — returns empty; use `kotlin_lsp_status` to check first
- **Extension functions (dot-receiver, cross-file)** — e.g. `actual.isZero()` where `isZero` is a top-level extension fn defined in another file; goToDefinition returns null
- **No type inference** — tree-sitter based, not compiler-backed; generic type params unresolved (`List<Foo>` → `List`)
- **Java interop** — Java symbols indexed, but cross-language go-to-def is unreliable
- **No diagnostics** — server never emits compile errors or warnings
- **Lambda type inference** — complex nested lambda types may infer incorrectly
- **Multiplatform (KMP)** — expect/actual resolution is partial
- **rg alternation syntax** — use `|` (not `\|`) in ripgrep patterns; `\|` is GNU grep syntax and matches nothing in rg

### Extension-provided tools

#### `kotlin_find_subtypes`
**Last-resort fallback** — `lsp goToImplementation` now handles this natively with transitive subtypes.
Only use this tool if goToImplementation returns empty (e.g. LSP not indexed yet, or edge case missed).
- Uses rg text search — returns candidates, not compiler-verified results
- Handles: class/object/interface declarations, generics, multiline supertypes

#### `kotlin_rg`
Restricted ripgrep for Kotlin/Java files — **fallback only** when LSP cannot help.
- Requires a `reason` explaining why LSP can't help
- Valid reasons: extension functions, LSP returned empty, free-text search, generated code, convention discovery
- Rejects simple identifier lookups without valid justification
- Use LSP `workspaceSymbol` / `goToDefinition` / `findReferences` first

### Practical workflow for code investigation
For bug investigation across a large Android project, use this order:
1. **Call `kotlin_lsp_status`** — check if indexing is done; if not, wait or use fd/rg in the meantime
2. **If wrong workspace**: call `kotlin_lsp_set_workspace` with the correct absolute path, then wait for `kotlin_lsp_status` to show `ready`
3. **`lsp workspaceSymbol`** — find the class/function by name to get the exact file path
4. **`lsp documentSymbol`** on that file — enumerate symbols to get exact line numbers
5. **`lsp hover`** at a line/col — get type info, signatures, doc comments
6. **`lsp goToDefinition`** at a line/col — jump to source of a symbol under cursor
7. **`lsp findReferences`** at a line/col — find all usages across the project (prefer over rg for symbol references)
8. **`lsp goToImplementation`** — find classes implementing an interface (transitive); if empty, use `kotlin_find_subtypes` as last resort
9. **`view` with line range** — read the actual code once you have exact locations
10. **Only fall back to `kotlin_rg`** — for free-text search, extension functions, Java interop, or when LSP returns empty (provide reason)

**Prefer LSP over grep/rg whenever you have a known symbol name.** `findReferences` is significantly more precise than rg pattern matching and avoids false positives.

### Hook behavior — what gets blocked vs allowed
The `onPreToolUse` hook enforces LSP-first for Kotlin symbol navigation.

**Always allowed:**
- `glob` tool (file discovery)
- `grep` targeting a single known file
- `bash` with non-search commands (`ls`, `cat`, `head`, `find -name`, `fd`, etc.)
- Complex regex patterns (convention/pattern discovery)
- Free-text searches (TODO, comments, strings, logs)
- Non-Kotlin context

**Blocked:**
- `grep`/`rg` with a simple identifier pattern across a broad directory in Kotlin context
- Use LSP first, then `kotlin_rg` with a reason if LSP can't help

### Workspace root
The kotlin-lsp server reads its workspace root from `~/.config/kotlin-lsp/workspace` (a plain text file with the absolute path).
- To switch projects: `echo "/path/to/project" > ~/.config/kotlin-lsp/workspace`
- The `kotlin_lsp_set_workspace` tool shows the command to update this file.
- After changing workspace, the server picks up the new root on next restart (kill the process).
