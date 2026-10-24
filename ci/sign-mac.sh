#!/bin/bash
# Sign, notarize and staple RealTimeX Link for macOS, on RealTimeX's release Mac runners (the
# GitLab `sign-mac-*` jobs): the unsigned archive from the draft release (checked against its
# SHA256SUMS) in, the signed archive (same name) and a signed, notarized, stapled .dmg out.
# Usage: ci/sign-mac.sh VERSION ARCH OUT_DIR
# Environment (CI variables, the ones RealTimeX's electron-builder signs with): CSC_NAME (the
# Developer ID Application identity in the runner's keychain), KEYCHAIN and KEYCHAIN_PASSWORD
# (optional: unlocked when set), APPLE_ID, APPLE_APP_SPECIFIC_PASSWORD, APPLE_TEAM_ID, and
# GITHUB_LINK_TOKEN (the draft release).
set -euo pipefail
VERSION="$1"
ARCH="$2"
OUT="$3"
NAME="realtimex-link-macos-${ARCH}.tar.gz"
DMG="RealTimeX-Link-${VERSION}-macos-${ARCH}.dmg"

for variable in CSC_NAME APPLE_ID APPLE_APP_SPECIFIC_PASSWORD APPLE_TEAM_ID; do
  if [ -z "${!variable:-}" ]; then
    echo "ERROR: ${variable} is not set" >&2
    exit 1
  fi
done
IDENTITY="$CSC_NAME"
case "$IDENTITY" in
  "Developer ID Application:"*) ;;
  *) IDENTITY="Developer ID Application: ${IDENTITY}" ;;
esac
KEYCHAIN_ARGS=()
if [ -n "${KEYCHAIN:-}" ] && security show-keychain-info "$KEYCHAIN" >/dev/null 2>&1; then
  security unlock-keychain -p "${KEYCHAIN_PASSWORD:-}" "$KEYCHAIN"
  KEYCHAIN_ARGS=(--keychain "$KEYCHAIN")
fi
echo "Developer ID identities on this runner: $(security find-identity -v -p codesigning | grep -c 'Developer ID Application' || true)"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$OUT"

# The unsigned build, only if it is the one the release lists.
python3 ci/release.py download "$VERSION" "$WORK" "$NAME"
mkdir "$WORK/app"
tar -xzf "$WORK/$NAME" -C "$WORK/app"
APP="$WORK/app/RealTimeX Link.app"
test -d "$APP"

# Inside out: the helpers, then the app (its main executable, rtx-link, and its seal). Hardened
# runtime and a secure timestamp, as notarization requires.
sign() {
  codesign --force --options runtime --timestamp ${KEYCHAIN_ARGS[@]+"${KEYCHAIN_ARGS[@]}"} --sign "$IDENTITY" "$@"
}
for helper in rtx-linkd rtx-control; do
  sign --identifier "ai.realtimex.link.${helper}" "$APP/Contents/MacOS/$helper"
done
sign "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"
codesign -dv "$APP" 2>&1 | grep -E "^(Identifier|TeamIdentifier|Authority=Developer ID Application)"

notarize() {
  local file="$1"
  local answer
  answer="$(xcrun notarytool submit "$file" --apple-id "$APPLE_ID" --password "$APPLE_APP_SPECIFIC_PASSWORD" \
    --team-id "$APPLE_TEAM_ID" --wait --timeout 45m --output-format json)"
  echo "$answer"
  local id status
  id="$(printf '%s' "$answer" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')"
  status="$(printf '%s' "$answer" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get("status",""))')"
  if [ "$status" != "Accepted" ]; then
    [ -n "$id" ] && xcrun notarytool log "$id" --apple-id "$APPLE_ID" --password "$APPLE_APP_SPECIFIC_PASSWORD" --team-id "$APPLE_TEAM_ID" || true
    echo "ERROR: notarization of $(basename "$file"): ${status:-no answer}" >&2
    exit 1
  fi
}

/usr/bin/ditto -c -k --keepParent "$APP" "$WORK/notarize.zip"
notarize "$WORK/notarize.zip"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
spctl --assess --type execute --verbose=2 "$APP"

# The signed archive, under the unsigned one's name (RealTimeX's pin and installers use it).
(cd "$WORK/app" && tar -czf "$OLDPWD/$OUT/$NAME" "RealTimeX Link.app")

# The .dmg to drag Link to Applications: signed, notarized and stapled itself.
packaging/macos/dmg.sh "$APP" "$OUT/$DMG"
codesign --force --timestamp ${KEYCHAIN_ARGS[@]+"${KEYCHAIN_ARGS[@]}"} --sign "$IDENTITY" "$OUT/$DMG"
notarize "$OUT/$DMG"
xcrun stapler staple "$OUT/$DMG"
spctl --assess --type open --context context:primary-signature --verbose=2 "$OUT/$DMG"
ls -l "$OUT"
