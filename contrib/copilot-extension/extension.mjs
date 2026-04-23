// kotlin-lsp-extension.mjs
import { execFile } from "node:child_process";
import { promises as fs } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { joinSession } from "@github/copilot-sdk/extension";

const README_PATH = ".github/extensions/kotlin-lsp/README.md";

// ── Grep-overuse detection ────────────────────────────────────────────

// Tools that count as "grep-type" (code navigation by text search)
const GREP_TOOLS = new Set(["grep", "kotlin_rg", "kotlin_find_subtypes"]);

// bash commands that are grep-type code navigation
// Matches: rg/grep/find with *.kt/*.kts/*.java, OR fd with -e kt/-e kts/-e java extension flags
const GREP_BASH_RE = /\b(rg|grep|fd|find)\b.*(-e\s+(?:kt|kts|java)|\.(kt|kts|java))/;

// glob patterns targeting Kotlin/Java files count as grep-type navigation
const GLOB_KT_RE = /\.(kt|java|kts)\b/;

// Tools that count as proper LSP usage (reset the streak)
const LSP_TOOLS = new Set(["lsp"]);

const STREAK_THRESHOLD = 3;  // consecutive grep calls before reinforcing

let grepStreak = 0;

const LSP_REMINDER = [
  "⚠️  LSP REMINDER — too many grep/glob calls for Kotlin/Java navigation.",
  "STOP using grep/rg/glob for symbol lookup. Use LSP tools instead:",
  "  • lsp workspaceSymbol   — find any class/function/property by name",
  "  • lsp goToDefinition    — jump to where a symbol is defined",
  "  • lsp findReferences    — find all usages of a symbol",
  "  • lsp goToImplementation — find interface implementors (transitive)",
  "  • lsp incomingCalls / outgoingCalls — call graph",
  "grep/rg/glob is ONLY valid for: free-text/comment search, extension functions,",
  "generated code, Java interop, or when LSP returned empty for a known-good symbol.",
  `Full guide: \`${README_PATH}\`.`,
].join("\n");

// ── Helpers ──────────────────────────────────────────────────────────

function asString(value) {
  return typeof value === "string" ? value : "";
}

function normalizeToolArgs(rawToolArgs) {
  if (rawToolArgs == null) return {};
  if (typeof rawToolArgs === "object") return rawToolArgs;
  if (typeof rawToolArgs === "string") {
    try { return JSON.parse(rawToolArgs); } catch { return { raw: rawToolArgs }; }
  }
  return { raw: String(rawToolArgs) };
}

function denyMessage() {
  return [
    "Blocked: Kotlin/Java symbol lookup must use Kotlin LSP first.",
    `Read \`${README_PATH}\` first.`,
    "Use Kotlin LSP symbol/navigation tools before grep/glob/bash search.",
    "Use grep/rg only for free-text search, extension functions, generated code, or Java interop cases where LSP cannot help.",
  ].join(" ");
}

// ── shell helper for tools ───────────────────────────────────────────

function runShell(cmd, timeout = 8000) {
  return new Promise((resolve) => {
    execFile("/bin/sh", ["-c", cmd], { maxBuffer: 1024 * 256, timeout }, (err, stdout, stderr) => {
      resolve({ code: err?.code ?? 0, stdout: stdout || "", stderr: stderr || "" });
    });
  });
}

const WORKSPACE_CONFIG = path.join(os.homedir(), ".config", "kotlin-lsp", "workspace");

// ── rg helper for tools ──────────────────────────────────────────────

function runRg(args, cwd, timeout = 10000) {
  return new Promise((resolve) => {
    execFile("rg", args, { cwd, maxBuffer: 1024 * 512, timeout }, (err, stdout, stderr) => {
      if (err && !stdout) resolve(stderr ? `Error: ${stderr}` : "No matches found.");
      else resolve(stdout || "No matches found.");
    });
  });
}

// ── Main ─────────────────────────────────────────────────────────────

