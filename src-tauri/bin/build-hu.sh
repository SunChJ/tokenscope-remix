#!/bin/sh
# Bundle the happyusage `hu` binary into the app package.
# Downloads the matching release asset for the current OS/arch so the app is
# self-contained; override the version with HAPPYUSAGE_VERSION=vX.Y.Z.
set -e
cd "$(dirname "$0")"

REPO="SunChJ/happyusage"
VERSION="${HAPPYUSAGE_VERSION:-}"

if [ -z "$VERSION" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"
fi
[ -n "$VERSION" ] || { echo "failed to resolve latest version" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) OS_GO="darwin" ;;
  Linux)  OS_GO="linux" ;;
  MINGW*|MSYS*|CYGWIN*) OS_GO="windows" ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64)  ARCH="amd64" ;;
  arm64|aarch64) ARCH="arm64" ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

if [ "$OS_GO" = "windows" ]; then
  EXT="zip"
else
  EXT="tar.gz"
fi
ASSET="hu-${OS_GO}-${ARCH}.${EXT}"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo "==> fetching ${URL}"
if [ "$OS_GO" = "windows" ]; then
  curl -fsSL -o "$TMP/hu.zip" "$URL"
  unzip -o -q "$TMP/hu.zip" -d "$TMP"
else
  curl -fsSL -o "$TMP/hu.tgz" "$URL"
  tar -xzf "$TMP/hu.tgz" -C "$TMP"
fi

BIN_PATH="$(find "$TMP" -type f \( -name 'hu' -o -name 'hu.exe' \) | head -1)"
[ -n "$BIN_PATH" ] || { echo "hu binary not found in ${ASSET}" >&2; exit 1; }

cp "$BIN_PATH" hu
chmod +x hu
echo "==> bundled hu ${VERSION}"
