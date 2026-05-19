#!/usr/bin/env bash
# Requires: bash 4+, git, cargo, sed, gh (GitHub CLI)
# release.sh — bump version, generate CHANGELOG entry, test, tag, push
#
# Usage: ./contrib/release.sh [patch|minor|major] [--dry-run]
# Default: patch
#
# Steps:
#  1. Validate clean working tree on main
#  2. Bump version in Cargo.toml
#  3. cargo test + cargo clippy
#  4. Prepend CHANGELOG.md entry from commits since last tag
#  5. Commit "chore: release vX.Y.Z", tag, push

set -euo pipefail

BUMP=${1:-patch}
DRY_RUN=false
for arg in "$@"; do [[ "$arg" == "--dry-run" ]] && DRY_RUN=true; done

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

# ── 1. Guard ─────────────────────────────────────────────────────────────────

if [[ "$(git branch --show-current)" != "main" ]]; then
  echo "error: must be on main branch" >&2
  exit 1
fi

if [[ -n "$(git status --porcelain)" ]] && [[ "$DRY_RUN" == false ]]; then
  echo "error: working tree is not clean — commit or stash changes first" >&2
  exit 1
fi

# ── 2. Bump version ──────────────────────────────────────────────────────────

CURRENT=$(grep -m1 '^version' Cargo.toml | grep -oP '[0-9]+\.[0-9]+\.[0-9]+')
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"

case "$BUMP" in
  patch) PATCH=$((PATCH + 1)) ;;
  minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
  major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
  *) echo "error: bump must be patch|minor|major" >&2; exit 1 ;;
esac

NEW="${MAJOR}.${MINOR}.${PATCH}"

# Conventional-commit section labels
# bash 4+ required for associative arrays (macOS ships bash 3.2 — use homebrew bash or run on Linux)
if (( BASH_VERSINFO[0] < 4 )); then
  echo "error: bash 4+ required (found $BASH_VERSION)" >&2; exit 1
fi
declare -A SECTIONS=( [feat]="Features" [fix]="Bug fixes" [perf]="Performance" [refactor]="Refactoring" [docs]="Docs" )
echo "Bumping ${CURRENT} → ${NEW} (${BUMP})"

if [[ "$DRY_RUN" == true ]]; then
  echo "[dry-run] would bump Cargo.toml, run tests, update CHANGELOG, tag v${NEW}"
  LAST_TAG=$(git describe --tags --abbrev=0 2>/dev/null || echo "")
  RANGE="${LAST_TAG:+${LAST_TAG}..HEAD}"
  echo ""
  echo "─── CHANGELOG entry preview ────────────────────────────────────────"
  echo "## ${NEW}"
  for prefix in feat fix perf refactor docs; do
    LABEL="${SECTIONS[$prefix]}"
    LINES=$(git log $RANGE --oneline --no-decorate --grep="^${prefix}" 2>/dev/null \
      | sed 's/^[a-f0-9]* //' \
      | sed 's/^'"${prefix}"'[^:]*: //' \
      | sed 's/^/- /' || true)
    if [[ -n "$LINES" ]]; then
      echo ""
      echo "### ${LABEL}"
      echo ""
      echo "$LINES"
    fi
  done
  exit 0
fi

# Update Cargo.toml (first occurrence of version = "...")
# Use perl for portability — GNU sed's 0,/pat/ address range is not supported by BSD sed (macOS)
perl -i -0pe "s/version = \"$CURRENT\"/version = \"$NEW\"/" Cargo.toml
# Regenerate Cargo.lock
cargo generate-lockfile 2>/dev/null || cargo check -q

# ── 3. Test ──────────────────────────────────────────────────────────────────

echo "Running tests…"
cargo test -q
echo "Running clippy…"
cargo clippy -- -D warnings -W clippy::cognitive_complexity -W clippy::too_many_lines

# ── 4. CHANGELOG ─────────────────────────────────────────────────────────────

LAST_TAG=$(git describe --tags --abbrev=0 2>/dev/null || echo "")
RANGE="${LAST_TAG:+${LAST_TAG}..HEAD}"

ENTRY="## ${NEW}\n"

for prefix in feat fix perf refactor docs; do
  LABEL="${SECTIONS[$prefix]}"
  LINES=$(git log $RANGE --oneline --no-decorate --grep="^${prefix}" 2>/dev/null \
    | sed 's/^[a-f0-9]* //' \
    | sed 's/^'"${prefix}"'[^:]*: //' \
    | sed 's/^/- /' || true)
  [[ -n "$LINES" ]] && ENTRY+="\n### ${LABEL}\n\n${LINES}\n"
done

# Prepend to CHANGELOG.md
TMPFILE=$(mktemp)
printf "%b\n" "$ENTRY" > "$TMPFILE"
echo "" >> "$TMPFILE"
cat CHANGELOG.md >> "$TMPFILE"
mv "$TMPFILE" CHANGELOG.md

# ── 5. Commit, tag, push ──────────────────────────────────────────────────────

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore: release v${NEW}

Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
git tag "v${NEW}"
git push origin main
git push origin "v${NEW}"

echo ""
echo "✓ Released v${NEW}"
echo "  cargo publish   # if you want to publish to crates.io"
