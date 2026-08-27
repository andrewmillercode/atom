#!/usr/bin/env bash
#
# Manual release: tag, cut a DRAFT GitHub release, then build + publish
# locally via scripts/upload-assets.sh.
#
# Merging a PR labeled `release` into main (or a manual workflow_dispatch)
# does the tag + draft-release part automatically via
# .github/workflows/release.yml; use this script only for manual releases.
#
# Usage:
#   scripts/release.sh [vX.Y.Z]
#
# The tag is optional: it defaults to the version in [workspace.package]
# (Cargo.toml) — the same source the release workflow uses. Pass it only
# to override.
#
# Requires: cargo, git, gh (authenticated: gh auth login).
# Commit your work first — the tag points at whatever is committed.
set -euo pipefail

if [ $# -ge 1 ]; then
  TAG="$1"
else
  VER="$(awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f && $1 == "version" {sub(/^version[[:space:]]*=[[:space:]]*"/, ""); sub(/".*$/, ""); print; exit}' Cargo.toml)"
  [ -n "$VER" ] || { echo "error: could not read [workspace.package] version from Cargo.toml; pass a tag explicitly" >&2; exit 1; }
  TAG="v${VER}"
  echo "==> version ${TAG} (from Cargo.toml)"
fi

if ! git diff --quiet; then
  echo "error: working tree has uncommitted changes; commit first" >&2
  exit 1
fi

if git ls-remote --exit-code --tags origin "refs/tags/${TAG}" >/dev/null 2>&1 \
  || gh release view "$TAG" >/dev/null 2>&1; then
  echo "error: ${TAG} already exists (tag or release); to finish it: scripts/upload-assets.sh ${TAG}" >&2
  exit 1
fi

echo "==> tagging ${TAG}"
git tag "$TAG"
git push origin "$TAG"

NOTES_FILE="releases/${TAG}.md"
echo "==> creating draft release ${TAG}"
if [ -f "$NOTES_FILE" ]; then
  gh release create "$TAG" --draft --verify-tag --title "atom ${TAG}" \
    --notes-file "$NOTES_FILE" --generate-notes
else
  echo "==> warning: ${NOTES_FILE} not found — falling back to auto-generated notes" >&2
  gh release create "$TAG" --draft --verify-tag --title "atom ${TAG}" --generate-notes
fi

exec scripts/upload-assets.sh "$TAG"
