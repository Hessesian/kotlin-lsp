# kotlin-lsp — Copilot Agent Instructions

## Project overview

`kotlin-lsp` is a lightweight LSP server for Kotlin and Java files, built in Rust using `tower-lsp` and `tree-sitter`. It is designed for **agentic use** (Copilot CLI, Neovim, Helix) where fast symbol lookup matters more than compiler accuracy.

Key design constraints:
- **No JVM/Gradle**: pure Rust, tree-sitter for parsing — startup is instant
- **< 200 MB RAM** target on large Android repos
- **Cross-file indexing** via in-memory HashMap, persisted to disk cache
- **Workspace root override** — reads `~/.config/kotlin-lsp/workspace` at startup

## Build & install

```sh
cargo build --release
cargo install --path .    # installs to ~/.cargo/bin/kotlin-lsp
```

Tests:
```sh
cargo test
```

## Source layout

| File | Purpose |
|---|---|
| `src/main.rs` | Entry point, stdio transport setup |
| `src/backend.rs` | LSP request handlers (`initialize`, `hover`, `definition`, `references`, `document_symbol`, `workspace_symbol`, `execute_command`) |
| `src/indexer.rs` | File discovery (`fd`), tree-sitter parsing, in-memory index, disk cache |
| `src/parser.rs` | Tree-sitter query execution, `SymbolEntry` extraction, `extract_detail()` |
| `src/resolver.rs` | Cross-file resolution, import handling, `rg` fallback |
| `src/types.rs` | `SymbolEntry`, `Location`, shared types |
| `contrib/copilot-extension/extension.mjs` | Copilot CLI skill extension — copy to `~/.copilot/extensions/kotlin-lsp/` |

## Key types

```rust
// types.rs
pub struct SymbolEntry {
    pub name: String,
    pub kind: String,       // "class", "fun", "val", "var", "interface", "object", "typealias"
    pub file: String,
    pub range: Range,
    pub container: String,  // enclosing class/object name, empty if top-level
    pub detail: String,     // truncated declaration signature, e.g. "fun foo(x: Int): String"
}
```

## Workspace root priority (backend.rs `initialize`)

1. `KOTLIN_LSP_WORKSPACE_ROOT` env var (if set and is a valid dir)
2. `~/.config/kotlin-lsp/workspace` plain-text file (trimmed)
3. LSP client `rootUri` / first `workspaceFolder`

To switch workspace at runtime:
```sh
echo /path/to/project > ~/.config/kotlin-lsp/workspace
# then kill & restart the LSP process
```

Or use the `kotlin-lsp/changeRoot` workspace command (programmatic).

## Custom LSP commands

Both registered in `execute_command_provider` capabilities:

| Command | Args | Effect |
|---|---|---|
| `kotlin-lsp/reindex` | none | Clear cache, re-scan all files |
| `kotlin-lsp/changeRoot` | `["/abs/path"]` | Swap workspace root, clear index, reindex |

Note: the built-in Copilot `lsp` tool does not support `executeCommand`. Use the skill extension's `kotlin_lsp_set_workspace` tool (writes config file + kills process) instead.

## LSP-first workflow for agentic code investigation

Prefer LSP over `grep`/`rg` in this order:

1. **`workspaceSymbol "Name"`** — find class/fun/val by name across all files; returns name + signature + location
2. **`documentSymbol file.kt`** — list all symbols in a file (methods, fields, nested classes)
3. **`hover file.kt line col`** — get declaration signature and type info at a position
4. **`goToDefinition`** — jump to declaration
5. **`findReferences`** — find all usages (warning: common method names return noise — see below)
6. **`rg` / `grep`** — last resort, or when method names are too common for findReferences

### findReferences noise mitigation

`findReferences` is name-based (no type resolution). For common method names:
- Use `rg` with a qualified pattern: `rg "ReceiverClass\.methodName\("` 
- Or scope to the declaring class's package directory

Planned improvement: import-aware filtering — only return refs from files that import the declaring class.

## Disk cache

Cache stored in `~/.cache/kotlin-lsp/index-<hash>.bin` (bincode format).  
Current `CACHE_VERSION = 2` — bump this in `indexer.rs` when `SymbolEntry` schema changes.

The `#[serde(default)]` attribute on new `SymbolEntry` fields allows old cache entries to deserialize without error (new field gets its default value).

