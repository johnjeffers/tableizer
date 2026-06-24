#!/usr/bin/env bash
# Build Tableizer.app into dist/ (macOS only); pass `dmg` to also produce Tableizer.dmg.
#
#   - Builds the release binary, assembles a standard .app bundle (binary + Info.plist + icon), and
#     ad-hoc code-signs it (so Gatekeeper doesn't kill an unsigned arm64 binary locally). With the
#     `dmg` argument it also wraps the bundle in a drag-to-Applications .dmg.
#   - Uses only macOS built-ins (cargo, plutil, codesign, hdiutil) + the committed assets/icon.icns.
#   - Distribution to *other* machines additionally needs a Developer-ID signature + notarization,
#     which require an Apple Developer account and credentials — slot those in where noted below.
set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="Tableizer"
BIN="tableizer"
BUNDLE_ID="cc.jeffers.tableizer"
VERSION="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
DIST="dist"
APP="$DIST/$APP_NAME.app"
DMG="$DIST/$APP_NAME.dmg"

echo "==> Building release binary (v$VERSION)"
cargo build --release -p "$BIN"

echo "==> Assembling $APP"
rm -rf "$APP" "$DMG"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/release/$BIN" "$APP/Contents/MacOS/$BIN"
cp "assets/icon.icns" "$APP/Contents/Resources/icon.icns"
printf 'APPL????' > "$APP/Contents/PkgInfo"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleExecutable</key><string>$BIN</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSMinimumSystemVersion</key><string>10.15</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>

  <!-- Offer Tableizer as a handler (Open With / settable default) for the formats it reads. -->
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>Delimited text</string>
      <key>CFBundleTypeRole</key><string>Viewer</string>
      <key>LSHandlerRank</key><string>Alternate</string>
      <key>LSItemContentTypes</key>
      <array>
        <string>public.comma-separated-values-text</string>
        <string>public.tab-separated-values-text</string>
        <string>public.plain-text</string>
      </array>
    </dict>
    <dict>
      <key>CFBundleTypeName</key><string>JSON</string>
      <key>CFBundleTypeRole</key><string>Viewer</string>
      <key>LSHandlerRank</key><string>Alternate</string>
      <key>LSItemContentTypes</key>
      <array>
        <string>public.json</string>
        <string>public.ndjson</string>
      </array>
    </dict>
    <dict>
      <key>CFBundleTypeName</key><string>Apache Parquet</string>
      <key>CFBundleTypeRole</key><string>Viewer</string>
      <key>LSHandlerRank</key><string>Alternate</string>
      <key>LSItemContentTypes</key>
      <array><string>org.apache.parquet</string></array>
    </dict>
  </array>

  <!-- Declare UTIs for the formats with no system UTI (we support but don't own them → Imported). -->
  <key>UTImportedTypeDeclarations</key>
  <array>
    <dict>
      <key>UTTypeIdentifier</key><string>public.ndjson</string>
      <key>UTTypeDescription</key><string>JSON Lines</string>
      <key>UTTypeConformsTo</key><array><string>public.text</string></array>
      <key>UTTypeTagSpecification</key>
      <dict>
        <key>public.filename-extension</key>
        <array><string>ndjson</string><string>jsonl</string></array>
      </dict>
    </dict>
    <dict>
      <key>UTTypeIdentifier</key><string>org.apache.parquet</string>
      <key>UTTypeDescription</key><string>Apache Parquet</string>
      <key>UTTypeConformsTo</key><array><string>public.data</string></array>
      <key>UTTypeTagSpecification</key>
      <dict>
        <key>public.filename-extension</key>
        <array><string>parquet</string><string>pqt</string></array>
      </dict>
    </dict>
  </array>
</dict>
</plist>
PLIST

plutil -lint "$APP/Contents/Info.plist" >/dev/null

echo "==> Ad-hoc signing (replace '-' with a Developer ID identity to notarize for distribution)"
codesign --force --deep --sign - "$APP"
codesign --verify --deep --strict "$APP" && echo "    signature OK"

if [ "${1:-}" = "dmg" ]; then
  echo "==> Building $DMG (drag-to-Applications)"
  STAGE="$(mktemp -d)"
  cp -R "$APP" "$STAGE/"
  ln -s /Applications "$STAGE/Applications"
  hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
  rm -rf "$STAGE"
fi

echo "==> Done:"
echo "    $APP"
if [ "${1:-}" = "dmg" ]; then echo "    $DMG"; fi
