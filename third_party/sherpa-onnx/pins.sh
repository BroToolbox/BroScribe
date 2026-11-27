# Shared sherpa-onnx pins, sourced (never executed) by fetch.sh,
# scripts/build-target.sh and scripts/bundle-macos.sh so the archive names,
# checksums, byte sizes and the verified-folder marker have one definition.
#
# SHERPA_VERSION must equal the sherpa-onnx-sys version locked in Cargo.lock:
# the Rust binding and the prebuilt native archive are one pinned pair.

SHERPA_VERSION="2.0.1"
SHERPA_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Archive file name for a sherpa target (osx-arm64, linux-aarch64, linux-x64, win-x64).
sherpa_archive() {
  case "$1" in
    osx-arm64 | linux-aarch64 | linux-x64) echo "sherpa-onnx-v${SHERPA_VERSION}-$1-static-lib.tar.bz2" ;;
    win-x64) echo "sherpa-onnx-v${SHERPA_VERSION}-win-x64-static-MT-Release-lib.tar.bz2" ;;
    *) return 1 ;;
  esac
}

# Sherpa target for a Rust target triple; fails for triples without pinned libraries.
sherpa_target_for_triple() {
  case "$1" in
    aarch64-apple-darwin) echo osx-arm64 ;;
    aarch64-unknown-linux-gnu) echo linux-aarch64 ;;
    x86_64-unknown-linux-gnu) echo linux-x64 ;;
    x86_64-pc-windows-msvc | x86_64-pc-windows-gnu) echo win-x64 ;;
    *) return 1 ;;
  esac
}

# The single pinned value for an archive in a "<value>  <archive>" file.
sherpa_pinned_field() {
  awk -v archive="$2" '$2 == archive { print $1; found++ } END { exit found == 1 ? 0 : 1 }' "${SHERPA_DIR}/$1"
}

sherpa_sha256() { sherpa_pinned_field SHA256SUMS "$1"; }
sherpa_size() { sherpa_pinned_field ARCHIVE_SIZES "$1"; }

sherpa_file_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}

sherpa_file_size() {
  wc -c <"$1" | tr -d '[:space:]'
}

# Marker written into an unpacked folder: changing the version, the archive or
# its pinned checksum invalidates every folder unpacked under the old pin.
sherpa_marker() {
  local archive sha
  archive="$(sherpa_archive "$1")" || return 1
  sha="$(sherpa_sha256 "$archive")" || return 1
  printf 'version=%s\narchive=%s\nsha256=%s\n' "$SHERPA_VERSION" "$archive" "$sha"
}

# True when <dir> holds libraries unpacked from the current pin for <target>.
sherpa_lib_verified() {
  local expected
  expected="$(sherpa_marker "$2")" || return 1
  [[ -f "$1/.verified" ]] && [[ "$(cat "$1/.verified")" == "$expected" ]]
}

# Fails unless Cargo.lock locks exactly one sherpa-onnx-sys, at SHERPA_VERSION.
sherpa_check_lock() {
  local lock="$1" locked
  if [[ ! -f "$lock" ]]; then
    echo "missing ${lock}: cannot check the sherpa-onnx binding pin" >&2
    return 1
  fi
  locked="$(awk '
    $0 == "name = \"sherpa-onnx-sys\"" { want = 1; next }
    want && $1 == "version" { gsub(/"/, "", $3); print $3; want = 0 }
  ' "$lock")"
  if [[ "$locked" != "$SHERPA_VERSION" ]]; then
    echo "Cargo.lock locks sherpa-onnx-sys '${locked//$'\n'/, }' but the native archive pin is ${SHERPA_VERSION}:" >&2
    echo "update SHERPA_VERSION, SHA256SUMS and ARCHIVE_SIZES together with the binding" >&2
    return 1
  fi
}