## Release process

1. Bump version in `Cargo.toml`
2. Update `README.md` changelog / feature notes
3. `cargo build --release && cargo test`
4. `git tag v0.x.y && git push --tags`
5. `cargo publish`

## Copilot CLI integration

Install the skill extension:
```sh
mkdir -p ~/.copilot/extensions/kotlin-lsp
cp contrib/copilot-extension/extension.mjs ~/.copilot/extensions/kotlin-lsp/
```

LSP config (`~/.copilot/lsp-config.json`):
```json
{
  "servers": [
    {
      "name": "kotlin-lsp",
      "command": ["kotlin-lsp"],
      "languages": ["kotlin", "java"],
      "fileExtensions": [".kt", ".kts", ".java"]
    }
  ]
}
```

The skill extension provides:
- `kotlin_lsp_status` — check indexing progress
- `kotlin_lsp_set_workspace` — write config file and restart LSP for a new project
- `kotlin_lsp_info` — capabilities and known limitations

## Rust coding guidelines

These rules are distilled from the actionbook/rust-skills layer framework and leonardomso/rust-skills,
cherry-picked for relevance to kotlin-lsp's architecture.

### Design tracing (actionbook layer model)

Before making a design decision, trace through three layers top-down:

1. **WHY (Domain)** — What constraint does this solve? (e.g. "infer functions are pure reads over a snapshot")
2. **WHAT (Design)** — What pattern fits? (e.g. `InferDeps` trait, `CursorPos` newtype)
3. **HOW (Mechanics)** — Which Rust feature? (e.g. generic bound, struct, method)

Never jump straight to HOW. A misdiagnosed WHY produces technically correct but wrong abstractions.

### Newtypes for semantic safety

Adjacent `usize` params like `(cursor_line, cursor_col)` are a transposition bug waiting to happen.
Wrap them in a named struct with documented units:

```rust
/// Cursor position in a document. `col` is UTF-16 code units (LSP protocol).
pub struct CursorPos { pub line: usize, pub col: usize }
```

Apply when: two same-type params appear together in ≥2 function signatures with swappable semantics.

### Rule of Three before abstracting

Don't introduce a generic bound until you have ≥2 distinct concrete implementations that actually
differ. For the `InferDeps` trait pattern: the rule is met — `Indexer` (production) and `TestDeps`
(test stub) are genuinely different. If only one concrete type exists, keep the function concrete.

### Prefer generics over `Box<dyn Trait>`

Use `impl Trait` or `<T: Trait>` for infer functions (static dispatch, zero cost, no heap).
Reserve `Box<dyn Trait>` only for heterogeneous runtime collections or plugin-style registries.

```rust
// Good: infer function with generic bound
fn infer_it_type<D: InferDeps>(deps: &D, pos: CursorPos) -> Option<String> { ... }

// Avoid: dyn Trait adds vtable overhead and heap allocation for no benefit here
fn infer_it_type(deps: &dyn InferDeps, pos: CursorPos) -> Option<String> { ... }
```

### Traits for testability

Extract snapshot access behind a trait so infer functions can be tested without a full `Indexer`:

```rust
pub trait InferDeps {
    fn lines(&self, uri: &str) -> Option<Arc<Vec<String>>>;
    fn live_doc(&self, uri: &str) -> Option<Arc<LiveDoc>>;
    fn symbol_detail(&self, name: &str, container: &str) -> Option<String>;
}
```

Unit tests implement `TestDeps` as a simple struct — no DashMap, no disk, fast.

### Purity in infer functions

Functions that read doc/index data and return inference results are pure: `(inputs) -> output`.
Do not let them mutate index state. Mutation (on-demand indexing, cache fills) belongs on `Indexer`,
not inside the infer call graph.

### Dedup before abstracting

Before introducing a new utility function, check if it already exists:
- `utf16_col_to_byte` — in `src/indexer/live_tree.rs`; don't inline the loop
- `lines_for(uri)` — in `src/indexer/scope.rs` (moving to `indexer.rs`); don't duplicate the pattern

## Pre-commit checklist

Run these checks before every commit. A commit that fails any of them should not be pushed.

### 1. Build and tests

```sh
cargo test
cargo clippy -- -D warnings -W clippy::cognitive_complexity -W clippy::too_many_lines
```

