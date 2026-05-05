# Technology Stack

## Core Sections (Required)

### 1) Runtime Summary

| Area | Value | Evidence |
|------|-------|----------|
| Primary language | Rust | Cargo.toml, src/*.rs |
| Runtime + version | Rust 1.76+ (edition 2021) | Cargo.toml |
| Package manager | Cargo | Cargo.toml, Cargo.lock |
| Module/build system | Cargo workspace (single-package) | Cargo.toml `[workspace] members = ["."]` |

### 2) Production Frameworks and Dependencies

| Dependency | Version | Role in system | Evidence |
|------------|---------|----------------|----------|
| tower-lsp | 0.20 | LSP protocol implementation (async tower service) | Cargo.toml |
| tokio | 1 (full features) | Async runtime for spawning tasks, I/O, timers | Cargo.toml |
| tree-sitter | 0.22 | Parsing library for Kotlin, Java, Swift grammars | Cargo.toml |
| tree-sitter-kotlin | 0.3 | Kotlin grammar for tree-sitter | Cargo.toml |
| tree-sitter-java | 0.21 | Java grammar for tree-sitter | Cargo.toml |
| tree-sitter-swift-bundled | 0.1.0 | Swift grammar for tree-sitter | Cargo.toml |
| dashmap | 5 | Concurrent HashMap (no Mutex overhead) for index storage | Cargo.toml |
| log / env_logger | 0.4 / 0.11 | Logging framework with environment-based config | Cargo.toml |
| walkdir | 2 | Fallback file discovery when `fd` is unavailable | Cargo.toml |
| ignore | 0.4 | gitignore parsing for file filtering | Cargo.toml |
| globset | 0.4 | glob pattern matching for path filtering | Cargo.toml |
| serde / serde_json / bincode | 1 / 1 / 1 | Serialization for index cache persistence | Cargo.toml |
| sha2 | 0.10 | SHA2 hashing for file content checksums | Cargo.toml |
| futures | 0.3.32 | Futures utilities for async composition | Cargo.toml |

### 3) Development Toolchain

| Tool | Purpose | Evidence |
|------|---------|----------|
| cargo build | Compile binary | Cargo.toml (implicit) |
| cargo test | Run unit and integration tests | Cargo.toml (implicit) |
| cargo clippy | Lint (enabled by default; used in CI) | Recent commits mention "clippy" fixes |
| rustfmt | Code formatting (implicit, Rust standard) | Commit "chore(fmt): apply cargo fmt" |

### 4) Key Commands

```bash
# Install (from crates.io)
cargo install kotlin-lsp

# Build from source
cargo build --release

# Run tests (unit + integration)
cargo test

# Install from local source
cargo install --path .

# Lint with strict settings
cargo clippy -- -W clippy::cognitive_complexity -W clippy::too_many_lines
```

### 5) Environment and Config

- **Config sources:** LSP `initializationOptions` (JSON), environment variables
- **Required env vars:** 
  - `KOTLIN_LSP_MAX_FILES` — max files to eagerly index (default: unlimited as of v0.9.3)
  - `RUST_LOG` — logging level (e.g., `debug`, `info`; default: `off`)
- **Runtime dependencies (user must install):** 
  - `fd` — fast file discovery (fallback: walkdir)
  - `rg` (ripgrep) — fast text search (used for cross-file references, fallback resolution)
- **LSP features enabled at runtime:** macOS/Linux/Windows via stdio transport

### 6) Evidence

- Cargo.toml (main manifest)
- Cargo.lock (dependency lock file)
- README.md (install, quick start, features)
- docs/architecture.md (high-level overview)

## Extended Sections (Optional)

### Release Profile

```toml
[profile.release]
opt-level = 3           # Maximum optimization
lto = "thin"            # Thin Link Time Optimization
codegen-units = 1       # Single codegen unit for better LTO
strip = true            # Strip debug symbols for smaller binary
```

### Dependency Categories

**LSP & Async:**
- tower-lsp, tokio, futures

**Parsing:**
- tree-sitter, tree-sitter-kotlin, tree-sitter-java, tree-sitter-swift-bundled

**File System & Discovery:**
- walkdir, ignore, globset, fd (external binary)

**Data Storage & Serialization:**
- dashmap, serde, serde_json, bincode, sha2

**Logging:**
- log, env_logger

**Testing (dev-only):**
- tempfile

### Build Constraints

- **C compiler required:** tree-sitter grammars compile C code
- **Minimum Rust:** 1.76+ (for `impl Trait` in trait bounds and other modern features)
- **Target platforms:** Linux (primary), macOS, Windows
