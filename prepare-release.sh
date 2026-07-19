#!/usr/bin/env bash
set -euo pipefail

# ============================================================
# release.sh — prepare and push a new release to GitHub
#
# Usage: ./release.sh <version>
# Example: ./release.sh 0.5.1
#
# What this script does:
#   1. Validates the version argument
#   2. Updates version in Cargo.toml and Cargo.lock
#   3. Adds a new section to CHANGELOG.md if not present
#   4. Runs ./check.sh (fmt, clippy, tests)
#   5. Dry-runs cargo publish (validation only)
#   6. Commits changes, creates an annotated tag, and pushes
#
# GitHub Actions will then automatically:
#   - Create a GitHub Release with binaries
#   - Publish the crate to crates.io
# ============================================================

VERSION=${1:-}

# ── Validate arguments ───────────────────────────────────────
if [ -z "$VERSION" ]; then
  echo "Error: version argument is required"
  echo "Usage: $0 <version>  (example: $0 0.5.1)"
  exit 1
fi

# Reject versions with a leading 'v' to keep Cargo.toml clean
if [[ "$VERSION" == v* ]]; then
  echo "Error: do not include the 'v' prefix (use '0.5.1', not 'v0.5.1')"
  exit 1
fi

# Basic semver sanity check (X.Y.Z)
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Error: version must follow semver format X.Y.Z"
  exit 1
fi

echo "🚀 Preparing release v$VERSION..."

# ── Check working tree ───────────────────────────────────────
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "Error: working tree has uncommitted changes — commit or stash them first"
  exit 1
fi

# ── Ensure we are on main ────────────────────────────────────
CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$CURRENT_BRANCH" != "main" ]; then
  echo "Error: releases must be made from 'main' (current branch: $CURRENT_BRANCH)"
  exit 1
fi

# ── Ensure the tag does not already exist ───────────────────
if git rev-parse "v$VERSION" >/dev/null 2>&1; then
  echo "Error: tag v$VERSION already exists"
  exit 1
fi

# ── Update Cargo.toml version ────────────────────────────────
echo "📝 Updating Cargo.toml version to $VERSION..."
sed -i "s/^version = \".*\"/version = \"$VERSION\"/" Cargo.toml

# Regenerate Cargo.lock with the new version
cargo update -p retree

# ── Update CHANGELOG.md ──────────────────────────────────────
if ! grep -q "## \[$VERSION\]" CHANGELOG.md; then
  DATE=$(date +%Y-%m-%d)
  sed -i "/## \[Unreleased\]/a\\
\\
## [$VERSION] - $DATE" CHANGELOG.md
  echo "✅ Added [$VERSION] section to CHANGELOG.md"
else
  echo "ℹ️  [$VERSION] section already exists in CHANGELOG.md — skipping"
fi

# ── Run checks ───────────────────────────────────────────────
echo "🔍 Running check.sh..."
./check.sh

# ── Dry-run publish (validation only) ────────────────────────
# Real publish is handled by GitHub Actions (release.yml).
# This step catches packaging errors before the tag is pushed.
echo "📦 Validating crate package (dry-run)..."
cargo publish --dry-run

# ── Commit ───────────────────────────────────────────────────
echo "💾 Committing release changes..."
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore(build): release $VERSION"

# ── Tag ──────────────────────────────────────────────────────
echo "🏷️  Creating annotated tag v$VERSION..."
git tag -a "v$VERSION" -m "Release version $VERSION"

# ── Push ─────────────────────────────────────────────────────
echo "📤 Pushing main and tag to origin..."
git push origin main
git push origin "v$VERSION"

# ── Done ─────────────────────────────────────────────────────
echo ""
echo "✅ Release v$VERSION pushed successfully."
echo ""
echo "GitHub Actions will now:"
echo "  • Build binaries for all platforms"
echo "  • Create a GitHub Release with assets"
echo "  • Publish retree v$VERSION to crates.io"
echo ""
echo "Monitor progress at:"
echo "  https://github.com/M4dPac/retree/actions"