Zero warnings required. Fix, don't suppress with `#[allow]` unless the suppression is
accompanied by a comment explaining why the lint is inapplicable.

### 2. No hardcoded tree-sitter node kind strings

All node kind comparisons must use named constants from `src/queries.rs`.

- **Wrong:** `node.kind() == "function_declaration"`
- **Right:** `node.kind() == KIND_FUN_DECL`

If a constant doesn't exist yet, add it to the appropriate section in `src/queries.rs` before
using it. Same rule applies to modifier keywords (`"static"`, `"final"`) and Java node kinds.

### 3. Prefer trait bounds over concrete types

When a function only touches a subset of a struct's API, ask: is that subset already a trait?
If not, and if ≥2 distinct implementations could exist (production + test stub), extract one.

- Use `impl Trait` / `<T: Trait>` — static dispatch, zero heap cost.
- Reserve `Box<dyn Trait>` only for heterogeneous runtime collections or plugin registries.
- Apply the Rule of Three: wait for the second concrete implementation before abstracting.

### 4. No deep nesting

Functions should have at most 3 levels of indentation in their body.

Flatten with:
- Early `return` / `return None` guards at the top
- The `?` operator for error propagation
- `let … else` for mandatory destructuring
- Extracted helper functions for inner loops or match arms

A `match` nested inside another `match` inside an `if` is a signal to extract.

### 5. Section comments inside a function body signal a split

If you feel the need to write a `// --- Step 1: …` or `// Build the result` comment to
separate logical phases inside a function, that's a signal the function should be split.

- Each logical phase becomes a named helper function — the name replaces the comment.
- The top-level function becomes a readable sequence of helper calls.
- Exception: a single clarifying comment on a non-obvious line is fine; what's banned is
  using comments as section dividers to compensate for a function doing too many things.

**`and` in a function name is the same signal at the naming level.** If a function is
named `drain_and_apply`, `fetch_and_store`, or `parse_and_index`, it is doing two things.
Split into two functions called from a coordinator:

```rust
// Bad: one function doing two things, name says so
fn drain_and_apply_changes(&mut self) { … }

// Good: coordinator calls two focused functions
fn handle_file_changed(&mut self) {
    let changes = self.drain_pending_changes();
    self.apply_changes(changes);
}
```

The only time `and` in a name is acceptable is when the two parts are inseparable
(e.g., `read_and_advance` on a cursor where reading without advancing would corrupt
state) — document *why* they cannot be separated.

### 6. Long names signal missing structs or traits; avoid abbreviations

**No abbreviations.** `sym` → `symbol`, `idx` → `index`, `uri_str` → `uri` (or a newtype).
Short names save keystrokes and lose meaning. The compiler remembers the type; the reader
does not.

**A long function name signals a missing struct.** If you find yourself writing
`resolve_symbol_with_fallback_and_type_args(uri, name, container, type_args, fallback)`,
the parameters want to be a struct:

```rust
struct SymbolQuery { uri: Url, name: String, container: String, type_args: Vec<String> }
fn resolve_symbol(query: &SymbolQuery) -> Option<SymbolEntry> { … }
```

The function name shrinks because the struct name carries the context.

**A confusing function signature signals a missing trait.** If callers must read the body
to understand what a function does, extract the behaviour into a named trait. The trait name
and method name together should make the call site self-documenting:

```rust
// Unclear: what does `index` do here?
fn enrich(index: &Indexer, pos: CursorPos) -> Option<String>

// Clear: the trait name declares the contract
fn enrich<R: SymbolResolver>(resolver: &R, pos: CursorPos) -> Option<String>
```

Use traits to clarify *what role* a parameter plays, not just *what type* it is.

### 7. No `unwrap()` or `expect()` in production code

Use `?`, `if let`, `match`, or log-and-return patterns. Exception: `#[cfg(test)]` code may
use `unwrap()` / `expect()`.

### 8. Test-only code must be gated

Functions, imports, or types that are only used in tests must be annotated `#[cfg(test)]`.
Do not leave production code with dead-code warnings suppressed via `#[allow(dead_code)]`
without a comment explaining why the gate can't be used instead.

### 9. Cache version bump on schema changes

If `SymbolEntry` gains or loses fields, bump `CACHE_VERSION` in `src/indexer/cache.rs`.
New fields must carry `#[serde(default)]` so old cache files still deserialize.

