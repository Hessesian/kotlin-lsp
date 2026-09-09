---
name: kmp-lsp
description: 'Kotlin/Java/Swift LSP server for code navigation in Android and iOS codebases. Use when navigating Kotlin, Java, or Swift source files: finding class definitions, listing symbols, jumping to implementations, finding all usages, checking type signatures, or switching workspace between projects. Triggers for: "find this class", "go to definition", "find references", "list symbols", "switch to android/ios", "what implements this interface", "workspace symbol", "set workspace", "index kotlin files", "code navigation".'
compatibility: 'Requires kmp-lsp binary on PATH. Run with `copilot --experimental` to activate the lsp tool. KMP_LSP_PREFER_CONFIG_ROOT=1 must be set in lsp-config.json env block for workspace switching to work correctly.'
metadata:
  version: "0.13.0"
  languages:
    - Kotlin
    - Java
    - Swift
---

# kmp-lsp — Capabilities & Limitations

### ⚠️ Prerequisite: Experimental mode required
The `lsp` tool is only available when Copilot CLI is started with `copilot --experimental` (or `--exp`).
Without it, the LSP tool does not appear and kmp-lsp will not be connected.
If you see no `lsp` tool available, ask the user to restart with `copilot --experimental`.

You have access to a Kotlin/Java/Swift LSP server (kmp-lsp) via the `lsp` tool.

### Language support
- **Kotlin / Java** — full support: indexing, hover, goToDefinition, workspaceSymbol, goToImplementation, findReferences, rename
- **Swift** — structural support: documentSymbol (immediate), hover (property types), goToDefinition (cross-module); no type inference (no sourcekit-lsp backend)

### Indexing & Readiness
The server indexes files in the background on startup. **Before using workspaceSymbol, always call `kmp_lsp_status` to check if indexing is complete.**

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
- **textDocument/definition** — go to source; works before full index (current file indexed on-demand); rg fallback for cross-file. CST-verified: two classes with an identically-named member (`User.save()` / `File.save()`) no longer cross-contaminate results.
- **textDocument/references** — find all usages; needs index + rg fallback. Same CST verification as go-to-def — same-named-member false positives are filtered, an inherited member through a subtype still resolves.
- **textDocument/implementation** — interface implementors (transitive BFS); needs index
- **textDocument/rename** — CST-verified; needs index. A local variable/parameter rename recognizes full Kotlin block scoping (`if`/`for`/`while`/`when`/`try`/`catch`/`finally`, not just function/lambda bodies) and never touches a named-argument label that happens to share the local's name. Cross-file rename **refuses with an explicit reason** (surfaced as an LSP error — e.g. Helix's status line) instead of guessing wrong, when: the identity is ambiguous, the symbol is defined in a library/JAR, or it participates in an override relationship (rename the exact declaration you mean, on either the interface or the concrete side).
- **textDocument/codeAction** — several quick-fixes, including "Import '\<fqn\>'" for a flagged missing-import diagnostic (see below — needs full index for the FQN lookup, one action per candidate FQN if the bare name is ambiguous) and `Add import alias` when the cursor is on an existing `import` line.
- **Missing-import diagnostic** (live, via `textDocument/publishDiagnostics`, `source: "kmp-lsp"`) — flags a bare class/function reference with a known FQN elsewhere in the workspace that isn't reachable from the file's own scope. Recomputes on every edit, not just file-open. Scope is intentionally conservative: only bare calls (`Foo()`) and bare type annotations (`val x: Foo`, `List<Foo>`) are checked — a reference used only as a value or as a qualifier (`Foo.bar`, `val x = Foo`) is never flagged, even when genuinely unresolved.

### What works poorly or not at all ⚠️
- **workspaceSymbol before index is ready** — returns empty; use `kmp_lsp_status` to check first
- **workspaceSymbol immediately after workspace switch** — if called before any `did_open`, may return stale results from previous workspace; open a file first to trigger switch + re-index
- **Swift: hover on function definitions** — property type hover works; function def hover not yet supported
- **Swift: goToDefinition on local function calls** — cross-module works; same-file function calls not yet resolved via Swift type system
- **Extension functions (dot-receiver, cross-file)** — use `lsp workspaceSymbol` with dot-qualified query (e.g. `ReceiverType.methodName`) instead of goToDefinition
- **No type inference** — tree-sitter based, not compiler-backed; generic type params unresolved
- **Java interop** — Java symbols indexed, but cross-language go-to-def is unreliable
- **No compiler diagnostics** — tree-sitter only; use `kmp-lsp check <file>` (syntax errors, instant) or `kmp-lsp diagnose <file> --root .` (call-arg + syntax, needs index) for post-edit checks — note `diagnose` does **not** currently include the missing-import diagnostic, only the live editor `publishDiagnostics` does
- **Missing-import diagnostic scope** — only bare calls and bare type annotations are checked (see above); an unresolved object/enum/constant reference used as a value or a qualifier is never flagged, even after waiting
- **rg alternation syntax** — use `|` (not `\|`) in ripgrep; `\|` is GNU grep syntax

### Extension-provided tools

