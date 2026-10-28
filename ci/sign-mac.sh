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
# (optional: unlocked when set and left unlocked; a failed unlock is only a warning),
# APPLE_ID, APPLE_APP_SPECIFIC_PASSWORD, APPLE_TEAM_ID (every signature must carry this Team ID).
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
# Whatever happens: the throwaway notary keychain deleted, the work folder gone. The release
# keychain is never locked again: on the runners it is the login keychain of the signed-in session,
# which the session's own apps and the next job's git checkout (osxkeychain) use, and a locked one
# stops them at a password prompt nobody answers.
cleanup() {
  if [ -f "$NOTARY_KEYCHAIN" ]; then security delete-keychain "$NOTARY_KEYCHAIN" || true; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

# Unlock first: show-keychain-info fails on a locked keychain ("User interaction is not allowed").
# A failed unlock is not fatal: the runner's Aqua session may have the keychain unlocked already
# (KEYCHAIN_PASSWORD fits one runner, not every one), so codesign decides.
KEYCHAIN_ARGS=()
if [ -n "${KEYCHAIN:-}" ]; then
  if ! security unlock-keychain -p "${KEYCHAIN_PASSWORD:-}" "$KEYCHAIN"; then
    echo "WARNING: couldn't unlock $KEYCHAIN; going on, codesign decides" >&2
  fi
  if info="$(security show-keychain-info "$KEYCHAIN" 2>&1)"; then
    echo "Keychain $KEYCHAIN: $info"
    KEYCHAIN_ARGS=(--keychain "$KEYCHAIN")
  else
    echo "Keychain $KEYCHAIN not usable: $info" >&2
  fi
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

# Stapling fetches the ticket from Apple's CloudKit, whose connection drops now and then ("The
# network connection was lost", error 68) right after notarization was Accepted. Only the fetch is
# retried; the file is already notarized.
staple() {
  local attempt
  for attempt in 1 2 3 4 5; do
    if xcrun stapler staple "$1"; then return 0; fi
    echo "WARNING: stapling $(basename "$1") failed (attempt $attempt of 5)" >&2
    [ "$attempt" -lt 5 ] && sleep $((attempt * 15))
  done
  echo "ERROR: couldn't staple $(basename "$1")" >&2
  exit 1
}

/usr/bin/ditto -c -k --keepParent "$APP" "$WORK/notarize.zip"
notarize "$WORK/notarize.zip"
staple "$APP"
xcrun stapler validate "$APP"
spctl --assess --type execute --verbose=2 "$APP"

# The signed archive, under the unsigned one's name (RealTimeX's pin and installers use it).
(cd "$WORK/app" && tar -czf "$OLDPWD/$OUT/$NAME" "RealTimeX Link.app")

# The .dmg to drag Link to Applications: signed, notarized and stapled itself.
bash "$IN/tree/packaging/macos/dmg.sh" "$APP" "$OUT/$DMG"
codesign --force --timestamp ${KEYCHAIN_ARGS[@]+"${KEYCHAIN_ARGS[@]}"} --sign "$IDENTITY" "$OUT/$DMG"
check_team "$OUT/$DMG"
notarize "$OUT/$DMG"
staple "$OUT/$DMG"
spctl --assess --type open --context context:primary-signature --verbose=2 "$OUT/$DMG"
ls -l "$OUT"
