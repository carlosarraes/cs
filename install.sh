#!/bin/sh
# cs installer
#
# Usage:
#   curl -sSf https://raw.githubusercontent.com/carlosarraes/cs/main/install.sh | sh
#
# Pin a specific version (skips the "latest" lookup):
#   curl -sSf .../install.sh | VERSION=v0.0.1 sh
#
# The downloaded binary is verified against the release's SHA256SUMS before
# it is installed; a mismatch aborts.

set -e

REPO="carlosarraes/cs"
BINARY_NAME="cs"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
VERSION="${VERSION:-}"
GITHUB_LATEST="https://api.github.com/repos/${REPO}/releases/latest"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    echo "Cannot verify download: neither sha256sum nor shasum is available." >&2
    exit 1
  fi
}

get_arch() {
  ARCH=$(uname -m)
  case $ARCH in
  x86_64) ARCH="x86_64" ;;
  aarch64) ARCH="aarch64" ;;
  arm64) ARCH="aarch64" ;;
  *)
    echo "Unsupported architecture: $ARCH"
    exit 1
    ;;
  esac
}

get_os() {
  OS=$(uname -s)
  case $OS in
  Linux) OS="linux" ;;
  Darwin) OS="macos" ;;
  *)
    echo "Unsupported OS: $OS"
    exit 1
    ;;
  esac
}

download_binary() {
  if [ -n "$VERSION" ]; then
    echo "Using pinned version: $VERSION"
  else
    echo "Fetching latest release..."
    VERSION=$(curl -s $GITHUB_LATEST | grep -o '"tag_name": "[^"]*' | cut -d'"' -f4)
    if [ -z "$VERSION" ]; then
      echo "Failed to fetch latest version"
      exit 1
    fi
    echo "Latest version: $VERSION"
  fi

  TMP_DIR=$(mktemp -d)
  trap 'rm -rf "$TMP_DIR"' EXIT

  BINARY_SUFFIX="${BINARY_NAME}-${OS}-${ARCH}"
  BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
  DOWNLOAD_URL="${BASE_URL}/${BINARY_SUFFIX}"
  echo "Downloading from: $DOWNLOAD_URL"
  curl -fsSL "$DOWNLOAD_URL" -o "${TMP_DIR}/${BINARY_NAME}" || {
    echo "Download failed. Check URL/permissions/network."
    exit 1
  }

  echo "Verifying checksum..."
  curl -fsSL "${BASE_URL}/SHA256SUMS" -o "${TMP_DIR}/SHA256SUMS" || {
    echo "Failed to download SHA256SUMS for verification."
    exit 1
  }
  EXPECTED=$(awk -v f="$BINARY_SUFFIX" '$2 == f {print $1}' "${TMP_DIR}/SHA256SUMS")
  if [ -z "$EXPECTED" ]; then
    echo "No checksum entry for ${BINARY_SUFFIX} in SHA256SUMS; aborting."
    exit 1
  fi
  ACTUAL=$(sha256_of "${TMP_DIR}/${BINARY_NAME}")
  if [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "Checksum mismatch for ${BINARY_SUFFIX}; refusing to install."
    echo "  expected: $EXPECTED"
    echo "  actual:   $ACTUAL"
    exit 1
  fi
  echo "Checksum verified."

  chmod +x "${TMP_DIR}/${BINARY_NAME}"

  CREATED_DIR_MSG=""
  if [ ! -d "$BIN_DIR" ]; then
    echo "Installation directory '$BIN_DIR' not found."
    echo "Creating directory: $BIN_DIR"
    mkdir -p "$BIN_DIR"
    CREATED_DIR_MSG="Note: Created directory '$BIN_DIR'. You might need to add it to your PATH."
  fi

  echo "Installing to $BIN_DIR..."
  install -m 755 "${TMP_DIR}/${BINARY_NAME}" "$BIN_DIR/$BINARY_NAME"

  echo "${BINARY_NAME} ${VERSION} installed successfully to $BIN_DIR"

  if [ -n "$CREATED_DIR_MSG" ]; then
    echo ""
    echo "$CREATED_DIR_MSG"
  fi
}

get_arch
get_os
download_binary

echo ""
echo "Installation complete! Run '${BINARY_NAME} --help' to get started."
echo "Next: '${BINARY_NAME} add <alias>' to save your first account."
