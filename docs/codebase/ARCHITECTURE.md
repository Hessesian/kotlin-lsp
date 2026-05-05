# Architecture

## Core Sections (Required)

### 1) High-Level Data Flow

**Request entry → Response output:**

```
Client (VSCode, Neovim, Helix, etc.)
  ↓ (LSP JSON-RPC over stdio)
backend/handlers.rs (LSP LanguageServer impl)
  ├─ initialize() → Indexer::new() + spawn workspace scan
  ├─ hover(file, pos) → resolver::build_hover()
  ├─ definition(file, pos) → resolver::find_definition()
  ├─ completion(file, pos) → resolver::complete()
  ├─ references(file, pos) → rg (ripgrep fallback) + index
  ├─ documentSymbol(file) → indexer lookup
  └─ rename(file, pos, newName) → apply workspace edit
  ↓
resolver/ (type inference, definition resolution, completion scoring)
  ├─ Multi-hop field chains (e.g. obj.field.method())
  ├─ Superclass hierarchy lookup
  ├─ Type substitution for generics
  ├─ Import-aware symbol resolution
  └─ rg fallback for cross-file refs
  ↓
indexer/ (index maintenance, cache, file discovery)
  ├─ scan.rs: workspace scanning orchestration
  ├─ discover.rs: file enumeration (fd or walkdir)
  ├─ parser.rs: tree-sitter symbol extraction
  ├─ cache.rs: on-disk index persistence
  └─ live_tree.rs: dynamic AST for open documents
  ↓
parser.rs (tree-sitter grammar execution)
  └─ Extract symbols, imports, class hierarchy
  ↓
types.rs (domain model: SymbolEntry, FileData)
```

### 2) Layers and Responsibilities

**Hexagonal (Ports & Adapters):**

| Layer | Files | Responsibility | Inbound/Outbound |
|-------|-------|-----------------|------------------|
| **Inbound Adapter** | `backend/` | LSP protocol parsing, request routing, response serialization | Receives LSP messages via tower-lsp |
| **Application** | `indexer/`, `parser.rs` | Workspace scanning, file discovery, symbol extraction, index state | Maintains index, orchestrates parsing |
| **Domain** | `resolver/`, `types.rs` | Symbol resolution, type inference, completion scoring | Pure business logic, no I/O |
| **Outbound Port** | `indexer/scan.rs` (ProgressReporter trait) | Progress notification abstraction | Injects `LspProgressReporter` or `NoopReporter` |
| **Outbound Adapter** | `rg.rs`, `backend/mod.rs` (LspProgressReporter) | ripgrep CLI, LSP progress notifications | Executes external CLIs, sends LSP messages |

### 3) Key Design Patterns

#### a) **Index as Shared State**
- `Indexer` struct (in `indexer.rs`) holds all index state in DashMaps (concurrent, no Mutex)
- Cloned `Arc<Indexer>` passed to all async tasks, requests
- Updates: file-by-file on workspace scan, on-demand cache sync

#### b) **Live Parse Trees for Open Documents**
- Every open document keeps a live tree-sitter parse tree updated on `textDocument/didChange`
- Enables instant inlay hints, hover, without waiting for background reindex
- Fallback to cached index snapshot if document not open

#### c) **Outbound Port: ProgressReporter**
- Trait abstraction for progress notifications
- `LspProgressReporter(Client)` in `backend/` sends actual `$/progress` LSP messages
- `NoopReporter` used in CLI mode (`--index-only`) and tests
- Removes direct LSP dependency from indexer layer

#### d) **Completion Scoring & Ranking**
- Completions scored by match quality: exact prefix (0) → camelCase acronym (1) → substring (2)
- Same-file/package substring matches; cross-package requires ≥2-char prefix (prevents spam)
- Capped at 150 items per request; `isIncomplete: true` for client re-query

#### e) **Type Substitution via Generic Parameters**
- Superclass generics (e.g., `FlowReducer<E, Ef, S>`) stored in index
- When hovering a member in a subclass, substitution map applied (e.g., `EffectType` → concrete `Ef`)
- Works across enclosing class supertypes and member property hierarchies

