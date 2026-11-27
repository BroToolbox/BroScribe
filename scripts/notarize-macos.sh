#!/usr/bin/env bash
# Distribution only: requires a Developer ID Application certificate and an
# Apple notarytool keychain profile (App Store Connect/API-key credentials).
# Apple Development/ad-hoc signatures cannot be notarized.
#
# xcrun notarytool store-credentials BroScribe --key ... --key-id ... --issuer ...
# export BROSCRIBE_SIGN_IDENTITY='Developer ID Application: ...'
# scripts/bundle-macos.sh
# scripts/notarize-macos.sh BroScribe [target/BroScribe.app]
#
# BROSCRIBE_SIGN_IDENTITY (the app's Developer ID Application identity) also
# signs the disk image. On acceptance the app is stapled and only then
# archived as the downloadable artifacts, in $CARGO_TARGET_DIR/dist/:
#   BroScribe-<version>-<build>-macos-arm64.zip            stapled app (updates)
#   BroScribe-<version>-<build>-macos-arm64.zip.sha256     shasum -a 256 format
#   BroScribe-<version>-<build>-macos-arm64.dmg            first installs:
#       scripts/package-macos-dmg.sh signs, notarizes and staples it and checks
#       that it holds exactly the ZIP's app
#   BroScribe-<version>-<build>-macos-arm64.dmg.sha256
#   BroScribe-<version>-<build>-macos-arm64.provenance.json
#       version, build, target, commit, toolchain, Cargo.lock and native
#       archive checksums, licence inventory, signer and notarization result,
#       and the disk image's checksum, signer and notarization (`disk_image`)
# On rejection the Apple log, reduced to its diagnostic fields with builder
# paths, the user name and e-mail addresses redacted, is kept as
# dist/notarization-<submission-id>.log (notarization-dmg-<id>.log for the
# disk image) and the script fails. Each notarytool --wait is bounded by
# BROSCRIBE_NOTARY_TIMEOUT (default 45m), and each whole submission by that
# plus a short upload grace (common.sh notary_wall_seconds).
#
# All or nothing: from the moment the ZIP enters dist until the ZIP, the disk
# image and the provenance have passed their final verification, any failure
# (disk image packaging, signing metadata, provenance or verification)
# removes every release-named file of this build from dist (ZIP, DMG, both
# checksums, provenance, manifest, envelope). Sanitized notarization logs stay.
#
# Update envelope (SH23), only when all three are set (a partial setting
# fails before anything is submitted):
#   BROSCRIBE_UPDATE_URL_BASE      https folder the zip is published under
#   BROSCRIBE_RELEASE_KEYS         release-key labels, comma-separated, in the
#                                  platform secret store of this Mac (created
#                                  once with `broscribe update release-key
#                                  generate --label LABEL`)
#   BROSCRIBE_UPDATE_TRUSTED_KEYS  public keys installations pin
#                                  (`[updates] trusted_keys`), comma-separated
#   BROSCRIBE_UPDATE_CHANNEL       default stable
#   BROSCRIBE_UPDATE_EXPIRES_DAYS  default 30
# After every stapling, signature, provenance and artifact check passed it
# writes dist/<name>.manifest.json and dist/<name>.update-envelope.json,
# signed by the stapled app's own `broscribe update sign-manifest`; private
# keys never leave the secret store.
# BROSCRIBE_NOTARY_KEYCHAIN optionally selects the keychain holding the profile.
# No credentials are read from the repo or placed in arguments by this script.
set -euo pipefail
# Bundle entries must remain accessible to other users after extraction.
umask 022
if [[ "$(uname -s)" != Darwin ]]; then echo 'notarize-macos.sh: macOS only' >&2; exit 1; fi
profile="${1:?Usage: notarize-macos.sh KEYCHAIN_PROFILE [BroScribe.app]}"
notary_auth=(--keychain-profile "$profile")
if [[ -n "${BROSCRIBE_NOTARY_KEYCHAIN:-}" ]]; then
    notary_auth+=(--keychain "$BROSCRIBE_NOTARY_KEYCHAIN")
fi
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=common.sh
source "$repo/scripts/common.sh"
target_dir="${CARGO_TARGET_DIR:-$repo/target}"
export CARGO_TARGET_DIR="$target_dir"
app="${2:-$target_dir/BroScribe.app}"
dist="$target_dir/dist"
if [[ ! -d "$app" ]]; then echo "Missing app bundle: $app" >&2; exit 1; fi
if [[ "${BROSCRIBE_SIGN_IDENTITY:-}" != 'Developer ID Application:'* ]]; then
    echo 'Set BROSCRIBE_SIGN_IDENTITY to the Developer ID Application identity; it also signs the disk image.' >&2
    exit 1
