#!/usr/bin/env bash
# sync-skill.sh — install the kotlin-lsp Copilot skill from the repo
#
# Usage: ./contrib/sync-skill.sh
#
# Copies contrib/copilot-extension/extension.mjs to
#   ~/.copilot/extensions/kotlin-lsp/extension.mjs
# Safe to run any time; idempotent.

set -euo pipefail

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
SRC="$REPO_ROOT/contrib/copilot-extension/extension.mjs"
DEST="$HOME/.copilot/extensions/kotlin-lsp/extension.mjs"

mkdir -p "$(dirname "$DEST")"
cp "$SRC" "$DEST"

echo "✓ Synced skill extension to $DEST"
SIZE=$(wc -c < "$DEST")
echo "  $(wc -l < "$DEST") lines, ${SIZE} bytes"
