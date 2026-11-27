#!/usr/bin/env bash
# Fetch the prebuilt sherpa-onnx native libraries that sherpa-onnx-sys links against,
# verify them against SHA256SUMS and ARCHIVE_SIZES, and unpack them (design §13).
# The build never downloads unchecked code: SHERPA_ONNX_LIB_DIR points at the
# unpacked folder.
#
#   fetch.sh                      host libraries into ./lib (.cargo/config.toml points here)
#   fetch.sh --target <target>    another target's libraries into ./lib-<target>
#                                 (scripts/build-target.sh sets SHERPA_ONNX_LIB_DIR to it)
#
# Targets and archives (static libraries, the link mode sherpa-onnx-sys uses by default):
#   osx-arm64      macOS arm64, Apple clang / libc++
#   linux-aarch64  Linux arm64, GCC / libstdc++ (glibc)
#   linux-x64      Linux x86-64, GCC / libstdc++ (glibc)
#   win-x64        Windows x86-64, MSVC static CRT (/MT, Release): links with the
#                  *-pc-windows-msvc Rust targets only, not with *-windows-gnu
#
# The pins live in pins.sh, SHA256SUMS and ARCHIVE_SIZES; the native version must
# equal the sherpa-onnx-sys version in Cargo.lock. A download is HTTPS-only, may
# not exceed the pinned byte size, and is abandoned after
# BROSCRIBE_FETCH_MAX_SECONDS (default 600) in total; an interrupted or failed
# download leaves no partial file behind. Each unpacked folder records the
# version, archive and checksum it came from (.verified), so changing any pin
# refetches instead of reusing a stale folder.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=pins.sh
source "${here}/pins.sh"

usage() {
  echo "usage: $0 [--target osx-arm64|linux-aarch64|linux-x64|win-x64]" >&2
  exit 2
}

host_target() {
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) echo osx-arm64 ;;
    Linux-aarch64 | Linux-arm64) echo linux-aarch64 ;;
    Linux-x86_64) echo linux-x64 ;;
    MINGW*-x86_64 | MSYS*-x86_64 | CYGWIN*-x86_64) echo win-x64 ;;
    *) echo "unsupported host $(uname -s)-$(uname -m); pass --target" >&2; exit 1 ;;
  esac
}

target=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target) [[ $# -ge 2 ]] || usage; target="$2"; shift 2 ;;
    --target=*) target="${1#--target=}"; shift ;;
    -h | --help) usage ;;
    *) usage ;;
  esac
done

if [[ -z "$target" ]]; then
  target="$(host_target)"
  dest="lib"
else
  dest="lib-${target}"
fi

ARCHIVE="$(sherpa_archive "$target")" || usage
SHA256="$(sherpa_sha256 "$ARCHIVE")" || { echo "no single SHA256SUMS entry for ${ARCHIVE}" >&2; exit 1; }
SIZE="$(sherpa_size "$ARCHIVE")" || { echo "no single ARCHIVE_SIZES entry for ${ARCHIVE}" >&2; exit 1; }
URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/v${SHERPA_VERSION}/${ARCHIVE}"
max_seconds="${BROSCRIBE_FETCH_MAX_SECONDS:-600}"
if [[ ! "$max_seconds" =~ ^[1-9][0-9]*$ ]]; then
  echo "BROSCRIBE_FETCH_MAX_SECONDS must be a positive number of seconds" >&2
  exit 2
fi

sherpa_check_lock "${here}/../../Cargo.lock"

cd "$here"

# The host's libraries go where .cargo/config.toml points (lib/) on every host;
# explicit targets always use lib-<target>.
if sherpa_lib_verified "$dest" "$target"; then
  echo "sherpa-onnx ${SHERPA_VERSION} ${target} libraries already in place: ${here}/${dest}"
  exit 0
fi

part=""
work=""
cleanup() {
  [[ -n "$part" ]] && rm -f "$part"
  [[ -n "$work" ]] && rm -rf "$work"
  return 0
}
trap cleanup EXIT
trap 'echo "fetch.sh: interrupted" >&2; exit 130' INT TERM

archive_matches() {
  [[ -f "$1" ]] && [[ "$(sherpa_file_size "$1")" == "$SIZE" ]] && [[ "$(sherpa_file_sha256 "$1")" == "$SHA256" ]]
}

if [[ -f "$ARCHIVE" ]] && ! archive_matches "$ARCHIVE"; then
  echo "cached ${ARCHIVE} does not match its pinned size and checksum; downloading it again" >&2
  rm -f "$ARCHIVE"
fi

if [[ ! -f "$ARCHIVE" ]]; then
  part="$(mktemp "${ARCHIVE}.part.XXXXXX")"
  curl --fail --location --silent --show-error \
    --proto '=https' --proto-redir '=https' --tlsv1.2 \
    --connect-timeout 30 --max-time "$max_seconds" --max-filesize "$SIZE" \
    --output "$part" "$URL"
  if ! archive_matches "$part"; then
    echo "downloaded ${ARCHIVE} does not match its pinned size (${SIZE} bytes) and SHA-256; discarded" >&2
    exit 1
  fi
  mv "$part" "$ARCHIVE"
  part=""
fi

# Unpack beside the destination: the final mv is then a rename on the same
# volume, not a copy of about 1 GB from the system temp drive (Windows CI).
work="$(mktemp -d "${here}/unpack.XXXXXX")"
tar -xjf "$ARCHIVE" -C "$work"
src="$(find "$work" -mindepth 1 -maxdepth 2 -type d -name lib | head -n 1)"
[[ -n "$src" ]] || { echo "no lib/ folder in ${ARCHIVE}" >&2; exit 1; }

rm -rf "$dest"
mv "$src" "$dest"
# Downloaded binaries never belong in the repository.
printf '*\n' >"$dest/.gitignore"
sherpa_marker "$target" >"$dest/.verified"
echo "sherpa-onnx ${SHERPA_VERSION} ${target} libraries verified and unpacked: ${here}/${dest}"