### 10. Tests live in companion `*_tests.rs` files, not inline

Never write a `mod tests { … }` block inside a source file. Instead:

1. Create `src/foo_tests.rs` (next to `src/foo.rs`) with the test content.
2. In `src/foo.rs`, add only the three-line stub:

```rust
#[cfg(test)]
#[path = "foo_tests.rs"]
mod tests;
```

This keeps production code free of test noise. The only `#[cfg(test)]` allowed
directly in a source file is the stub above, plus `#[cfg(test)]` gates on helper
items (e.g. test-only trait impls or constructors) that must live alongside the
type they support.

### 11. Minimal visibility

Default to module-private (`fn`, `struct`). Widen to `pub(crate)` only when a sibling module
requires it; widen to `pub` only for items that form part of the external API surface.

### 12. Search before you write

Before implementing any new function that collects, resolves, discovers, or transforms data,
search the codebase for an existing implementation first:

```sh
rg -n "fn <keyword>" src/
```

Examples of duplication caught too late in this project:

- `collect_cli_source_paths()` in `cli/run.rs` duplicated `WorkspaceConfig::resolve_sources()` exactly — doing the same workspace.json + build-layout + user-sources discovery, then passing the result as `explicit_source_paths` so `resolve_sources()` ran it again.
- `cli/sources.rs::discover()` calls the same two `workspace_json` functions in the same order as `resolve_sources()` — still partly duplicated.
- `home_dir` resolved four different ways across four files; no shared helper existed.
- `poll_until` invented independently in two test modules during the same refactor wave.

**Rule:** if you are about to write a function that discovers paths, resolves symbols, reads
config, or deduplicates a collection — grep for the concept first. If a function already
exists, call it or extend it; don't write a parallel one.

### 12a. Refactoring tasks move code — they don't rewrite it

When a task says "extract X into its own module/struct", the implementation is:
1. **Read** the source file in full before touching anything
2. **Copy** the exact function body verbatim into the new location
3. **Adjust** only `self.` references to match the new struct's fields
4. **Delete** from the original location
5. **Verify** with `cargo test` that behaviour is identical

Do not rewrite logic, do not guess function signatures, do not invent helper names.
If a function name used in a task description does not appear in `rg -n "fn <name>" src/`,
stop and search for what actually exists — the name is wrong, not the codebase.

This applies especially to MVI actor refactoring: `src/workspace/actor.rs` already contains
all handler functions. Wave 5b extracts them into handler structs — it does not create new logic.

### 13. Start traits minimal (YAGNI)

Do not add methods to a trait "in case" they are needed later. Start with the smallest
interface that makes the current feature work. Adding methods is cheap; removing them from a
public trait is a breaking change.

Lesson: `WorkspaceRead` was introduced with 9 methods, all unused in production. A reviewer
caught this. It was trimmed to 1 method. The time spent designing and suppressing dead-code
warnings on the other 8 was wasted.

### 14. Module-level `#![allow(dead_code)]` is always wrong

A module-level allow hides real dead-code warnings across the entire module, including bugs.
Use per-item `#[allow(dead_code)]` with a comment referencing what will use the item:

```rust
// Used by Wave 3 read-handlers (read-handlers todo)
#[allow(dead_code)]
pub(crate) fn with_ready(&self) -> Option<&WorkspaceData> { … }
```

Remove the allow when the consuming code lands. If the consuming code never lands, the item
should be deleted.

### 15. Event dispatch functions are flat coordinators

The function that matches on an event enum (`run()`, `handle_event()`) must contain **zero logic**. It dispatches to named handlers — one line per variant. All logic lives in the handlers.

**Good example — `Actor::run()` and `handle_event()` (`src/workspace/actor.rs`):**
```rust
async fn handle_event(&mut self, event: Event) {
    match event {
        Event::Initialize { config }        => self.scan_handler.handle_initialize(config).await,
        Event::FileChanged { uri, changes } => self.file_change_handler.handle_file_changed(uri, changes).await,
        Event::FileOpened { uri, lang, text }=> self.document_handler.handle_file_opened(uri, lang, text).await,
        // … every arm is one line
    }
}
```

If a match arm body grows beyond one line, it belongs in a named method on the appropriate handler struct.

**Contrast — old `actor.rs`** had `handle_file_changed` (60 lines, 4-level nesting) directly in the Actor, with comments separating phases (`// batch drain`, `// spawn live-tree update`, `// reschedule debounce`). The comments were implicit function names; the refactor made them real.

