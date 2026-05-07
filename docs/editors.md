# Editor setup

Replace `/path/to/kotlin-lsp` with `~/.cargo/bin/kotlin-lsp` (or wherever `cargo install` placed it — run `which kotlin-lsp` to confirm).

## Helix

Add to `~/.config/helix/languages.toml`:

```toml
[[language]]
name = "kotlin"
language-servers = ["kotlin-lsp"]
auto-format = false

[[language]]
name = "java"
language-servers = ["kotlin-lsp"]
auto-format = false

[[language]]
name = "swift"
language-servers = ["kotlin-lsp"]
auto-format = false

[language-server.kotlin-lsp]
command = "/path/to/kotlin-lsp"
```

Then restart Helix (or run `:lsp-restart`).  
Check the server is running: `:lsp-workspace-command` or watch `:log-open`.

## Neovim (nvim-lspconfig)

```lua
local lspconfig = require('lspconfig')
local configs   = require('lspconfig.configs')

if not configs.kotlin_lsp then
  configs.kotlin_lsp = {
    default_config = {
      cmd       = { '/path/to/kotlin-lsp' },
      filetypes = { 'kotlin', 'java', 'swift' },
      root_dir  = lspconfig.util.root_pattern(
        'build.gradle', 'build.gradle.kts', 'pom.xml', 'settings.gradle', 'Package.swift', '.git'
      ),
      settings  = {},
    },
  }
end

lspconfig.kotlin_lsp.setup {}
```

Place this in your `init.lua` (or a dedicated `after/ftplugin/kotlin.lua`).

**Completion** — pair with [nvim-cmp](https://github.com/hrsh7th/nvim-cmp):

```lua
require('cmp').setup {
  sources = {
    { name = 'nvim_lsp' },
    -- other sources …
  },
}
```

## VS Code

A self-contained extension is included in `contrib/vscode/`. It registers the Kotlin language, provides syntax highlighting, and launches kotlin-lsp as the language server — no other Kotlin plugins needed.

**Install:**

```bash
cd contrib/vscode && npm install
```

Then symlink into your VS Code extensions directory (run from the repo root):

```bash
# VS Code
ln -s "$(pwd)/contrib/vscode" ~/.vscode/extensions/kotlin-lsp.kotlin-lsp-client-0.0.1

# VS Code OSS / Code
ln -s "$(pwd)/contrib/vscode" ~/.vscode-oss/extensions/kotlin-lsp.kotlin-lsp-client-0.0.1
```

Restart VS Code. The extension activates automatically for `.kt` and `.java` files.

> **Tip:** Disable other Kotlin extensions (`fwcd.kotlin`, `jetbrains.kotlin`) to avoid conflicts — kotlin-lsp handles language registration, syntax highlighting, and LSP on its own.

**Configuration** (optional) — in `.vscode/settings.json`:

```json
{
  "kotlinLsp.path": "/path/to/kotlin-lsp"
}
```

Default: `kotlin-lsp` (looks on `$PATH`).

## Zed

Add to your Zed settings (`~/.config/zed/settings.json` or `.zed/settings.json` per-project):

```json
{
  "languages": {
    "Kotlin": {
      "language_servers": ["kotlin-lsp"]
    },
    "Java": {
      "language_servers": ["kotlin-lsp"]
    },
    "Swift": {
      "language_servers": ["kotlin-lsp"]
    }
  },
  "lsp": {
    "kotlin-lsp": {
      "binary": {
        "path": "/path/to/kotlin-lsp",
        "arguments": ["--stdio"]
      }
    }
  }
}
```

> **Note:** Zed requires a restart (not just workspace reload) after changing LSP config. If Zed doesn't recognise Kotlin files, you may need the [Kotlin extension](https://zed.dev/extensions?query=kotlin) installed.
