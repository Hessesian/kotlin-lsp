# kotlin-lsp

[![crates.io](https://img.shields.io/crates/v/kotlin-lsp)](https://crates.io/crates/kotlin-lsp)
[![downloads](https://img.shields.io/crates/d/kotlin-lsp)](https://crates.io/crates/kotlin-lsp)
[![release](https://img.shields.io/github/v/release/Hessesian/kotlin-lsp)](https://github.com/Hessesian/kotlin-lsp/releases/latest)
[![build](https://img.shields.io/github/actions/workflow/status/Hessesian/kotlin-lsp/ci.yml)](https://github.com/Hessesian/kotlin-lsp/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/kotlin-lsp)](LICENSE)

A fast, low-memory LSP server for **Kotlin**, **Java**, and **Swift**, written in Rust.  
Built with [tree-sitter](https://tree-sitter.github.io/) — instant startup, no JVM.

![kotlin-lsp demo](demo/demo.gif)

## Install

```bash
cargo install kotlin-lsp
```

> No Cargo? Get it at [rustup.rs](https://rustup.rs). After install, `kotlin-lsp` is at `~/.cargo/bin/` — make sure it's on your `PATH`.

**Optional:** Install `fd` and `rg` (ripgrep) for faster file discovery and cross-file search.

## Quick start

**VS Code** — download and install the `.vsix` from the [latest release](https://github.com/Hessesian/kotlin-lsp/releases/latest):

```bash
code --install-extension kotlin-lsp-linux-x64-vX.Y.Z.vsix   # Linux
code --install-extension kotlin-lsp-darwin-arm64-vX.Y.Z.vsix # macOS Apple Silicon
```

The extension bundles syntax highlighting and launches `kotlin-lsp` automatically.

**Helix** — add to `~/.config/helix/languages.toml`:

```toml
[[language]]
name = "kotlin"
language-servers = ["kotlin-lsp"]

[[language]]
name = "java"
language-servers = ["kotlin-lsp"]

[language-server.kotlin-lsp]
command = "kotlin-lsp"
```

[Neovim, Zed setup →](docs/editors.md)

**Once your editor is wired up:**

1. Open a Kotlin/Java file — hover, go-to-definition, and completions work immediately via `rg` fallback while the index builds in the background.
2. _(Optional)_ Index library sources for hover and completions on third-party code:

```bash
kotlin-lsp extract-sources   # unpacks *-sources.jar from ~/.gradle; restart editor after
```

`~/.kotlin-lsp/sources` is picked up automatically — no config needed.

---

## Features

| Capability | Notes |
|---|---|
| **Go-to-definition** | Index → superclass hierarchy → `rg` fallback. Multi-hop chains, lambda params, `this`/`super` |
| **Hover** | Declaration signature, lambda param types, Kotlin stdlib docs |
| **Completion** | Dot-completion with type resolution, auto-import, scored ranking, stdlib entries |
| **References** | Project-wide `rg --word-regexp` + open buffers |
| **Document/workspace symbol** | Outline view, fuzzy search, dot-qualified extension function queries |
| **Rename** | Project-wide via `WorkspaceEdit` |
| **Inlay hints** | Lambda `it`, named params, `this`, untyped `val`/`var` |
| **Semantic tokens** | Full syntax highlighting via tree-sitter CST + cross-file resolution |
| **Diagnostics** | Syntax errors from tree-sitter (not type checking) |
| **Go-to-implementation** | Transitive subtype lookup (BFS) |
| **Signature help** | Active parameter highlighting |
| **Folding** | Brace regions + consecutive comment blocks |
| **CLI mode** | `find`, `refs`, `hover`, `index`, `tokens`, `tree`, `sources`, `extract-sources` — scriptable, no daemon |

All features work immediately — `rg` fallback handles symbols before indexing finishes.

### What gets indexed

| Language | Symbols |
|---|---|
| **Kotlin** | `class`, `interface`, `object`, `fun`, `val`, `var`, `typealias`, constructor params, enum entries |
| **Java** | `class`, `interface`, `enum`, `method`, `field`, `enum_constant` |
| **Swift** | `class`, `struct`, `enum`, `protocol`, `func`, `let`, `var`, `typealias`, `extension`, `init`, enum cases |

---

## CLI

`kotlin-lsp` works standalone — no editor, no daemon.

![kotlin-lsp CLI demo](demo/cli.gif)

```bash
kotlin-lsp find MyViewModel              # search declarations
kotlin-lsp refs MyViewModel              # find all references
kotlin-lsp hover src/Foo.kt 42 10        # hover info at line 42, col 10
kotlin-lsp index --root ./android        # pre-build cache
kotlin-lsp sources --root ./android      # list detected source roots
kotlin-lsp extract-sources               # unpack library sources from Gradle cache
```

| Flag | Behaviour |
|---|---|
| _(none)_ | Auto: use cached index if available, fall back to fast `rg`/`fd` |
| `--fast` | Always use `rg`/`fd`; instant, no index needed |
| `--smart` | Require index; build it if missing |
| `--json` | Machine-readable output |
| `--root <dir>` | Workspace root (default: nearest `.git` dir) |

[Full CLI reference →](docs/features.md#cli-subcommands)

---

## Configuration

### Workspace root

Resolved in order:

1. `KOTLIN_LSP_WORKSPACE_ROOT` env var
2. LSP client `rootUri` / `workspaceFolders`
3. `~/.config/kotlin-lsp/workspace` file (for clients that send no root)

### Ignore patterns

```toml
# ~/.config/helix/languages.toml
[language-server.kotlin-lsp.config.indexingOptions]
ignorePatterns = ["bazel-*", "build/**", "third-party/**"]
```

Patterns follow gitignore glob rules and apply to both `fd` and `walkdir` fallback.

### Source paths

`~/.kotlin-lsp/sources` (the default `extract-sources` output) is **automatically included** by both the LSP server and CLI — no config needed after running `kotlin-lsp extract-sources`.

To add other directories (generated stubs, custom source roots) for hover and completions — excluded from `findReferences` and `rename`:

```toml
[language-server.kotlin-lsp.config.indexingOptions]
sourcePaths = ["buildSrc/src", "/path/to/generated-stubs"]
```

[Full configuration reference →](docs/features.md)

---

## Limitations

- **No type inference** for generic lambda parameters — use explicit annotations for unresolvable cases
- **No type checking** — syntax errors only; use Gradle/Xcode/CI for semantic diagnostics
- **Swift support is structural** — all symbols indexed; no module boundaries or closure type inference
- **Java completion** is less refined than Kotlin
- **`findReferences` on common names** returns noise — name-based search via `rg`, no import filtering yet
- **Binary `.aar`/`.jar`** cannot be indexed — requires a `*-sources.jar` (use `kotlin-lsp extract-sources`)

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

They can coexist — use kotlin-lsp for fast navigation, the official one for type-checked diagnostics.

---

## Learn more

- [Feature details](docs/features.md) — resolution chain, completion, CLI reference
- [Editor setup](docs/editors.md) — Helix, Neovim, VS Code, Zed
- [GitHub Copilot CLI](docs/copilot.md) — agent integration, skill extension
- [Architecture & performance](docs/architecture.md) — source layout, memory model
- [Performance & profiling](docs/performance.md) — benchmarks, flamegraph setup
- [Changelog](CHANGELOG.md)

---

## Acknowledgements

Superclass hierarchy resolution, `this`/`super` qualifier handling, and lambda parameter recognition were inspired by [**code-compass.nvim**](https://github.com/emmanueltouzery/code-compass.nvim) by Emmanuel Touzery.
