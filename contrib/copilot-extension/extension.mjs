// kotlin-lsp Copilot CLI skill extension
//
// Injects kotlin-lsp capabilities and indexing status as context when working
// with Kotlin/Java projects. Provides tools for checking index state and
// switching the workspace root at runtime.
//
// Installation:
//   mkdir -p ~/.copilot/extensions/kotlin-lsp
//   cp extension.mjs ~/.copilot/extensions/kotlin-lsp/
//
// Requires: Copilot CLI with experimental mode (`copilot --experimental`)

import { joinSession } from "@github/copilot-sdk/extension";
import { execSync } from "child_process";
import { readFileSync, existsSync } from "fs";
import { join } from "path";
import { homedir } from "os";

const KOTLIN_LSP_CONTEXT = `
## kotlin-lsp — Capabilities & Limitations

### ⚠️ Prerequisite: Experimental mode required
The \`lsp\` tool is only available when Copilot CLI is started with \`copilot --experimental\` (or \`--exp\`).
Without it, the LSP tool does not appear and kotlin-lsp will not be connected.
If you see no \`lsp\` tool available, ask the user to restart with \`copilot --experimental\`.

You have access to a Kotlin/Java LSP server (kotlin-lsp) via the \`lsp\` tool.

### Indexing & Readiness
The server indexes files in the background on startup. **Before using workspaceSymbol or goToDefinition, always call \`kotlin_lsp_status\` to check if indexing is complete.**

- Cold index (no cache): 30–70s depending on project size
- Warm start (from cache): 1–3s
- Status file updated every ~5% during indexing with elapsed time and ETA

**Critical**: \`workspaceSymbol\` and \`goToDefinition\` return empty/null until indexing completes.
\`documentSymbol\` and \`hover\` have an on-demand disk fallback — they work immediately on any file.

**Cache staleness**: If the kotlin-lsp binary is updated, the cache version check triggers a full re-index automatically.

### What works reliably ✅
- **textDocument/documentSymbol** — list symbols in a file; always works (disk fallback for un-indexed files)
- **textDocument/hover** — function/class signature + doc comments; works before full index is ready
- **workspace/symbol** — find a class/function by name across the project (includes declaration signature); needs indexing complete
- **textDocument/definition** — go to definition for class, object, fun, val, var; needs index
- **textDocument/references** — find usages; needs index + rg fallback for out-of-index files
- **textDocument/rename** — cross-file rename; needs index
- **textDocument/codeAction** — add missing import; uses rg, works without full index
- **callHierarchy/incomingCalls** — find all callers of a function
- **callHierarchy/outgoingCalls** — find all functions called by a function

### What works poorly or not at all ⚠️
- **workspaceSymbol before index is ready** — returns empty; use \`kotlin_lsp_status\` to check first
- **Extension functions (dot-receiver, cross-file)** — e.g. \`actual.isZero()\` where \`isZero\` is a top-level extension fn defined in another file; goToDefinition returns null
- **No type inference** — tree-sitter based, not compiler-backed; generic type params unresolved (\`List<Foo>\` → \`List\`)
- **Java interop** — Java symbols indexed, but cross-language go-to-def is unreliable
- **No diagnostics** — server never emits compile errors or warnings
- **Lambda type inference** — complex nested lambda types may infer incorrectly
- **Multiplatform (KMP)** — expect/actual resolution is partial
- **rg alternation syntax** — use \`|\` (not \`\\|\`) in ripgrep patterns; \`\\|\` is GNU grep syntax and matches nothing in rg

### Practical workflow for code investigation
For bug investigation across a large Android project, use this order:
1. **Call \`kotlin_lsp_status\`** — check if indexing is done; if not, wait or use fd/rg in the meantime
2. **If wrong workspace**: call \`kotlin_lsp_set_workspace\` with the correct absolute path, then wait for \`kotlin_lsp_status\` to show \`ready\`
3. **\`lsp workspaceSymbol\`** — find the class/function by name to get the exact file path + signature
4. **\`lsp documentSymbol\`** on that file — enumerate symbols to get exact line numbers
5. **\`lsp hover\`** at a line/col — get type info, signatures, doc comments
6. **\`lsp goToDefinition\`** at a line/col — jump to source of a symbol under cursor
7. **\`lsp findReferences\`** at a line/col — find all usages across the project (prefer over rg for symbol references)
8. **\`lsp incomingCalls\`** at a line/col — find all callers of a function
9. **\`view\` with line range** — read the actual code once you have exact locations
10. **Only fall back to \`rg\`** — for free-text search, extension functions, Java interop, or when LSP returns empty

**Prefer LSP over grep/rg whenever you have a known symbol name.** \`findReferences\` is significantly more precise than rg pattern matching and avoids false positives.

### Workspace root
The kotlin-lsp server reads its workspace root from \`~/.config/kotlin-lsp/workspace\` (a plain text file with the absolute path).
- To switch projects: \`echo "/path/to/project" > ~/.config/kotlin-lsp/workspace\`
- The \`kotlin_lsp_set_workspace\` tool does this for you and gives restart instructions.
- After changing workspace, the server picks up the new root on next restart.
`.trim();

