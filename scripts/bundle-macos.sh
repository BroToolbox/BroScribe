#!/usr/bin/env bash
# Builds BroScribe.app and its signed capture-agent helper for Apple silicon.
# Ad-hoc signing works locally; set BROSCRIBE_SIGN_IDENTITY to a stable Apple
# Development identity for local TCC grants, or Developer ID Application for
# distribution. Notarization: scripts/notarize-macos.sh (requires Developer ID
# credentials).
#
#   scripts/bundle-macos.sh                   # → signed target/BroScribe.app
#   scripts/bundle-macos.sh --build-unsigned  # → sign-macos.sh can sign it later
#
# Environment:
#   BROSCRIBE_SIGN_IDENTITY  codesign identity (default '-', ad hoc)
#   BROSCRIBE_BUILD_NUMBER   CFBundleVersion; an integer from 1 to 999999999
#                            (at most 9 digits, e.g. 202611171) that must grow
#                            with every distributed build (default: the number
#                            of commits in HEAD's history)
#   CARGO_TARGET_DIR         Cargo's target folder (default target/)
#   MACOSX_DEPLOYMENT_TARGET unset or 14.2, the LSMinimumSystemVersion both
#                            bundles declare; Rust, C and C++ all build for it
#   CFLAGS_aarch64_apple_darwin, CXXFLAGS_aarch64_apple_darwin
#                            kept; the builder-path maps are appended to them
#
# The build always names its target (aarch64-apple-darwin), so a
# CARGO_BUILD_TARGET or a host-target build can never be packaged by mistake.
# Builder paths are remapped out of the executables and their dSYMs: rustc gets
# --remap-path-prefix and the C, C++ and assembly that cc and CMake build
# (whisper.cpp, SQLite, ring, …) get -ffile-prefix-map, so HOME, CARGO_HOME
# (registry sources), the repository and the target folder must be plain
# absolute paths (letters, digits and . _ / + @ , -). The executables are
# stripped of debug symbols; the matching dSYMs are archived under
# dist/symbols/. The app ships LICENSE, NOTICE, THIRD_PARTY_NOTICES.txt and
# third-party-inventory.json for exactly the crates and native libraries linked
# for this target.
set -euo pipefail
# Bundle entries must remain accessible to other users after extraction.
umask 022

