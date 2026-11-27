#!/usr/bin/env bash
# Build (or check, test, clippy…) for one Rust target with that target's
# sherpa-onnx native libraries:
#
#   scripts/build-target.sh <rust-target> [cargo-subcommand [args…]]
#   scripts/build-target.sh x86_64-unknown-linux-gnu test -p broscribe-core
#   scripts/build-target.sh x86_64-pc-windows-gnu clippy -p broscribe-platform-windows
#
# Sets SHERPA_ONNX_LIB_DIR=third_party/sherpa-onnx/lib-<target> and
# fails, never downloads, when that folder is missing or was unpacked from
# another archive, version or checksum: run
# `third_party/sherpa-onnx/fetch.sh --target <target>` first (design §13).
set -euo pipefail

usage() {
  echo "usage: $0 <rust-target> [cargo-subcommand [args…]]" >&2
  exit 2
}

[[ $# -ge 1 ]] || usage
triple="$1"
shift
subcommand="${1:-build}"
[[ $# -ge 1 ]] && shift

root="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=../third_party/sherpa-onnx/pins.sh
source "${root}/third_party/sherpa-onnx/pins.sh"
# shellcheck source=common.sh
source "${root}/scripts/common.sh"

sherpa="$(sherpa_target_for_triple "$triple")" || {
  echo "no pinned sherpa-onnx libraries for ${triple}" >&2
  exit 1
}
if [[ "$triple" == x86_64-pc-windows-gnu ]]; then
  # The prebuilt Windows libraries are MSVC (/MT) objects: MinGW can type-check
  # against them but not link them.
  case "$subcommand" in
    check | clippy | doc) ;;
    *)
      echo "x86_64-pc-windows-gnu: sherpa-onnx ships MSVC libraries only; use check/clippy/doc," >&2
      echo "or build on Windows for x86_64-pc-windows-msvc" >&2
      exit 1
      ;;
  esac
fi

if [[ "$triple" == x86_64-pc-windows-msvc ]]; then
  # The agent's build script locates the Windows SDK's rc.exe. RC can
  # explicitly select the resource compiler.
  if [[ -n "${RUSTFLAGS+x}${CARGO_ENCODED_RUSTFLAGS+x}${CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS+x}" ]]; then
    echo "build-target.sh: unset RUSTFLAGS, CARGO_ENCODED_RUSTFLAGS and CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS; MSVC CRT flags are set in .cargo/config.toml" >&2
    exit 1
  fi
  # whisper-rs-sys forwards every CMAKE_* variable as a CMake definition.
  # Its CMake 3.5 policy baseline otherwise keeps /MD defaults; debug builds
  # also select RelWithDebInfo flags while compiling the Release configuration.
  # Select the release static CRT for every configuration, matching sherpa.
  export CMAKE_POLICY_DEFAULT_CMP0091=NEW
  export CMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded
fi

sherpa_check_lock "${root}/Cargo.lock"
lib_dir="${root}/third_party/sherpa-onnx/lib-${sherpa}"
if ! sherpa_lib_verified "$lib_dir" "$sherpa"; then
  echo "${lib_dir} does not hold the pinned, verified sherpa-onnx ${SHERPA_VERSION} ${sherpa} archive" >&2
  echo "run: third_party/sherpa-onnx/fetch.sh --target ${sherpa}" >&2
  exit 1
fi

cd "$root"
require_cargo_1_100
export SHERPA_ONNX_LIB_DIR="$lib_dir"
exec cargo "$subcommand" --target "$triple" "$@"
