#!/usr/bin/env bash
# Build the pinned XFCE desktop image and the debug binaries it runs.
# The sherpa-onnx archive is fetched and verified inside the image, so the
# host needs only Docker. CARGO_BUILD_JOBS defaults to 2, which fits a
# hosted arm64 runner and Docker Desktop's default memory.
set -euo pipefail
harness="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$harness/../.." && pwd)"
script="$root/scripts/docker-linux-test.sh"
image="$(sed -n 's/^IMAGE="\(.*\)"$/\1/p' "$script")"
toolchain="$(sed -n 's/^IMAGE_TOOLCHAIN="\(.*\)"$/\1/p' "$script")"
packages="$(sed -n '/apt-get install -y -qq --no-install-recommends/,/ffmpeg/{ /apt-get install/d; s/\\//; s@>/dev/null@@; p; }' "$script" | xargs)"
test -n "$image"
test -n "$packages"
channel="$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$root/rust-toolchain.toml")"
if [[ "$channel" != "$toolchain" ]]; then
  echo "rust-toolchain.toml pins ${channel} but the image is ${toolchain}: update $script" >&2
  exit 1
fi
tag=broscribe-linux-xfce:20261118
for attempt in 1 2 3; do
  if docker build --platform linux/arm64 --progress plain \
    --build-arg "RUST_IMAGE=$image" --build-arg "BUILD_PACKAGES=$packages" \
    -t "$tag" "$harness"; then
    break
  fi
  [[ $attempt != 3 ]] || exit 1
  echo "docker build failed (attempt $attempt); retrying" >&2
  sleep 30
done
# fetch.sh checks the archive against SHA256SUMS and ARCHIVE_SIZES. Only its
# own folder is writable.
docker run --rm --platform linux/arm64 --user "$(id -u):$(id -g)" -e HOME=/tmp \
  -v "$root:/src:ro" -v "$root/third_party/sherpa-onnx:/src/third_party/sherpa-onnx" \
  -w /src "$tag" third_party/sherpa-onnx/fetch.sh --target linux-aarch64
# Share caches with the workspace Linux test script. This build has network
# for the registry; the desktop run uses the binaries with networking disabled.
# RUSTUP_TOOLCHAIN selects the image's toolchain over rust-toolchain.toml, so
# rustup does not download the rustfmt and clippy components it lists, which
# a build does not need; RUSTUP_AUTO_INSTALL=0 refuses any other download.
docker run --rm --platform linux/arm64 --user 0:0 \
  -e "RUSTUP_TOOLCHAIN=$toolchain" -e RUSTUP_AUTO_INSTALL=0 \
  -e CARGO_TARGET_DIR=/target -e "CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}" \
  -e CARGO_INCREMENTAL=0 \
  -e SHERPA_ONNX_LIB_DIR=/src/third_party/sherpa-onnx/lib-linux-aarch64 \
  -v "$root:/src:ro" -v broscribe-linux-cargo:/usr/local/cargo/registry \
  -v broscribe-linux-target:/target -w /src "$tag" \
  cargo build --locked -p broscribe-app -p broscribe-cli -p broscribed -p broscribe-worker
