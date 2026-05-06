---
name: kotlin-lsp
description: 'Kotlin/Java/Swift LSP server for code navigation in Android and iOS codebases. Use when navigating Kotlin, Java, or Swift source files: finding class definitions, listing symbols, jumping to implementations, finding all usages, checking type signatures, or switching workspace between projects. Triggers for: "find this class", "go to definition", "find references", "list symbols", "switch to android/ios", "what implements this interface", "workspace symbol", "set workspace", "index kotlin files", "code navigation".'
compatibility: 'Requires kotlin-lsp binary on PATH. Run with `copilot --experimental` to activate the lsp tool. KOTLIN_LSP_PREFER_CONFIG_ROOT=1 must be set in lsp-config.json env block for workspace switching to work correctly.'
metadata:
  version: "0.9.4"
  languages:
    - Kotlin
    - Java
    - Swift
---

# kotlin-lsp — Capabilities & Limitations

### ⚠️ Prerequisite: Experimental mode required
The `lsp` tool is only available when Copilot CLI is started with `copilot --experimental` (or `--exp`).
Without it, the LSP tool does not appear and kotlin-lsp will not be connected.
If you see no `lsp` tool available, ask the user to restart with `copilot --experimental`.

You have access to a Kotlin/Java/Swift LSP server (kotlin-lsp) via the `lsp` tool.

### Language support
- **Kotlin / Java** — full support: indexing, hover, goToDefinition, workspaceSymbol, goToImplementation, findReferences, rename
- **Swift** — structural support: documentSymbol (immediate), hover (property types), goToDefinition (cross-module); no type inference (no sourcekit-lsp backend)

### Indexing & Readiness
The server indexes files in the background on startup. **Before using workspaceSymbol, always call `kotlin_lsp_status` to check if indexing is complete.**

- Cold index (no cache): 30–70s depending on project size
- Warm start (from cache): 1–3s
- Progress shown in editor (0% → live % every 500ms → done)

**Cold-start navigation**: `documentSymbol`, `hover`, and `goToDefinition` work immediately on any opened file — the current file is indexed on-demand before symbol lookup. `workspaceSymbol` requires full indexing.

**Cache staleness**: cache version bump triggers a full re-index automatically. A "Cache deserialize failed" warning means a one-time re-index is occurring.

### Workspace root auto-detection
When no config file is set, `did_open` detects the workspace root using tiered marker priority:
1. **Strong markers** (nearest wins): `settings.gradle.kts`, `build.gradle`, `pom.xml`, `Cargo.toml` — subproject root in a mono-repo
2. **`.git`** — repo root; wins over weak markers
3. **Weak markers**: `Package.swift` — last resort (present at every Swift module, not reliable as root)

This correctly handles mono-repos (e.g. `android/settings.gradle.kts` beats monorepo `.git`) and Swift mono-repos (e.g. `ios/.git` beats nested `ios/Modules/*/Package.swift`).

### What works reliably ✅
- **textDocument/documentSymbol** — list symbols in a file; always works (disk fallback for un-indexed files)
- **textDocument/hover** — signature + doc comments; works before full index (on-demand index of current file)
- **workspace/symbol** — find class/function by name; supports dot-qualified extension fn queries (e.g. `StoreState.isReady`); needs full index
- **textDocument/definition** — go to source; works before full index (current file indexed on-demand); rg fallback for cross-file
- **textDocument/references** — find all usages; needs index + rg fallback
- **textDocument/implementation** — interface implementors (transitive BFS); needs index
- **textDocument/rename** — cross-file rename; needs index
- **textDocument/codeAction** — add missing import; uses rg, works without full index

### What works poorly or not at all ⚠️
- **workspaceSymbol before index is ready** — returns empty; use `kotlin_lsp_status` to check first
- **workspaceSymbol immediately after workspace switch** — if called before any `did_open`, may return stale results from previous workspace; open a file first to trigger switch + re-index
- **Swift: hover on function definitions** — property type hover works; function def hover not yet supported
- **Swift: goToDefinition on local function calls** — cross-module works; same-file function calls not yet resolved via Swift type system
- **Extension functions (dot-receiver, cross-file)** — use `lsp workspaceSymbol` with dot-qualified query (e.g. `ReceiverType.methodName`) instead of goToDefinition
- **No type inference** — tree-sitter based, not compiler-backed; generic type params unresolved
- **Java interop** — Java symbols indexed, but cross-language go-to-def is unreliable
- **No diagnostics** — server never emits compile errors or warnings
- **rg alternation syntax** — use `|` (not `\|`) in ripgrep; `\|` is GNU grep syntax

