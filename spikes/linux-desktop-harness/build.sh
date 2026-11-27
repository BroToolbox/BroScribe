#!/usr/bin/env bash
set -euo pipefail
harness="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$harness/../.." && pwd)"
script="$root/scripts/docker-linux-test.sh"
image="$(sed -n 's/^IMAGE="\(.*\)"$/\1/p' "$script")"
packages="$(sed -n '/apt-get install -y -qq --no-install-recommends/,/ffmpeg/{ /apt-get install/d; s/\\//; s@>/dev/null@@; p; }' "$script" | xargs)"
test -n "$image"
test -n "$packages"
docker build --platform linux/arm64 --progress plain \
  --build-arg "RUST_IMAGE=$image" --build-arg "BUILD_PACKAGES=$packages" \
  -t broscribe-linux-xfce:20261118 "$harness"
"$root/third_party/sherpa-onnx/fetch.sh" --target linux-aarch64
# Share caches with the workspace Linux test script. This build has network
# for the registry; the desktop run uses the binaries with networking disabled.
docker run --rm --platform linux/arm64 --user 0:0 \
  -e CARGO_TARGET_DIR=/target -e "CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}" \
  -e SHERPA_ONNX_LIB_DIR=/src/third_party/sherpa-onnx/lib-linux-aarch64 \
  -v "$root:/src:ro" -v broscribe-linux-cargo:/usr/local/cargo/registry \
  -v broscribe-linux-target:/target -w /src broscribe-linux-xfce:20261118 \
  cargo build --locked -p broscribe-app -p broscribe-cli -p broscribed -p broscribe-worker
