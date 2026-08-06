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
# the binary self-bootstraps its payload into ~/Library/Application Support/MACVM/<mode>
# (a WRITABLE home — the .app itself is read-only), seeds the SQLite image
# there, and runs from it. A version bump re-copies + reseeds; your edited
# image survives an unchanged version.
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # repo root
ROOT="$PWD"
DIST="$ROOT/dist"
# The payload stamp — what the in-binary bootstrap compares to decide whether
# to reinstall the runtime home. Stays the git hash: it changes on every commit,
# which is exactly the reseed trigger we want.
VERSION="$(git rev-parse --short HEAD 2>/dev/null || date +%Y%m%d%H%M)"
# Apple-facing versions must be numeric dotted; a git hash is rejected at
# upload and looks wrong in Finder. MARKETING_VERSION is what users see;
# BUILD_NUMBER must increase monotonically across submissions, and the commit
# count does that for free.
MARKETING_VERSION="${MARKETING_VERSION:-$(sed -nE 's/^version = "([^"]+)".*/\1/p' "$ROOT/Cargo.toml" | head -1)}"
MARKETING_VERSION="${MARKETING_VERSION:-0.0.0}"
BUILD_NUMBER="${BUILD_NUMBER:-$(git rev-list --count HEAD 2>/dev/null || echo 1)}"
# Optional signing. Unset -> the app is built unsigned exactly as before.
#   SIGN_ID="Developer ID Application: Name (TEAMID)"  tools/make-macapp.sh cocoa
# and, once notarytool credentials are stored, additionally:
#   NOTARY_PROFILE=macvm ...
SIGN_ID="${SIGN_ID:-}"
NOTARY_PROFILE="${NOTARY_PROFILE:-}"
ENTITLEMENTS="$ROOT/tools/macvm.entitlements"

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

  # --- the real binary IS the entry point ---
  # It self-bootstraps its payload (cocoa_gui/src/bundle.rs) — what a `launcher`
  # shell script used to do. A Mach-O entry point is what hardened runtime and
  # entitlements attach to; a script CFBundleExecutable runs under Apple-signed
  # /bin/bash before exec'ing the real binary, which makes the entitlement chain
  # hard to reason about and is flagged by notarization.
  cp "$ROOT/target/release/$binname" "$APP/Contents/MacOS/$binname"

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
  <key>CFBundleVersion</key><string>$BUILD_NUMBER</string>
  <key>CFBundleShortVersionString</key><string>$MARKETING_VERSION</string>
  <key>MACVMGitRevision</key><string>$VERSION</string>
  <key>CFBundleExecutable</key><string>$binname</string>
  <key>CFBundleIconFile</key><string>appicon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHumanReadableCopyright</key><string>© Alban Read 2026</string>
</dict>
</plist>
PLIST
  /bin/echo -n "APPL????" > "$APP/Contents/PkgInfo"

  # --- signing (skipped unless SIGN_ID is set) ---
  # Inside-out: the Mach-O first, then the bundle. NOT --deep — it is deprecated
  # and applies the outer entitlements to nested code. `--options runtime` is
  # the hardened runtime, which notarization requires and which is precisely
  # what makes the allow-jit entitlement necessary.
  if [ -n "$SIGN_ID" ]; then
    echo "▸ [$mode] signing as $SIGN_ID"
    codesign --force --options runtime --timestamp \
      --entitlements "$ENTITLEMENTS" --sign "$SIGN_ID" \
      "$APP/Contents/MacOS/$binname"
    codesign --force --options runtime --timestamp \
      --entitlements "$ENTITLEMENTS" --sign "$SIGN_ID" "$APP"
    codesign --verify --strict --verbose=2 "$APP" 2>&1 | sed 's/^/    /'
  else
    echo "▸ [$mode] UNSIGNED (set SIGN_ID to sign; see tools/macvm.entitlements)"
  fi

  # --- .dmg (with a drag-to-Applications target) ---
  local DMG="$DIST/${appname// /-}.dmg"
  echo "▸ [$mode] packaging $DMG"
  local stage; stage="$(mktemp -d)/dmg"; mkdir -p "$stage"
  /usr/bin/ditto "$APP" "$stage/$appname.app"
  ln -s /Applications "$stage/Applications"
  rm -f "$DMG"
  hdiutil create -volname "$appname" -srcfolder "$stage" -ov -format UDZO "$DMG" >/dev/null
  # --- notarize + staple (skipped unless NOTARY_PROFILE is set) ---
  # Store credentials once with:
  #   xcrun notarytool store-credentials macvm --apple-id you@example.com --team-id TEAMID
  # (that prompts for an app-specific password from appleid.apple.com, NOT the
  # account password). Stapling lets the dmg validate offline.
  if [ -n "$NOTARY_PROFILE" ]; then
    if [ -z "$SIGN_ID" ]; then
      echo "▸ [$mode] NOTARY_PROFILE set but SIGN_ID is not — notarization needs a signed app; skipping" >&2
    else
      echo "▸ [$mode] notarizing (this waits on Apple)"
      xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
      xcrun stapler staple "$DMG"
      spctl -a -vvv -t install "$APP" 2>&1 | sed 's/^/    /'
    fi
  fi
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
