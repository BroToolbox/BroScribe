#!/usr/bin/env bash
# Packs the stapled BroScribe.app into the drag-to-Applications disk image
# macOS users install from. scripts/notarize-macos.sh runs it after stapling
# the app and writing the ZIP, before the provenance; run by hand it needs the
# same inputs. Only scripts/notarize-macos.sh produces a consistent release
# set (ZIP, DMG, both checksums and the one provenance describing both): run
# alone, this script removes any dist/<name>.provenance.json, which would no
# longer describe the new disk image, and writes no provenance itself.
#
#   BROSCRIBE_SIGN_IDENTITY='Developer ID Application: …' \
#       scripts/package-macos-dmg.sh KEYCHAIN_PROFILE [target/BroScribe.app]
#   scripts/package-macos-dmg.sh --unsigned [target/BroScribe.app]
#
# Distribution (KEYCHAIN_PROFILE): the app must be Developer ID signed and
# stapled, and dist/<name>.zip (notarize-macos.sh) must hold it. The image is
# signed (no hardened runtime, entitlements or --deep: a disk image has no
# code of its own), notarized, stapled and only then checksummed and checked
# with `verify-macos-artifact.py --dmg … --compare-zip dist/<name>.zip
# --codesign --require-stapled`. Only then are these moved into
# $CARGO_TARGET_DIR/dist/:
#   BroScribe-<version>-<build>-macos-arm64.dmg
#   BroScribe-<version>-<build>-macos-arm64.dmg.sha256   shasum -a 256 format
# and the signer and notarization are written as JSON to
# BROSCRIBE_DMG_REPORT (default $CARGO_TARGET_DIR/dmg-report.json) for
# `release-metadata.py provenance --disk-image` (the report is in place before
# the DMG pair enters dist). On rejection the Apple log,
# redacted as for the app, is kept as dist/notarization-dmg-<submission-id>.log
# and the script fails. The update manifest never names the disk image: the
# updater installs the ZIP.
#
# --unsigned (development and tests only, never published): the same image
# from any local app, unsigned and not notarized, checked against a ZIP of
# the same app, in $CARGO_TARGET_DIR/dmg-unsigned/ as
# BroScribe-<version>-<build>-macos-arm64-unsigned.{dmg,dmg.sha256,zip}.
# An ad-hoc or Apple Development app never gets a distributable disk image.
#
# The image: an APFS volume named BroScribe in a ULFO (lzfse) UDIF image,
# holding only BroScribe.app and an Applications link to /Applications; no
# Finder layout, background, licence or hidden files. The app is copied
# without resource forks, extended attributes, quarantine or ACLs.
#
# Environment:
#   BROSCRIBE_SIGN_IDENTITY    Developer ID Application identity (distribution)
#   BROSCRIBE_NOTARY_KEYCHAIN  keychain holding the notarytool profile
#   BROSCRIBE_NOTARY_TIMEOUT   notarytool --wait limit (default 45m); the whole
#                              submission is stopped after that plus a short
#                              upload grace (common.sh notary_wall_seconds)
#   BROSCRIBE_DMG_REPORT       where the distribution report is written
#   CARGO_TARGET_DIR           Cargo's target folder (default target/)
# No credentials are read from the repo or placed in arguments by this script.
set -euo pipefail
# Bundle entries must remain accessible to other users after extraction.
umask 022
if [[ "$(uname -s)" != Darwin ]]; then echo 'package-macos-dmg.sh: macOS only' >&2; exit 1; fi
usage='Usage: package-macos-dmg.sh KEYCHAIN_PROFILE|--unsigned [BroScribe.app]'
if (( $# < 1 || $# > 2 )) || [[ -z "$1" ]]; then echo "$usage" >&2; exit 2; fi
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=common.sh
source "$repo/scripts/common.sh"
target_dir="${CARGO_TARGET_DIR:-$repo/target}"
export CARGO_TARGET_DIR="$target_dir"
app="${2:-$target_dir/BroScribe.app}"
dist="$target_dir/dist"
if [[ ! -d "$app" || -L "$app" ]]; then echo "Missing app bundle: $app" >&2; exit 1; fi
notary_timeout="$(checked_notary_timeout)"
notary_wall="$(notary_wall_seconds "$notary_timeout")"

if [[ "$1" == --unsigned ]]; then
    unsigned=true
else
    unsigned=false
    profile="$1"
    notary_auth=(--keychain-profile "$profile")
    if [[ -n "${BROSCRIBE_NOTARY_KEYCHAIN:-}" ]]; then
        notary_auth+=(--keychain "$BROSCRIBE_NOTARY_KEYCHAIN")
    fi
    identity="${BROSCRIBE_SIGN_IDENTITY:-}"
    if [[ "$identity" != 'Developer ID Application:'* ]]; then
        echo 'A distributable disk image needs BROSCRIBE_SIGN_IDENTITY set to a Developer ID Application identity; use --unsigned for a local test image.' >&2
        exit 1
    fi
    report="${BROSCRIBE_DMG_REPORT:-$target_dir/dmg-report.json}"
    if ! grep -q '^Authority=Developer ID Application:' <<<"$(codesign --display --verbose=2 "$app" 2>&1)"; then
        echo 'The app is not Developer ID signed; a distributable disk image holds only a notarized app.' >&2
        exit 1
    fi
    xcrun stapler validate "$app" >/dev/null
fi

plist="$app/Contents/Info.plist"
version="$(plutil -extract CFBundleShortVersionString raw -o - "$plist")"
build="$(plutil -extract CFBundleVersion raw -o - "$plist")"
name="BroScribe-${version}-${build}-macos-arm64"

work="$(mktemp -d "${TMPDIR:-/tmp}/broscribe-dmg.XXXXXX")"
trap 'rm -rf "$work"' EXIT
out="$work/out"
mkdir "$out"
if [[ "$unsigned" == true ]]; then
    name="$name-unsigned"
    zip="$out/$name.zip"
    ditto -c -k --norsrc --noextattr --noacl --keepParent "$app" "$zip"
else
    zip="$dist/$name.zip"
    if [[ ! -f "$zip" ]]; then echo "Missing $zip; scripts/notarize-macos.sh writes it first." >&2; exit 1; fi
fi
dmg="$out/$name.dmg"

stage="$work/stage"
mkdir "$stage"
ditto --norsrc --noextattr --noqtn --noacl "$app" "$stage/BroScribe.app"
ln -s /Applications "$stage/Applications"
# "Resource busy" is transient (Spotlight or another image still holding the
# device): retried with a growing pause, 5 + 10 + 15 + 20 s at most.
create_attempts=5
for (( attempt = 1; ; attempt++ )); do
    rm -f "$dmg"
    if bounded 900 hdiutil create -quiet -volname BroScribe -srcfolder "$stage" -fs APFS -format ULFO \
            "$dmg" 2>"$work/hdiutil.err"; then
        break
    fi
    cat "$work/hdiutil.err" >&2
    if (( attempt == create_attempts )) || ! grep -qi 'resource busy' "$work/hdiutil.err"; then
        echo "package-macos-dmg.sh: hdiutil could not create the disk image (attempt $attempt of $create_attempts)" >&2
        exit 1
    fi
    echo "hdiutil: resource busy; retrying in $((attempt * 5)) s" >&2
    sleep $((attempt * 5))
done

verify=(python3 "$repo/scripts/verify-macos-artifact.py" --dmg "$dmg" --compare-zip "$zip")
if [[ "$unsigned" == true ]]; then
    (cd "$out" && shasum -a 256 "$name.dmg" >"$name.dmg.sha256")
    "${verify[@]}"
    local_dir="$target_dir/dmg-unsigned"
    mkdir -p "$local_dir"
    rm -f "$local_dir/$name.dmg" "$local_dir/$name.dmg.sha256" "$local_dir/$name.zip"
    mv "$zip" "$dmg" "$out/$name.dmg.sha256" "$local_dir/"
    echo "Unsigned test disk image (never publish): $local_dir/$name.dmg"
    echo "SHA-256: $(cut -d ' ' -f 1 "$local_dir/$name.dmg.sha256")"
    exit 0
fi

codesign --force --sign "$identity" --timestamp "$dmg"
codesign --verify --strict --verbose=2 "$dmg"

set +e
bounded "$notary_wall" xcrun notarytool submit "$dmg" "${notary_auth[@]}" --wait --timeout "$notary_timeout" \
    --output-format json >"$work/submit.json"
submit_status=$?
set -e
read_field() {
    python3 -c 'import json, sys
try:
    print(json.load(open(sys.argv[1])).get(sys.argv[2], ""))
except (OSError, ValueError):
    print("")' "$work/submit.json" "$1"
}
submission="$(read_field id)"
status="$(read_field status)"
if [[ "$status" != Accepted ]]; then
    echo "disk image notarization ${status:-failed} (notarytool exit ${submit_status})" >&2
    if [[ "$submission" =~ ^[0-9A-Fa-f-]{36}$ ]]; then
        if xcrun notarytool log "$submission" "${notary_auth[@]}" "$work/notary-log.json" >/dev/null; then
            redact=(--redact "$HOME" --redact "$repo" --redact "$target_dir")
            if [[ -n "${USER:-}" ]]; then redact+=(--redact "$USER"); fi
            mkdir -p "$dist"
            log="$dist/notarization-dmg-${submission}.log"
            python3 "$repo/scripts/release-metadata.py" notary-log \
                --in "$work/notary-log.json" --out "$log" "${redact[@]}"
            echo "sanitized notarization log: $log" >&2
        else
            echo "could not retrieve the notarization log for submission $submission" >&2
        fi
    else
        echo "notarytool returned no submission id; nothing was accepted" >&2
    fi
    exit 1
fi

xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"
# Checksummed only once stapled: stapling changes the file.
(cd "$out" && shasum -a 256 "$name.dmg" >"$name.dmg.sha256")
"${verify[@]}" --require-complete-licences --codesign --require-stapled

codesign --display --verbose=2 "$dmg" >"$work/codesign.txt" 2>&1
python3 - "$work/codesign.txt" "$submission" >"$work/report.json" <<'PY'
import json
import sys

lines = open(sys.argv[1]).read().splitlines()
json.dump({
    "format": "ULFO",
    "filesystem": "APFS",
    "signing": {
        "authorities": [line.split("=", 1)[1] for line in lines if line.startswith("Authority=")],
        "team_identifier": next((line.split("=", 1)[1] for line in lines if line.startswith("TeamIdentifier=")), None),
    },
    "notarization": {"submission_id": sys.argv[2], "status": "Accepted", "stapled": True},
}, sys.stdout, indent=2, sort_keys=True)
PY
# The report first, so a DMG pair in dist always has its report; a stale
# provenance (from an earlier run) never sits beside the new image.
mkdir -p "$(dirname "$report")" "$dist"
mv "$work/report.json" "$report"
rm -f "$dist/$name.dmg" "$dist/$name.dmg.sha256" "$dist/$name.provenance.json"
# The checksum before the image: a DMG in dist always has its sidecar.
mv "$out/$name.dmg.sha256" "$dist/$name.dmg.sha256"
mv "$dmg" "$dist/$name.dmg"
echo "Disk image: $dist/$name.dmg"
echo "SHA-256:    $(cut -d ' ' -f 1 "$dist/$name.dmg.sha256")"
