# kotlin-lsp

A fast, low-memory LSP server for **Kotlin**, **Java**, and **Swift**, written in Rust.  
Built with [tower-lsp](https://github.com/ebkalderon/tower-lsp) and [tree-sitter](https://tree-sitter.github.io/), designed for large Android/JVM/iOS codebases where heavier LSP servers feel sluggish.

![kotlin-lsp demo](demo/demo.gif)

## Install

```bash
cargo install kotlin-lsp
```

> **Runtime dependencies** — `fd` and `rg` (ripgrep) must be on your `PATH`:  
> macOS: `brew install fd ripgrep`  
> Debian/Ubuntu: `apt install fd-find ripgrep`

## Quick start (Helix)

```toml
# ~/.config/helix/languages.toml
[[language]]
name = "kotlin"
language-servers = ["kotlin-lsp"]

[[language]]
name = "java"
language-servers = ["kotlin-lsp"]

[[language]]
name = "swift"
language-servers = ["kotlin-lsp"]

[language-server.kotlin-lsp]
command = "kotlin-lsp"
```

More editors: [Neovim, VS Code, Zed →](docs/editors.md)

---

## Features

| Capability | Notes |
|---|---|
| **Go-to-definition** | Index → superclass hierarchy → `rg` fallback. Multi-hop chains, lambda params, `this`/`super` |
| **Hover** | Declaration signature, lambda param types, Kotlin stdlib docs |
| **Completion** | Dot-completion with type resolution, bare-word, stdlib entries, visibility filtering |
| **References** | Project-wide `rg --word-regexp` + open buffers |
| **Document/workspace symbol** | Outline view, fuzzy search, dot-qualified extension function queries |
| **Rename** | Project-wide via `WorkspaceEdit` |
| **Inlay hints** | Lambda `it`, named params, `this`, untyped `val`/`var` |
| **Diagnostics** | Syntax errors from tree-sitter (not type checking) |
| **Go-to-implementation** | Transitive subtype lookup (BFS) |
| **Signature help** | Active parameter highlighting |
| **Folding** | Brace regions + consecutive comment blocks |

All features work immediately — `rg` fallback handles symbols before indexing finishes (applies to Kotlin, Java and Swift).

[Full feature details →](docs/features.md)

## What gets indexed

| Language | Symbols |
|---|---|
| **Kotlin** | `class`, `interface`, `object`, `fun`, `val`, `var`, `typealias`, constructor params, enum entries |
| **Java** | `class`, `interface`, `enum`, `method`, `field`, `enum_constant` |
| **Swift** | `class`, `struct`, `enum`, `protocol`, `func`, `let`, `var`, `typealias`, `extension`, `init`, enum cases |

---

## Configuration

| Variable | Default | Description |
|---|---|---|
| `KOTLIN_LSP_MAX_FILES` | `2000` | Max files indexed eagerly. Deeper files resolved on-demand. |
| `KOTLIN_LSP_WORKSPACE_ROOT` | _(auto)_ | Override workspace root. Default: LSP client's `rootUri` (your CWD). |

The workspace root resolution order:
1. `KOTLIN_LSP_WORKSPACE_ROOT` env var — always wins, pins the workspace
2. LSP client `rootUri` / `workspaceFolders` — used when the editor sends a root (normal Helix/Neovim session)
3. `~/.config/kotlin-lsp/workspace` file — fallback for clients that send no root (e.g. Copilot CLI agentic use)

---

## Limitations

- **No type inference** for generic lambda parameters — use explicit type annotations for unresolvable cases
- **No type checking** — syntax errors only (tree-sitter). Use Gradle/Xcode/CI for semantic diagnostics
- **Swift support is structural** — all symbols indexed, but no module boundaries, no closure type inference, no extension member resolution
- **Java support is lighter** than Kotlin — definition and hover work; completion less refined
- **`findReferences` on common names** returns noise — no import-aware filtering yet

---

## More

- [Feature details](docs/features.md) — resolution chain, completion, go-to-definition specifics
- [Editor setup](docs/editors.md) — Helix, Neovim, VS Code
- [GitHub Copilot CLI](docs/copilot.md) — agent integration, skill extension
- [Architecture & performance](docs/architecture.md) — source layout, memory model, build from source

---

## vs. Official Kotlin LSP

| | **kotlin-lsp** | **[Kotlin/kotlin-lsp](https://github.com/Kotlin/kotlin-lsp)** (JetBrains) |
|---|---|---|
| **Runtime** | Native Rust, no JVM | JVM 17+, ~500 MB |
| **Startup** | Instant | Gradle import (slow) |
| **Memory** | < 200 MB | 1+ GB |
| **Accuracy** | Syntactic (tree-sitter) | Full IntelliJ Analysis API |
| **Editor support** | Any LSP editor | VS Code (official) |
| **Swift** | ✓ | ✗ |

They can coexist — use kotlin-lsp for fast navigation, the official one for diagnostics when it stabilises.

---

## Changelog

### 0.5.0

- **Workspace pinning** — workspace set once at `initialize` from env var / `~/.config/kotlin-lsp/workspace` / `rootUri`; never overridden at runtime by `did_open`
- **Removed `changeRoot` command** — one LSP instance per workspace; restart to switch projects
- **Outside-root file isolation** — files opened by the LSP client outside the workspace root are skipped for workspace-wide indexing (prevents `workspaceSymbol` pollution)
- **Tiered root auto-detection** — strong project markers (`settings.gradle.kts`, `Cargo.toml`) > `.git` > `Package.swift`; correctly handles mono-repos (iOS + Android)
- **Cold-start navigation** — `hover`, `goToDefinition`, `documentSymbol` work immediately on first file open via on-demand `index_content`; no waiting for full workspace index
- **`rg` fallback at cold start** — `lines_for` reads from disk when file not yet indexed
- **Live indexing progress** — `WorkDoneProgress::Report` notifications every 500ms with percentage
- **`kotlin-lsp/clearCache`** — now advertised in `execute_command_provider` (was hidden)
- **Extension tools** — `kotlin_lsp_status` (reads live `status.json`), `kotlin_lsp_set_workspace`

### 0.4.1

- **SOLID refactoring** — pure functions, coordinator pattern, `WorkspaceIndexResult` pipeline
- **Async indexing** — concurrent file parsing with semaphore-guarded `spawn_blocking`
- **iOS indexing fixes** — non-blocking parse, deadlock prevention
- **Cache versioning** — `CACHE_VERSION` bump invalidates stale on-disk indexes
- **`--index-only` CLI mode** — headless one-shot indexing for CI/tooling

### 0.4.0

- **Swift support** — full structural indexing of `.swift` files with all LSP features. SwiftPM `.build` and Xcode `DerivedData` directories excluded automatically.
- **Centralized parser dispatch** — `parse_by_extension()` routes `.kt`/`.java`/`.swift` to the correct tree-sitter parser
- **Dynamic file discovery** — `fd`/`rg` glob patterns and file watchers automatically include all supported extensions

### 0.3.13

- **Inlay hints** — type hints for lambda `it`, named params, `this`, untyped `val`/`var`
- **Go-to-implementation** — transitive subtype lookup via BFS
- **Syntax diagnostics** — tree-sitter ERROR/MISSING nodes
- **Cross-file lambda resolution** — named-arg lambdas resolve parameter types from constructor signatures
- **Instant feature availability** — all features work immediately via `rg` fallback
- **Race condition fix** — semaphore permit held through `spawn_blocking`
- **Workspace symbol** — dot-qualified queries for extension functions

---

## Acknowledgements

Superclass hierarchy resolution, `this`/`super` qualifier handling, and lambda parameter recognition were inspired by [**code-compass.nvim**](https://github.com/emmanueltouzery/code-compass.nvim) by Emmanuel Touzery.
