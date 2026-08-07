#!/bin/sh
# Bundle the HappyUsage binary matching the Tauri build target.
# Override the version with HAPPYUSAGE_VERSION=vX.Y.Z.
set -e
cd "$(dirname "$0")"

REPO="SunChJ/happyusage"
VERSION="${HAPPYUSAGE_VERSION:-}"

if [ -z "$VERSION" ]; then
  VERSION="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest" \
    | sed -E 's#.*/tag/##' | sed 's#/$##')"
fi
[ -n "$VERSION" ] || { echo "failed to resolve latest version" >&2; exit 1; }

case "${TAURI_ENV_PLATFORM:-$(uname -s)}" in
  darwin|Darwin) OS_GO="darwin" ;;
  linux|Linux) OS_GO="linux" ;;
  windows|Windows|MINGW*|MSYS*|CYGWIN*) OS_GO="windows" ;;
  *) echo "unsupported OS: ${TAURI_ENV_PLATFORM:-$(uname -s)}" >&2; exit 1 ;;
esac

# beforeBuildCommand receives TAURI_ENV_ARCH from the requested Tauri target.
# Fall back to the host architecture for direct/local script invocations.
case "${TAURI_ENV_ARCH:-$(uname -m)}" in
  x86_64|x86|amd64) ARCH="amd64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *) echo "unsupported architecture: ${TAURI_ENV_ARCH:-$(uname -m)}" >&2; exit 1 ;;
esac

if [ "$OS_GO" = "windows" ]; then EXT="zip"; else EXT="tar.gz"; fi
ASSET="hu-${OS_GO}-${ARCH}.${EXT}"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "==> fetching ${URL}"
if [ "$OS_GO" = "windows" ]; then
  curl -fsSL -o "$TMP/hu.zip" "$URL"
  unzip -o -q "$TMP/hu.zip" -d "$TMP/extract"
else
  mkdir -p "$TMP/extract"
  curl -fsSL -o "$TMP/hu.tgz" "$URL"
  tar -xzf "$TMP/hu.tgz" -C "$TMP/extract"
fi

BIN_PATH="$(find "$TMP/extract" -type f \( -name 'hu' -o -name 'hu.exe' \) | head -1)"
[ -n "$BIN_PATH" ] || { echo "hu binary not found in ${ASSET}" >&2; exit 1; }
cp "$BIN_PATH" hu
chmod +x hu
echo "==> bundled hu ${VERSION} (${OS_GO}/${ARCH})"
