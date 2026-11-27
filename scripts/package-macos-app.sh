#!/usr/bin/env bash
# Package a Kova release binary into a native macOS .app bundle (KOVA-062).
#
# Usage:
#   ./scripts/package-macos-app.sh [BINARY_PATH] [APP_BUNDLE_PATH]
#
# Defaults:
#   BINARY_PATH     = ${CARGO_TARGET_DIR:-./target}/release/kova
#   APP_BUNDLE_PATH = $HOME/Applications/Kova.app
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEFAULT_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
BINARY_PATH="${1:-$DEFAULT_TARGET_DIR/release/kova}"
APP_BUNDLE_PATH="${2:-$HOME/Applications/Kova.app}"
ICON_SOURCE="$REPO_ROOT/assets/icons/AppIcon.icns"

if [ ! -f "$BINARY_PATH" ]; then
  echo "error: Kova binary not found at '$BINARY_PATH'" >&2
  echo "hint: build release binary first or pass its path as \$1:" >&2
  echo "  cargo build --release -p kova_app" >&2
  echo "  $0 path/to/kova [output/Kova.app]" >&2
  exit 1
fi

if [ ! -f "$ICON_SOURCE" ]; then
  echo "error: AppIcon.icns not found at '$ICON_SOURCE'" >&2
  exit 1
fi

APP_VERSION="$(
  sed -n '/^\[workspace\.package\]/,/^\[/s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$REPO_ROOT/Cargo.toml" | head -n 1
)"
APP_VERSION="${APP_VERSION:-0.1.0}"

CONTENTS_DIR="$APP_BUNDLE_PATH/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
PLIST_PATH="$CONTENTS_DIR/Info.plist"

mkdir -p "$MACOS_DIR" "$RESOURCES_DIR"

cp "$BINARY_PATH" "$MACOS_DIR/kova"
chmod 755 "$MACOS_DIR/kova"

cp "$ICON_SOURCE" "$RESOURCES_DIR/AppIcon.icns"
chmod 644 "$RESOURCES_DIR/AppIcon.icns"

cat >"$PLIST_PATH" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>Kova IDE</string>
	<key>CFBundleExecutable</key>
	<string>kova</string>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
	<key>CFBundleIdentifier</key>
	<string>dev.kova.ide</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>Kova</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>${APP_VERSION}</string>
	<key>CFBundleVersion</key>
	<string>${APP_VERSION}</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
	<key>LSMinimumSystemVersion</key>
	<string>13.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
</dict>
</plist>
EOF
chmod 644 "$PLIST_PATH"

if command -v plutil >/dev/null 2>&1; then
  plutil -lint "$PLIST_PATH" >/dev/null
fi

if command -v codesign >/dev/null 2>&1; then
  codesign --force --deep --sign - "$APP_BUNDLE_PATH" >/dev/null 2>&1 || true
fi

touch "$APP_BUNDLE_PATH"
echo "Packaged Kova.app -> $APP_BUNDLE_PATH (version $APP_VERSION)"