### 16. Side effects belong at the write site, not scattered at call sites

When a mutation always has a companion side effect (e.g., writing X always invalidates Y), put the side effect *inside the write helper*, not at each call site. Call sites forget; the write helper cannot.

**Good example — `ScanHandler::set_root()` (`src/workspace/scan_handler.rs`):**
```rust
fn set_root(&self, root: PathBuf) {
    if let Ok(mut guard) = self.indexer.workspace_root.write() {
        *guard = Some(root);
    } else {
        log::warn!("Actor: failed to write workspace root");
        return;   // ← don't bump if write failed
    }
    // Every root change invalidates in-flight scans — this cannot be forgotten.
    self.indexer.root_generation.fetch_add(1, Ordering::SeqCst);
}
```

Three callers (`handle_initialize`, `handle_change_root`, `switch_workspace_root_for_opened_document`) previously each bumped `root_generation` manually. One caller had missed it entirely (the bug). Moving the bump into `set_root` makes forgetting impossible.

**Contrast — before:** `root_generation.fetch_add(…)` repeated at three call sites; one was missing, causing a race window.

### 17. When multiple functions must each "do" the same thing — that's an architectural gap

If you find yourself adding the same side effect (bump a counter, notify a channel, update a flag) to two or more call sites because "every caller must remember to do X", that repetition is a symptom: **X is not owned by the right abstraction**.

The fix is not discipline — it's architecture:
- Extract a write helper that performs X automatically (Rule 16).
- Or wrap the shared state in a newtype whose only mutation method performs X (e.g. `WorkspaceRoot::set()` always bumps the generation — callers cannot forget because there is no lower-level path).
- Or route all mutations through a single owner (e.g. the actor) so there is physically only one call site.

**Signal:** "every function that does A must also do B" → `A` and `B` are not separate concerns; they are one atomic operation that belongs in one place.

**Anti-pattern:**
```rust
// Three callers each bump root_generation manually after changing workspace_root.
// One missed it → race window.
self.indexer.workspace_root.write()...;        // caller 1
self.indexer.root_generation.fetch_add(1, ...); // caller 1 (and 2, and 3...)
```

**Fix — collapse into one write path:**
```rust
// WorkspaceRoot::set() is the only mutation path.
// The generation bump is inside set() — impossible to call one without the other.
self.indexer.workspace_root.set(new_root);
```

When you see "must also", ask: who should own both halves so the contract is enforced by construction?

### 18. Tree-sitter node traversal: use cursor API, not index arithmetic

When iterating over a node's children, **always** use the tree-sitter `TreeCursor` API or
`next_sibling()` / `next_named_sibling()`. Never use `for i in 0..node.child_count()` with
`node.child(i)` — it's brittle, enables `i + 1` bugs, and bypasses tree-sitter's efficient
internal iteration.

**Iterating all children:**
```rust
// Wrong: index arithmetic, Option noise, off-by-one risk
for i in 0..node.child_count() {
    let Some(child) = node.child(i) else { continue };
    // ...
}

// Right: cursor walks the sibling chain directly
let mut cursor = node.walk();
if cursor.goto_first_child() {
    loop {
        let child = cursor.node();
        // ...
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}
```

**Peeking at the next sibling:**
```rust
// Wrong: manual index tracking, requires passing container + index
let next = container.child(param_idx + 1)?;
if next.kind() == "=" { /* ... */ }

// Right: tree-sitter handles the linked-list walk
if param.next_sibling().is_some_and(|s| s.kind() == "=") { /* ... */ }
```

**Finding a child of a specific kind:**

Use `NodeExt::first_child_of_kind` (defined in `src/indexer/node_ext.rs`) instead of writing
a manual loop:
```rust
// Wrong: reinventing first_child_of_kind
for i in 0..node.child_count() {
    if let Some(c) = node.child(i) {
        if c.kind() == KIND_FORMAL_PARAMS {
            return Some(c);
        }
    }
}

// Right: use the existing helper
node.first_child_of_kind(KIND_FORMAL_PARAMS)
```

Similarly, `children_of_kind(kind)` collects all matching children.

**Key helpers (all in `src/indexer/node_ext.rs` on `NodeExt` trait):**

