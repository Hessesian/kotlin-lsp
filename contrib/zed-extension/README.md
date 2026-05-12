# Zed Extension for kotlin-lsp

A lightweight Zed extension that wires `kotlin-lsp` (tree-sitter, no JVM) as the
language server for Kotlin, Java and Swift files.

## Prerequisites

Install the binary:
```sh
cargo install kotlin-lsp
```

## Installation (local dev)

```sh
# From the repo root
cd contrib/zed-extension
zed --install-dev-extension .
```

Or copy the directory to `~/.config/zed/extensions/kotlin-lsp/` and restart Zed.

## Zed settings

Add to `~/.config/zed/settings.json` to suppress the default JVM-based server:

```json
{
  "languages": {
    "Kotlin": {
      "language_servers": ["kotlin-lsp", "!kotlin-language-server"],
      "format_on_save": "off"
    },
    "Java": {
      "language_servers": ["kotlin-lsp"],
      "format_on_save": "off"
    }
  },
  "lsp": {
    "kotlin-lsp": {
      "initialization_options": {
        "indexingOptions": {
          "sourcePaths": []
        }
      }
    }
  }
}
```

## Why this exists

Zed only starts language servers registered by an extension. The community Kotlin
extension always downloads from JetBrains TeamCity and ignores `binary.path`
overrides. This extension registers `kotlin-lsp` as a first-class server name,
resolving the binary from `$PATH` — no symlinks required.