### Extension-provided tools

#### `kotlin_lsp_set_workspace`
Switch the Copilot CLI kotlin-lsp instance to a different workspace directory.
- Writes the path to `~/.config/kotlin-lsp/workspace`
- Kills only the Copilot-managed server (PID from `~/.cache/kotlin-lsp/status.json`)
- Editor LSP instances are **not** affected
- Requires `KOTLIN_LSP_PREFER_CONFIG_ROOT=1` in lsp-config.json env block

#### `kotlin_lsp_status`
Check current workspace, indexing phase, symbol count, and server PID.
Call before `workspaceSymbol` to confirm indexing is complete.

#### `kotlin_find_subtypes`
**Last-resort fallback** — `lsp goToImplementation` handles this natively with transitive subtypes.
Only use if goToImplementation returns empty (LSP not indexed yet, or edge case).
- Uses rg text search — returns candidates, not compiler-verified results

#### `kotlin_rg`
Restricted ripgrep for Kotlin/Java/Swift files — **fallback only** when LSP cannot help.
- Requires a `reason` explaining why LSP can't help
- Valid reasons: extension functions, LSP returned empty, free-text search, generated code, convention discovery
- Rejects simple identifier lookups without valid justification

### Practical workflow for code investigation

#### Kotlin/Java (Android)
1. **`kotlin_lsp_status`** — wait for indexing to complete before workspaceSymbol
2. **`lsp workspaceSymbol "ClassName"`** — get exact file path + line
3. **`lsp documentSymbol file.kt`** — enumerate all symbols in file
4. **`lsp hover file.kt line col`** — type info, signature, doc comment
5. **`lsp goToDefinition file.kt line col`** — jump to source
6. **`lsp findReferences file.kt line col`** — all usages cross-project
7. **`lsp goToImplementation file.kt line col`** — interface subtypes (transitive)
8. **`view` with line range** — read code at known location
9. **`kotlin_rg`** — only for free-text, extension fns, generated code (provide reason)

#### Swift (iOS)
1. **`lsp documentSymbol file.swift`** — always works immediately; get symbols + line numbers
2. **`lsp hover file.swift line col`** — property type info; works immediately (on-demand index)
3. **`lsp goToDefinition file.swift line col`** — cross-module jump works; local func calls may not
4. **`lsp workspaceSymbol "ClassName"`** — wait for full indexing; may be empty until then
5. **`view` with line range** — read code at known location
6. **`kotlin_rg`** — for free-text, protocol conformance patterns (provide reason)

**Note**: For Swift, `documentSymbol` + `view` is often more reliable than waiting for full indexing.

### Hook behavior — what gets blocked vs allowed
The `onPreToolUse` hook enforces LSP-first for Kotlin/Java/Swift symbol navigation.

**Always allowed:**
- `glob` tool (file discovery)
- `grep` targeting a single known file
- `bash` with non-search commands (`ls`, `cat`, `head`, `find -name`, `fd`, etc.)
- Complex regex patterns (convention/pattern discovery)
- Free-text searches (TODO, comments, strings, logs)
- Non-Kotlin/Swift context

**Blocked:**
- `grep`/`rg` with a simple identifier pattern across a broad directory in Kotlin/Swift context
- Use LSP first, then `kotlin_rg` with a reason if LSP can't help

### Workspace root
The kotlin-lsp server reads its workspace root from `~/.config/kotlin-lsp/workspace` (plain text, absolute path).
- To switch projects: `echo "/path/to/project" > ~/.config/kotlin-lsp/workspace`
- The `kotlin_lsp_set_workspace` tool writes this file and kills the server to force restart.
- Without this file, the server auto-detects root from the first opened file (see auto-detection above).
- **After kill+restart**: open a file before calling `workspaceSymbol` — `did_open` triggers root detection and indexing.
