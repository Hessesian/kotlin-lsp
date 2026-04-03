# kotlin-lsp

A fast, low-memory LSP server for **Kotlin** and **Java**, written in Rust.  
Built with [tower-lsp](https://github.com/ebkalderon/tower-lsp) and [tree-sitter](https://tree-sitter.github.io/), designed for large Android/JVM codebases where heavier LSP servers feel sluggish.

![kotlin-lsp demo](demo/kotlin-lsp-demo.gif)

## Install

```bash
cargo install kotlin-lsp
```

The binary is placed at `~/.cargo/bin/kotlin-lsp`. Use that path in your editor config below.

> **Runtime dependencies** — `fd` and `rg` (ripgrep) must be on your `PATH`:  
> macOS: `brew install fd ripgrep`  
> Debian/Ubuntu: `apt install fd-find ripgrep`

---

## vs. Official Kotlin Language Server

There are two Kotlin LSP implementations. Here is how they differ:

| | **kotlin-lsp** (this project) | **[kotlin-language-server](https://github.com/fwcd/kotlin-language-server)** |
|---|---|---|
| **Runtime** | Native Rust binary, no JVM | Requires JVM 11+ |
| **Startup** | Instant | 3–10 seconds (JVM boot) |
| **Memory** | < 200 MB for 10k+ files | 500 MB–2 GB for large projects |
| **Type checking / diagnostics** | ✗ — structural only | ✓ full compiler diagnostics |
| **Semantic accuracy** | Syntactic (tree-sitter) | Full type resolution |
| **Rename / refactor** | ✗ | ✓ |
| **Go-to-definition** | Fast (index + rg fallback) | Accurate (type-safe) |
| **Superclass hierarchy** | ✓ | ✓ |
| **Kotlin stdlib hover** | ✓ built-in signatures | ✓ |
| **Java support** | ✓ (lighter) | ✓ |
| **Best for** | Fast navigation in large Android repos, editors without JVM support | Full IDE-grade experience |

**Choose kotlin-lsp** when you want instant startup and low memory overhead and can live without compiler diagnostics — typical for developers who run Gradle / CI for errors and just want fast jump-to-definition and completion.

**Choose kotlin-language-server** when you need error squiggles and semantic-accurate rename/refactor.

They can coexist: configure one for `filetypes = ["kotlin"]` and the other for a different set of capabilities.

---

## Features

| LSP capability | Notes |
|---|---|
| `textDocument/definition` | Index lookup → superclass hierarchy → `rg` fallback |
| `textDocument/hover` | Declaration kind, source line, Kotlin stdlib signatures |
| `textDocument/documentSymbol` | All symbols in the current file (outline view) |
| `textDocument/completion` | Dot-completion and bare-word completion with stdlib entries |
| `textDocument/references` | Project-wide `rg --word-regexp` usages |
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

- **No type inference for generic lambda parameters** — `list.map { item -> item.field }` cannot resolve `item`'s type from generic parameters without full type inference. Untyped lambda parameters that appear in the same file are found; cross-file inference is not. Use explicit type annotations (`list.map { item: MyType -> … }`) as a workaround.
- **No incremental re-index** — each `did_change` re-parses the whole file after a 120 ms debounce. Very large files (5000+ lines) may feel slightly delayed.
- **No diagnostics / type checking** — kotlin-lsp is purely structural; it doesn't compile or type-check your code.
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

Example for Helix:

```toml
[language-server.kotlin-lsp]
command = "/path/to/kotlin-lsp"
environment = { KOTLIN_LSP_MAX_FILES = "4000" }
```

---

## Architecture

```
main.rs      – tokio entry point, wires stdin/stdout to tower-lsp
backend.rs   – LanguageServer trait: initialize / hover / definition / completion / documentSymbol / references
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

- **Startup** — the server starts instantly and indexes in the background. The editor is usable before indexing completes.
- **CPU** — a 120 ms debounce prevents re-parsing on every keystroke. A semaphore caps concurrent parse workers at 8 during workspace scan.
- **Content dedup** — files are only re-parsed when their content actually changes (FNV-1a hash check).
- **Completion cache** — dot-completion results are cached per type-file; cleared only when that file changes.
- **fd `--full-path` search** — when resolving an import like `cz.moneta.data.compat.EProductScreen`, the fd command searches for `*/cz/moneta/data/compat/EProductScreen.(kt|java)$` — a single O(1) traversal that skips unrelated modules entirely.

---

## Acknowledgements

The superclass hierarchy resolution, `this`/`super` qualifier handling, lambda parameter recognition, and `textDocument/references` implementation were inspired by ideas in [**code-compass.nvim**](https://github.com/emmanueltouzery/code-compass.nvim) by Emmanuel Touzery — a Neovim plugin that uses similar structural (non-compiler) techniques to provide navigation in Java/Kotlin projects.
