#!/usr/bin/env bash
# Builds and tests the whole workspace on Linux (arm64 by default)
# in a container pinned by digest to the toolchain in rust-toolchain.toml:
#
#   scripts/docker-linux-test.sh                 build + test the workspace
#   scripts/docker-linux-test.sh --amd64          Ubuntu 24.04 x64 baseline
#   scripts/docker-linux-test.sh -p broscribe-platform-linux   only these packages
#
# Steps: install the native build dependencies, fetch and verify the
# selected sherpa-onnx libraries, reject Metal in the workspace feature
# tree (except wgpu's own inert "metal" feature, below), then warning-free
# Clippy, `cargo build --all-targets` and `cargo test`.
# Linux builds the portable CPU baseline.
#
# Cargo's registry cache and the Linux target folder live in named Docker
# volumes, never in the host's target/. CARGO_BUILD_JOBS (default 4) caps
# parallel rustc/link jobs so the final links fit Docker Desktop's memory.
# CARGO_PROFILE_DEV_DEBUG, when set, reaches the container unchanged.
set -euo pipefail

# rust:1.100.0-trixie (multi-arch index), verified from Docker Hub on 2026-11-16.
# Update together with rust-toolchain.toml.
IMAGE_TOOLCHAIN="1.100.0"
IMAGE="docker.io/library/rust:1.100.0-trixie@sha256:e19840879045c951c905d0eaa9887f3439c62000e429963371b376bac8d38c31"
PLATFORM="linux/arm64"
ARCHIVE_TARGET="linux-aarch64"
TRIPLE="aarch64-unknown-linux-gnu"
VOLUME_NAME=broscribe-linux
ubuntu_baseline=false
if [[ "${1:-}" == --amd64 ]]; then
  shift
  PLATFORM="linux/amd64"
  ARCHIVE_TARGET="linux-x64"
  TRIPLE="x86_64-unknown-linux-gnu"
  VOLUME_NAME=broscribe-linux-x64
  ubuntu_baseline=true
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
channel="$(sed -n 's/^channel = "\(.*\)"$/\1/p' "${root}/rust-toolchain.toml")"
if [[ "$channel" != "$IMAGE_TOOLCHAIN" ]]; then
  echo "rust-toolchain.toml pins ${channel} but the image is ${IMAGE_TOOLCHAIN}: update IMAGE in $0" >&2
  exit 1
fi

if "$ubuntu_baseline"; then
  # Use the same pinned Rust distribution with the supported Ubuntu userland.
  # Ubuntu glibc and system libraries are used for compilation and linking.
  toolchain_image="$IMAGE"
  IMAGE="broscribe-linux-test:noble-${IMAGE_TOOLCHAIN}"
  docker build --platform "$PLATFORM" --tag "$IMAGE" - <<DOCKERFILE
FROM $toolchain_image AS rust
FROM docker.io/library/ubuntu:24.04@sha256:9a04f13b3f80ef056361e30868593b1cfd2279fe19e4081cb93a90bf56819d6a
COPY --from=rust /usr/local/rustup /usr/local/rustup
COPY --from=rust /usr/local/cargo /usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo
ENV PATH="/usr/local/cargo/bin:\$PATH"
DOCKERFILE
fi

full_workspace=false
if [[ $# -gt 0 ]]; then
  packages="$(printf '%q ' "$@")"
else
  packages="--workspace"
  full_workspace=true
fi

# Packages: PipeWire and ALSA (capture, cpal), clang (bindgen for pipewire-sys),
# cmake (whisper.cpp), and the X11/Wayland/font/Vulkan headers GPUI's Linux
# backend builds against.
read -r -d '' inner <<EOF || true
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
# The container is thrown away after this run: skip dpkg's fsyncs and the
# documentation, manual pages and translations of the build packages.
printf 'force-unsafe-io\n' >/etc/dpkg/dpkg.cfg.d/broscribe-unsafe-io
printf '%s\n' 'path-exclude=/usr/share/doc/*' 'path-exclude=/usr/share/man/*' \
  'path-exclude=/usr/share/locale/*' >/etc/dpkg/dpkg.cfg.d/broscribe-no-docs
echo "installing the native build packages"
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  libpipewire-0.3-dev libspa-0.2-dev libasound2-dev libclang-dev clang pkg-config cmake \
  libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libwayland-dev \
  libfontconfig-dev libfreetype-dev libvulkan-dev libzstd-dev libssl-dev \
  build-essential git bzip2 ca-certificates curl ffmpeg python3 >/dev/null
echo "native build packages installed"
cd /src
rustup component add clippy --toolchain "$IMAGE_TOOLCHAIN"
third_party/sherpa-onnx/fetch.sh --target "$ARCHIVE_TARGET"
export SHERPA_ONNX_LIB_DIR=/src/third_party/sherpa-onnx/lib-$ARCHIVE_TARGET
export CARGO_TARGET_DIR=/target
cargo --version
cargo tree --locked -e features --workspace > /target/linux-features.txt
# gpui-pre-wgpu enables wgpu's "metal" feature on Linux too; it reaches only
# wgpu and wgpu-core, whose Metal dependencies (wgpu-core-deps-apple) are
# Apple-only, so it builds nothing here. Any other metal feature or crate fails,
# including wgpu-hal's, which would compile MSL output on Linux.
if grep -E '"metal"|[ /]metal( |$)' /target/linux-features.txt \
  | grep -vE '── (wgpu|wgpu-core) feature "metal"( \(\*\))?$'; then
  echo "error: a metal feature is enabled in the Linux build" >&2
  exit 1
fi
cargo clippy --locked --all-targets ${packages} -- -D warnings
cargo build --locked --all-targets ${packages}
cargo test --locked --no-fail-fast ${packages}
if $full_workspace; then
  python3 scripts/third-party-notices.py --target "$TRIPLE" --native-lib-dir "/src/third_party/sherpa-onnx/lib-$ARCHIVE_TARGET" --out-dir /target/notices
fi
echo "linux build, clippy, tests and feature check passed"
EOF

exec docker run --rm --platform "$PLATFORM" \
  -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
  -e CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}" \
  -e CARGO_PROFILE_DEV_DEBUG \
  -v "${root}:/src" \
  -v "${VOLUME_NAME}-cargo:/usr/local/cargo/registry" \
  -v "${VOLUME_NAME}-target:/target" \
  "$IMAGE" bash -c "$inner"
