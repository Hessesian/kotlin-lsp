# Architecture

```
main.rs              – tokio entry point, wires stdin/stdout to tower-lsp
backend/             – LSP request handlers (module)
  mod.rs             – LanguageServer trait impl: initialize, shutdown, capabilities
  handlers.rs        – textDocument/* handlers (hover, definition, completion, references, etc.)
  helpers.rs         – shared handler utilities
  nav.rs             – navigation helpers (go-to-definition, implementation)
  rename.rs          – textDocument/rename handler
  actions.rs         – textDocument/codeAction handler
  cursor.rs          – cursor position helpers
  format.rs          – response formatting
indexer.rs           – file discovery (fd/walkdir), in-memory index, progress reporting
indexer/             – indexer submodules
  cache.rs           – disk cache (bincode serialization, versioning)
  scope.rs           – scope analysis, local bindings
  infer/             – type inference (args, it/this, generics)
  live_tree.rs       – per-document live tree-sitter parse trees
  resolution.rs      – cross-file symbol resolution, enrichment
parser.rs            – tree-sitter symbol extraction, SymbolEntry construction
resolver/            – definition resolution (module)
  complete.rs        – dot-completion, bare-word completion, auto-import
  find.rs            – go-to-definition resolution chain
  infer.rs           – type inference for completion
semantic_tokens/     – semantic token generation (module)
  mod.rs             – two-phase pipeline orchestration
  kotlin.rs          – Kotlin CST → token classification
  java.rs            – Java CST → token classification
  helpers.rs         – shared token helpers
  params.rs          – parameter/argument detection
  resolve.rs         – Phase 2 cross-file resolution
cli/                 – CLI mode (module)
  args.rs            – argument parsing (lexopt)
  run.rs             – subcommand execution
queries.rs           – tree-sitter query constants, node kind constants
stdlib.rs            – built-in Kotlin stdlib signatures for hover and completion
types.rs             – SymbolEntry, FileData, Visibility, CursorPos
rg.rs                – ripgrep subprocess helpers
inlay_hints.rs       – textDocument/inlayHint handler
```

## Memory model

Each file stores symbols, import paths, declared names, and raw source lines.  
At ~50 chars/line × 300 lines/file ≈ 15 KB/file. At 2 000 files that is ~30 MB for lines alone; with symbol metadata the total stays well under 200 MB for typical Android projects.

## Performance

- **Startup** — the server starts instantly and indexes in the background. All features (hover, go-to-definition, inlay hints) work immediately via `rg` fallback — no need to wait for indexing to finish.
- **CPU** — a 120 ms debounce prevents re-parsing on every keystroke. A semaphore caps concurrent parse workers at 8 during workspace scan.
- **Content dedup** — files are only re-parsed when their content actually changes (FNV-1a hash check).
- **Completion cache** — dot-completion results are cached per type-file; cleared only when that file changes.
- **fd `--full-path` search** — when resolving an import like `com.example.data.compat.EProductScreen`, the fd command searches for `*/com/example/data/compat/EProductScreen.(kt|java|swift)$` — a single O(1) traversal that skips unrelated modules entirely.

## Build from source

**Requirements:** Rust 1.76+, a C compiler (for tree-sitter grammars)  
**Optional:** `fd`, `rg` (ripgrep) — improve file discovery and reference search speed but are not required

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
