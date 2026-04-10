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

VS Code does not support arbitrary LSP binaries natively. Use the
[**Custom Language Server**](https://marketplace.visualstudio.com/items?itemName=cesium.custom-language-server)
extension, then add to `.vscode/settings.json`:

```json
{
  "custom-language-server.servers": [
    {
      "name": "kotlin-lsp",
      "command": "/path/to/kotlin-lsp",
      "filetypes": ["kotlin", "java", "swift"]
    }
  ]
}
```

> **Note:** The [Kotlin language plugin](https://marketplace.visualstudio.com/items?itemName=mathiasfrohlich.Kotlin) must be installed so VS Code recognises `.kt` files as `kotlin`.  
> For a production-grade Kotlin experience in VS Code, consider [Kotlin Language Server](https://github.com/fwcd/kotlin-language-server) alongside this one (they can coexist on different capabilities).

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
