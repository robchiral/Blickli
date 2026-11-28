#!/bin/sh
# Build, sign, and install Blickli.app to /Applications.
set -eu

cd "$(dirname "$0")/.."
cargo build --locked --release --target-dir target -p blickli

VERSION=$(./scripts/version.sh)

APP=target/Blickli.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/blickli "$APP/Contents/MacOS/blickli"

ICON_BUILD_DIR=$(mktemp -d target/blickli-icon.XXXXXX)
trap 'rm -rf "$ICON_BUILD_DIR"' EXIT
trap 'exit 1' HUP INT TERM
ICONSET=$ICON_BUILD_DIR/AppIcon.iconset
mkdir "$ICONSET"
for size in 16 32 128 256 512; do
  for scale in '' @2x; do
    cp "branding/macos/AppIcon.appiconset/icon-$size$scale.png" \
      "$ICONSET/icon_${size}x$size$scale.png"
  done
done
iconutil -c icns -o "$APP/Contents/Resources/AppIcon.icns" "$ICONSET"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>blickli</string>
  <key>CFBundleIdentifier</key><string>local.blickli</string>
  <key>CFBundleName</key><string>Blickli</string>
  <key>CFBundleDisplayName</key><string>Blickli</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>27.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.photography</string>
  <key>NSDesktopFolderUsageDescription</key><string>Blickli reads and edits images you browse on your Desktop.</string>
  <key>NSDocumentsFolderUsageDescription</key><string>Blickli reads and edits images you browse in your Documents folder.</string>
  <key>NSDownloadsFolderUsageDescription</key><string>Blickli reads and edits images you browse in your Downloads folder.</string>
  <key>NSRemovableVolumesUsageDescription</key><string>Blickli reads and edits images you browse on external drives.</string>
  <key>NSNetworkVolumesUsageDescription</key><string>Blickli reads and edits images you browse on network volumes.</string>
  <!-- Offered in Finder's Open With for these types and folders. -->
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>Image</string>
      <key>CFBundleTypeRole</key><string>Viewer</string>
      <key>LSHandlerRank</key><string>Default</string>
      <key>LSItemContentTypes</key>
      <array>
        <string>public.jpeg</string>
        <string>public.png</string>
        <string>public.heic</string>
        <string>com.compuserve.gif</string>
        <string>org.webmproject.webp</string>
        <string>public.tiff</string>
        <string>public.heif</string>
        <string>com.microsoft.bmp</string>
        <string>public.avif</string>
        <string>public.jpeg-xl</string>
      </array>
    </dict>
    <dict>
      <key>CFBundleTypeName</key><string>Folder</string>
      <key>CFBundleTypeRole</key><string>Viewer</string>
      <key>LSHandlerRank</key><string>Alternate</string>
      <key>LSItemContentTypes</key>
      <array>
        <string>public.folder</string>
      </array>
    </dict>
  </array>
</dict>
</plist>
PLIST

if [ -z "${CODESIGN_IDENTITY:-}" ]; then
  IDENTITIES=$(security find-identity -v -p codesigning 2>/dev/null || true)
  for pattern in "Blickli Local" "Apple Development:" "Developer ID Application:"; do
    FOUND=$(printf '%s\n' "$IDENTITIES" | sed -En "s/.*\"(${pattern}[^\"]*)\".*/\1/p" | head -n 1)
    if [ -n "$FOUND" ]; then
      CODESIGN_IDENTITY="$FOUND"
      break
    fi
  done
fi
SIGN_IDENTITY="${CODESIGN_IDENTITY:--}"
codesign --force --options runtime --sign "$SIGN_IDENTITY" "$APP"
codesign --verify --strict "$APP"

DEST=/Applications/Blickli.app
rm -rf "$DEST"
ditto "$APP" "$DEST"
codesign --verify --strict "$DEST"

echo "built $APP ($VERSION, signed: $SIGN_IDENTITY) and installed to $DEST"
