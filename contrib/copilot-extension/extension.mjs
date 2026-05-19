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
const LSP_TOOLS = new Set(["lsp", "kotlin_lsp_complete", "kotlin_find_dead_code", "kotlin_find_implementors", "kotlin_extract_interface", "kotlin_rename"]);

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

/** Run a command with an explicit argument array — no shell interpolation. */
function runCommand(cmd, args, timeout = 8000) {
  return new Promise((resolve) => {
    execFile(cmd, args, { maxBuffer: 1024 * 256, timeout }, (err, stdout, stderr) => {
      resolve({ code: err?.code ?? 0, stdout: stdout || "", stderr: stderr || "" });
    });
  });
}

const WORKSPACE_CONFIG = path.join(os.homedir(), ".config", "kotlin-lsp", "workspace");
const STATUS_PATH = path.join(os.homedir(), ".cache", "kotlin-lsp", "status.json");
const KOTLIN_CLI_CONFIG = path.join(os.homedir(), ".config", "kotlin-lsp", "cli-script");

/**
 * Resolve the path to kotlin-cli.py.
 * Priority: KOTLIN_CLI_PATH env var > ~/.config/kotlin-lsp/cli-script file
 * Returns the path string, or throws an informative error string if not configured.
 */
async function resolveCliScript() {
  if (process.env.KOTLIN_CLI_PATH) return process.env.KOTLIN_CLI_PATH;
  try {
    const p = (await fs.readFile(KOTLIN_CLI_CONFIG, "utf8")).trim();
    if (p) return p;
  } catch { /* not configured */ }
  throw [
    "kotlin-cli.py path not configured.",
    `Set it with: echo '/path/to/kotlin-lsp/contrib/kotlin-cli.py' > ${KOTLIN_CLI_CONFIG}`,
    "Or set KOTLIN_CLI_PATH environment variable.",
  ].join("\n");
}

