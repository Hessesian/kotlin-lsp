# External Integrations

## Core Sections (Required)

### 1) External Services and APIs

| Service | Role | Usage Pattern | Evidence |
|---------|------|----------------|----------|
| **ripgrep (rg)** | Text search for cross-file symbol references | Invoked via `Command::new("rg")` when symbol not in index | src/rg.rs |
| **fd** | Fast file discovery | Primary file enumeration; used in `find_source_files()` | src/indexer/discover.rs |
| **walkdir** | Fallback file discovery | Used when `fd` unavailable or `ignore` patterns needed | src/indexer/discover.rs |
| **crates.io** | Binary distribution | `cargo install kotlin-lsp`; automated publish via CI | .github/workflows/release.yml, Cargo.toml |
| **GitHub Releases** | Binary downloads | Pre-built binaries for 4 targets; built in CI on tag push | .github/workflows/release.yml |

### 2) Credentials and Secrets

- **No runtime secrets:** kotlin-lsp is a local tool with no authentication required for use
- **CI secrets (build-time only):**
  - `CARGO_REGISTRY_TOKEN` — used by the `publish` job in `.github/workflows/release.yml` to publish to crates.io; stored as a GitHub Actions repository secret
  - `GITHUB_TOKEN` — standard Actions token for GitHub Release creation
- **.env / config:** No environment variables beyond `RUST_LOG`, `KOTLIN_LSP_WORKSPACE_ROOT`, and `GRADLE_USER_HOME`

### 3) Databases and Persistence

- **Local only:** No remote database connections
- **Disk cache:** `~/.cache/kotlin-lsp/index-<hash>.bin` (bincode + SHA2)
  - Cache invalidated on file content change (SHA2 checksum mismatch)
  - Manually cleared via `kotlin-lsp/reindex` command (LSP workspace command)
- **Status file:** `~/.cache/kotlin-lsp/status.json` (indexing progress, last seen in Copilot CLI extension)

### 4) Message Queues and Event Buses

- **None:** No message queue or pub/sub system
- **Tokio channels:** Internal task coordination only (background parse workers)

### 5) Monitoring and Observability

- **No APM integration:** No external monitoring
- **Logging only:** `env_logger` to stderr (captured by editor integration)
- **No metrics export:** Copilot CLI reads `status.json` for progress visibility
- **LSP diagnostics:** Syntax errors reported via `textDocument/publishDiagnostics`

### 6) External Code / Libraries

- **tree-sitter grammars:**
  - `tree-sitter-kotlin` (0.3) — compiled C code for Kotlin parsing
  - `tree-sitter-java` (0.21) — Java grammar
  - `tree-sitter-swift-bundled` (0.1.0) — Swift grammar
  - All downloaded from crates.io at build time; no network calls at runtime
  
- **LSP protocol library:**
  - `tower-lsp` (0.20) — async LSP service framework
  - `lsp_types` (re-exported from tower_lsp) — LSP protocol types

- **Serialization / CLI:**
  - `serde / serde_json / bincode` — index cache format
  - `walkdir / ignore / globset` — file filtering

### 7) Evidence

- src/rg.rs (ripgrep invocation)
- src/indexer/discover.rs (fd/walkdir file discovery)
- src/indexer/cache.rs (disk cache path, bincode serialization)
- Cargo.toml (tree-sitter dependencies)
- README.md (runtime dependencies: fd, rg)
- docs/codebase/.codebase-scan.txt (dependency manifest)

## Extended Sections (Optional)

### Command-Line Tool Invocations

#### ripgrep (rg) Usage

```rust
// src/rg.rs
Command::new("rg")
  .arg("--word-regexp")
  .arg("--json")
  .arg("--case-sensitive")
  .arg(pattern)
  .current_dir(workspace_root)
  .output()
```

Patterns:
- Word-based search: `\bSymbolName\b`
- Returns JSON output; parsed by `parse_rg_results()`
- Used for: cross-file references, fallback definition resolution

