#!/usr/bin/env bash
# pr.sh — open a GitHub PR with smart defaults
#
# Usage: ./contrib/pr.sh [--base BRANCH] [--title "title"] [--draft]
#
# Auto-detects base branch from naming convention:
#   fix/*          → main
#   feature/*      → main
#   refactor/*     → main
#   perf/*         → main
#   feat/kotlin-*  → main
#   <anything>     → main (fallback)
# Override with --base.
#
# Title is auto-derived from branch name if not provided.

set -euo pipefail

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

BRANCH=$(git branch --show-current)
if [[ -z "$BRANCH" ]]; then
  echo "error: not on a branch (detached HEAD?)" >&2
  exit 1
fi

# ── Defaults ─────────────────────────────────────────────────────────────────

BASE="main"
DRAFT_FLAG=""
TITLE=""

# Branch-name → base detection (extend as needed)
# Default base-branch detection: override with --base if your project uses other long-lived branches
case "$BRANCH" in
  feature/mvi-*|feature/mvi*) BASE="feature/mvi-architecture" ;;
  *) BASE="main" ;;
esac

# Auto-title: strip prefix, replace hyphens, title-case
RAW_TITLE=$(echo "$BRANCH" | sed 's|.*/||' | tr '-' ' ')
# Capitalise first letter
TITLE="$(tr '[:lower:]' '[:upper:]' <<< "${RAW_TITLE:0:1}")${RAW_TITLE:1}"

# ── Parse args ────────────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base)  BASE="$2"; shift 2 ;;
    --title) TITLE="$2"; shift 2 ;;
    --draft) DRAFT_FLAG="--draft"; shift ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done

# ── Confirm ──────────────────────────────────────────────────────────────────

echo "Branch : $BRANCH"
echo "Base   : $BASE"
echo "Title  : $TITLE"
[[ -n "$DRAFT_FLAG" ]] && echo "Draft  : yes"
echo ""
read -rp "Open PR? [y/N] " confirm
[[ "$confirm" =~ ^[Yy]$ ]] || { echo "Aborted."; exit 0; }

# ── Push branch if not already on remote ─────────────────────────────────────

if ! git ls-remote --exit-code --heads origin "$BRANCH" >/dev/null 2>&1; then
  echo "Pushing branch to origin…"
  git push -u origin "$BRANCH"
fi

# ── Open PR ───────────────────────────────────────────────────────────────────

gh pr create \
  --base "$BASE" \
  --head "$BRANCH" \
  --title "$TITLE" \
  $DRAFT_FLAG \
  --body ""

echo ""
echo "✓ PR opened: $BRANCH → $BASE"