/** Run kotlin-cli.py with the given subcommand args. workspace is passed as --workspace. */
async function runKotlinCli(workspace, cliArgs, timeout = 120000) {
  let script;
  try { script = await resolveCliScript(); }
  catch (msg) { return { code: 1, stdout: "", stderr: msg }; }
  const args = ["--workspace", workspace, ...cliArgs];
  return runCommand("python3", [script, ...args], timeout);
}

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
        "Builds/loads the index automatically on first call; for best performance pre-warm with `kotlin-lsp index`.",
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
        if (dot && eol) return "Error: --dot and --eol are mutually exclusive";
        let col = null;
        if (!dot && !eol) {
          col = Number(args.col);
          if (!Number.isInteger(col) || col < 1) return "Error: col must be a positive integer (or use dot/eol)";
        }

        const cmdArgs = ["complete", file, String(line)];
        if (dot) cmdArgs.push("--dot");
        else if (eol) cmdArgs.push("--eol");
        else cmdArgs.push(String(col));
        cmdArgs.push("--json");
        if (noStdlib) cmdArgs.push("--no-stdlib");
        if (args.root) { cmdArgs.push("--root"); cmdArgs.push(path.resolve(args.root)); }
        const { code, stdout, stderr } = await runCommand("kotlin-lsp", cmdArgs, 30000);

        if (code !== 0 && !stdout.trim()) {
          return `No completions at ${file}:${line}${stderr ? `\n${stderr}` : ""}`;
        }
        return stdout.trim() || `No completions at ${file}:${line}`;
      },
    },
    {
      name: "kotlin_find_dead_code",
      description: [
        "Find unreferenced (potentially dead) Kotlin/Java symbols in the workspace.",
        "Walks every source file with documentSymbol, then calls findReferences for each",
        "class/fun/property/interface — skipping entry points (Activity, Fragment, ViewModel,",
        "Composable, @Provides/@Binds etc.) that are invoked by framework, not source code.",
        "Use this before a cleanup sprint or when doing a dead-code audit.",
        "Returns: list of symbols with zero references (name, kind, file, line).",
        "Note: needs a fully indexed workspace; call kotlin_lsp_status first if unsure.",
      ].join(" "),
      parameters: {
        type: "object",
        properties: {
          workspace: {
            type: "string",
            description: "Absolute path to the workspace root. Defaults to cwd.",
          },
          kind: {
            type: "string",
            enum: ["class", "fun", "property", "interface", "all"],
            description: "Symbol kind to check. Defaults to 'all'.",
          },
          limit: {
            type: "integer",
            description: "Max results to return (default: unlimited).",
          },
          exclude_tests: {
            type: "boolean",
            description: "Skip symbols in files under test/ or *Test.kt / *Spec.kt paths.",
          },
        },
        required: [],
      },
      handler: async (args) => {
        const workspace = path.resolve(args.workspace || process.cwd());
        const cliArgs = ["find-dead-code", "--json"];
        if (args.kind && args.kind !== "all") { cliArgs.push("--kind"); cliArgs.push(args.kind); }
        if (args.limit) { cliArgs.push("--limit"); cliArgs.push(String(args.limit)); }
        if (args.exclude_tests) cliArgs.push("--exclude-tests");
        const { code, stdout, stderr } = await runKotlinCli(workspace, cliArgs, 300000);
        if (code !== 0 && !stdout.trim()) return `Error running find-dead-code:\n${stderr}`;
        return stdout.trim() || "No unreferenced symbols found.";
      },
    },
    {
      name: "kotlin_find_implementors",
      description: [
        "Find all classes/objects that implement a given interface or extend a class.",
        "Wraps kotlin-cli.py find-implementors — uses lsp goToImplementation under the hood.",
        "Returns: list of implementing symbols with file and line.",
        "Use this instead of grep for reliable, index-based results.",
      ].join(" "),
      parameters: {
        type: "object",
        properties: {
          name: {
            type: "string",
            description: "Interface or class name to find implementors for (e.g. 'NewsRepository')",
          },
          workspace: {
            type: "string",
            description: "Absolute path to the workspace root. Defaults to cwd.",
          },
        },
        required: ["name"],
      },
      handler: async (args) => {
        const workspace = path.resolve(args.workspace || process.cwd());
        const { code, stdout, stderr } = await runKotlinCli(workspace, ["find-implementors", "--json", args.name]);
        if (code !== 0 && !stdout.trim()) return `Error: ${stderr}`;
        return stdout.trim() || `No implementors found for '${args.name}'.`;
      },
    },
    {
      name: "kotlin_extract_interface",
      description: [
        "Generate an interface stub from a class's public non-private members.",
        "Reads the class's documentSymbol, filters out private/internal/local members,",
        "and emits a ready-to-paste Kotlin interface with the public API surface.",
        "Use this when extracting an interface for dependency inversion without reading the whole file.",
        "Returns: Kotlin source for the interface (or JSON with --json).",
      ].join(" "),
      parameters: {
        type: "object",
        properties: {
          class_name: {
            type: "string",
            description: "Name of the class to extract an interface from (e.g. 'OfflineFirstNewsRepository')",
          },
          workspace: {
            type: "string",
            description: "Absolute path to the workspace root. Defaults to cwd.",
          },
        },
        required: ["class_name"],
      },
      handler: async (args) => {
        const workspace = path.resolve(args.workspace || process.cwd());
        const { code, stdout, stderr } = await runKotlinCli(workspace, ["extract-interface", "--json", args.class_name]);
        if (code !== 0 && !stdout.trim()) return `Error: ${stderr}`;
        return stdout.trim() || `Could not extract interface from '${args.class_name}'.`;
      },
    },
    {
      name: "kotlin_rename",
      description: [
        "Rename a symbol across the entire workspace using LSP workspace/rename.",
        "Resolves the symbol by name, applies TextEdits to all files, and reports a summary.",
        "Use --dry-run to preview changes without writing files.",
        "Fails with a candidate list if the name is ambiguous (multiple matches).",
        "Known limitation: TextEdit positions are UTF-16; files with non-BMP characters may misalign.",
      ].join(" "),
      parameters: {
        type: "object",
        properties: {
          old_name: {
            type: "string",
            description: "Current symbol name to rename (e.g. 'AnalyticsHelper')",
          },
          new_name: {
            type: "string",
            description: "New name for the symbol",
          },
          dry_run: {
            type: "boolean",
            description: "Preview changes without writing files.",
          },
          workspace: {
            type: "string",
            description: "Absolute path to the workspace root. Defaults to cwd.",
          },
        },
        required: ["old_name", "new_name"],
      },
      handler: async (args) => {
        const workspace = path.resolve(args.workspace || process.cwd());
        const cliArgs = ["rename", "--json", args.old_name, args.new_name];
        if (args.dry_run) cliArgs.push("--dry-run");
        const { code, stdout, stderr } = await runKotlinCli(workspace, cliArgs, 60000);
        if (code !== 0 && !stdout.trim()) return `Error: ${stderr}`;
        return stdout.trim() || "Rename returned no output.";
      },
    },
    {
      name: "kotlin_list_templates",
      description: [
        "List IDEA file templates available in the current project's .idea/fileTemplates/ directory.",
        "Templates are families of files (e.g. 'New Contract' generates Contract, Mapper, Screen, ViewModel, Interactor).",
        "Use this to discover available scaffolding before calling kotlin_scaffold_feature.",
        "Does NOT require the LSP server — reads the .idea directory directly.",
      ].join(" "),
      parameters: {
        type: "object",
        properties: {
          workspace: {
            type: "string",
            description: "Absolute path to the project root containing .idea/. Defaults to cwd.",
          },
        },
        required: [],
      },
      handler: async (args) => {
        const workspace = path.resolve(args.workspace || process.cwd());
        const { code, stdout, stderr } = await runKotlinCli(workspace, ["list-templates"]);
        if (code !== 0 && !stdout.trim()) return `Error: ${stderr}`;
        return stdout.trim() || "No IDEA file templates found in this project.";
      },
    },
    {
      name: "kotlin_scaffold_feature",
      description: [
        "Generate a complete MVI feature scaffold from an IDEA file template family.",
        "Reads templates from .idea/fileTemplates/, substitutes ${VAR} placeholders, and",
        "writes the generated files under src-root/package-path/.",
        "Use --json to get [{path, content}] output for AI inspection before writing.",
        "Use --dry-run to preview file paths without writing.",
        "Call kotlin_list_templates first to discover available template families and variable requirements.",
        "Example: scaffold-feature GoldConversion --package com.example.gold --src-root src/main/kotlin",
      ].join(" "),
      parameters: {
        type: "object",
        properties: {
          feature_name: {
            type: "string",
            description: "PascalCase feature name (e.g. 'GoldConversion'). Lowercase/snake_case is auto-derived.",
          },
          package: {
            type: "string",
            description: "Kotlin package for the generated files (e.g. 'com.example.gold.conversion')",
          },
          template: {
            type: "string",
            description: "Template family name (default: 'New Contract'). Use kotlin_list_templates to discover.",
          },
          src_root: {
            type: "string",
            description: "Absolute or relative path to the source root where files will be written.",
          },
          vars: {
            type: "array",
            items: { type: "string" },
            description: "Extra KEY=VALUE substitutions for template variables (e.g. ['MODULE_NAME=gold'])",
          },
          dry_run: {
            type: "boolean",
            description: "Print file paths without writing them.",
          },
          json: {
            type: "boolean",
            description: "Output [{path, content}] JSON for AI review before writing.",
          },
          workspace: {
            type: "string",
            description: "Absolute path to the project root. Defaults to cwd.",
          },
        },
        required: ["feature_name", "package"],
      },
      handler: async (args) => {
        const workspace = path.resolve(args.workspace || process.cwd());
        const cliArgs = ["scaffold-feature", args.feature_name, "--package", args.package];
        if (args.template) { cliArgs.push("--template"); cliArgs.push(args.template); }
        if (args.src_root) { cliArgs.push("--src-root"); cliArgs.push(path.resolve(args.src_root)); }
        if (args.vars?.length) {
          for (const v of args.vars) { cliArgs.push("--var"); cliArgs.push(v); }
        }
        if (args.dry_run) cliArgs.push("--dry-run");
        if (args.json) cliArgs.push("--json");
        const { code, stdout, stderr } = await runKotlinCli(workspace, cliArgs, 30000);
        if (code !== 0 && !stdout.trim()) return `Error: ${stderr}`;
        return stdout.trim() || "No files generated.";
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
          "For refactoring tasks, prefer kotlin-cli tools over raw LSP: `kotlin_find_dead_code` (dead-code audit),",
          "`kotlin_find_implementors` (find all implementors), `kotlin_extract_interface` (generate interface stub),",
          "`kotlin_rename` (safe workspace-wide rename). Set up path once: echo '/path/to/kotlin-cli.py' > ~/.config/kotlin-lsp/cli-script.",
          "For feature scaffolding from IDEA templates: call `kotlin_list_templates` then `kotlin_scaffold_feature` with --json to preview before writing.",
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
