#!/usr/bin/env bash
# Sign a bundle prepared by bundle-macos.sh --build-unsigned. No compilation.
set -euo pipefail
# Bundle entries must remain accessible to other users after extraction.
umask 022

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "$(uname -s)" != Darwin ]]; then echo 'sign-macos.sh: macOS only' >&2; exit 1; fi
if (( $# != 1 )); then echo 'Usage: sign-macos.sh BroScribe.app' >&2; exit 2; fi
app="$1"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$(dirname "$app")}"
helper="$app/Contents/Library/LoginItems/BroScribe Agent.app"
resources="$app/Contents/Resources"
identity="${BROSCRIBE_SIGN_IDENTITY:--}"
verify=(python3 "$repo/scripts/verify-macos-artifact.py" "$app")
require_clean_build() {
    python3 - "$resources/build-info.json" <<'PYCODE'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as source:
    if json.load(source)["git"]["dirty"] is not False:
        raise SystemExit("sign-macos.sh: Developer ID signing requires a clean build")
PYCODE
}

distribution=false
if [[ "$identity" == 'Developer ID Application:'* ]]; then
    distribution=true
    require_clean_build
    verify+=(--require-complete-licences)
fi
"${verify[@]}"

work="$(mktemp -d "${TMPDIR:-/tmp}/broscribe-sign.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# The app and its login-item agent share one signature profile: macOS
# attributes the agent's permission requests to the app that owns it, so the
# app must declare every protected resource the agent uses.
entitlements="$work/entitlements.plist"
cat > "$entitlements" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>com.apple.security.device.audio-input</key><true/>
    <key>com.apple.security.personal-information.calendars</key><true/>
</dict></plist>
PLIST
timestamp=(--timestamp)
if [[ "$identity" == "-" ]]; then timestamp=(--timestamp=none); fi
sign=(codesign --force --sign "$identity" --options runtime --entitlements "$entitlements" "${timestamp[@]}")
# Inside out, no --deep signing: sign every nested executable and then each bundle.
for binary in broscribe-worker broscribe; do
    "${sign[@]}" "$app/Contents/MacOS/$binary"
done
# The update runner needs no protected resource: same signer and hardened
# runtime, no entitlements. Its signature is embedded, so the copy it runs
# from outside the app keeps it.
codesign --force --sign "$identity" --options runtime "${timestamp[@]}" \
    "$app/Contents/MacOS/broscribe-update-runner"
"${sign[@]}" "$helper"
"${sign[@]}" "$app"
codesign --verify --deep --strict --verbose=2 "$app"

# With a real identity the helper must carry the app's signer: TCC grants and
# the login-item relationship follow the designated requirement, which then
# differs only by bundle identifier. (Ad-hoc requirements are per-code hashes.)
requirement_identity() {
    codesign --display --requirements - "$1" 2>/dev/null \
        | sed -n 's/^designated => //p' | sed -E 's/identifier "[^"]*"( and)?//'
}
if [[ "$identity" != "-" ]]; then
    app_signer="$(requirement_identity "$app")"
    if [[ -z "$app_signer" || "$app_signer" != "$(requirement_identity "$helper")" ]]; then
        echo "sign-macos.sh: the helper's designated requirement names another signer than the app's" >&2
        exit 1
    fi
    signature="$(codesign --display --verbose=2 "$app" 2>&1)"
    if grep -q '^Authority=Developer ID Application:' <<<"$signature"; then
        # codesign also accepts selectors such as a certificate hash.
        if [[ "$distribution" == false ]]; then
            require_clean_build
            verify+=(--require-complete-licences)
        fi
    elif [[ "$distribution" == true ]]; then
        echo "sign-macos.sh: expected a Developer ID Application signature" >&2
        exit 1
    fi
fi
# Verify the metadata added by codesign as well.
"${verify[@]}"
inventory_state="complete"
if ! grep -q '"complete": true' "$resources/third-party-inventory.json"; then
    inventory_state="INCOMPLETE (local use only)"
fi
echo "$app (signed with '${identity}', licence inventory $inventory_state)"
