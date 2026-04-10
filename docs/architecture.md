# Architecture

```
main.rs      – tokio entry point, wires stdin/stdout to tower-lsp
backend.rs   – LanguageServer trait: initialize / hover / definition / completion / documentSymbol / references / signatureHelp / rename / foldingRange / symbol
indexer.rs   – file discovery (fd), in-memory index, rg fallback, progress reporting
parser.rs    – tree-sitter-kotlin + tree-sitter-java + tree-sitter-swift symbol & visibility extraction
resolver.rs  – definition resolution, multi-hop field chains, class hierarchy, completion logic
stdlib.rs    – built-in Kotlin stdlib signatures for hover and completion
types.rs     – SymbolEntry, FileData, Visibility
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
