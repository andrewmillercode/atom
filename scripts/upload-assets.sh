#!/usr/bin/env bash
#
# Build atom/atoms locally and attach them to a release, then publish it.
#
# Releases are cut as DRAFTS (by .github/workflows/release.yml or
# scripts/release.sh) and stay invisible to install.sh and the auto-updater
# until this script attaches the binary and publishes. The publish step
# refuses to go live without an asset, so a release can never exist without
# a binary in it.
#
# Usage:
#   scripts/upload-assets.sh [vX.Y.Z]   # e.g. v0.1.1
#
# The tag is optional: it defaults to the version in [workspace.package]
# (Cargo.toml) — the same source the release workflow uses. Pass it only
# to override.
#
# Requires: cargo, gh (authenticated: gh auth login). Run on macOS —
# assets are macOS-only; there is no Linux build.
set -euo pipefail

if [ $# -ge 1 ]; then
  TAG="$1"
else
  VER="$(awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f && $1 == "version" {sub(/^version[[:space:]]*=[[:space:]]*"/, ""); sub(/".*$/, ""); print; exit}' Cargo.toml)"
  [ -n "$VER" ] || { echo "error: could not read [workspace.package] version from Cargo.toml; pass a tag explicitly" >&2; exit 1; }
  TAG="v${VER}"
  echo "==> version ${TAG} (from Cargo.toml)"
fi

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$ARCH" in
  arm64|aarch64) ARCH="arm64" ;;
  x86_64|amd64)  ARCH="x86_64" ;;
esac
if [ "$OS" != "Darwin" ]; then
  echo "error: assets are macOS-only; run this on a mac (got $OS)" >&2
  exit 1
fi

if ! gh release view "$TAG" >/dev/null 2>&1; then
  echo "error: no GitHub release ${TAG}; cut it first (Actions → release → Run workflow, or scripts/release.sh)" >&2
  exit 1
fi

echo "==> building"
cargo build --release --locked --bin atom --bin atoms

echo "==> smoke test"
./target/release/atom -help >/dev/null

ASSET="atom-${TAG}-Darwin-${ARCH}.tar.gz"
echo "==> packaging ${ASSET}"
tar -czf "$ASSET" -C target/release atom atoms

echo "==> uploading ${ASSET} to ${TAG}"
gh release upload "$TAG" "$ASSET" --clobber

# Publish guard: only go live with a binary attached.
ASSET_COUNT="$(gh release view "$TAG" --json assets -q '.assets | length')"
if [ "${ASSET_COUNT:-0}" -eq 0 ]; then
  echo "error: refusing to publish ${TAG} with no binary attached" >&2
  exit 1
fi

echo "==> publishing ${TAG}"
gh release edit "$TAG" --draft=false
echo "done: ${TAG} published with ${ASSET}"
