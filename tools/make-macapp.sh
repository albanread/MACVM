#!/usr/bin/env bash
#
# Build a self-contained, double-clickable .app + .dmg for a MACVM GUI mode.
#
#   tools/make-macapp.sh cocoa    # -> dist/MACVM Cocoa.app + dist/MACVM-Cocoa.dmg
#   tools/make-macapp.sh web      # -> dist/MACVM Web.app   + dist/MACVM-Web.dmg
#   tools/make-macapp.sh both     # both
#
# The app is UNSIGNED (signing/notarization needs an Apple Developer ID this
# script deliberately never touches). On your own Mac: right-click -> Open the
# first time, or `xattr -dr com.apple.quarantine "MACVM Cocoa.app"`.
#
# How it's self-contained: the app carries its whole runtime payload (the
# world/*.mst source, gui/ assets, docs) under Contents/Resources/payload. The
# ObjC runtime + AppKit/Foundation/WebKit/Cocoa + POSIX libc are loaded from
# the OS at launch (dlopen), so nothing system-level is bundled. On first run
# the launcher copies the payload to ~/Library/Application Support/MACVM/<mode>
# (a WRITABLE home — the .app itself is read-only), seeds the SQLite image
# there, and runs from it. A version bump re-copies + reseeds; your edited
# image survives an unchanged version.
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # repo root
ROOT="$PWD"
DIST="$ROOT/dist"
VERSION="$(git rev-parse --short HEAD 2>/dev/null || date +%Y%m%d%H%M)"

make_icns() {   # $1 = source png, $2 = out.icns
  local src="$1" out="$2" set
  set="$(mktemp -d)/icon.iconset"; mkdir -p "$set"
  local s
  for s in 16 32 128 256 512; do
    sips -z "$s" "$s"             "$src" --out "$set/icon_${s}x${s}.png"    >/dev/null
    sips -z "$((s*2))" "$((s*2))" "$src" --out "$set/icon_${s}x${s}@2x.png" >/dev/null
  done
  iconutil -c icns "$set" -o "$out"
}

build_one() {
  local mode="$1" crate binname appname bundleid iconpng
  case "$mode" in
    cocoa) crate="cocoa_gui"; binname="macvm-cocoa"; appname="MACVM Cocoa"
           bundleid="com.macvm.cocoa"; iconpng="$ROOT/tools/app-icon-cocoa.png" ;;
    web)   crate="macvm-gui"; binname="macvm-gui";   appname="MACVM Web"
           bundleid="com.macvm.web";   iconpng="$ROOT/tools/app-icon-web.png" ;;
    *) echo "unknown mode: $mode" >&2; return 2 ;;
  esac

  echo "▸ [$mode] building $binname (release)…"
  cargo build -p "$crate" --release >/dev/null

  local APP="$DIST/$appname.app"
  echo "▸ [$mode] assembling $APP"
  rm -rf "$APP"
  mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/payload"

  # --- the real binary + a launcher (CFBundleExecutable) ---
  cp "$ROOT/target/release/$binname" "$APP/Contents/MacOS/$binname"
  cat > "$APP/Contents/MacOS/launcher" <<LAUNCH
#!/bin/bash
set -e
HERE="\$(cd "\$(dirname "\$0")" && pwd)"                 # Contents/MacOS
RES="\$(cd "\$HERE/../Resources" && pwd)"
SUP="\$HOME/Library/Application Support/MACVM/$mode"
# First run (or a new app version) -> refresh the writable runtime home.
if [ ! -f "\$SUP/.version" ] || ! cmp -s "\$RES/payload/.version" "\$SUP/.version" 2>/dev/null; then
  mkdir -p "\$SUP"
  /usr/bin/ditto "\$RES/payload/" "\$SUP/"
  rm -f "\$SUP/world/image.sqlite3"    # force a fresh reseed for the new world
fi
cd "\$SUP"
export MACVM_GUI_ROOT="\$SUP/gui"
export MACVM_WORLD_PATH="\$SUP/world"
export MACVM_IMAGE_PATH="\$SUP/world/image.sqlite3"
exec "\$HERE/$binname" "\$@"
LAUNCH
  chmod +x "$APP/Contents/MacOS/launcher"

  # --- runtime payload (source of truth; the image is seeded at first run) ---
  rsync -a --exclude='*.sqlite3' --exclude='.DS_Store' "$ROOT/world/" "$APP/Contents/Resources/payload/world/"
  mkdir -p "$APP/Contents/Resources/payload/gui"
  rsync -a --exclude='.DS_Store' "$ROOT/gui/assets/" "$APP/Contents/Resources/payload/gui/assets/"
  if [ "$mode" = "web" ]; then
    rsync -a --exclude='.DS_Store' "$ROOT/gui/reference/" "$APP/Contents/Resources/payload/gui/reference/"
  fi
  mkdir -p "$APP/Contents/Resources/payload/docs"
  cp "$ROOT/docs/macvm_help.md" "$APP/Contents/Resources/payload/docs/macvm_help.md"
  echo "$VERSION" > "$APP/Contents/Resources/payload/.version"

  # --- icon + Info.plist ---
  make_icns "$iconpng" "$APP/Contents/Resources/appicon.icns"
  cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$appname</string>
  <key>CFBundleDisplayName</key><string>$appname</string>
  <key>CFBundleIdentifier</key><string>$bundleid</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleExecutable</key><string>launcher</string>
  <key>CFBundleIconFile</key><string>appicon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST
  /bin/echo -n "APPL????" > "$APP/Contents/PkgInfo"

  # --- .dmg (with a drag-to-Applications target) ---
  local DMG="$DIST/${appname// /-}.dmg"
  echo "▸ [$mode] packaging $DMG"
  local stage; stage="$(mktemp -d)/dmg"; mkdir -p "$stage"
  /usr/bin/ditto "$APP" "$stage/$appname.app"
  ln -s /Applications "$stage/Applications"
  rm -f "$DMG"
  hdiutil create -volname "$appname" -srcfolder "$stage" -ov -format UDZO "$DMG" >/dev/null
  echo "▸ [$mode] done: $DMG  ($(du -h "$DMG" | cut -f1))"
}

mkdir -p "$DIST"
case "${1:-both}" in
  cocoa) build_one cocoa ;;
  web)   build_one web ;;
  both)  build_one cocoa; build_one web ;;
  *) echo "usage: tools/make-macapp.sh [cocoa|web|both]" >&2; exit 2 ;;
esac
echo "✓ artifacts in $DIST"