| Method | Purpose |
|---|---|
| `first_child_of_kind(kind)` | Find first direct child with matching `kind()` |
| `children_of_kind(kind)` | Collect all direct children with matching `kind()` |
| `next_sibling()` / `next_named_sibling()` | Walk to adjacent sibling (built-in tree-sitter) |
| `node.walk()` + cursor | Efficient iteration without index allocation |

**Exception:** `node.child(0)` for a known-first-child (e.g. callee in `call_expression`) is
fine — the position is structural, not arithmetic.

**Rationale:** index loops on CST nodes caused three bugs in this project:
- `param_has_default` had a 4-parameter signature with manual `while j < container_len` walking
- `named_arg_label` used `child(i + 1)` which fails if `child(i)` returns `None` early
- `count_provided_args` reinvented `children_of_kind` with 8 lines of boilerplate

## SOLID principles (Rust mapping)

These are mapped to Rust idioms. Good examples are added here as they emerge from refactoring — when you write code that cleanly illustrates a principle, add it below.

### S — Single Responsibility

One struct/module = one reason to change.

**Signal that SRP is violated:** a handler function does I/O, mutation, *and* decision logic in the same body. Extract the decision into a pure helper, the mutation into a named method.

**Good example — `FileChangeHandler` (`src/workspace/file_change_handler.rs`):**
The old `Actor::handle_file_changed` was ~60 lines doing 5 things inline (extract text, update live lines, spawn tree parse, cancel debounce, schedule reindex). After Wave 5b it became:

```rust
// Each name replaces a section comment in the old code
async fn drain_and_apply_file_changes(&mut self, uri: Url, changes: Vec<…>) {
    let Some(text) = self.drain_file_changed_batch(changes) else { return };
    self.indexer.set_live_lines(&uri, &text);
    self.spawn_live_tree_update(uri.clone(), text.clone());
    self.reschedule_debounced_reindex(uri, text);
}
```

`FileChangeHandler` has one reason to change: the file-edit debounce strategy.
`ScanHandler` has one reason to change: when/how workspace scans are enqueued.
`DocumentHandler` has one reason to change: how opened/saved/closed files affect index state.

**Contrast — `src/backend/mod.rs`** is still 765 lines mixing LSP protocol dispatch, workspace config resolution, and direct indexer writes. This is the next refactor target.

### O — Open/Closed

Open for extension (new trait implementors), closed for modification (existing match arms untouched).

**Rust form:** define a trait, implement it for new types. Avoid exhaustive `match` on concrete enums in library code — prefer trait dispatch.

**Good example — `ProgressReporter` trait (`src/indexer/mod.rs`):**
New reporter implementations (LSP client, CLI no-op, test stub) can be added without touching scan logic. `ScanHandler<R: ProgressReporter>` compiles for any `R` — adding a new reporter variant does not require modifying `ScanHandler`.

*Add further examples as they emerge.*

### L — Liskov Substitution

Any `impl Trait` must honour the documented contract of the trait, not just satisfy the type checker. If `fn process<R: ProgressReporter>(r: &R)` says it calls `r.begin()`/`r.end()` in pairs, every impl must tolerate that sequence.

*Add examples as they emerge.*

### I — Interface Segregation

Keep traits small. See rule 13 (YAGNI). A caller should not be forced to implement methods it does not use.

**Good example — Wave 6 `WorkspaceRead` target:** only add methods that backend handlers actually call. Don't pre-populate with 9 methods "in case" (lesson from rule 13 — `WorkspaceRead` was trimmed from 9 to 1 method after review).

*Add further examples as they emerge.*

### D — Dependency Inversion

Depend on trait bounds (`impl Trait`, `<T: Trait>`), not concrete types.

**Good example — `ScanHandler<R: ProgressReporter>` (`src/workspace/scan_handler.rs`):**
```rust
pub(crate) struct ScanHandler<R: ProgressReporter + 'static> {
    indexer: Arc<Indexer>,
    reporter: Arc<R>,   // ← trait bound, not Arc<Client>
    state: Arc<RwLock<State>>,
}
```
In tests, `R = NoopReporter`. In LSP mode, `R = LspProgressReporter`. `ScanHandler` never imports `tower_lsp::Client` — it cannot accidentally depend on LSP transport details.

**Contrast — old `Actor`** held `client: Option<Client>` directly and passed it into every handler. Adding a new notification type required touching `Actor` even when the change was only relevant to one handler.

