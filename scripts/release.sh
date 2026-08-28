#!/usr/bin/env bash
#
# Interactive release driver (run via `make release` on the dev machine).
#
#   1. confirm the version; a bump is committed + pushed before anything
#      is tagged, so the tagged commit always contains its own version
#   2. build the release binaries and smoke-test them
#   3. pick release notes from releases/ (offer to create the file)
#   4. pick the commit to tag (must contain the confirmed version)
#   5. tag, push, publish the GitHub release with the binary attached
#
# Releases are macOS-only; assets are named atom-v<ver>-Darwin-<arch>.tar.gz
# — the exact names install.sh and the auto-updater look for.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$ARCH" in
  arm64|aarch64) ARCH="arm64" ;;
  x86_64|amd64)  ARCH="x86_64" ;;
esac
[ "$OS" = "Darwin" ] || { echo "error: releases are macOS-only (got $OS)" >&2; exit 1; }

# confirm <prompt> [y|n] — loop until y/n; empty takes the default.
confirm() {
  local DEF="${2:-y}" A
  while true; do
    if [ "$DEF" = "y" ]; then printf "%s [Y/n]: " "$1"; else printf "%s [y/N]: " "$1"; fi
    read -r A
    A="${A:-$DEF}"
    case "$A" in
      y|Y|yes|Yes|YES) return 0 ;;
      n|N|no|No|NO)    return 1 ;;
      *) echo "    please answer y or n" ;;
    esac
  done
}

ver_from() {
  awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f && $1 == "version" {sub(/^version[[:space:]]*=[[:space:]]*"/, ""); sub(/".*$/, ""); print; exit}' "$1"
}

# --- 1. version -----------------------------------------------------------
VERSION="$(ver_from Cargo.toml)"
[ -n "$VERSION" ] || { echo "error: could not read [workspace.package] version from Cargo.toml" >&2; exit 1; }

if confirm "release v${VERSION}?"; then
  :
else
  printf "new version (e.g. 0.1.2, or empty to abort): "
  read -r ANSWER
  WANT="${ANSWER#v}"
  [ -n "$WANT" ] || { echo "aborted" >&2; exit 1; }

  N="$(grep -cE '^version = "' Cargo.toml || true)"
  [ "$N" = "1" ] || { echo "error: expected exactly one version line in root Cargo.toml, found ${N:-0} — bump manually" >&2; exit 1; }
  sed -i.bak -E "s/^version = \".*\"/version = \"${WANT}\"/" Cargo.toml
  rm -f Cargo.toml.bak
  VERSION="$(ver_from Cargo.toml)"
  [ "$VERSION" = "$WANT" ] || { echo "error: version edit did not stick" >&2; exit 1; }
fi
TAG="v${VERSION}"

# --- 2. build ---------------------------------------------------------------
echo "==> building release binaries"
cargo build --release --bin atom --bin atoms
./target/release/atom -help >/dev/null

# --- commit the bump so the tagged commit contains its own version ----------
if ! git diff --quiet -- Cargo.toml Cargo.lock; then
  echo "==> committing version bump to ${VERSION}"
  git add Cargo.toml Cargo.lock
  git commit -m "bump version to ${VERSION}"
  git push origin HEAD
fi
git diff --quiet || { echo "error: uncommitted changes — commit or stash first" >&2; exit 1; }

# --- 3. notes ----------------------------------------------------------------
NOTES_FILE="releases/${TAG}.md"
if [ -f "$NOTES_FILE" ]; then
  confirm "use ${NOTES_FILE} for notes?" || {
    i=1
    for f in releases/*.md; do
      [ -e "$f" ] || continue
      printf "  %d) %s\n" "$i" "$f"
      i=$((i+1))
    done
    printf "notes file (number or path): "
    read -r PICK
    case "$PICK" in
      ''|*[!0-9]*) NOTES_FILE="$PICK" ;;
      *) NOTES_FILE="$(ls releases/*.md | sed -n "${PICK}p")" ;;
    esac
    [ -n "$NOTES_FILE" ] && [ -f "$NOTES_FILE" ] || { echo "error: notes file not found: ${NOTES_FILE:-<none>}" >&2; exit 1; }
  }
else
  confirm "create ${NOTES_FILE} and edit it now?" || { echo "error: add ${NOTES_FILE} first" >&2; exit 1; }
  printf "## What's New\n\n- \n" > "$NOTES_FILE"
  "${VISUAL:-${EDITOR:-vi}}" "$NOTES_FILE"
  [ -s "$NOTES_FILE" ] || { echo "error: ${NOTES_FILE} is empty" >&2; exit 1; }
fi

# --- 4. commit to tag --------------------------------------------------------
HEAD_SHA="$(git rev-parse --short HEAD)"
echo "==> recent commits:"
git log --oneline -n 10 --decorate=short | sed 's/^/    /'
REF="HEAD"
confirm "tag HEAD (${HEAD_SHA})?" || {
  printf "which commit (ref/sha) [HEAD]: "
  read -r REF
  REF="${REF:-HEAD}"
}
git rev-parse -q --verify "${REF}^{commit}" >/dev/null || { echo "error: unknown commit: ${REF}" >&2; exit 1; }
REF_SHA="$(git rev-parse --short "${REF}^{commit}")"

if ! git show "${REF_SHA}:Cargo.toml" | grep -q "^version = \"${VERSION}\""; then
  echo "error: ${REF_SHA} does not carry version ${VERSION} in Cargo.toml — commit/push the bump first" >&2
  exit 1
fi

if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null; then
  echo "error: tag ${TAG} already exists locally (git tag -d ${TAG} to redo)" >&2
  exit 1
fi
if git ls-remote --exit-code --tags origin "refs/tags/${TAG}" >/dev/null 2>&1; then
  echo "error: tag ${TAG} already exists on origin" >&2
  exit 1
fi

# --- 5. package, tag, release -------------------------------------------------
ASSET="atom-${TAG}-Darwin-${ARCH}.tar.gz"
echo "==> packaging ${ASSET}"
tar -czf "$ASSET" -C target/release atom atoms

echo "==> tagging ${TAG} at ${REF_SHA}"
git tag "$TAG" "$REF_SHA"
git push origin "$TAG"

echo "==> creating GitHub release ${TAG}"
gh release create "$TAG" "$ASSET" \
  --title "atom ${TAG}" \
  --notes-file "$NOTES_FILE" \
  --verify-tag

REPO="$(git remote get-url origin | sed -E 's#.*github\.com[:/]##; s#\.git$##')"
echo "done: https://github.com/${REPO}/releases/tag/${TAG}"