#### `kmp_lsp_complete`
Show completion candidates at a file position — the primary tool for discovering what APIs are available in scope.
- Takes an absolute file path, 1-based line and col (cursor position **after** the last typed character)
- Returns JSON: `[{label, kind, detail?, import?}]` where `import` is the auto-import text edit
- Works for bare-word completion (class names, functions, annotations), dot-completion, and annotation context
- Requires the index to be built first (`kmp_lsp_status` phase=done, or run `kmp-lsp index`)
- Use to answer: "what classes match prefix X?", "does `@Composable` exist in scope?", "what's available after this dot?"
- Library symbols from `~/.kmp-lsp/sources` (populated by `kmp-lsp extract-sources`) appear here with import edits

```
kmp_lsp_complete file="/path/Screen.kt" line=10 col=14 root="/path/android"
```

#### `kmp_lsp_set_workspace`
Switch the Copilot CLI kmp-lsp instance to a different workspace directory.
- Writes the path to `~/.config/kmp-lsp/workspace`
- Kills only the Copilot-managed server (PID from `~/.cache/kmp-lsp/status.json`)
- Editor LSP instances are **not** affected
- Requires `KMP_LSP_PREFER_CONFIG_ROOT=1` in lsp-config.json env block

#### `kmp_lsp_status`
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
1. **`kmp_lsp_status`** — wait for indexing to complete before workspaceSymbol
2. **`lsp workspaceSymbol "ClassName"`** — get exact file path + line
3. **`lsp documentSymbol file.kt`** — enumerate all symbols in file
4. **`lsp hover file.kt line col`** — type info, signature, doc comment
5. **`lsp goToDefinition file.kt line col`** — jump to source
6. **`lsp findReferences file.kt line col`** — all usages cross-project; for common names (`Event`, `Result`, `State`) use `kmp-lsp refs <Name> --exclude-imports` instead to strip import noise. Add `-C <N>` (e.g. `-C 2`) to inline N lines of source around each hit — cuts the follow-up `view` calls otherwise needed to read every match site
7. **`lsp goToImplementation file.kt line col`** — interface subtypes (transitive)
8. **`kmp_lsp_complete file line col`** — discover available APIs / check what's in scope
9. **`view` with line range** — read code at known location
10. **`kmp-lsp check <file>`** — verify syntax after edits (instant, no index needed)
11. **`kotlin_rg`** — only for free-text, extension fns, generated code (provide reason)

#### Swift (iOS)
1. **`lsp documentSymbol file.swift`** — always works immediately; get symbols + line numbers
2. **`lsp hover file.swift line col`** — property type info; works immediately (on-demand index)
3. **`lsp goToDefinition file.swift line col`** — cross-module jump works; local func calls may not
4. **`lsp workspaceSymbol "ClassName"`** — wait for full indexing; may be empty until then
5. **`view` with line range** — read code at known location
6. **`kotlin_rg`** — for free-text, protocol conformance patterns (provide reason)

**Note**: For Swift, `documentSymbol` + `view` is often more reliable than waiting for full indexing.

### Serena MCP integration (when available)

When Serena is active (`.mcp.json` present at repo root, tools prefixed `serena-`), both layers are available simultaneously. They are complementary — use them in the same task turn without hesitation.

| Task | Preferred tool |
|---|---|
| List symbols in a file (no line needed) | `serena-get_symbols_overview` |
| Find all callers / referencing symbols | `serena-find_referencing_symbols` |
| Replace a method or class body | `serena-replace_symbol_body` |
| Find which files implement an interface | `lsp goToImplementation` (type-safe, transitive) |
| Cross-file semantic rename | `lsp rename` (compiler-aware, atomic) |
| Locate a symbol's exact file + line | `lsp workspaceSymbol` |
| Type signatures and doc comments | `lsp hover` |
| All usages of a symbol | `lsp findReferences` |

**Division of responsibility:**
- **Serena** — structural orientation and body-level edits (tree-sitter, instant, no index wait)
- **kmp-lsp** — type-safe navigation, rename, and implementation lookup (needs index)

**Practical combined workflow** (e.g. rename interface method + patch implementations):
1. `lsp goToImplementation` → find all implementors (transitive, type-safe)
2. `lsp rename` → atomic cross-file rename
3. `serena-get_symbols_overview` → locate method bodies without knowing line numbers
4. `serena-replace_symbol_body` → patch any body logic that changed semantically

**Caveats:**
- `serena-replace_symbol_body` is reliable for focused swaps; fall back to `edit` for large structural rewrites
- Serena's LSP backend (kmp-lsp) provides structural symbol awareness only — it does not feed type information back through Serena's MCP tools
- `lsp rename` requires full index; check `kmp_lsp_status` if called cold

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
The kmp-lsp server reads its workspace root from `~/.config/kmp-lsp/workspace` (plain text, absolute path).
- To switch projects: `echo "/path/to/project" > ~/.config/kmp-lsp/workspace`
- The `kmp_lsp_set_workspace` tool writes this file and kills the server to force restart.
- Without this file, the server auto-detects root from the first opened file (see auto-detection above).
- **After kill+restart**: open a file before calling `workspaceSymbol` — `did_open` triggers root detection and indexing.
