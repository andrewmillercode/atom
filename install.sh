#!/bin/sh
# atom installer
#
#   curl -fsSL https://raw.githubusercontent.com/andrewmillercode/atom/main/install.sh | bash
#
# Installs the prebuilt `atom` binary to ~/.local/bin, adds it to PATH if
# needed, and checks the runtime deps (rg, uv, merman-cli). Override with:
#   ATOM_VERSION=v0.1.0     pin a version (default: latest release)
#   ATOM_INSTALL_DIR=/path  install elsewhere (default: ~/.local/bin)
#   ATOM_NO_DEPS=1          skip dependency install/check
set -eu

REPO="andrewmillercode/atom"
BASE="https://github.com/${REPO}/releases/download"

# --- platform -----------------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
  Darwin|Linux) ;;
  *) echo "error: unsupported OS: $OS" >&2; exit 1 ;;
esac
case "$ARCH" in
  arm64|aarch64) ARCH="arm64" ;;
  x86_64|amd64)  ARCH="x86_64" ;;
  *) echo "error: unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

# --- version ------------------------------------------------------------
VERSION="${ATOM_VERSION:-${1:-}}"
if [ -z "$VERSION" ]; then
  echo "==> resolving latest release..."
  VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
fi
if [ -z "$VERSION" ]; then
  echo "error: could not determine the latest version (set ATOM_VERSION to pin one)" >&2
  exit 1
fi

# --- download + extract ---------------------------------------------------
ASSET="atom-${VERSION}-${OS}-${ARCH}.tar.gz"
URL="${BASE}/${VERSION}/${ASSET}"
echo "==> downloading ${URL}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
curl -fsSL -o "$TMP/${ASSET}" "$URL"
tar -xzf "$TMP/${ASSET}" -C "$TMP"

# --- install ---------------------------------------------------------------
INSTALL_DIR="${ATOM_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$INSTALL_DIR"
install -m 755 "$TMP/atom" "$INSTALL_DIR/atom"
echo "==> installed $INSTALL_DIR/atom"

# --- PATH -------------------------------------------------------------------
if ! printf '%s' ":$PATH:" | grep -qF ":$INSTALL_DIR:"; then
  RC_FILE=""
  if [ -n "${ZSH_VERSION:-}" ] || [ -f "$HOME/.zshrc" ]; then
    RC_FILE="$HOME/.zshrc"
  elif [ -f "$HOME/.bashrc" ]; then
    RC_FILE="$HOME/.bashrc"
  else
    RC_FILE="$HOME/.profile"
  fi
  if ! grep -qF "$INSTALL_DIR" "$RC_FILE" 2>/dev/null; then
    printf '\nexport PATH="%s:$PATH"\n' "$INSTALL_DIR" >> "$RC_FILE"
  fi
  echo "==> added $INSTALL_DIR to PATH in $RC_FILE (open a new shell, or run: export PATH=\"$INSTALL_DIR:\$PATH\")"
fi

# --- runtime deps (rg, uv, merman-cli) ---------------------------------------
if [ -z "${ATOM_NO_DEPS:-}" ]; then
  MISSING=""
  command -v rg >/dev/null 2>&1 || MISSING="${MISSING} rg"
  command -v uv >/dev/null 2>&1 || MISSING="${MISSING} uv"
  command -v merman-cli >/dev/null 2>&1 || MISSING="${MISSING} merman-cli"
  if [ -n "$MISSING" ]; then
    echo "==> atom needs these at runtime, missing:$MISSING"
    if command -v brew >/dev/null 2>&1; then
      echo "==> installing with Homebrew: brew install ripgrep uv merman-cli"
      brew install ripgrep uv merman-cli
    else
      echo "    install them, e.g.:"
      echo "      apt install ripgrep   # or: dnf install ripgrep"
      echo "      curl -LsSf https://astral.sh/uv/install.sh | sh"
      echo "      cargo install merman-cli   # or: brew install merman-cli"
      echo "    (rerun with ATOM_NO_DEPS=1 to skip this check)"
    fi
  fi
fi

# --- smoke test ---------------------------------------------------------------
if "$INSTALL_DIR/atom" -help >/dev/null 2>&1; then
  echo "==> done — run 'atom' to start"
else
  echo "warning: installed binary failed its smoke test" >&2
  exit 1
fi
