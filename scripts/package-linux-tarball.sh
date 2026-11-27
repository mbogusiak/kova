#!/usr/bin/env bash
# Package a Kova Linux release binary into a tarball (KOVA-070).
#
# Usage:
#   ./scripts/package-linux-tarball.sh [VERSION] [BINARY_PATH] [OUTPUT_DIR]
#
# Defaults:
#   VERSION     = [workspace.package] version from Cargo.toml
#   BINARY_PATH = ${CARGO_TARGET_DIR:-./target}/release/kova
#   OUTPUT_DIR  = ./dist
#
# Produces: OUTPUT_DIR/kova-<version>-linux-x86_64.tar.gz containing
#   kova-<version>-linux-x86_64/{kova, kova.desktop, kova.svg, kova-512.png}
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

WORKSPACE_VERSION="$(
  sed -n '/^\[workspace\.package\]/,/^\[/s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$REPO_ROOT/Cargo.toml" | head -n 1
)"
VERSION="${1:-${WORKSPACE_VERSION:-0.1.0}}"
VERSION="${VERSION#v}"
DEFAULT_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
BINARY_PATH="${2:-$DEFAULT_TARGET_DIR/release/kova}"
OUTPUT_DIR="${3:-$REPO_ROOT/dist}"

DESKTOP_FILE="$REPO_ROOT/assets/linux/kova.desktop"
ICON_SVG="$REPO_ROOT/assets/icons/kova.svg"
ICON_PNG="$REPO_ROOT/assets/icons/kova-512.png"

if [ ! -f "$BINARY_PATH" ]; then
  echo "error: Kova binary not found at '$BINARY_PATH'" >&2
  echo "hint: cargo build --release -p kova_app" >&2
  exit 1
fi

for asset in "$DESKTOP_FILE" "$ICON_SVG" "$ICON_PNG"; do
  if [ ! -f "$asset" ]; then
    echo "error: required asset not found: '$asset'" >&2
    exit 1
  fi
done

PACKAGE_NAME="kova-${VERSION}-linux-x86_64"
STAGING_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGING_DIR"' EXIT

PACKAGE_DIR="$STAGING_DIR/$PACKAGE_NAME"
mkdir -p "$PACKAGE_DIR" "$OUTPUT_DIR"

install -m 755 "$BINARY_PATH" "$PACKAGE_DIR/kova"
install -m 644 "$DESKTOP_FILE" "$PACKAGE_DIR/kova.desktop"
install -m 644 "$ICON_SVG" "$PACKAGE_DIR/kova.svg"
install -m 644 "$ICON_PNG" "$PACKAGE_DIR/kova-512.png"

OUTPUT_DIR="$(cd "$OUTPUT_DIR" && pwd)"
TARBALL="$OUTPUT_DIR/${PACKAGE_NAME}.tar.gz"
tar -C "$STAGING_DIR" -czf "$TARBALL" "$PACKAGE_NAME"

echo "Packaged Kova for Linux -> $TARBALL"
