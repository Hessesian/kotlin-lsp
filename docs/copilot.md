# GitHub Copilot CLI integration

kotlin-lsp integrates with the [GitHub Copilot CLI](https://githubnext.com/projects/copilot-cli/) to give Copilot full code-intelligence tools when working on Kotlin/Java/Swift projects.

> **Requires:** `copilot --experimental` (or `--exp`) — the `lsp` tool is only available in experimental mode.

## Setup

**1. Add kotlin-lsp to Copilot's LSP config** (`~/.copilot/lsp-config.json`):

```json
{
  "lspServers": {
    "kotlin-lsp": {
      "command": "/home/user/.cargo/bin/kotlin-lsp",
      "args": [],
      "env": {
        "KOTLIN_LSP_MAX_FILES": "20000"
      },
      "fileExtensions": {
        ".kt": "kotlin",
        ".kts": "kotlin",
        ".java": "java",
        ".swift": "swift"
      }
    }
  }
}
```

> **Note:** `command` must be an **absolute path** — Copilot does not expand `~` or use your shell's `PATH`.  
> The default cargo install location is `~/.cargo/bin/kotlin-lsp`; substitute your actual home directory (or run `which kotlin-lsp` to confirm).

**2. (Optional) Install the Copilot skill extension** for a richer agent experience — it injects indexing status context automatically and provides `kotlin_lsp_status` and `kotlin_lsp_set_workspace` tools:

```bash
mkdir -p ~/.copilot/extensions/kotlin-lsp
cp contrib/copilot-extension/extension.mjs ~/.copilot/extensions/kotlin-lsp/
```

The extension provides:
- **`kotlin_lsp_status`** — check indexing phase, file counts, symbol count, and ETA before running queries
- **`kotlin_lsp_set_workspace`** — switch the indexed project at runtime without restarting Copilot
- **Auto-injected context** — when you open a session, indexing status and LSP capabilities are injected automatically

## Agentic workflow

Once configured, Copilot can navigate your codebase using:

```
lsp workspaceSymbol "MyClass"         → find any class/function by name (includes signature)
lsp documentSymbol <file>             → list all symbols in a file with line numbers
lsp hover <file> <line> <col>         → get type signature and docs at a position
lsp goToDefinition <file> <line> <col>→ jump to the definition
lsp findReferences <file> <line> <col>→ find all usages across the project
lsp incomingCalls <file> <line> <col> → find all callers of a function
lsp outgoingCalls <file> <line> <col> → find all functions called by a function
```

**Tip:** `workspaceSymbol` results include the declaration signature (e.g. `fun processPayment(amount: BigDecimal, currency: String): Result<Unit>`), so you rarely need a follow-up `hover` call.

## Workspace root

By default, kotlin-lsp uses the LSP client's `rootUri` — which is your current working directory. This means switching between projects works automatically.

If you need to override, set the `KOTLIN_LSP_WORKSPACE_ROOT` env var, or write a path to `~/.config/kotlin-lsp/workspace`.
