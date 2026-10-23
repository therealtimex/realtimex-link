#!/bin/bash
# RealTimeX Link.app from built executables: rtx-link is the app (window and menu bar icon),
# rtx-linkd and rtx-control sit next to it, where rtx-link and rtx-linkd look for them. A menu bar
# app (LSUIElement): no Dock icon, and it keeps running when its window is closed.
# Usage: scripts/bundle-macos.sh BIN_DIR OUTPUT_DIR (BIN_DIR holds rtx-link, rtx-linkd, rtx-control)
set -euo pipefail
BIN_DIR="$1"
OUTPUT_DIR="$2"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml")
APP="$OUTPUT_DIR/RealTimeX Link.app"

if [ -e "$APP" ]; then
  echo "ERROR: $APP already exists" >&2
  exit 1
fi
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
for bin in rtx-link rtx-linkd rtx-control; do
  cp "$BIN_DIR/$bin" "$APP/Contents/MacOS/$bin"
  chmod +x "$APP/Contents/MacOS/$bin"
done
cp "$ROOT/assets/icon.icns" "$APP/Contents/Resources/icon.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>ai.realtimex.link</string>
  <key>CFBundleName</key>
  <string>RealTimeX Link</string>
  <key>CFBundleDisplayName</key>
  <string>RealTimeX Link</string>
  <key>CFBundleExecutable</key>
  <string>rtx-link</string>
  <key>CFBundleIconFile</key>
  <string>icon</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$VERSION</string>
  <key>CFBundleVersion</key>
  <string>$VERSION</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>LSUIElement</key>
  <true/>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST
plutil -lint "$APP/Contents/Info.plist" >/dev/null
echo "$APP ($VERSION)"