const session = await joinSession({
  tools: [
    {
      name: "kotlin_find_subtypes",
      description: [
        "Last-resort fallback for finding subtypes — use `lsp goToImplementation` first.",
        "Only use this if goToImplementation returns empty (LSP not indexed, or edge case).",
        "Uses rg text search — returns candidates, not compiler-verified results.",
        "Handles: class/object/interface declarations, generics, multiline supertypes.",
      ].join(" "),
      parameters: {
        type: "object",
        properties: {
          typeName: {
            type: "string",
            description: "Simple name of the interface/class/abstract class to find subtypes of (e.g. 'ISimpleLoadDataInteractor')",
          },
          path: {
            type: "string",
            description: "Directory to search in. Defaults to cwd.",
          },
        },
        required: ["typeName"],
      },
      handler: async (args) => {
        const name = args.typeName.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
        const cwd = args.path || process.cwd();
        // Match: class/object/interface Foo ... : ... TypeName
        // Handles generics like TypeName<T>, qualified like pkg.TypeName
        const pattern = `(?:class|object|interface)\\s+\\w+[^{]*[:(,]\\s*(?:\\w+\\.)*${name}(?:<[^>]*>)?`;
        const rgArgs = [
          "--line-number", "--no-heading", "--color", "never",
          "--multiline", "--glob", "*.kt",
          pattern, ".",
        ];
        const result = await runRg(rgArgs, cwd);
        if (!result || result === "No matches found.") {
          return `No direct subtypes of '${args.typeName}' found in ${cwd}`;
        }
        return `Direct subtype candidates for '${args.typeName}':\n${result}`;
      },
    },
    {
      name: "kotlin_rg",
      description: [
        "Restricted ripgrep for Kotlin/Java files — use ONLY as fallback when LSP cannot help.",
        "Valid reasons: extension functions, generated code, free-text/comment search, Java interop,",
        "convention/pattern discovery, or when LSP returned empty for a known-good symbol.",
        "NOT for: simple class/function/symbol lookup (use LSP workspaceSymbol/goToDefinition first).",
      ].join(" "),
      parameters: {
        type: "object",
        properties: {
          pattern: { type: "string", description: "Regex pattern to search for" },
          path: { type: "string", description: "File or directory to search. Defaults to cwd." },
          glob: { type: "string", description: "Glob filter (e.g. '*.kt'). Defaults to '*.{kt,java}'." },
          reason: {
            type: "string",
            description: "Why LSP can't help (e.g. 'extension function', 'LSP returned empty', 'free-text search', 'generated code')",
          },
        },
        required: ["pattern", "reason"],
      },
      handler: async (args) => {
        const pattern = asString(args.pattern);
        const cwd = args.path || process.cwd();
        const glob = args.glob || "*.{kt,java}";
        const rgArgs = [
          "--line-number", "--no-heading", "--color", "never",
          "--glob", glob,
          pattern, cwd,
        ];
        return await runRg(rgArgs, cwd);
      },
    },
    {
      name: "kotlin_lsp_status",
      description: [
        "Check kotlin-lsp server status: active workspace, indexing phase, symbol count.",
        "Reads live status from ~/.cache/kotlin-lsp/status.json written by the server.",
        "Call before workspaceSymbol when uncertain if indexing has completed.",
        "If no server is running, the next LSP tool call will auto-start it.",
      ].join(" "),
      parameters: { type: "object", properties: {}, required: [] },
      handler: async () => {
        const lines = [];

        // 1. Live server status from status.json
        const statusPath = path.join(os.homedir(), ".cache", "kotlin-lsp", "status.json");
        try {
          const raw = await fs.readFile(statusPath, "utf8");
          const s = JSON.parse(raw);
          lines.push(`Active workspace: ${s.workspace ?? "(unknown)"}`);
          lines.push(`Indexing phase:   ${s.phase ?? "unknown"}`);
          if (s.phase === "indexing") {
            const pct = s.total > 0 ? Math.round((s.indexed / s.total) * 100) : 0;
            lines.push(`Progress:         ${s.indexed}/${s.total} files (${pct}%, ${s.cache_hits ?? 0} cached)`);
          } else if (s.phase === "done") {
            lines.push(`Indexed:          ${s.total} files, ${s.symbols ?? 0} symbols`);
          }
        } catch {
          lines.push("Active workspace: (status.json not found — server not started yet)");
        }

        // 2. Config override (if set, this wins over rootUri at startup)
        try {
          const override_ = (await fs.readFile(WORKSPACE_CONFIG, "utf8")).trim();
          lines.push(`Config override:  ${override_}`);
        } catch { /* not set */ }

        // 3. Server process
        const { stdout: pids } = await runShell("pgrep -x kotlin-lsp 2>/dev/null || true");
        const pidList = pids.trim();
        lines.push(pidList ? `Server running:   yes (PID ${pidList.replace(/\n/g, ", ")})` : "Server running:   no");

        lines.push("");
        lines.push("Note: workspaceSymbol needs phase=done. Open any file (lsp documentSymbol) to trigger indexing.");
        return lines.join("\n");
      },
    },
    {
      name: "kotlin_lsp_set_workspace",
      description: [
        "Switch kotlin-lsp to a different workspace directory.",
        "Writes the path to ~/.config/kotlin-lsp/workspace and kills the current server process.",
        "The server will auto-restart on next LSP tool call and re-index the new workspace.",
        "Always call this (not manual config edits) when switching between projects.",
      ].join(" "),
      parameters: {
        type: "object",
        properties: {
          workspacePath: {
            type: "string",
            description: "Absolute path to the workspace root directory (e.g. /home/user/Work/MyProject/android)",
          },
        },
        required: ["workspacePath"],
      },
      handler: async (args) => {
        const workspacePath = path.resolve(args.workspacePath);

        // Validate directory exists
        try {
          const stat = await fs.stat(workspacePath);
          if (!stat.isDirectory()) {
            return `Error: '${workspacePath}' is not a directory.`;
          }
        } catch {
          return `Error: '${workspacePath}' does not exist.`;
        }

        // Write config
        await fs.mkdir(path.dirname(WORKSPACE_CONFIG), { recursive: true });
        await fs.writeFile(WORKSPACE_CONFIG, workspacePath + "\n", "utf8");

        // Kill server
        const { stdout: pids } = await runShell("pgrep -x kotlin-lsp 2>/dev/null || true");
        const pidList = pids.trim().split("\n").filter(Boolean);
        for (const pid of pidList) {
          await runShell(`kill ${pid} 2>/dev/null || true`);
        }

        const killed = pidList.length > 0
          ? `Killed PID ${pidList.join(", ")}.`
          : "No running server found (will auto-start on next LSP call).";

        return [
          `Workspace set to: ${workspacePath}`,
          killed,
          "Open any file with `lsp documentSymbol` to start indexing, then wait for progress to complete before using workspaceSymbol.",
        ].join("\n");
      },
    },
  ],

  hooks: {
    onSessionStart: async () => {
      grepStreak = 0;
      return {
        additionalContext: [
          "Kotlin/Java code navigation must use Kotlin LSP first.",
          "Use grep/rg only for free-text, generated code, extension functions, or Java interop fallback.",
          "Use `lsp goToImplementation` for interface implementors (transitive). Only use `kotlin_find_subtypes` if LSP returns empty.",
          "For extension functions, use `lsp workspaceSymbol` with dot-qualified query (e.g. 'StoreState.isReady').",
          `Guide: \`${README_PATH}\`.`,
        ].join(" "),
      };
    },

    onPostToolUse: async (input) => {
      const { toolName, toolArgs } = input;

      if (LSP_TOOLS.has(toolName)) {
        grepStreak = 0;
        return;
      }

      if (GREP_TOOLS.has(toolName)) {
        grepStreak++;
        return;
      }

      if (toolName === "bash") {
        const cmd = asString(normalizeToolArgs(toolArgs).command);
        if (GREP_BASH_RE.test(cmd)) {
          grepStreak++;
        }
      }

      if (toolName === "glob") {
        const pattern = asString(normalizeToolArgs(toolArgs).pattern);
        if (GLOB_KT_RE.test(pattern)) {
          grepStreak++;
        }
      }
    },

    onUserPromptSubmitted: async () => {
      if (grepStreak >= STREAK_THRESHOLD) {
        return { additionalContext: `[grep streak: ${grepStreak}]\n${LSP_REMINDER}` };
      }
    },
  },
});