---

## Architecture patterns (from rust-analyzer)

rust-analyzer is the gold standard for LSP server architecture in Rust. These patterns from its
`GlobalState` / `main_loop` are directly applicable here and should be followed when extending
the workspace actor or adding new background work.

### Pattern A: GlobalState + Snapshot split

rust-analyzer has two types:
- `GlobalState` — owns all mutable state; only the main loop touches it via `&mut self`
- `GlobalStateSnapshot` — cheap clone of `Arc<>` pointers; handed to **read-only** handlers

Apply to kotlin-lsp: `WorkspaceActor` is `GlobalState`. Read-path handlers (`hover`, `definition`,
`references`) must never receive `&mut WorkspaceActor` — they get a `WorkspaceRead` impl
(our equivalent of `GlobalStateSnapshot`). `snapshot()` clones `Arc<Indexer>` + reads
`Arc<RwLock<WorkspacePhase>>` once.

### Pattern B: OpQueue — coalesce slow operations

```rust
// op_queue.rs (rust-analyzer pattern, ~70 lines total)
pub(crate) struct OpQueue<Args = (), Output = ()> {
    op_requested: Option<(Cause, Args)>,
    op_in_progress: bool,
    last_op_result: Option<Output>,
}
impl OpQueue { 
    fn request_op(&mut self, reason: &str, args: Args);    // idempotent: replaces pending
    fn should_start_op(&mut self) -> Option<(Cause, Args)>; // None if already running
    fn op_completed(&mut self, result: Output);
}
```

Use when: multiple events can trigger the same slow operation (e.g. workspace reload). Prevents
thundering-herd. Currently applies to `WorkspaceEvent::Initialize` and `WorkspaceEvent::ChangeRoot`
— if two arrive before the indexer finishes, the second should coalesce, not spawn a second pass.

### Pattern C: Event enum unifying all input sources

rust-analyzer's `Event` enum is:
```rust
enum Event { Lsp(Message), Task(Task), DeferredTask(DeferredTask), Vfs(vfs::loader::Message), Flycheck(...), ... }
```

One `crossbeam::select!` dispatches all. Apply to kotlin-lsp: `WorkspaceEvent` already follows
this — keep all async inputs (LSP notifications, file-watcher, CLI) as variants of one enum,
dispatched in one `select!` / `tokio::select!`.

### Pattern D: Event coalescing in the loop

rust-analyzer drains batch queues after each event:
```rust
Event::Task(task) => {
    self.handle_task(task);
    while let Ok(task) = self.task_pool.receiver.try_recv() { // drain the rest
        self.handle_task(task);
    }
}
```

Apply to kotlin-lsp: when handling `WorkspaceEvent::FileChanged`, drain remaining `FileChanged`
events in the same loop turn before triggering a re-parse. Avoids N re-parse spawns for N rapid
saves.

### Pattern E: Quiescent state predicate

rust-analyzer's `is_quiescent()` is a pure function of multiple flags:
```rust
fn is_quiescent(&self) -> bool {
    self.vfs_done && !self.fetch_workspaces_queue.op_in_progress() && ...
}
```

Post-quiescent work (diagnostics, cache priming, status bar update) only runs when this
transitions `false → true`. Apply to kotlin-lsp: `WorkspacePhase::is_ready()` is the simpler
equivalent. Post-ready side-effects (e.g. emitting `$/progress` end notification) should check
phase transition, not trigger on every event.

### Pattern F: DeferredTaskQueue

Heavy work that depends on a consistent index state must not run inside the notification handler
that triggers it — it blocks the event loop. rust-analyzer enqueues it as a `DeferredTask` and
runs it *after* `process_changes()`.

Apply to kotlin-lsp: `WorkspaceActor::handle_file_changed` already uses
`drop(spawn_blocking(...))` for the live-tree parse (fire-and-forget). For any future work that
needs the fully-indexed state, use the same deferred pattern rather than blocking inline.

## Known limitations

- **No type resolution** — tree-sitter gives structure, not type-checked references
- **`findReferences` on common names** — returns all files with that identifier, not just typed callers
- **No incremental parse** — file changes require reindex (or manual `reindex` command)
- **Java support** — indexed but less thoroughly tested than Kotlin
- **No completion** — textDocument/completion not implemented
