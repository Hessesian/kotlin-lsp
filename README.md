# kotlin-lsp

A fast, low-memory LSP server for **Kotlin**, **Java**, and **Swift**, written in Rust.  
Built with [tower-lsp](https://github.com/ebkalderon/tower-lsp) and [tree-sitter](https://tree-sitter.github.io/), designed for large Android/JVM/iOS codebases where heavier LSP servers feel sluggish.

![kotlin-lsp demo](demo/demo.gif)

## Install

```bash
cargo install kotlin-lsp
```

> **Rust/Cargo not installed?** Get it via [rustup](https://rustup.rs):
> ```bash
> curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
> ```
> After install, `kotlin-lsp` lands in `~/.cargo/bin/` — make sure it's on your `PATH`.

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
| **Completion** | Dot-completion with type resolution, bare-word, auto-import, scored ranking, stdlib entries, visibility filtering |
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

### Ignore patterns

Exclude directories or files from indexing using `initializationOptions`:

```toml
# ~/.config/helix/languages.toml
[language-server.kotlin-lsp.config.indexingOptions]
ignorePatterns = [
  "bazel-bin/**",   # Bazel output tree (symlinked — avoids double-indexing)
  "bazel-out/**",
  "bazel-*",        # any bazel-* dir at any depth (bare pattern)
  "third-party/**",
  "build/**",
]
```

Pattern semantics follow gitignore glob rules:

| Pattern | Matches |
|---|---|
| `bazel-*` | Any dir/file named `bazel-*` at **any depth** |
| `third-party/**` | Everything inside `third-party/` relative to workspace root |
| `/abs/path/**` | Absolute path — normalized to relative before matching |

Patterns are applied to both `fd` (fast path) and the `walkdir` fallback, and also filter the warm-start cached manifest so newly added patterns take effect without clearing the cache.

### Source paths

Index extra directories (like library sources or generated stubs) for hover, go-to-definition and autocomplete — while keeping them out of `findReferences` and rename results:

```toml
# ~/.config/helix/languages.toml
[language-server.kotlin-lsp.config.indexingOptions]
sourcePaths = [
  "~/.kotlin-lsp/sources",  # extracted Gradle library sources (see below)
  "buildSrc/src",           # relative to workspace root
]
```

| Behaviour | `sourcePaths` files |
|---|---|
| Hover / go-to-definition | ✓ |
| Autocomplete | ✓ |
| `findReferences` | ✗ (excluded) |
| `rename` | ✗ (excluded) |

- Paths can be absolute (including `~/…`), or relative to the workspace root.
- Unlike `ignorePatterns`, hardcoded directory excludes (`.gradle`, `build`, `target`, …) are **not** applied — the full path is trusted.
- Files that happen to overlap with the workspace root are indexed but **not** excluded from findReferences (they are workspace files, not library sources).

#### Extracting Gradle library sources

Use the included script to unpack `*-sources.jar` files from your Gradle cache:

```bash
# Extract all androidx.compose sources (latest version of each artifact)
python3 contrib/extract-sources.py androidx.compose

# Multiple filters
python3 contrib/extract-sources.py androidx.compose org.jetbrains.kotlinx

# Extract everything (can be large)
python3 contrib/extract-sources.py

# Preview without writing files
python3 contrib/extract-sources.py --dry-run androidx.compose

# Custom Gradle home / output dir
python3 contrib/extract-sources.py --gradle-home ~/work/.gradle --output ~/my-sources androidx.compose
```

Sources are extracted to `~/.kotlin-lsp/sources/<group>.<artifact>/`. Re-run the script after `./gradlew build` to pick up new dependencies. The script deduplicates by keeping only the latest downloaded version of each artifact.

Then add the output directory to your LSP config (printed at the end of each run):

```toml
[language-server.kotlin-lsp.config.indexingOptions]
sourcePaths = ["~/.kotlin-lsp/sources"]
```

### Auto-import

When completing an unimported symbol (class, interface, object), kotlin-lsp automatically adds the import statement:

- Start typing a class name (uppercase, ≥ 2 chars) → completion shows candidates from all indexed files including `sourcePaths`
- Select a candidate → the symbol is inserted **and** `import pkg.ClassName` is added at the correct position
- If two classes share the same name (e.g. `Button` from `material` and `material3`), both appear with their package shown in the detail column — pick the right one
- Already-imported symbols appear without a duplicate edit
- Same-package symbols appear without any import edit
- Star imports (`import pkg.*`) are respected — no redundant explicit import added

### Completion ranking

Completions are scored by match quality so the most relevant items appear first:

| Score | Match type | Example |
|---|---|---|
| 0 | Exact prefix (case-insensitive) | `Col` → **Col**umn |
| 1 | CamelCase acronym | `CB` → **C**olumn**B**utton, `mSF` → **m**y**S**tate**F**low |
| 2 | Substring (same-file/package only) | `View` → RecyclerView |

Results are capped at 150 items. When the cap is hit, `isIncomplete: true` is returned so the client re-queries on every keystroke — the list tightens naturally as you type more characters.

**Context-aware filtering:**
- Lowercase prefix → only functions, vars, params (no classes)
- Uppercase prefix → only classes, objects, types (no functions)
- `@` prefix → only annotation/class kinds (no functions or variables)
- Cross-package symbols require prefix ≥ 2 characters to prevent noise

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

### 0.8.0

- **Auto-import completion** — selecting an unimported class automatically inserts the `import` statement; multiple same-named classes from different packages appear as separate items showing the package
- **Completion relevance scoring** — results sorted by match quality: exact prefix → camelCase acronym (e.g. `CB` → `ColumnButton`) → substring; capped at 150 with `isIncomplete: true` so the client re-queries as you type
- **Cross-package gate** — auto-import symbols require prefix ≥ 2 chars; `@` context restricts completions to class/annotation kinds
- **Warm-start improvements** — skip branch-deleted cached paths; warm starts always bypass the file-count cap
- **Java import handling** — `parse_imports_from_lines` strips trailing `;` and `static` prefix; auto-import inserts Java-style `import foo.Bar;` for `.java` files

### 0.7.1

- **`ignorePatterns` configuration** — exclude directories/files from indexing via `initializationOptions` (gitignore-style globs, any depth, warm-start aware)
- **Swift hover keyword fix** — Swift functions now show `func` instead of `fun` in hover code blocks

### 0.7.0

- **`it`/`this` type-directed inference** — when `it` or `this` is a call argument (named or positional), the expected parameter type is inferred from the function signature
- **`this` in receiver vs regular lambdas** — correctly hints enclosing class in `(T) -> R`, receiver type in `T.() -> R` and scope functions
- **`fun interface` recognition** — fix tree-sitter not recognising `fun interface` declarations
- **Suspend lambda type inference** — correct type inference for `suspend` lambda parameters
- **Copilot extension** — remove overly restrictive `kotlin_rg` pre-hook

[Full changelog →](CHANGELOG.md)

---

## Acknowledgements

Superclass hierarchy resolution, `this`/`super` qualifier handling, and lambda parameter recognition were inspired by [**code-compass.nvim**](https://github.com/emmanueltouzery/code-compass.nvim) by Emmanuel Touzery.
