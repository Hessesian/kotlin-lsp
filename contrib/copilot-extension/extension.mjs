// kotlin-lsp-extension.mjs
import { execFile } from "node:child_process";
import { promises as fs } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { joinSession } from "@github/copilot-sdk/extension";

const README_PATH = ".github/extensions/kotlin-lsp/README.md";

// ── Trigger regexes ──────────────────────────────────────────────────

const KOTLIN_FILE_TRIGGER = /\.(?:kts?|swift)\b/i;

const KOTLIN_HINT_TRIGGER =
  /\b(?:kotlin|java|swift|\.kt|\.kts|\.java|\.swift|ViewModel|Repository|UseCase|Composable|Activity|Fragment|CPageHeader|CPage|UIViewController|SwiftUI)\b/i;

const FREE_TEXT_TRIGGER =
  /\b(?:TODO|FIXME|HACK|text|string|comment|comments|message|messages|translation|copy|literal|log|logs)\b/i;

// Identifier-like: PascalCase, camelCase, UPPER_SNAKE, or dot-qualified name
const IDENTIFIER_PATTERN = /^[A-Za-z_][A-Za-z0-9_.]*$/;

// Commands that are never "search" — always allow
const SAFE_BASH_CMDS = /^\s*(?:ls|cat|head|tail|wc|mkdir|cp|mv|rm|touch|chmod|echo|cd|pwd|tree|stat)\b/;

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

// ── Classification ───────────────────────────────────────────────────

function isSingleFileScope(toolName, toolArgs) {
  if (toolName === "grep") {
    const path = asString(toolArgs?.path);
    // Looks like a single file (has extension, no trailing slash, no glob wildcard)
    return /\.[a-zA-Z]+$/.test(path) && !path.endsWith("/") && !/[*?]/.test(path);
  }
  return false;
}

function isFileDiscoveryOnly(toolName, toolArgs) {
  if (toolName === "glob") return true;

  if (toolName === "bash" || toolName === "shell") {
    const cmd = asString(toolArgs?.command) || asString(toolArgs?.cmd) || asString(toolArgs?.raw);
    // find -name/-type is file discovery
    if (/\bfind\b/.test(cmd) && /\s-(?:name|type|iname)\s/.test(cmd)) return true;
    // fd is file discovery
    if (/\bfd\b/.test(cmd) && !/\brg\b/.test(cmd) && !/\bgrep\b/.test(cmd)) return true;
    // safe read-only commands
    if (SAFE_BASH_CMDS.test(cmd)) return true;
  }

  return false;
}

function isSearchTool(toolName, toolArgs) {
  if (toolName === "grep" || toolName === "search") return true;
  if (toolName === "bash" || toolName === "shell") {
    const cmd = asString(toolArgs?.command) || asString(toolArgs?.cmd) || asString(toolArgs?.raw);
    return /\b(?:rg|grep|git\s+grep)\b/.test(cmd);
  }
  return false;
}

function getSearchPattern(toolName, toolArgs) {
  if (toolName === "grep") return asString(toolArgs?.pattern);
  if (toolName === "bash" || toolName === "shell") {
    const cmd = asString(toolArgs?.command) || asString(toolArgs?.cmd) || asString(toolArgs?.raw);
    // Extract pattern from: rg 'pattern' or rg "pattern" or grep 'pattern'
    const match = cmd.match(/\b(?:rg|grep)\s+(?:(?:-[a-zA-Z]+\s+)*)?['"]([^'"]+)['"]/);
    if (match) return match[1];
    // Bare pattern: rg pattern path
    const bare = cmd.match(/\b(?:rg|grep)\s+(?:(?:-[a-zA-Z]+\s+)*)?(\S+)/);
    if (bare) return bare[1];
  }
  return "";
}

function looksLikeIdentifier(pattern) {
  if (!pattern || pattern.length < 2) return false;
  return IDENTIFIER_PATTERN.test(pattern);
}

