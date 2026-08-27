#!/usr/bin/env bash
#
# Build the atom and atoms binaries and attach them to a GitHub release.
#
# Merging a PR labeled `release` into main does this automatically via
# .github/workflows/release.yml; use this script only for manual releases.
#
# Usage:
#   scripts/release.sh <vX.Y.Z>
#
#   <vX.Y.Z>  the version tag, e.g. v0.1.0
#
# Steps:
#   1. cargo build --release --bin atom --bin atoms
#   2. package both binaries as atom-<version>-<os>-<arch>.tar.gz
#   3. tag + push
#   4. create the GitHub release and attach the archive
#
# Requires: cargo, git, gh (authenticated: gh auth login).
# Commit your work first — the tag points at whatever is committed.
set -euo pipefail

VERSION="${1:?usage: scripts/release.sh <vX.Y.Z>  (e.g. v0.1.0)}"

if ! git diff --quiet; then
  echo "error: working tree has uncommitted changes; commit first" >&2
  exit 1
fi

echo "==> building"
cargo build --release --bin atom --bin atoms

# Normalize arch so names match what install.sh requests (aarch64 -> arm64).
ARCH="$(uname -m)"
case "$ARCH" in
  arm64|aarch64) ARCH="arm64" ;;
  x86_64|amd64)  ARCH="x86_64" ;;
esac
ASSET="atom-${VERSION}-$(uname -s)-${ARCH}.tar.gz"
echo "==> packaging ${ASSET}"
tar -czf "$ASSET" -C target/release atom atoms

echo "==> tagging ${VERSION}"
git tag "$VERSION"
git push origin "$VERSION"

echo "==> creating release + attaching binary"
gh release create "$VERSION" "$ASSET" \
  --title "atom $VERSION" \
  --notes "Prebuilt binary for $(uname -s) $(uname -m)."

echo "done: https://github.com/andrewmillercode/atom/releases/tag/${VERSION}"
