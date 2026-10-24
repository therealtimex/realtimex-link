#!/bin/bash
# RealTimeX Link's .dmg: the (signed, notarized) app and a link to /Applications, to drag it there,
# in an arranged window with the app's icon as the volume's (dmgbuild, dmg-settings.py; no Finder
# needed). dmgbuild comes pinned with hashes (dmg-requirements.txt) into a throwaway venv.
# Usage: packaging/macos/dmg.sh "RealTimeX Link.app" OUTPUT.dmg
set -euo pipefail
APP="$1"
OUTPUT="$2"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VENV="$(mktemp -d)"
trap 'rm -rf "$VENV"' EXIT
/usr/bin/python3 -m venv "$VENV"
"$VENV/bin/pip" install --quiet --disable-pip-version-check --require-hashes --only-binary :all: \
  -r "$HERE/dmg-requirements.txt"
rm -f "$OUTPUT"
"$VENV/bin/dmgbuild" -s "$HERE/dmg-settings.py" -D app="$APP" "RealTimeX Link" "$OUTPUT"
echo "$OUTPUT"
