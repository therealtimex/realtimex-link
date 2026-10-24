#!/bin/bash
# RealTimeX Link's .dmg: the (signed, notarized) app and a link to /Applications, to drag it there.
# Usage: packaging/macos/dmg.sh "RealTimeX Link.app" OUTPUT.dmg
set -euo pipefail
APP="$1"
OUTPUT="$2"
STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT
/usr/bin/ditto "$APP" "$STAGING/RealTimeX Link.app"
ln -s /Applications "$STAGING/Applications"
rm -f "$OUTPUT"
hdiutil create -volname "RealTimeX Link" -srcfolder "$STAGING" -fs HFS+ -format UDZO -ov "$OUTPUT"
echo "$OUTPUT"
