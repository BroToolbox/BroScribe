#!/usr/bin/env bash
set -euo pipefail
harness="$(cd "$(dirname "$0")" && pwd)"
evidence="${BROSCRIBE_XFCE_EVIDENCE:-/tmp/broscribe-linux-verify}"
mkdir -p "$evidence"
exec docker run --rm --detach --init --platform linux/arm64 \
  --name broscribe-linux-xfce --network none --cap-drop ALL \
  --security-opt no-new-privileges --shm-size 512m \
  -v "$harness:/harness:ro" -v "$evidence:/evidence" \
  -v broscribe-linux-cargo:/usr/local/cargo/registry:ro \
  -v broscribe-linux-target:/target:ro \
  broscribe-linux-xfce:20261118 /harness/session.sh