if (( $# == 0 )); then
    build_unsigned=false
elif (( $# == 1 )) && [[ "$1" == --build-unsigned ]]; then
    build_unsigned=true
else
    echo 'Usage: bundle-macos.sh [--build-unsigned]' >&2
    exit 2
fi

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -L)"
cd "$repo"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "bundle-macos.sh: macOS only" >&2
    exit 1
fi

# shellcheck source=../third_party/sherpa-onnx/pins.sh
source "$repo/third_party/sherpa-onnx/pins.sh"
# shellcheck source=common.sh
source "$repo/scripts/common.sh"

triple="aarch64-apple-darwin"
macos_min="14.2"
sherpa_target="$(sherpa_target_for_triple "$triple")"
target_input="${CARGO_TARGET_DIR:-$repo/target}"
if [[ "$target_input" != /* ]]; then
    target_input="$repo/$target_input"
fi
mkdir -p "$target_input"
target_dir="$(cd "$target_input" && pwd -P)"
export CARGO_TARGET_DIR="$target_input"
app="$target_dir/BroScribe.app"
out="$target_dir/$triple/release"
dist="$target_dir/dist"

require_cargo_1_100
sherpa_check_lock "$repo/Cargo.lock"

# The native libraries must come from the pinned osx-arm64 archive: lib/ when
# fetch.sh ran on this Mac, otherwise lib-osx-arm64/.
sherpa_lib=""
for candidate in "$SHERPA_DIR/lib" "$SHERPA_DIR/lib-$sherpa_target"; do
    if sherpa_lib_verified "$candidate" "$sherpa_target"; then
        sherpa_lib="$candidate"
        break
    fi
done
if [[ -z "$sherpa_lib" ]]; then
    echo "bundle-macos.sh: no verified sherpa-onnx ${SHERPA_VERSION} ${sherpa_target} libraries" >&2
    echo "run: third_party/sherpa-onnx/fetch.sh" >&2
    exit 1
fi
sherpa_archive_name="$(sherpa_archive "$sherpa_target")"
sherpa_archive_sha="$(sherpa_sha256 "$sherpa_archive_name")"

if [[ -n "${RUSTFLAGS:-}${CARGO_ENCODED_RUSTFLAGS:-}" ]]; then
    echo "bundle-macos.sh: unset RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS; release flags are set here" >&2
    exit 1
fi

git_commit="$(git rev-parse --verify HEAD)"
git_dirty="$(git_tree_dirty "$repo")" || {
    echo "bundle-macos.sh: cannot tell whether the working tree matches $git_commit" >&2
    exit 1
}
build_number="${BROSCRIBE_BUILD_NUMBER:-$(git rev-list --count HEAD)}"
if [[ ! "$build_number" =~ ^[1-9][0-9]{0,8}$ ]]; then
    echo "bundle-macos.sh: build number must be an integer from 1 to 999999999 (at most 9 digits), got '$build_number'" >&2
    exit 1
fi
copyright="$(grep -m 1 '^Copyright ' "$repo/NOTICE")" || {
    echo "bundle-macos.sh: NOTICE has no Copyright line" >&2
    exit 1
}

# Use the same roots as the verifiers. rustc and native compilers may record
# either spelling (for example /tmp or /private/tmp).
remaps="$(python3 - "$repo" <<'PYTHON'
import re
import sys
from pathlib import Path

repo = Path(sys.argv[1])
sys.path.insert(0, str(repo / "scripts"))
from desktop_artifacts import build_roots, build_root_spellings

destinations = {"__build_cargo__": "cargo", "__build_repo__": "broscribe", "__build_target__": "target"}
for label, path in build_root_spellings(build_roots(repo_dir=repo)):
    # cc and CMake need paths safe to pass as unquoted flags.
    if not re.fullmatch(r"/[A-Za-z0-9._/+@,-]+", str(path)):
        sys.exit(f"bundle-macos.sh: cannot remap {str(path)!r}; use paths with only letters, digits and . _ / + @ , -")
    print(f"{path}=/{destinations.get(label, label)}")
PYTHON
)"
rustflags=("-C" "link-arg=-mmacosx-version-min=$macos_min")
prefix_maps=""
while IFS= read -r remap; do
    rustflags+=("--remap-path-prefix=$remap")
    prefix_maps+=" -ffile-prefix-map=$remap"
done <<< "$remaps"
CARGO_ENCODED_RUSTFLAGS="$(IFS=$'\x1f'; printf '%s' "${rustflags[*]}")"
export CARGO_ENCODED_RUSTFLAGS
# cc reads TARGET_CFLAGS only when cross compiling (HOST_CFLAGS when the build
# machine is itself aarch64-apple-darwin), but the exact-target variables in
# both cases, after CFLAGS and HOST_/TARGET_CFLAGS. The cmake crate hands the
# same flags to CMake (CMAKE_C_FLAGS, CMAKE_CXX_FLAGS, CMAKE_ASM_FLAGS). The
# maps follow the builder's own flags, so they decide how paths are written.
export CFLAGS_aarch64_apple_darwin="${CFLAGS_aarch64_apple_darwin:+$CFLAGS_aarch64_apple_darwin }${prefix_maps# }"
export CXXFLAGS_aarch64_apple_darwin="${CXXFLAGS_aarch64_apple_darwin:+$CXXFLAGS_aarch64_apple_darwin }${prefix_maps# }"
# One minimum macOS for everything: the LSMinimumSystemVersion of both
# Info.plists, rustc's objects and links, and cc's and CMake's
# -mmacosx-version-min (unset, cc would use the SDK's version, above what the
# bundle declares).
if [[ -n "${MACOSX_DEPLOYMENT_TARGET:-}" && "$MACOSX_DEPLOYMENT_TARGET" != "$macos_min" ]]; then
    echo "bundle-macos.sh: MACOSX_DEPLOYMENT_TARGET=$MACOSX_DEPLOYMENT_TARGET differs from the bundle's minimum macOS $macos_min; unset it" >&2
    exit 1
fi
export MACOSX_DEPLOYMENT_TARGET="$macos_min"
export SHERPA_ONNX_LIB_DIR="$sherpa_lib"

# whisper-rs-sys builds whisper.cpp through the cmake crate, which keeps the C
# flags and MACOSX_DEPLOYMENT_TARGET out of Cargo's view: its build script
# reruns only when its headers, a few bindgen variables or the Rust flags
# change, so a cached build would ship the paths and minimum of an earlier one.
# Invalidate only when those effective native settings changed. A successful
# build records a cache hint; an interrupted build never marks stale objects
# current. `cargo clean -p` misses Cargo 1.100's build/<package>/<hash> layout.
native_flags="$(env | LC_ALL=C sort | grep -E '^(CFLAGS|CXXFLAGS|HOST_CFLAGS|HOST_CXXFLAGS|TARGET_CFLAGS|TARGET_CXXFLAGS|CC|CXX|AR|CMAKE|SDKROOT|DEVELOPER_DIR|MACOSX_DEPLOYMENT_TARGET)(=|_|-)' | shasum -a 256 | awk '{print $1}')"
native_stamp="$out/.broscribe-native-build-flags"
if [[ ! -f "$native_stamp" ]] || [[ "$(cat "$native_stamp")" != "$native_flags" ]]; then
    rm -rf "$target_dir/$triple/release/build/whisper-rs-sys" \
        "$target_dir/$triple/release/build/whisper-rs-sys-"*
fi

binaries=(broscribe-app broscribe broscribe-worker broscribe-update-runner broscribed)
cargo build --release --locked --target "$triple" \
    -p broscribe-app -p broscribe-cli -p broscribed -p broscribe-worker -p broscribe-update-runner
printf '%s\n' "$native_flags" > "$native_stamp"

# `cargo pkgid` prints e.g. `path+file:///…/crates/broscribe-app#0.1.0`
# (or `…#broscribe-app@0.1.0`); the version follows the last `#` or `@`.
pkgid="$(cargo pkgid -p broscribe-app)"
version="${pkgid##*[#@]}"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "bundle-macos.sh: cannot read the version from '$pkgid'" >&2
    exit 1
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/broscribe-bundle.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# rustc does not tell Cargo that it read MACOSX_DEPLOYMENT_TARGET, so check
# that every link used the bundle's minimum.
for binary in "${binaries[@]}"; do
    if [[ "$(lipo -archs "$out/$binary")" != arm64 ]]; then
        echo "bundle-macos.sh: $out/$binary is not an arm64 executable" >&2
        exit 1
    fi
    minos="$(vtool -show-build "$out/$binary" | awk '$1 == "minos" { print $2 }')"
    if [[ "$minos" != "$macos_min" ]]; then
        echo "bundle-macos.sh: $out/$binary requires macOS '$minos', not $macos_min" >&2
        exit 1
    fi
done

cargo metadata --locked --offline --format-version 1 --filter-platform "$triple" >"$work/metadata.json"
python3 scripts/third-party-notices.py --target "$triple" --out-dir "$work/notices" \
    --metadata "$work/metadata.json" --native-lib-dir "$sherpa_lib" --allow-incomplete
python3 scripts/release-metadata.py icon --out "$work/BroScribe.iconset"
python3 scripts/release-metadata.py build-info --out "$work/build-info.json" \
    --version "$version" --build "$build_number" --target "$triple" --profile release \
    --git-commit "$git_commit" --git-dirty "$git_dirty" \
    --rustc "$(rustc --version)" --cargo "$(cargo --version)" --cargo-lock "$repo/Cargo.lock" \
    --sherpa-archive "$sherpa_archive_name" --sherpa-sha256 "$sherpa_archive_sha" \
    --copyright "$copyright"

rm -rf "$app"
helper="$app/Contents/Library/LoginItems/BroScribe Agent.app"
resources="$app/Contents/Resources"
mkdir -p "$app/Contents/MacOS" "$resources" \
    "$app/Contents/Library/LaunchAgents" "$helper/Contents/MacOS"
# The capture daemon ships only inside the login-item helper: launchd starts it
# there (BundleProgram below), development installs resolve the helper path,
# and the helper finds the worker and app in the outer Contents/MacOS.
# `broscribe update` copies broscribe-update-runner from beside itself to a
# folder outside the app and runs it from there while BroScribe is replaced.
for binary in broscribe-app broscribe-worker broscribe broscribe-update-runner; do
    cp "$out/$binary" "$app/Contents/MacOS/$binary"
done
cp "$out/broscribed" "$helper/Contents/MacOS/broscribed"
for executable in "$app/Contents/MacOS/"* "$helper/Contents/MacOS/broscribed"; do
    strip -S "$executable"
done
cp "$repo/LICENSE" "$repo/NOTICE" "$resources/"
cp "$work/notices/THIRD_PARTY_NOTICES.txt" "$work/notices/third-party-inventory.json" \
    "$work/build-info.json" "$resources/"
iconutil -c icns -o "$resources/BroScribe.icns" "$work/BroScribe.iconset"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>BroScribe</string>
    <key>CFBundleExecutable</key>
    <string>broscribe-app</string>
    <key>CFBundleIconFile</key>
    <string>BroScribe</string>
    <key>CFBundleIdentifier</key>
    <string>org.broscribe.BroScribe</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>BroScribe</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${version}</string>
    <key>CFBundleVersion</key>
    <string>${build_number}</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.productivity</string>
    <key>LSMinimumSystemVersion</key>
    <string>${macos_min}</string>
    <key>LSUIElement</key>
    <false/>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>${copyright}</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>BroScribe records your side of meetings and your voice sample. Audio stays on this Mac.</string>
    <key>NSAudioCaptureUsageDescription</key>
    <string>BroScribe records the other side of meetings using system audio. Audio stays on this Mac.</string>
    <key>NSCalendarsFullAccessUsageDescription</key>
    <string>BroScribe reads only the calendars you select, to name meetings and participants. It never changes events.</string>
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist" >/dev/null

cat > "$helper/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>CFBundleExecutable</key><string>broscribed</string>
    <key>CFBundleIdentifier</key><string>org.broscribe.Agent</string>
    <key>CFBundleDisplayName</key><string>BroScribe Agent</string>
    <key>CFBundleName</key><string>BroScribe Agent</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${version}</string>
    <key>CFBundleVersion</key><string>${build_number}</string>
    <key>LSMinimumSystemVersion</key><string>${macos_min}</string>
    <key>LSUIElement</key><true/>
    <key>NSHumanReadableCopyright</key><string>${copyright}</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>BroScribe records your side of meetings and your voice sample. Audio stays on this Mac.</string>
    <key>NSAudioCaptureUsageDescription</key>
    <string>BroScribe records the other side of meetings using system audio. Audio stays on this Mac.</string>
    <key>NSCalendarsFullAccessUsageDescription</key>
    <string>BroScribe reads only the calendars you select, to name meetings and participants. It never changes events.</string>
</dict></plist>
PLIST
cat > "$app/Contents/Library/LaunchAgents/org.broscribe.broscribed.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>Label</key><string>org.broscribe.broscribed</string>
    <key>BundleProgram</key>
    <string>Contents/Library/LoginItems/BroScribe Agent.app/Contents/MacOS/broscribed</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Interactive</string>
</dict></plist>
PLIST
plutil -lint "$helper/Contents/Info.plist" >/dev/null
plutil -lint "$app/Contents/Library/LaunchAgents/org.broscribe.broscribed.plist" >/dev/null
# Copies can inherit modes from a build made with a restrictive umask.
chmod -R a+rX,go-w "$app"

if ! python3 "$repo/scripts/verify-macos-artifact.py" "$app"; then
    rm -rf "$app"
    echo "bundle-macos.sh: the bundle failed verification and was removed" >&2
    exit 1
fi

# Debug symbols stay out of the app but are kept for symbolicating reports.
symbols="$dist/symbols/BroScribe-${version}-${build_number}-macos-arm64"
rm -rf "$symbols" "$symbols.zip"
mkdir -p "$symbols"
symbols_ok=false
trap 'rm -rf "$work"; if [[ "$symbols_ok" != true ]]; then rm -rf "$symbols" "$symbols.zip"; fi' EXIT
for binary in "${binaries[@]}"; do
    if [[ ! -d "$out/$binary.dSYM" ]]; then
        echo "bundle-macos.sh: missing $out/$binary.dSYM (profile.release split-debuginfo)" >&2
        exit 1
    fi
    cp -RL "$out/$binary.dSYM" "$symbols/"
    # dsymutil's input maps contain absolute linker paths, not symbolication
    # data for these executables. DWARF and UUIDs remain unchanged.
    rm -rf "$symbols/$binary.dSYM/Contents/Resources/Relocations"
    expected_uuid="$(dwarfdump --uuid "$out/$binary" | awk '{ print $2, $3 }')"
    symbol_uuid="$(dwarfdump --uuid "$symbols/$binary.dSYM" | awk '{ print $2, $3 }')"
    if [[ -z "$expected_uuid" || "$symbol_uuid" != "$expected_uuid" ]]; then
        echo "bundle-macos.sh: symbols do not match $binary" >&2
        exit 1
    fi
done
dwarfdump --uuid "$symbols"/*.dSYM | sed "s|$symbols/||" >"$symbols/UUIDS.txt"
symbol_verify=(python3 "$repo/scripts/verify-macos-artifact.py" --symbols)
"${symbol_verify[@]}" "$symbols"
ditto -c -k --norsrc --noextattr --noqtn --noacl --keepParent "$symbols" "$symbols.zip"
"${symbol_verify[@]}" "$symbols.zip"
rm -rf "$symbols"
symbols_ok=true

if [[ "$build_unsigned" == true ]]; then
    echo "$app (version $version, build $build_number, awaiting bundle signing)"
elif ! "$repo/scripts/sign-macos.sh" "$app"; then
    rm -rf "$app"
    exit 1
fi
echo "symbols: $symbols.zip"
