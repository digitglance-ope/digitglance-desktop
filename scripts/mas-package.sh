#!/usr/bin/env bash
#
# Package a built .app for Mac App Store upload.
#
#   scripts/mas-package.sh <app name> <bundle id> <path to .provisionprofile>
#
# e.g. scripts/mas-package.sh "DigitGlance Trade" com.digitglance.trade.mobile \
#          ~/Downloads/DigitGlance_Trade_Mac_App_Store.provisionprofile
#
# Tauri already signs the .app, but an App Store build additionally needs the
# provisioning profile embedded in the bundle -- which invalidates that
# signature, hence the re-sign -- and then a .pkg signed with the *installer*
# certificate, which is what Transporter actually uploads.

set -euo pipefail

APP_NAME="${1:?usage: mas-package.sh <app name> <bundle id> <profile>}"
BUNDLE_ID="${2:?missing bundle id}"
PROFILE="${3:?missing provisioning profile path}"

TEAM_ID="5NPJPGZ77K"
APP_CERT="Apple Distribution: Digitglance Reliance Limited (${TEAM_ID})"
INSTALLER_CERT="3rd Party Mac Developer Installer: Digitglance Reliance Limited (${TEAM_ID})"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUNDLE_DIR="${REPO_ROOT}/src-tauri/target/universal-apple-darwin/release/bundle/macos"
APP="${BUNDLE_DIR}/${APP_NAME}.app"
PKG="${BUNDLE_DIR}/${APP_NAME}.pkg"

[ -d "$APP" ] || { echo "error: no such app: $APP" >&2; exit 1; }
[ -f "$PROFILE" ] || { echo "error: no such profile: $PROFILE" >&2; exit 1; }

# The bundle id baked into the app must match the one the profile authorises,
# or Transporter rejects the upload with a signature error.
ACTUAL_ID="$(/usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' "${APP}/Contents/Info.plist")"
if [ "$ACTUAL_ID" != "$BUNDLE_ID" ]; then
  echo "error: app is ${ACTUAL_ID}, expected ${BUNDLE_ID}" >&2
  exit 1
fi

PROFILE_ID="$(security cms -D -i "$PROFILE" 2>/dev/null \
  | plutil -extract Entitlements.com\\.apple\\.application-identifier raw - 2>/dev/null)"
if [ "$PROFILE_ID" != "${TEAM_ID}.${BUNDLE_ID}" ]; then
  echo "error: profile authorises ${PROFILE_ID}, expected ${TEAM_ID}.${BUNDLE_ID}" >&2
  exit 1
fi

echo "==> embedding provisioning profile"
cp "$PROFILE" "${APP}/Contents/embedded.provisionprofile"

# Profiles downloaded via a browser carry com.apple.quarantine, and cp preserves
# extended attributes -- App Store validation rejects the upload if any file in
# the bundle has one (error 91109). Strip before signing, since this rewrites
# files the signature covers.
echo "==> stripping extended attributes"
xattr -cr "$APP"

# App Store entitlements are the shipped sandbox ones plus the identifiers tying
# the binary to this team and app record. They must stay a subset of what the
# provisioning profile grants.
ENTITLEMENTS="$(mktemp -t mas-entitlements).plist"
trap 'rm -f "$ENTITLEMENTS"' EXIT
cat > "$ENTITLEMENTS" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.app-sandbox</key>
    <true/>
    <key>com.apple.security.network.client</key>
    <true/>
    <key>com.apple.application-identifier</key>
    <string>${TEAM_ID}.${BUNDLE_ID}</string>
    <key>com.apple.developer.team-identifier</key>
    <string>${TEAM_ID}</string>
</dict>
</plist>
EOF

echo "==> re-signing (inner executable first, then the bundle)"
codesign --force --sign "$APP_CERT" --entitlements "$ENTITLEMENTS" \
  --options runtime --timestamp "${APP}/Contents/MacOS/digitglance"
codesign --force --sign "$APP_CERT" --entitlements "$ENTITLEMENTS" \
  --options runtime --timestamp "$APP"

echo "==> verifying signature"
codesign --verify --deep --strict --verbose=2 "$APP"

# Catch a stray xattr here rather than after a round trip through Transporter.
# Only these are rejected by App Store validation; com.apple.provenance is a
# system attribute the OS reapplies and cannot be stripped, so ignore it.
if xattr -lr "$APP" | grep -qE "com\.apple\.(quarantine|FinderInfo|ResourceFork)"; then
  echo "error: disallowed extended attributes remain in the bundle:" >&2
  xattr -lr "$APP" | grep -E "com\.apple\.(quarantine|FinderInfo|ResourceFork)" >&2
  exit 1
fi

echo "==> building signed installer package"
rm -f "$PKG"
productbuild --component "$APP" /Applications --sign "$INSTALLER_CERT" "$PKG"

echo
echo "ready to upload: ${PKG}"
