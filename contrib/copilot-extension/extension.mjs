// kotlin-lsp-extension.mjs
import { approveAll } from "@github/copilot-sdk";
import { joinSession } from "@github/copilot-sdk/extension";

const README_PATH = ".github/extensions/kotlin-lsp/README.md";

const KOTLIN_HINT_TRIGGER =
  /\b(?:kotlin|java|\.kt|\.kts|\.java|ViewModel|Repository|UseCase|Composable|Activity|Fragment|CPageHeader|CPage)\b/i;

const SYMBOLISH_TRIGGER =
  /\b(?:class|interface|object|enum|data\s+class|fun|function|method|property|symbol|definition|references?|implementation|impl|where\s+is|go\s+to|find|usage|usages)\b/i;

const FREE_TEXT_TRIGGER =
  /\b(?:TODO|FIXME|HACK|text|string|comment|comments|message|messages|translation|copy|literal|log|logs)\b/i;

const SHELL_SEARCH_TRIGGER =
  /\b(?:rg|grep|fd|find)\b/i;

function asString(value) {
  return typeof value === "string" ? value : "";
}

function normalizeToolArgs(rawToolArgs) {
  if (rawToolArgs == null) return {};
  if (typeof rawToolArgs === "object") return rawToolArgs;

  if (typeof rawToolArgs === "string") {
    try {
      return JSON.parse(rawToolArgs);
    } catch {
      return { raw: rawToolArgs };
    }
  }

  return { raw: String(rawToolArgs) };
}

function combinedSearchText(input) {
  const prompt = asString(input?.prompt);
  const toolName = asString(input?.toolName);
  const toolArgs = normalizeToolArgs(input?.toolArgs);

  let joinedArgs = "";
  try {
    joinedArgs = JSON.stringify(toolArgs);
  } catch {
    joinedArgs = "";
  }

  return {
    prompt,
    toolName,
    toolArgs,
    text: `${toolName}\n${joinedArgs}\n${prompt}`,
  };
}

function isSearchTool(toolName, toolArgs) {
  if (toolName === "grep" || toolName === "glob" || toolName === "search") {
    return true;
  }

  if (toolName === "bash" || toolName === "powershell" || toolName === "shell") {
    const command =
      asString(toolArgs?.command) ||
      asString(toolArgs?.cmd) ||
      asString(toolArgs?.raw);

    return SHELL_SEARCH_TRIGGER.test(command);
  }

  return false;
}

function looksLikeKotlinContext(text) {
  return KOTLIN_HINT_TRIGGER.test(text);
}

function looksLikeSymbolLookup(text) {
  return SYMBOLISH_TRIGGER.test(text);
}

function looksLikeFreeText(text) {
  return FREE_TEXT_TRIGGER.test(text);
}

function denyMessage() {
  return [
    "Blocked: Kotlin/Java symbol lookup must use Kotlin LSP first.",
    `Read \`${README_PATH}\` first.`,
    "Then call `kotlin_lsp_status` and use Kotlin LSP symbol/navigation tools before grep/glob/bash search.",
    "Use grep/rg only for free-text search, extension functions, generated code, or Java interop cases where LSP cannot help.",
  ].join(" ");
}

await joinSession({
  onPermissionRequest: approveAll,

  tools: [
    // Keep your actual tools here, for example:
    // kotlin_lsp_info,
    // kotlin_lsp_status,
    // kotlin_lsp_set_workspace,
    // kotlin_lsp_workspace_symbol,
    // kotlin_lsp_definition,
    // kotlin_lsp_references,
    // read_kotlin_lsp_guide,
  ],

  hooks: {

    onSessionStart: async () => {
      try {
        return {
          additionalContext: [
            "Kotlin/Java code navigation must use Kotlin LSP first.",
            "Call `kotlin_lsp_status` before symbol lookup.",
            "Use grep/rg only for free-text, generated code, extension functions, or Java interop fallback.",
            "Guide: `.github/extensions/kotlin-lsp/README.md`.",
          ].join(" "),
        };
      } catch (error) {
        console.error("onSessionStart failed:", error);
        return null;
      }
    },


    onUserPromptSubmitted: async () => {
      try {
        return null;
      } catch (error) {
        console.error("onUserPromptSubmitted failed:", error);
        return null;
      }
    },

    onPreToolUse: async (input) => {
      try {
        const { toolName, toolArgs, text } = combinedSearchText(input);

        if (!isSearchTool(toolName, toolArgs)) {
          return { permissionDecision: "allow" };
        }

        const kotlinContext = looksLikeKotlinContext(text);
        if (!kotlinContext) {
          return { permissionDecision: "allow" };
        }

        const freeText = looksLikeFreeText(text);
        const symbolLookup = looksLikeSymbolLookup(text);

        if (freeText && !symbolLookup) {
          return { permissionDecision: "allow" };
        }

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
