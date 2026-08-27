#!/bin/sh
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
APP="$ROOT/src-tauri/target/release/bundle/macos/VoxType.app"
DMG_DIR="$ROOT/src-tauri/target/release/bundle/dmg"
DMG="$DMG_DIR/VoxType_0.1.0_aarch64.dmg"
ENTITLEMENTS="$ROOT/src-tauri/Entitlements.plist"

if [ "$(uname -m)" != "arm64" ]; then
  echo "VoxType v1 must be packaged on Apple Silicon." >&2
  exit 1
fi

cd "$ROOT"
npm run tauri build -- --bundles app

codesign \
  --force \
  --deep \
  --options runtime \
  --identifier com.nbutton.voxtype \
  --entitlements "$ENTITLEMENTS" \
  --sign - \
  "$APP"
codesign --verify --deep --strict "$APP"

if [ "${1:-}" = "--app-only" ]; then
  echo "Created $APP"
  exit 0
fi

mkdir -p "$DMG_DIR"
STAGING="$(mktemp -d "${TMPDIR:-/tmp}/voxtype-dmg.XXXXXX")"
trap 'rm -rf "$STAGING"' EXIT INT TERM
ditto "$APP" "$STAGING/VoxType.app"
ln -s /Applications "$STAGING/Applications"
rm -f "$DMG"
hdiutil create \
  -volname "VoxType" \
  -srcfolder "$STAGING" \
  -ov \
  -format UDZO \
  "$DMG" >/dev/null

echo "Created $APP"
echo "Created $DMG"
