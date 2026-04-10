# kotlin-lsp

A fast, low-memory LSP server for **Kotlin** and **Java**, written in Rust.  
Built with [tower-lsp](https://github.com/ebkalderon/tower-lsp) and [tree-sitter](https://tree-sitter.github.io/), designed for large Android/JVM codebases where heavier LSP servers feel sluggish.

![kotlin-lsp demo](demo/demo.gif)

## Install

```bash
cargo install kotlin-lsp
```

The binary is placed at `~/.cargo/bin/kotlin-lsp`. Use that path in your editor config below.

> **Runtime dependencies** — `fd` and `rg` (ripgrep) must be on your `PATH`:  
> macOS: `brew install fd ripgrep`  
> Debian/Ubuntu: `apt install fd-find ripgrep`

---

## vs. Official Kotlin LSP

There are two Kotlin LSP implementations. Here is how they differ:

| | **kotlin-lsp** (this project) | **[Kotlin/kotlin-lsp](https://github.com/Kotlin/kotlin-lsp)** (JetBrains, pre-alpha) |
|---|---|---|
| **Status** | Stable, usable today | Experimental / pre-alpha, no stability guarantees |
| **Runtime** | Native Rust binary, no JVM | Requires JVM 17+, ~500 MB download |
| **Startup** | Instant | Gradle project import required (slow on large projects) |
| **Memory** | < 200 MB for 10k+ files | IntelliJ engine — typically 1+ GB |
| **Build system** | Any (no build required) | JVM-only Gradle projects officially supported |
| **Type checking / diagnostics** | Syntax errors (tree-sitter) | ✓ on-the-fly Kotlin diagnostics |
| **Semantic accuracy** | Syntactic (tree-sitter) | Full IntelliJ Analysis API |
| **Rename / refactor** | ✓ project-wide text search | ✓ semantic (+ Move, Change signature) |
| **Go-to-definition** | Fast (index + rg fallback) | Accurate (binary + source, builtins) |
| **Completion** | Fast, type-aware for lambdas | Analysis-API based, full type inference |
| **Signature help** | ✓ | ✓ |
| **KDoc hover** | ✓ stdlib built-in | ✓ in-project + dependency source jars |
| **Java support** | ✓ (structural) | ✓ (semantic) |
| **Editor support** | Any LSP editor | VS Code (official), others experimental |
| **Source available** | ✓ fully open source | Partially closed-source (temporary) |
| **Best for** | Fast navigation in large Android repos, any LSP editor, no build setup | Full IDE-grade experience in VS Code once stable |

**Choose kotlin-lsp** when you want instant startup, zero build-system configuration, low memory, and broad editor support (Helix, Neovim, Emacs, etc.) — and can rely on Gradle/CI for error reporting.

**Choose Kotlin/kotlin-lsp** when you need full compiler diagnostics, semantic refactoring, and are working in VS Code with a Gradle JVM project.

They can coexist: configure kotlin-lsp for fast navigation and use the official one for diagnostics when it stabilises.

---

## Features

| LSP capability | Notes |
|---|---|
| `textDocument/definition` | Index lookup → superclass hierarchy → `rg` fallback |
| `textDocument/hover` | Declaration kind, source line, lambda param types, Kotlin stdlib signatures |
| `textDocument/documentSymbol` | All symbols in the current file (outline view) |
| `textDocument/completion` | Dot-completion (`it.`, `this.`, named params), bare-word, stdlib entries |
| `textDocument/references` | Project-wide `rg --word-regexp` + in-memory scan of open buffers |
| `textDocument/signatureHelp` | Active function signature + highlighted parameter as you type |
| `textDocument/rename` | Renames symbol across all files via `WorkspaceEdit`; index updated via file watcher |
| `textDocument/foldingRange` | Brace-based region folds + consecutive comment block folds |
| `textDocument/inlayHint` | Type hints for lambda `it`, named lambda params, `this`, untyped `val`/`var` |
| `textDocument/publishDiagnostics` | Syntax errors from tree-sitter (ERROR/MISSING nodes) — not type checking |
| `textDocument/implementation` | Transitive subtype lookup (interface → all implementing classes, BFS) |
| `workspace/symbol` | Fuzzy substring search; supports dot-qualified queries for extension functions |
| `$/progress` | Spinner while workspace is indexed; non-blocking |

### What gets indexed

**Kotlin:** `class`, `interface`, `object`, `fun`, `val`, `var`, `typealias`, constructor parameters, enum entries  
**Java:** `class`, `interface`, `enum`, `method`, `field`, `enum_constant`

### Resolution chain

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

### Completion details

- **Dot-completion** (`repo.`) — resolves the variable's declared type, finds the matching file, returns its public members. Private members are hidden.
- **Bare-word completion** — matches symbols from the current file and the workspace index by prefix (case-aware: lowercase prefix → lowercase suggestions first).
- **Kotlin stdlib** — scope functions (`run`, `apply`, `let`, `also`, `with`), collection extensions (`map`, `filter`, `find`, …), string extensions, and nullable helpers all appear in completion with proper signatures. They sort after project symbols.
- **Lazy loading** — files beyond the initial index limit are parsed on-demand the first time you trigger completion on one of their types.
- **Pre-warming** — when you open a file, its injected/constructor types are pre-warmed in the background so the first Ctrl+X is instant.
- **Live line scanning** — dot-detection uses the current document text (not the debounced index) so typing `.`, deleting it, and re-typing it always works correctly.
- **Visibility filtering** — `private` members are hidden from dot-completion; `protected`/`internal` members are shown.

### Definition / Go-to

- Single-hop: `ClassName`, `functionName`, `CONSTANT`
- Multi-hop field chains: `account.profile.email`
- Constructor parameter declarations (without `val`/`var`)
- Lambda parameters: `{ account -> account.name }` jumps to the `account ->` binding
- `this.method()` and `super.method()` qualifier handling
- Precise `fd --full-path` search uses the full package path from the import, not just the filename — dramatically faster in multi-module projects
- Cross-file fallback via `rg` for symbols not yet in the index

---

## Limitations

- **No type inference for generic lambda parameters** — `list.map { item -> item.field }` cannot resolve `item`'s type from generic parameters without full type inference. Named-arg and trailing lambdas with known function signatures are resolved cross-file (with `rg` fallback if the dependency isn't indexed yet). For unresolvable cases, use explicit type annotations (`list.map { item: MyType -> … }`).
- **No incremental re-index** — each `did_change` re-parses the whole file after a 120 ms debounce. Very large files (5000+ lines) may feel slightly delayed.
- **No type checking** — kotlin-lsp reports structural syntax errors (unmatched braces, missing tokens) but does not compile or type-check your code. Use Gradle/CI for semantic diagnostics.
- **Visibility is line-scanned** — visibility is detected from the declaration line. Multi-line modifier blocks (modifier on a separate line) default to `public`.
- **`protected` not filtered** — protected members appear in dot-completion from outside the class hierarchy.
- **Nested lambda scope** — variables introduced by nested lambdas (e.g. inner `.map {}` inside outer `.mapSuccess {}`) are not resolved.
- **Java support is lighter** — definition and hover work; completion is present but less refined than Kotlin.
- **Index cap** — by default only the 2 000 shallowest files are indexed eagerly (configurable; see below). Deeper files are resolved on-demand.

---

## Build from source

If you want to build from source instead of `cargo install`:

**Requirements:** Rust 1.76+, a C compiler (for tree-sitter grammars), `fd`, `rg`

```bash
git clone <this-repo>
cd kotlin-lsp
cargo build --release
# binary: target/release/kotlin-lsp
```

> **Tip:** If `tree-sitter-kotlin = "0.3"` fails to resolve, replace it in `Cargo.toml`:
> ```toml
> tree-sitter-kotlin = { git = "https://github.com/fwcd/tree-sitter-kotlin" }
> ```

### Runtime dependencies

| Tool | Purpose |
|---|---|
| [`fd`](https://github.com/sharkdp/fd) | Workspace file discovery |
| [`rg`](https://github.com/BurntSushi/ripgrep) | Cross-file fallback for symbols not in the index |

Install on macOS: `brew install fd ripgrep`  
Install on Debian/Ubuntu: `apt install fd-find ripgrep` (binary may be `fdfind`)

---

## Editor setup

Replace `/path/to/kotlin-lsp` with `~/.cargo/bin/kotlin-lsp` (or wherever `cargo install` placed it — run `which kotlin-lsp` to confirm).

### Helix

Add to `~/.config/helix/languages.toml`:

```toml
[[language]]
name = "kotlin"
language-servers = ["kotlin-lsp"]
auto-format = false

[[language]]
name = "java"
language-servers = ["kotlin-lsp"]
auto-format = false

[language-server.kotlin-lsp]
command = "/path/to/kotlin-lsp"
```

Then restart Helix (or run `:lsp-restart`).  
Check the server is running: `:lsp-workspace-command` or watch `:log-open`.

### Neovim (nvim-lspconfig)

```lua
local lspconfig = require('lspconfig')
local configs   = require('lspconfig.configs')

if not configs.kotlin_lsp then
  configs.kotlin_lsp = {
    default_config = {
      cmd       = { '/path/to/kotlin-lsp' },
      filetypes = { 'kotlin', 'java' },
      root_dir  = lspconfig.util.root_pattern(
        'build.gradle', 'build.gradle.kts', 'pom.xml', 'settings.gradle', '.git'
      ),
      settings  = {},
    },
  }
end

lspconfig.kotlin_lsp.setup {}
```

Place this in your `init.lua` (or a dedicated `after/ftplugin/kotlin.lua`).

**Completion** — pair with [nvim-cmp](https://github.com/hrsh7th/nvim-cmp):

```lua
require('cmp').setup {
  sources = {
    { name = 'nvim_lsp' },
    -- other sources …
  },
}
```

### VS Code

VS Code does not support arbitrary LSP binaries natively. Use the
[**Custom Language Server**](https://marketplace.visualstudio.com/items?itemName=cesium.custom-language-server)
extension, then add to `.vscode/settings.json`:

```json
{
  "custom-language-server.servers": [
    {
      "name": "kotlin-lsp",
      "command": "/path/to/kotlin-lsp",
      "filetypes": ["kotlin", "java"]
    }
  ]
}
```

> **Note:** The [Kotlin language plugin](https://marketplace.visualstudio.com/items?itemName=mathiasfrohlich.Kotlin) must be installed so VS Code recognises `.kt` files as `kotlin`.  
> For a production-grade Kotlin experience in VS Code, consider [Kotlin Language Server](https://github.com/fwcd/kotlin-language-server) alongside this one (they can coexist on different capabilities).

---

## Configuration

Set environment variables before launching the binary (or in your editor's LSP env config):

| Variable | Default | Description |
|---|---|---|
| `KOTLIN_LSP_MAX_FILES` | `2000` | Max files indexed eagerly at startup. Files beyond this are parsed on-demand. |
| `KOTLIN_LSP_WORKSPACE_ROOT` | _(none)_ | Override the workspace root sent by the LSP client. Useful when the client is started from a different directory. |

You can also set the workspace root via a config file (takes lower priority than the env var):

```bash
echo "/path/to/your/project" > ~/.config/kotlin-lsp/workspace
```

This is read on every startup, so it persists across LSP server restarts without needing to change editor config.

Example for Helix:

```toml
[language-server.kotlin-lsp]
command = "/path/to/kotlin-lsp"
environment = { KOTLIN_LSP_MAX_FILES = "4000" }
```

---

## GitHub Copilot CLI agent

kotlin-lsp integrates with the [GitHub Copilot CLI](https://githubnext.com/projects/copilot-cli/) to give Copilot full code-intelligence tools when working on Kotlin/Java projects.

> **Requires:** `copilot --experimental` (or `--exp`) — the `lsp` tool is only available in experimental mode.

### Setup

**1. Add kotlin-lsp to Copilot's LSP config** (`~/.copilot/lsp-config.json`):

```json
{
  "lspServers": {
    "kotlin-lsp": {
      "command": "/path/to/kotlin-lsp",
      "args": [],
      "env": {
        "KOTLIN_LSP_MAX_FILES": "20000"
      },
      "fileExtensions": {
        ".kt": "kotlin",
        ".kts": "kotlin",
        ".java": "java"
      }
    }
  }
}
```

**2. (Optional) Set a fixed workspace root** so Copilot always indexes your project regardless of which directory it's started from:

```bash
mkdir -p ~/.config/kotlin-lsp
echo "/path/to/your/project" > ~/.config/kotlin-lsp/workspace
```

**3. (Optional) Install the Copilot skill extension** for a richer agent experience — it injects indexing status context automatically and provides `kotlin_lsp_status` and `kotlin_lsp_set_workspace` tools:

```bash
# Copy the extension to your Copilot user extensions directory
mkdir -p ~/.copilot/extensions/kotlin-lsp
cp contrib/copilot-extension/extension.mjs ~/.copilot/extensions/kotlin-lsp/
```

The extension provides:
- **`kotlin_lsp_status`** — check indexing phase, file counts, symbol count, and ETA before running queries
- **`kotlin_lsp_set_workspace`** — switch the indexed project at runtime without restarting Copilot
- **Auto-injected context** — when you open a session in a Kotlin project, indexing status and LSP capabilities are injected automatically

### Agentic workflow

Once configured, Copilot can navigate your codebase using:

```
lsp workspaceSymbol "MyClass"         → find any class/function by name (includes signature)
lsp documentSymbol <file>             → list all symbols in a file with line numbers
lsp hover <file> <line> <col>         → get type signature and docs at a position
lsp goToDefinition <file> <line> <col>→ jump to the definition
lsp findReferences <file> <line> <col>→ find all usages across the project
lsp incomingCalls <file> <line> <col> → find all callers of a function
lsp outgoingCalls <file> <line> <col> → find all functions called by a function
```

**Tip:** `workspaceSymbol` results now include the declaration signature (e.g. `fun processPayment(amount: BigDecimal, currency: String): Result<Unit>`), so you rarely need a follow-up `hover` call.

---

## Architecture

```
main.rs      – tokio entry point, wires stdin/stdout to tower-lsp
backend.rs   – LanguageServer trait: initialize / hover / definition / completion / documentSymbol / references / signatureHelp / rename / foldingRange / symbol
indexer.rs   – file discovery (fd), in-memory index, rg fallback, progress reporting
parser.rs    – tree-sitter-kotlin + tree-sitter-java symbol & visibility extraction
resolver.rs  – definition resolution, multi-hop field chains, class hierarchy, completion logic
stdlib.rs    – built-in Kotlin stdlib signatures for hover and completion
types.rs     – SymbolEntry, FileData, Visibility
```

### Memory model

Each file stores symbols, import paths, declared names, and raw source lines.  
At ~50 chars/line × 300 lines/file ≈ 15 KB/file. At 2 000 files that is ~30 MB for lines alone; with symbol metadata the total stays well under 200 MB for typical Android projects.

---

## Performance notes

- **Startup** — the server starts instantly and indexes in the background. All features (hover, go-to-definition, inlay hints) work immediately via `rg` fallback — no need to wait for indexing to finish.
- **CPU** — a 120 ms debounce prevents re-parsing on every keystroke. A semaphore caps concurrent parse workers at 8 during workspace scan.
- **Content dedup** — files are only re-parsed when their content actually changes (FNV-1a hash check).
- **Completion cache** — dot-completion results are cached per type-file; cleared only when that file changes.
- **fd `--full-path` search** — when resolving an import like `com.example.data.compat.EProductScreen`, the fd command searches for `*/com/example/data/compat/EProductScreen.(kt|java)$` — a single O(1) traversal that skips unrelated modules entirely.

---

## Changelog

### 0.3.13

- **Inlay hints** — type hints for lambda `it`, named lambda params (`{ loanId, isWustenrot -> }`), `this` in scope functions, and untyped `val`/`var` declarations
- **Go-to-implementation** — transitive subtype lookup via BFS (`interface → all implementing classes`), cached in subtypes index
- **Syntax diagnostics** — tree-sitter ERROR/MISSING nodes reported as `publishDiagnostics` (unmatched braces, missing tokens)
- **Cross-file lambda resolution** — named-arg lambdas (e.g. `SheetReloadActions(loan = { loanId -> })`) resolve parameter types from constructor signatures in other files, with `rg` fallback for unindexed dependencies
- **Instant feature availability** — all features (hover, goto-def, inlay hints) work immediately via `rg` on-demand fallback, no need to wait for background indexing
- **Live-lines consistency** — hover, goto-def, and inlay hints read current editor text instead of stale index data after edits
- **Race condition fix** — semaphore permit held through `spawn_blocking` in `did_change`, preventing concurrent reindex corruption
- **Workspace symbol** — dot-qualified queries for extension functions (e.g. `StoreState.isReady`)

---

## Acknowledgements

The superclass hierarchy resolution, `this`/`super` qualifier handling, lambda parameter recognition, and `textDocument/references` implementation were inspired by ideas in [**code-compass.nvim**](https://github.com/emmanueltouzery/code-compass.nvim) by Emmanuel Touzery — a Neovim plugin that uses similar structural (non-compiler) techniques to provide navigation in Java/Kotlin projects.
