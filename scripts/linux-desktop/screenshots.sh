#!/usr/bin/env bash
# Capture the README screenshots from invented data:
#
#   scripts/linux-desktop/screenshots.sh <out-dir> [<diagnostics-dir>]
#
# Builds the pinned XFCE image and the debug binaries (build.sh), then runs
# the screenshot desktop in a fresh, uniquely named container without
# network or capabilities. <out-dir> receives exactly library.png,
# meeting.png, meeting-dark.png, recording.png and manifest.json; logs and
# AT-SPI dumps go to <diagnostics-dir> (default <out-dir>.diagnostics) and
# are never published (CI uploads them only when a run fails).
# CARGO_BUILD_JOBS defaults to 2. Needs Docker and GNU `timeout`.
set -euo pipefail
command -v timeout > /dev/null || { echo "GNU timeout is required" >&2; exit 1; }
harness="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$harness/../.." && pwd)"
out="${1:?usage: $0 <out-dir> [<diagnostics-dir>]}"
diagnostics="${2:-$out.diagnostics}"
commit="${BROSCRIBE_SHOT_COMMIT:-$(git -C "$root" rev-parse HEAD)}"
[[ $commit =~ ^[0-9a-f]{40}$ ]] || { echo "not a commit: $commit" >&2; exit 1; }
if [[ -e $out && -n "$(ls -A "$out")" ]]; then
  echo "$out exists and is not empty" >&2
  exit 1
fi

"$harness/build.sh"

name="broscribe-shots-$$-$RANDOM"
private="$(mktemp -d "${TMPDIR:-/tmp}/broscribe-shots.XXXXXX")"
evidence="$private/evidence"
cleanup() {
  docker rm --force "$name" > /dev/null 2>&1 || :
  rm -rf "$private" 2>/dev/null || :
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# The desktop user (uid 1000) writes here. Its private parent keeps other
# local users out.
mkdir -m 777 "$evidence"
chmod 777 "$evidence"
docker run --detach --init --platform linux/arm64 --name "$name" \
  --network none --cap-drop ALL --security-opt no-new-privileges --shm-size 512m \
  -e BROSCRIBE_SHOT_MODE=1 \
  -v "$harness:/harness:ro" -v "$evidence:/evidence" \
  -v broscribe-linux-cargo:/usr/local/cargo/registry:ro \
  -v broscribe-linux-target:/target:ro \
  broscribe-linux-xfce:20261118 /harness/session.sh > /dev/null
collect_diagnostics() {
  mkdir -p "$diagnostics"
  cp -R "$evidence/diagnostics/." "$diagnostics/" 2>/dev/null || :
  # A timed-out capture never ran its own exit trap: fetch its logs directly
  # (a snapshot: the capture inside may still be running).
  [[ -d "$evidence/diagnostics/logs" ]] || docker cp "$name:/home/qa/logs" "$diagnostics/logs" > /dev/null 2>&1 || :
  docker logs "$name" > "$diagnostics/session.log" 2>&1 || :
}

# session.sh writes ~/session.env (for inside.sh) before it marks the
# desktop ready; wait for that mark, and stop early if the desktop died.
deadline=$((SECONDS + 120))
until docker exec "$name" test -f /home/qa/desktop-ready 2>/dev/null; do
  if [[ "$(docker inspect --format '{{.State.Running}}' "$name" 2>/dev/null)" != true ]]; then
    collect_diagnostics
    echo "the screenshot desktop exited before it was ready; diagnostics in $diagnostics" >&2
    exit 1
  fi
  if ((SECONDS >= deadline)); then
    collect_diagnostics
    echo "the screenshot desktop was not ready within 120 s; diagnostics in $diagnostics" >&2
    exit 1
  fi
  sleep 1
done

status=0
timeout --kill-after 30s 20m \
  docker exec "$name" /harness/inside.sh /harness/capture.sh "$commit" || status=$?
collect_diagnostics
if ((status == 124 || status == 137)); then
  echo "capture timed out after 20 minutes" >&2
fi
if ((status != 0)); then
  echo "capture failed ($status); diagnostics in $diagnostics" >&2
  exit "$status"
fi
mkdir -p "$out"
for file in library.png meeting.png meeting-dark.png recording.png manifest.json; do
  cp "$evidence/out/$file" "$out/$file"
done
python3 "$harness/shots.py" validate "$out" --commit "$commit"