fi
notary_timeout="$(checked_notary_timeout)"
notary_wall="$(notary_wall_seconds "$notary_timeout")"
update_url_base="${BROSCRIBE_UPDATE_URL_BASE:-}"
release_keys="${BROSCRIBE_RELEASE_KEYS:-}"
trusted_keys="${BROSCRIBE_UPDATE_TRUSTED_KEYS:-}"
update_channel="${BROSCRIBE_UPDATE_CHANNEL:-stable}"
update_expires_days="${BROSCRIBE_UPDATE_EXPIRES_DAYS:-30}"
cli="$app/Contents/MacOS/broscribe"
if [[ -n "$update_url_base$release_keys$trusted_keys" ]]; then
    if [[ -z "$update_url_base" || -z "$release_keys" || -z "$trusted_keys" ]]; then
        echo 'An update envelope needs BROSCRIBE_UPDATE_URL_BASE, BROSCRIBE_RELEASE_KEYS and BROSCRIBE_UPDATE_TRUSTED_KEYS together.' >&2
        exit 1
    fi
    if [[ "$update_url_base" != https://* ]]; then echo 'BROSCRIBE_UPDATE_URL_BASE must be https.' >&2; exit 1; fi
    update_url_base="${update_url_base%/}"
    if [[ ! -x "$cli" ]]; then echo "Missing CLI in the bundle: $cli" >&2; exit 1; fi
fi
signature="$(codesign --display --verbose=4 "$app" 2>&1)"
if ! grep -q '^Authority=Developer ID Application:' <<<"$signature"; then
    echo 'Notarization requires a Developer ID Application signature; Apple Development/ad-hoc is local-only.' >&2
    exit 1
fi
codesign --verify --deep --strict --verbose=2 "$app"

python3 "$repo/scripts/verify-macos-artifact.py" "$app" --require-complete-licences

plist="$app/Contents/Info.plist"
version="$(plutil -extract CFBundleShortVersionString raw -o - "$plist")"
build="$(plutil -extract CFBundleVersion raw -o - "$plist")"
name="BroScribe-${version}-${build}-macos-arm64"
mkdir -p "$dist"
chmod 700 "$dist"

artifact="$dist/$name.zip"
disk_image="$dist/$name.dmg"
release_files=("$artifact" "$artifact.sha256" "$disk_image" "$disk_image.sha256" "$dist/$name.provenance.json"
    "$dist/$name.manifest.json" "$dist/$name.update-envelope.json")
# true from the first move into dist until the final verification passed.
release_pending=false
work="$(mktemp -d "${TMPDIR:-/tmp}/broscribe-notarize.XXXXXX")"
cleanup() {
    if [[ "$release_pending" == true ]]; then
        rm -f "${release_files[@]}"
        echo "notarize-macos.sh failed before the artifacts were verified; removed this build's release files from $dist" >&2
    fi
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
ditto -c -k --norsrc --noextattr --noacl --keepParent "$app" "$work/submission.zip"

set +e
bounded "$notary_wall" xcrun notarytool submit "$work/submission.zip" "${notary_auth[@]}" --wait --timeout "$notary_timeout" \
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
    echo "notarization ${status:-failed} (notarytool exit ${submit_status})" >&2
    if [[ "$submission" =~ ^[0-9A-Fa-f-]{36}$ ]]; then
        if xcrun notarytool log "$submission" "${notary_auth[@]}" "$work/notary-log.json" >/dev/null; then
            redact=(--redact "$HOME" --redact "$repo" --redact "$target_dir")
            if [[ -n "${USER:-}" ]]; then redact+=(--redact "$USER"); fi
            log="$dist/notarization-${submission}.log"
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

xcrun stapler staple "$app"
xcrun stapler validate "$app"
spctl --assess --type execute --verbose=2 "$app"

# The downloadable artifact is made from the stapled app, never the submission.
# No resource forks, extended attributes or ACLs: the zip must hold only the
# app's own files (no AppleDouble ._* entries), which the updater requires.
rm -f "${release_files[@]}"
ditto -c -k --norsrc --noextattr --noacl --keepParent "$app" "$work/$name.zip"
release_pending=true
mv "$work/$name.zip" "$artifact"

# The disk image holds the same stapled app, checked against this ZIP.
if ! BROSCRIBE_DMG_REPORT="$work/disk-image.json" BROSCRIBE_NOTARY_TIMEOUT="$notary_timeout" \
        "$repo/scripts/package-macos-dmg.sh" "$profile" "$app"; then
    echo "the disk image was not produced; the ZIP is removed with it" >&2
    exit 1
fi

codesign --display --verbose=2 "$app" >"$work/codesign.txt" 2>&1
codesign --display --requirements - "$app" >"$work/requirement.txt" 2>&1
python3 - "$work/codesign.txt" "$work/requirement.txt" >"$work/signing.json" <<'PY'
import json
import sys

lines = open(sys.argv[1]).read().splitlines()
requirement = open(sys.argv[2]).read().splitlines()
json.dump({
    "authorities": [line.split("=", 1)[1] for line in lines if line.startswith("Authority=")],
    "team_identifier": next((line.split("=", 1)[1] for line in lines if line.startswith("TeamIdentifier=")), None),
    "designated_requirement": next((line[len("designated => "):] for line in requirement
                                    if line.startswith("designated => ")), ""),
}, sys.stdout)
PY

# One provenance for both artifacts, written once both exist, then each is
# verified against it.
if ! python3 "$repo/scripts/release-metadata.py" provenance --artifact "$artifact" --app "$app" \
        --signing-json "$work/signing.json" --submission-id "$submission" --notary-status "$status" \
        --dist-dir "$dist" --disk-image "$disk_image" --disk-image-json "$work/disk-image.json" \
    || ! python3 "$repo/scripts/verify-macos-artifact.py" "$artifact" \
        --provenance "$dist/$name.provenance.json" --require-complete-licences \
        --codesign --require-stapled \
    || ! python3 "$repo/scripts/verify-macos-artifact.py" --dmg "$disk_image" --compare-zip "$artifact" \
        --provenance "$dist/$name.provenance.json" --require-complete-licences \
        --codesign --require-stapled; then
    echo "the stapled artifacts failed their provenance or verification checks and are removed" >&2
    exit 1
fi
# A consistent, verified set: a later envelope failure leaves it in place.
release_pending=false

if [[ -n "$update_url_base" ]]; then
    key_args=()
    IFS=, read -r -a labels <<<"$release_keys"
    for label in "${labels[@]}"; do key_args+=(--key "$label"); done
    trusted_args=()
    IFS=, read -r -a keys <<<"$trusted_keys"
    for key in "${keys[@]}"; do trusted_args+=(--trusted-key "$key"); done
    # Only now, from the stapled and verified artifact: the manifest, then the
    # signatures by the release keys in the secret store through the bundle's
    # own CLI (which runs the updater's verifier before printing), then the
    # envelope checks against the manifest and the pinned keys.
    if ! python3 "$repo/scripts/release-metadata.py" manifest --artifact "$artifact" \
            --provenance "$dist/$name.provenance.json" --url "$update_url_base/$name.zip" \
            --channel "$update_channel" --expires-days "$update_expires_days" --out "$work/manifest.json"; then
        echo 'no update envelope: the release manifest was refused' >&2
        exit 1
    fi
    set +e
    "$cli" --json update sign-manifest "${key_args[@]}" --manifest-stdin \
        <"$work/manifest.json" >"$work/signed.json"
    sign_status=$?
    set -e
    if ! python3 "$repo/scripts/release-metadata.py" envelope --manifest "$work/manifest.json" \
            --signed "$work/signed.json" "${trusted_args[@]}" --out "$work/envelope.json" \
        || [[ $sign_status -ne 0 ]]; then
        echo "no update envelope (broscribe update sign-manifest exit ${sign_status})" >&2
        exit 1
    fi
    mv "$work/manifest.json" "$dist/$name.manifest.json"
    mv "$work/envelope.json" "$dist/$name.update-envelope.json"
fi
echo "Notarized and stapled: $app"
echo "Artifact: $artifact"
echo "SHA-256:  $(cut -d ' ' -f 1 "$artifact.sha256")"
echo "Disk image: $disk_image"
echo "SHA-256:  $(cut -d ' ' -f 1 "$disk_image.sha256")"
echo "Provenance: $dist/$name.provenance.json"
if [[ -n "$update_url_base" ]]; then
    echo "Update manifest: $dist/$name.manifest.json"
    echo "Update envelope: $dist/$name.update-envelope.json"
else
    echo 'Update envelope: not produced (BROSCRIBE_UPDATE_URL_BASE, BROSCRIBE_RELEASE_KEYS and BROSCRIBE_UPDATE_TRUSTED_KEYS unset)'
fi