function looksLikeKotlinContext(toolName, toolArgs, text) {
  // Check glob/path for .kt files
  const path = asString(toolArgs?.path) || asString(toolArgs?.glob) || "";
  if (KOTLIN_FILE_TRIGGER.test(path)) return true;
  // Check combined text for kotlin keywords
  return KOTLIN_HINT_TRIGGER.test(text);
}

function looksLikeFreeText(text) {
  return FREE_TEXT_TRIGGER.test(text);
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
        const reason = asString(args.reason).toLowerCase();
        const pattern = asString(args.pattern);

        // Block if this is clearly a simple symbol lookup with no valid reason
        const suspiciousReasons = ["", "need to find", "looking for", "searching"];
        if (looksLikeIdentifier(pattern) && suspiciousReasons.some((r) => reason.startsWith(r))) {
          return {
            textResultForLlm: "Rejected: pattern looks like a symbol name. Use LSP workspaceSymbol first. Provide a specific reason why LSP cannot help.",
            resultType: "rejected",
          };
        }

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
        "Check kotlin-lsp server status: whether it's running, which workspace is configured,",
        "and roughly how many source files the workspace contains.",
        "Call before workspaceSymbol when uncertain if indexing has completed.",
        "If no server is running, the next LSP tool call will auto-start it.",
      ].join(" "),
      parameters: { type: "object", properties: {}, required: [] },
      handler: async () => {
        const lines = [];

        // 1. Configured workspace
        let configuredRoot = "(not set)";
        try {
          configuredRoot = (await fs.readFile(WORKSPACE_CONFIG, "utf8")).trim();
        } catch { /* file missing */ }
        lines.push(`Configured workspace: ${configuredRoot}`);

        // 2. Server process
        const { stdout: pids } = await runShell("pgrep -x kotlin-lsp 2>/dev/null || true");
        const pidList = pids.trim();
        lines.push(pidList ? `Server running: yes (PID ${pidList.replace(/\n/g, ", ")})` : "Server running: no");

        // 3. Approximate source file count in configured workspace
        if (configuredRoot !== "(not set)") {
          const { stdout: countOut } = await runShell(
            `fd --type f -e kt -e kts -e java -e swift . '${configuredRoot}' 2>/dev/null | wc -l`
          );
          const count = countOut.trim();
          lines.push(`Source files in workspace: ~${count}`);
        }

        lines.push("");
        lines.push("Note: workspaceSymbol requires full indexing. Open any file first (lsp documentSymbol) to trigger it, then wait for progress to finish.");
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

    onPreToolUse: async (input) => {
      try {
        const toolName = asString(input?.toolName);
        const toolArgs = normalizeToolArgs(input?.toolArgs);
        const text = `${toolName}\n${JSON.stringify(toolArgs)}`;

        // 1. Not a search tool → always allow
        if (!isSearchTool(toolName, toolArgs)) {
          return { permissionDecision: "allow" };
        }

        // 2. File discovery (glob, find -name, fd, ls) → always allow
        if (isFileDiscoveryOnly(toolName, toolArgs)) {
          return { permissionDecision: "allow" };
        }

        // 3. Not kotlin context → allow
        if (!looksLikeKotlinContext(toolName, toolArgs, text)) {
          return { permissionDecision: "allow" };
        }

        // 4. Single-file search → allow (viewing content of a known file)
        if (isSingleFileScope(toolName, toolArgs)) {
          return { permissionDecision: "allow" };
        }

        // 5. Free-text search → allow
        if (looksLikeFreeText(text)) {
          return { permissionDecision: "allow" };
        }

        // 6. Check the search pattern — only block identifier-like patterns
        const pattern = getSearchPattern(toolName, toolArgs);
        if (!looksLikeIdentifier(pattern)) {
          // Complex regex, multi-word, special chars → likely pattern/convention search
          return { permissionDecision: "allow" };
        }

        // 7. Broad scope + identifier pattern + kotlin context → block
        return {
          permissionDecision: "deny",
          permissionDecisionReason: denyMessage(),
        };
      } catch (error) {
        console.error("onPreToolUse failed:", error);
        return { permissionDecision: "allow" };
      }
    },
  },
});
