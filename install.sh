#!/bin/sh
# appflowy one-line installer (Linux / macOS):
#   curl -fsSL https://raw.githubusercontent.com/weironz/appflowy_mcp/main/install.sh | sh
#
# Optional env:
#   APPFLOWY_VERSION=v0.9.0   pin a release (default: latest)
#   APPFLOWY_BIN_DIR=~/.local/bin   install directory (default: ~/.local/bin)
#
# The script downloads the prebuilt binary from GitHub Releases, verifies it
# actually runs `--version` before installing (a truncated download or wrong
# architecture fails here), and drops two hard links into the bin dir:
#   appflowy       — the CLI (`appflowy serve` runs the MCP server)
#   appflowy-mcp   — same bytes; starts the MCP server directly by name
set -eu

REPO="weironz/appflowy_mcp"
BIN_DIR="${APPFLOWY_BIN_DIR:-$HOME/.local/bin}"

fail() { echo "install: $*" >&2; exit 1; }

uname_s=$(uname -s) || fail "uname -s failed"
uname_m=$(uname -m) || fail "uname -m failed"
case "$uname_s" in
  Linux)
    [ "$uname_m" = "x86_64" ] || fail "prebuilt Linux binaries are x86_64 only; build from source instead (docs/installation.md)"
    ASSET="appflowy-linux-x64" ;;
  Darwin)
    case "$uname_m" in
      arm64)  ASSET="appflowy-macos-aarch64" ;;
      x86_64) ASSET="appflowy-macos-x64" ;;
      *)      fail "unsupported macOS arch: $uname_m" ;;
    esac ;;
  *) fail "unsupported OS: $uname_s (use install.ps1 on Windows)" ;;
esac

if [ -n "${APPFLOWY_VERSION:-}" ]; then
  TAG="$APPFLOWY_VERSION"
  case "$TAG" in v*) ;; *) TAG="v$TAG" ;; esac
else
  TAG=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)
  [ -n "$TAG" ] || fail "could not resolve the latest release tag from GitHub"
fi
echo "install: $ASSET @ $TAG"

URL="https://github.com/$REPO/releases/download/$TAG/$ASSET"
TMP=$(mktemp "${TMPDIR:-/tmp}/appflowy.XXXXXX") || fail "mktemp failed"
trap 'rm -f "$TMP"' EXIT

curl -fsSL "$URL" -o "$TMP" \
  || fail "download failed: $URL
install: the asset may not exist for this platform yet (Windows x64 shipped with
install: v0.8.0; Linux/macOS assets are produced by CI from the next release on)
install: — or GitHub is unreachable. Fallback: manual download / cargo build."

chmod +x "$TMP"
GOT=$("$TMP" --version 2>/dev/null) || fail "downloaded file failed to run --version (truncated or wrong-arch binary)"
echo "install: verified ($GOT)"

mkdir -p "$BIN_DIR"
mv "$TMP" "$BIN_DIR/appflowy"
ln -f "$BIN_DIR/appflowy" "$BIN_DIR/appflowy-mcp"

case ":$PATH:" in *":$BIN_DIR:"*) ;; *)
  echo "install: NOTE - $BIN_DIR is not on your PATH; add it, e.g.:"
  echo "  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

echo "install: done -> $BIN_DIR/appflowy (+ appflowy-mcp)"
echo "next: appflowy login    # or set APPFLOWY_EMAIL / APPFLOWY_PASSWORD"
echo "next: connect an MCP client -> https://github.com/$REPO/blob/main/docs/mcp-connect.md"