#### fd (File Discovery)

```rust
// src/indexer/discover.rs
Command::new("fd")
  .arg("--full-path")
  .arg(pattern)  // e.g. "*/com/example/.*\.kt$"
  .current_dir(workspace_root)
  .output()
```

Patterns:
- Fast traversal with gitignore respecting
- Optimization: `--full-path` searches entire path depth in one pass (O(1) per import)
- Fallback to `walkdir` crate if `fd` unavailable

### Cache Location and Lifecycle

**Path:** `~/.cache/kotlin-lsp/index-<sha2_of_workspace_root>.bin`

**Lifecycle:**
1. On server initialize: attempt `try_load_cache()` for workspace
2. If cache exists + not invalidated: use cached symbols
3. During/after workspace scan: `save_cache()` after indexing completes
4. On file change: re-parse file, update cache entry, invalidate if content hash differs
5. On user request: `kotlin-lsp/reindex` command clears cache, rescans all files

**Invalidation triggers:**
- File content checksum (SHA2) mismatch
- Explicit reindex command
- (No TTL; cache persists until invalidated)

### LSP Progress Reporting

**Mechanism:** `$/progress` LSP notification with `WorkDoneProgress` tokens

**Adapter flow:**
```
Indexer (outbound port: ProgressReporter trait)
  ↓
LspProgressReporter(Client) in backend/mod.rs (adapter impl)
  ↓
client.send_notification("$/progress", WorkDoneProgressBegin)
client.send_notification("$/progress", WorkDoneProgressReport)
client.send_notification("$/progress", WorkDoneProgressEnd)
```

**NoopReporter** used in CLI mode: all notifications are no-ops.

### Copilot CLI Extension Integration

**Location:** `.github/extensions/kotlin-lsp/extension.mjs`

**Purpose:** Provide custom LSP tools for Copilot CLI:
- `kotlin_lsp_status` — read `~/.cache/kotlin-lsp/status.json` for progress
- `kotlin_lsp_set_workspace` — write workspace root to `~/.config/kotlin-lsp/workspace`
- `kotlin_lsp_info` — display capabilities and limitations

**Status file format:** JSON with `{ files_indexed, total_files, phase, start_time }`

### External Configuration via initializationOptions

**LSP server accepts:**
```json
{
  "initializationOptions": {
    "sourcePaths": ["/path/to/sources", "~/android-sources"],
    "ignorePatterns": ["bazel-*", "third_party/**", "generated/**"]
  }
}
```

- `sourcePaths` — additional directories to index (e.g., Gradle cache, sources.jar extracts)
- `ignorePatterns` — gitignore-style exclusions (v0.7.1+)

**Python helper:** `contrib/extract-sources.py` finds `*-sources.jar` in Gradle cache, extracts, prepares for `sourcePaths`. **As of v0.12.0, this functionality is built-in:** `kotlin-lsp extract-sources` (written in Rust, ships in the binary). The Python script is retained for reference only.

### Transport Options

kotlin-lsp supports two transport modes:
- **Stdio (default):** JSON-RPC over stdin/stdout; standard for all LSP clients
- **TCP (`--port N`):** Listens on `127.0.0.1:N`, loopback only; useful for Android Studio (via LSP4J), Sora Editor, or other clients that cannot spawn a subprocess

### crates.io Publish Pipeline

**Automated:** The `publish` job in `.github/workflows/release.yml` runs `cargo publish --no-verify` on every tag push (or manual `workflow_dispatch`).

**Trigger sequence:**
1. Push tag `v*.*.*` → `build` job cross-compiles 4 targets
2. `release` job uploads binaries to GitHub Releases
3. `publish` job publishes to crates.io (needs `CARGO_REGISTRY_TOKEN` secret)

**crate size:** 309 KB compressed (demo/contrib/docs excluded via `[package] exclude = [...]` in Cargo.toml).
