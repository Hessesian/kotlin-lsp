// kotlin-lsp-extension.mjs
import { execFile } from "node:child_process";
import { promises as fs } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { joinSession } from "@github/copilot-sdk/extension";

const SKILL_NAME = "kotlin-lsp";
const SKILL_NUDGE = "Invoke the `kotlin-lsp` skill for full navigation guidance (use the skill tool with skill: \"kotlin-lsp\").";

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
  SKILL_NUDGE,
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
    "Use Kotlin LSP symbol/navigation tools before grep/glob/bash search.",
    "Use grep/rg only for free-text search, extension functions, generated code, or Java interop cases where LSP cannot help.",
    SKILL_NUDGE,
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
const STATUS_PATH = path.join(os.homedir(), ".cache", "kotlin-lsp", "status.json");

/**
 * Kill only the kotlin-lsp process(es) managed by Copilot CLI.
 * 1. Reads the PID from status.json and sends SIGKILL.
 * 2. Also kills any remaining kotlin-lsp processes (handles stale/zombie PIDs).
 * 3. Resets status.json so the CLI doesn't try to contact a dead process.
 */
async function killCopilotServer() {
  const killed = [];

  // Kill the PID recorded in status.json (Copilot-managed server)
  try {
    const s = JSON.parse(await fs.readFile(STATUS_PATH, "utf8"));
    if (s.pid) {
      await runShell(`kill -9 ${s.pid} 2>/dev/null || true`);
      killed.push(String(s.pid));
    }
  } catch { /* status.json missing or malformed */ }

  // Kill any remaining kotlin-lsp processes (handles stale PIDs / zombie siblings)
  const { stdout } = await runShell("pgrep -x kotlin-lsp 2>/dev/null || true");
  for (const pid of stdout.trim().split(/\s+/).filter(Boolean)) {
    if (!killed.includes(pid)) {
      await runShell(`kill -9 ${pid} 2>/dev/null || true`);
      killed.push(pid);
    }
  }

  // Reset status.json so the CLI doesn't try to contact a dead process
  try {
    await fs.writeFile(STATUS_PATH, "{}\n", "utf8");
  } catch { /* ignore */ }

  return killed.length > 0
    ? `Killed PID(s) ${killed.join(", ")}.`
    : "No running server found (will auto-start on next LSP call).";
}

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
      name: "kotlin_lsp_complete",
      description: [
        "List completion candidates at a file position.",
        "Useful for discovering what functions, classes, or properties are available in scope",
        "without knowing the exact name upfront — e.g. after typing a partial identifier or",
        "after a dot. Returns label, kind (class/fun/method/var/…), detail qualifier, and",
        "the auto-import text that the editor would insert.",
        "FILE must be an absolute path. LINE is 1-based.",
        "Use dot=true to auto-place cursor after the last '.' on the line (no col needed).",
        "Use eol=true to auto-place cursor at end of trimmed line content (no col needed).",
        "Use no_stdlib=true to skip ~/.kotlin-lsp/sources and return only workspace symbols.",
        "This makes completion ~5x faster (~1s vs ~10s) and is sufficient for project types,",
        "sealed classes, and enum members. Omit no_stdlib when stdlib/library completions matter.",
        "Requires the workspace index to be built (run `kotlin-lsp index` first, or trigger",
        "indexing via `lsp documentSymbol` if the server is running).",
        "Returns JSON: [{label, kind, detail?, import?}, …].",
      ].join(" "),
      parameters: {
        type: "object",
        properties: {
          file: {
            type: "string",
            description: "Absolute path to the Kotlin/Java file",
          },
          line: {
            type: "integer",
            description: "1-based line number",
          },
          col: {
            type: "integer",
            description: "1-based column number. Optional when dot or eol is true.",
          },
          dot: {
            type: "boolean",
            description: "Auto-place cursor just after the last '.' on the line. Overrides col.",
          },
          eol: {
            type: "boolean",
            description: "Auto-place cursor at end of trimmed line content. Overrides col.",
          },
          no_stdlib: {
            type: "boolean",
            description: "Skip ~/.kotlin-lsp/sources (extracted stdlib/libraries). Returns only workspace symbols. Much faster (~1s vs ~10s). Recommended for project-type completion.",
          },
          root: {
            type: "string",
            description: "Optional workspace root (default: nearest .git parent of file)",
          },
        },
        required: ["file", "line"],
      },
      handler: async (args) => {
        const file = path.resolve(args.file);
        const line = Number(args.line);
        const dot = args.dot === true;
        const eol = args.eol === true;
        const noStdlib = args.no_stdlib === true;

        if (!Number.isInteger(line) || line < 1) return "Error: line must be a positive integer";

        let colArg = "";
        if (dot) {
          colArg = "--dot";
        } else if (eol) {
          colArg = "--eol";
        } else {
          const col = Number(args.col);
          if (!Number.isInteger(col) || col < 1) return "Error: col must be a positive integer (or use dot/eol)";
          colArg = String(col);
        }

        const rootFlag = args.root ? `--root ${JSON.stringify(path.resolve(args.root))}` : "";
        const noStdlibFlag = noStdlib ? "--no-stdlib" : "";
        const cmd = `kotlin-lsp complete ${JSON.stringify(file)} ${line} ${colArg} --json ${noStdlibFlag} ${rootFlag}`.trim();
        const { code, stdout, stderr } = await runShell(cmd, 30000);

        if (code !== 0 && !stdout.trim()) {
          return `No completions at ${file}:${line}${stderr ? `\n${stderr}` : ""}`;
        }
        return stdout.trim() || `No completions at ${file}:${line}`;
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

        // 2. Config override (KOTLIN_LSP_PREFER_CONFIG_ROOT=1 must be set in lsp-config.json)
        try {
          const override_ = (await fs.readFile(WORKSPACE_CONFIG, "utf8")).trim();
          if (override_) lines.push(`Config override:  ${override_}`);
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
        "Switch the Copilot CLI kotlin-lsp instance to a different workspace directory.",
        "Writes the path to ~/.config/kotlin-lsp/workspace and kills only the Copilot-managed",
        "server process — editor LSP instances are not affected.",
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

        // Write workspace config (safe: no hot-reload side effects)
        await fs.mkdir(path.dirname(WORKSPACE_CONFIG), { recursive: true });
        await fs.writeFile(WORKSPACE_CONFIG, workspacePath + "\n", "utf8");

        // Kill only the Copilot-managed server (PID from status.json)
        const killed = await killCopilotServer();

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
          SKILL_NUDGE,
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
