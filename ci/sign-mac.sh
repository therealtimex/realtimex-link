#!/bin/bash
# Sign, notarize and staple RealTimeX Link for macOS, on RealTimeX's release Mac runners (the
# GitLab `sign-mac-*` jobs): the unsigned archive in, the signed archive (same name) and a signed,
# notarized, stapled .dmg out.
# Usage: ci/sign-mac.sh VERSION ARCH IN_DIR OUT_DIR
# IN_DIR is the verify job's output (ci/release.py fetch): the unsigned archives, taken from the
# tag's build run and checked there, and tree/, packaging/ as the release's commit has it (the .dmg
# script comes from there, not from this checkout, which is main's when signing again).
# Environment (CI variables, the ones RealTimeX's electron-builder signs with): CSC_NAME (the
# Developer ID Application identity in the runner's keychain), KEYCHAIN and KEYCHAIN_PASSWORD
# (optional: unlocked when set, and locked again at the end), APPLE_ID, APPLE_APP_SPECIFIC_PASSWORD,
# APPLE_TEAM_ID (every signature must carry this Team ID).
set -euo pipefail
VERSION="$1"
ARCH="$2"
IN="$3"
OUT="$4"
NAME="realtimex-link-macos-${ARCH}.tar.gz"
DMG="RealTimeX-Link-${VERSION}-macos-${ARCH}.dmg"

for variable in CSC_NAME APPLE_ID APPLE_APP_SPECIFIC_PASSWORD APPLE_TEAM_ID; do
  if [ -z "${!variable:-}" ]; then
    echo "ERROR: ${variable} is not set" >&2
    exit 1
  fi
done
test -f "$IN/$NAME"
test -f "$IN/tree/packaging/macos/dmg.sh"
IDENTITY="$CSC_NAME"
case "$IDENTITY" in
  "Developer ID Application:"*) ;;
  *) IDENTITY="Developer ID Application: ${IDENTITY}" ;;
esac

WORK="$(mktemp -d)"
NOTARY_KEYCHAIN="$WORK/notary.keychain-db"
UNLOCKED=""
# Whatever happens: the release keychain locked again (if this unlocked it), the throwaway notary
# keychain deleted, the work folder gone.
cleanup() {
  if [ -n "$UNLOCKED" ]; then security lock-keychain "$UNLOCKED" || true; fi
  if [ -f "$NOTARY_KEYCHAIN" ]; then security delete-keychain "$NOTARY_KEYCHAIN" || true; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

KEYCHAIN_ARGS=()
if [ -n "${KEYCHAIN:-}" ] && security show-keychain-info "$KEYCHAIN" >/dev/null 2>&1; then
  security unlock-keychain -p "${KEYCHAIN_PASSWORD:-}" "$KEYCHAIN"
  UNLOCKED="$KEYCHAIN"
  KEYCHAIN_ARGS=(--keychain "$KEYCHAIN")
fi
echo "Developer ID identities on this runner: $(security find-identity -v -p codesigning | grep -c 'Developer ID Application' || true)"
mkdir -p "$OUT"

# notarytool's credentials as a keychain profile, so the app-specific password never sits in a
# command line (where any process on the runner can read it with ps). The profile lives in a
# throwaway keychain of this job's own (its random password is worthless once it's deleted), and
# store-credentials is answered at its password prompt by expect, which reads the password from the
# environment.
security create-keychain -p "$(uuidgen)" "$NOTARY_KEYCHAIN" >/dev/null
security set-keychain-settings "$NOTARY_KEYCHAIN" # no auto-lock while notarization waits
cat >"$WORK/notary-profile.exp" <<'EXPECT'
set timeout 120
log_user 0
spawn xcrun notarytool store-credentials link-notary --apple-id $env(APPLE_ID) --team-id $env(APPLE_TEAM_ID) --keychain $env(NOTARY_KEYCHAIN)
expect {
  -re {(?i)password[^\n]*:} { send -- "$env(APPLE_APP_SPECIFIC_PASSWORD)\r"; exp_continue }
  eof
}
catch wait result
exit [lindex $result 3]
EXPECT
if ! NOTARY_KEYCHAIN="$NOTARY_KEYCHAIN" /usr/bin/expect "$WORK/notary-profile.exp"; then
  echo "ERROR: notarytool rejected APPLE_ID / APPLE_TEAM_ID / APPLE_APP_SPECIFIC_PASSWORD" >&2
  exit 1
fi
NOTARY=(--keychain-profile link-notary --keychain "$NOTARY_KEYCHAIN")

# The Team ID a signature carries must be ours: a wrong CSC_NAME (another team's Developer ID on
# the runner) would otherwise ship, and every installed Link would refuse the update.
check_team() {
  local team
  team="$(codesign -dv "$1" 2>&1 | sed -n 's/^TeamIdentifier=//p')"
  if [ "$team" != "$APPLE_TEAM_ID" ]; then
    echo "ERROR: $(basename "$1") is signed for Team ID '${team}', not APPLE_TEAM_ID" >&2
    exit 1
  fi
}

mkdir "$WORK/app"
tar -xzf "$IN/$NAME" -C "$WORK/app"
APP="$WORK/app/RealTimeX Link.app"
test -d "$APP"

# Inside out: the helpers, then the app (its main executable, rtx-link, and its seal). Hardened
# runtime and a secure timestamp, as notarization requires.
sign() {
  codesign --force --options runtime --timestamp ${KEYCHAIN_ARGS[@]+"${KEYCHAIN_ARGS[@]}"} --sign "$IDENTITY" "$@"
}
for helper in rtx-linkd rtx-control; do
  sign --identifier "ai.realtimex.link.${helper}" "$APP/Contents/MacOS/$helper"
  check_team "$APP/Contents/MacOS/$helper"
done
sign "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"
codesign -dv "$APP" 2>&1 | grep -E "^(Identifier|TeamIdentifier|Authority=Developer ID Application)"
check_team "$APP"

notarize() {
  local file="$1"
  local answer
  answer="$(xcrun notarytool submit "$file" "${NOTARY[@]}" --wait --timeout 45m --output-format json)"
  echo "$answer"
  local id status
  id="$(printf '%s' "$answer" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')"
  status="$(printf '%s' "$answer" | /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get("status",""))')"
  if [ "$status" != "Accepted" ]; then
    [ -n "$id" ] && xcrun notarytool log "$id" "${NOTARY[@]}" || true
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
bash "$IN/tree/packaging/macos/dmg.sh" "$APP" "$OUT/$DMG"
codesign --force --timestamp ${KEYCHAIN_ARGS[@]+"${KEYCHAIN_ARGS[@]}"} --sign "$IDENTITY" "$OUT/$DMG"
check_team "$OUT/$DMG"
notarize "$OUT/$DMG"
xcrun stapler staple "$OUT/$DMG"
spctl --assess --type open --context context:primary-signature --verbose=2 "$OUT/$DMG"
ls -l "$OUT"