function readStatusFile() {
    const statusPath = join(homedir(), ".cache", "kotlin-lsp", "status.json");
    if (!existsSync(statusPath)) return null;
    try {
        return JSON.parse(readFileSync(statusPath, "utf8"));
    } catch {
        return null;
    }
}

function formatStatus(s) {
    if (!s) return "No status file found — LSP server may not have started yet, or binary is older than v0.3.11.";
    if (s.phase === "ready") {
        const ago = Math.round(Date.now() / 1000 - s.completed_at);
        const agoStr = ago < 60 ? `${ago}s ago` : `${Math.round(ago / 60)}m ago`;
        const fromCache = s.cache_hits > 0
            ? ` (${s.cache_hits} from cache, ${s.elapsed_secs}s)`
            : ` (${s.elapsed_secs}s cold index)`;
        return (
            `✅ Ready — ${s.symbols} symbols across ${s.indexed} files${fromCache}, completed ${agoStr}.\n` +
            `workspaceSymbol, goToDefinition, and references are all available.\n` +
            `Workspace: ${s.workspace}`
        );
    }
    if (s.phase === "indexing") {
        const pct = s.total > 0 ? Math.round(s.indexed * 100 / s.total) : 0;
        const eta = s.estimated_remaining_secs != null
            ? `~${Math.round(s.estimated_remaining_secs)}s remaining`
            : "estimating…";
        return (
            `⏳ Indexing in progress — ${s.indexed}/${s.total} files (${pct}%), ${eta}.\n` +
            `⚠️ workspaceSymbol and goToDefinition will return empty until done.\n` +
            `Use \`lsp documentSymbol\` and \`lsp hover\` in the meantime (disk fallback, always works).\n` +
            `Elapsed: ${s.elapsed_secs}s. Workspace: ${s.workspace}`
        );
    }
    return `Unknown phase: ${JSON.stringify(s)}`;
}

function hasKotlinFiles(cwd) {
    try {
        const result = execSync(
            `fd -e kt -e java -e kts . "${cwd}" --max-depth 5 --max-results 1 2>/dev/null`,
            { encoding: "utf8", timeout: 3000 }
        ).trim();
        return result.length > 0;
    } catch {
        return false;
    }
}

const session = await joinSession({
    tools: [
        {
            name: "kotlin_lsp_info",
            description:
                "Returns a description of kotlin-lsp capabilities and known limitations. " +
                "Call this before using the LSP tool on Kotlin/Java files to understand what is and isn't supported.",
            parameters: { type: "object", properties: {} },
            skipPermission: true,
            handler: async () => KOTLIN_LSP_CONTEXT,
        },
        {
            name: "kotlin_lsp_status",
            description:
                "Check whether kotlin-lsp has finished indexing the workspace. " +
                "Always call this before using workspaceSymbol, goToDefinition, or references — " +
                "those return empty if indexing isn't complete. " +
                "Returns current phase (indexing/ready), file counts, symbol count, elapsed time, and ETA.",
            parameters: { type: "object", properties: {} },
            skipPermission: true,
            handler: async () => formatStatus(readStatusFile()),
        },
        {
            name: "kotlin_lsp_set_workspace",
            description:
                "Point the kotlin-lsp server at a different workspace root and trigger a full reindex. " +
                "Use this when the LSP was started from the wrong directory (e.g., started from a " +
                "different project than the one you want to navigate). " +
                "Writes ~/.config/kotlin-lsp/workspace and gives restart instructions. " +
                "After calling this, use kotlin_lsp_status to wait for indexing to complete before " +
                "using workspaceSymbol or other LSP tools.",
            parameters: {
                type: "object",
                properties: {
                    path: {
                        type: "string",
                        description: "Absolute path to the new workspace root directory.",
                    },
                },
                required: ["path"],
            },
            skipPermission: false,
            handler: async ({ path: newPath }) => {
                const workspaceFile = join(homedir(), ".config", "kotlin-lsp", "workspace");
                try {
                    const { mkdirSync, writeFileSync } = await import("fs");
                    mkdirSync(join(homedir(), ".config", "kotlin-lsp"), { recursive: true });
                    writeFileSync(workspaceFile, newPath, "utf8");
                    return (
                        `✅ Workspace root updated to \`${newPath}\`.\n\n` +
                        "The kotlin-lsp server will use this root on next startup. " +
                        "If it's currently running, kill it so the CLI restarts it:\n\n" +
                        "```bash\nkill $(pgrep -f 'kotlin-lsp')\n```\n\n" +
                        "Then call `kotlin_lsp_status` and wait for phase `ready`."
                    );
                } catch (e) {
                    return (
                        `Failed to write workspace file: ${e.message}\n\n` +
                        `Run manually: \`echo "${newPath}" > ${workspaceFile}\``
                    );
                }
            },
        },
    ],
    hooks: {
        onSessionStart: async ({ cwd }) => {
            if (cwd && hasKotlinFiles(cwd)) {
                const status = readStatusFile();
                const statusLine = status
                    ? `\n\n**Current indexing status**: ${formatStatus(status)}`
                    : "";
                return { additionalContext: KOTLIN_LSP_CONTEXT + statusLine };
            }
        },
    },
});