#### f) **Multi-Hop Resolution**
- Definition resolution follows field access chains (e.g., `obj.field1.field2.method()`)
- Each hop: lookup symbol → resolve type → extract class members
- Falls back to ripgrep (`rg`) for symbols not in index

### 4) Concurrency Model

- **tokio runtime:** multi-threaded with 4 worker threads, 512 blocking threads
- **Workspace scanning:** 8 concurrent parse workers (semaphore), bounded to prevent memory spike
- **Index storage:** DashMap (concurrent HashMap without Mutex)
- **Live tree updates:** spawned as background tasks, applied on next request
- **No blocking I/O:** `tokio::fs::read_to_string` used instead of `std::fs`

### 5) Fallback Strategy

When symbol is not in index:
1. Try index lookup
2. Try local imports + same-package lookup
3. Try superclass hierarchy
4. Fall back to `rg --word-regexp` across entire workspace
5. (Optional) Use pre-indexed stdlib entries for Kotlin standard library

### 6) Evidence

- src/backend/mod.rs (LanguageServer trait impl, handler dispatch)
- src/backend/handlers.rs (individual handlers)
- src/indexer.rs (Indexer struct, Arc<Indexer> pattern)
- src/indexer/scan.rs (ProgressReporter port, NoopReporter impl)
- src/resolver/mod.rs (type inference, definition resolution)
- src/resolver/complete.rs (completion scoring)
- src/rg.rs (ripgrep wrapper)

## Extended Sections (Optional)

### Request Lifecycle Example: Go-to-Definition

```
backend/handlers.rs::goto_definition(uri, line, col)
  ↓
resolver::find_definition(indexer, uri, line, col)
  ├─ Get file from index
  ├─ Find symbol at (line, col) using live tree or snapshot
  ├─ Resolve symbol type (imports, scopes, supertypes)
  ├─ Check superclass hierarchy if member access (e.g., obj.method)
  ├─ Multi-hop chain: follow field types across assignments
  └─ If not found, spawn rg search for symbol name
  ↓
Send Location back to client (file URI + range)
```

### Performance Optimizations

1. **Query caching:** tree-sitter `Query` objects compiled once per process (OnceLock)
2. **Parser reuse:** Per-worker-thread parser instance (thread-local storage)
3. **Live tree indexing:** O(1) line offset lookup via precomputed `line_starts`
4. **FNV-1a hashing:** File content checksums to skip re-parsing unchanged files
5. **Completion caching:** Dot-completion results memoized per type-file
6. **120ms parse debounce:** On `textDocument/didChange`, parse delayed to avoid thrashing
7. **Unlimited file indexing:** As of v0.9.3, all files eagerly indexed upfront (parse cost low due to query/parser caching)

### Error Handling

- **Parsing errors:** tree-sitter never panics; missing symbols are reported as syntax errors in diagnostics
- **Missing imports:** fallback to ripgrep search
- **File access errors:** logged (up to 5 per scan), index continues with available files
- **LSP protocol errors:** tower-lsp handles malformed JSON-RPC; server remains responsive
- **No exceptions:** Rust's Result-based error handling ensures no uncaught panics

### State Mutations

- **Synchronized via DashMap:** concurrent read/insert operations, no blocking
- **Transactional per-file:** file is fully indexed (parsed, symbols extracted, cache updated) before marking done
- **No partial updates:** if parse fails, index for that file remains from previous scan
- **Cache consistency:** SHA2 hash of file contents stored; cache misses trigger re-parse

### Testing Strategy

- **Unit tests:** inline `#[cfg(test)]` modules per source file, with fake indexers
- **Integration tests:** `tests/` directory, use real tree-sitter grammars
- **Fixture-based:** test data in `tests/fixtures/kotlin/`, `tests/fixtures/mvi/`
- **No mocking framework:** manual test double structs (e.g., `TestIndexer`)
