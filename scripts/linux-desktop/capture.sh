#!/usr/bin/env bash
# Capture the README screenshots inside the screenshot desktop
# (session.sh with BROSCRIBE_SHOT_MODE=1). Run through inside.sh:
#
#   capture.sh <commit>
#
# Writes the four PNGs and manifest.json to /evidence/out and logs and
# AT-SPI dumps to /evidence/diagnostics. Every screen waits for its own
# content and for three identical captures in a row; a screen that never
# settles fails the run instead of producing a half-loaded image. Every wait
# is bounded and names what it was waiting for.
set -euo pipefail
commit="${1:?source commit}"
harness=/harness
out=/evidence/out
diag=/evidence/diagnostics
work="$(mktemp -d)"
mkdir -p "$out" "$diag"
chmod 755 "$out" "$diag"
publish_diagnostics() {
  cp -r "$HOME/logs" "$diag/logs" 2>/dev/null || :
  # Readable by the host user, and removable by it once the run ends.
  chmod -R a+rwX "$diag" "$out" 2>/dev/null || :
}
trap publish_diagnostics EXIT
trap 'printf "[capture] failed at line %s: %s\n" "$LINENO" "$BASH_COMMAND" >&2' ERR

faketime_lib="/usr/lib/$(uname -m)-linux-gnu/faketime/libfaketime.so.1"
test -e "$faketime_lib"
now="$(cd "$harness" && python3 -c 'import showcase; print(showcase.NOW)')"
featured_title="Observatory Night run of show"
width=1280
height=800

log() { printf '[capture] %s\n' "$*" >&2; }
fail() { log "error: $*"; exit 1; }

# screenshots.sh waits for the desktop before it starts this script.
[[ -f "$HOME/desktop-ready" ]] || fail "desktop not ready"
gsettings set org.gnome.desktop.interface color-scheme prefer-light
gsettings set org.gnome.desktop.a11y.interface high-contrast false

log "seeding the invented library"
(cd "$harness" && SHOWCASE_FAKETIME_LIB="$faketime_lib" python3 seed.py --showcase)

agent_ready() {
  broscribe --json daemon status > "$work/status.json" 2>/dev/null || return 1
  jq -e '.running and .agent.readiness == "normal_operational"
         and .state.jobs.running == null and .state.jobs.waiting == 0
         and .state.index.state == "current" and .state.recording == null' \
    "$work/status.json" > /dev/null
}

start_agent() {
  broscribed > "$HOME/logs/agent.log" 2>&1 &
  echo $! > "$HOME/agent.pid"
  for _ in {1..240}; do agent_ready && return; sleep .5; done
  cp "$work/status.json" "$diag/agent-status.json" 2>/dev/null || :
  fail "agent not ready"
}

stop_agent() {
  broscribe --json daemon stop > /dev/null 2>&1 || kill "$(cat "$HOME/agent.pid")" 2>/dev/null || :
  for _ in {1..60}; do pgrep -x broscribed > /dev/null || break; sleep .25; done
  ! pgrep -x broscribed > /dev/null || fail "agent did not stop"
}

fixture_pid=""

# A process that exited but was not reaped yet still answers kill -0.
alive() {
  local state
  state="$(ps -o stat= -p "$1" 2>/dev/null)" && [[ $state != Z* ]]
}

# Print the first visible app window whose name matches, other than
# <except>. Polls for up to 60 s and fails, naming the window, if the app
# (or the panel fixture) exits first or the window never shows.
wait_window() {
  local label=$1 pattern=$2 except=${3:-} window deadline=$((SECONDS + 60))
  while ((SECONDS < deadline)); do
    alive "$app_pid" || fail "broscribe-app exited before its $label window appeared"
    if [[ -n $fixture_pid ]] && ! alive "$fixture_pid"; then
      fail "panel-fixture.py exited before the $label window appeared"
    fi
    window="$(xdotool search --all --onlyvisible --pid "$app_pid" --name "$pattern" 2>/dev/null \
      | grep -vx -e "${except:-none}" | sed -n 1p || :)"
    if [[ -n $window ]]; then
      printf '%s\n' "$window"
      return
    fi
    sleep .2
  done
  fail "the $label window did not appear within 60 s"
}

# Only the app runs on the pinned, ticking clock; its CLI children inherit it.
launch_app() {
  (
    export LD_PRELOAD="$faketime_lib" FAKETIME="@$now" FAKETIME_DONT_FAKE_MONOTONIC=1
    exec broscribe-app > "$HOME/logs/app-$1.log" 2>&1
  ) &
  app_pid=$!
  echo "$app_pid" > "$HOME/app.pid"
}

stop_app() {
  kill "$app_pid" 2>/dev/null || :
  wait "$app_pid" 2>/dev/null || :
}

# AT-SPI bindings warn about GPUI's RemoveAccessible signature; keep the rest.
a11y() { python3 "$harness/a11y.py" "$app_pid" "$@" 2> >(grep -v -e 'dbind-WARNING' -e '^$' >&2); }

# Size the client area exactly and assert it.
place() {
  local window=$1 w=$2 h=$3 x=$4 y=$5
  xdotool windowsize --sync "$window" "$w" "$h"
  xdotool windowmove --sync "$window" "$x" "$y"
  for _ in {1..40}; do
    if xwininfo -id "$window" | grep -q "Width: $w\$" && xwininfo -id "$window" | grep -q "Height: $h\$"; then
      return
    fi
    sleep .1
  done
  xwininfo -id "$window" >&2
  fail "window $window is not ${w}x${h}"
}

origin() {
  xwininfo -id "$1" | awk '/Absolute upper-left X/ {x=$NF} /Absolute upper-left Y/ {y=$NF} END {print x, y}'
}

park_pointer() { xdotool mousemove 1439 899; }

# Click the centre of the named accessible node with the real pointer, so
# no keyboard focus ring is drawn.
click() {
  local window=$1 name=$2 role=${3:-}
  local node
  node="$(a11y locate "$name" ${role:+--role "$role"} --timeout 20)"
  read -r x y < <(jq -r '"\(.x + (.width / 2) | floor) \(.y + (.height / 2) | floor)"' <<<"$node")
  xdotool mousemove --window "$window" "$x" "$y" click 1
}

# Clicking the meeting heading moves focus off buttons and list rows.
settle_focus() {
  click "$main_window" "$featured_title" heading
  park_pointer
}

# Nothing on screen but the app's own windows (no panel, desktop or toast).
# A named, visible window without _NET_WM_PID cannot be attributed, so it
# fails too.
assert_only_app_windows() {
  local window pid
  for window in $(xdotool search --onlyvisible --name '.' 2>/dev/null); do
    pid="$(xdotool getwindowpid "$window" 2>/dev/null || :)"
    [[ -n "$pid" ]] || fail "window $window ($(xdotool getwindowname "$window" 2>/dev/null || :)) has no _NET_WM_PID"
    [[ "$pid" == "$app_pid" ]] && continue
    [[ "$(ps -o comm= -p "$pid")" == xfwm4 ]] && continue
    fail "unexpected window $window ($(xdotool getwindowname "$window")) from $(ps -o comm= -p "$pid")"
  done
}

# No loading, offline or error state, and no "no tray icons" banner (or any
# Dismiss button) that appeared after it was dismissed.
absent_indicators() {
  a11y dump > "$work/tree.json"
  # Keep the tree that failed: diagnostics then show which control it was.
  cp "$work/tree.json" "$diag/indicators.atspi.json"
  python3 - "$work/tree.json" <<'PY'
import json, sys
words = ("Loading", "loading", "Reconnecting", "not running", "isn't running", "offline",
         "Offline", "Needs attention", "didn't understand", "Couldn't", "couldn't",
         "tray icons")
for node in json.load(open(sys.argv[1])):
    text = f"{node['name']} {node['description']}"
    if any(word in text for word in words) or node["name"].strip() == "Dismiss":
        sys.exit(f"indicator on screen: {text!r}")
PY
}

# Three identical captures in a row, ~300 ms apart, then a metadata-free PNG.
capture() {
  local name=$1 target=$2 crop=${3:-}
  local previous="" current same=0 deadline=$((SECONDS + 60))
  assert_only_app_windows
  park_pointer
  a11y dump > "$diag/${name%.png}.atspi.json"
  while ((SECONDS < deadline)); do
    sleep .3
    if [[ -n $crop ]]; then
      import -window "$target" -crop "$crop" +repage -depth 8 "rgb:$work/frame.rgb"
    else
      import -window "$target" -depth 8 "rgb:$work/frame.rgb"
    fi
    current="$(sha256sum < "$work/frame.rgb")"
    if [[ $current == "$previous" ]]; then
      same=$((same + 1))
    else
      same=1
    fi
    previous=$current
    if ((same == 3)); then
      # The settled frame must still have no loading, error or banner state.
      absent_indicators
      cp "$work/tree.json" "$diag/${name%.png}.settled.atspi.json"
      [[ "$(stat -c %s "$work/frame.rgb")" == $((width * height * 3)) ]] || fail "$name is not ${width}x${height}"
      magick -size "${width}x${height}" -depth 8 "rgb:$work/frame.rgb" -strip \
        -define png:exclude-chunks=date,time "PNG24:$out/$name"
      chmod 644 "$out/$name"
      log "captured $name"
      return
    fi
  done
  fail "$name never settled"
}

luminance() {
  import -window "$1" -colorspace gray -format '%[fx:mean]' info:
}

set_scheme() {
  local scheme=$1 expected=$2
  gsettings set org.gnome.desktop.interface color-scheme "prefer-$scheme"
  for _ in {1..50}; do
    if gdbus call --session -d org.freedesktop.portal.Desktop -o /org/freedesktop/portal/desktop \
         -m org.freedesktop.portal.Settings.ReadOne org.freedesktop.appearance color-scheme \
         | grep -q "<uint32 $expected>"; then
      return
    fi
    sleep .2
  done
  fail "portal did not report the $scheme scheme"
}

wait_luminance() {
  local window=$1 comparison=$2
  for _ in {1..50}; do
    if python3 -c "import sys; sys.exit(0 if float(sys.argv[1]) $comparison else 1)" "$(luminance "$window")"; then
      return
    fi
    sleep .2
  done
  fail "window luminance never became $comparison"
}

# --- Library, meeting and dark meeting, with the real agent --------------
start_agent
launch_app main
main_window="$(wait_window main ' - BroScribe$')"
place "$main_window" "$width" "$height" 80 50
park_pointer
# No panel means no tray: on its first launch the app shows a banner, which
# a user dismisses. Wait for the meeting list first, then require it.
a11y find "$featured_title," --role "list item" --timeout 60 > /dev/null
a11y find Dismiss --role button --timeout 30 > /dev/null || fail "the no-tray banner did not appear"
click "$main_window" Dismiss button
click "$main_window" "$featured_title," "list item"
a11y find "$featured_title" --role heading --timeout 20 > /dev/null
settle_focus
a11y find "Ana Lindqvist and the team locked the run of show" --contains --timeout 20 > /dev/null
a11y find Decisions --role heading --timeout 20 > /dev/null
absent_indicators
wait_luminance "$main_window" '> 0.7'
capture library.png "$main_window"

click "$main_window" Transcript "page tab"
a11y find "Morning, everyone. Let's lock the run of show" --contains --timeout 20 > /dev/null
settle_focus
absent_indicators
capture meeting.png "$main_window"

set_scheme dark 1
wait_luminance "$main_window" '< 0.3'
a11y find "Morning, everyone. Let's lock the run of show" --contains --timeout 20 > /dev/null
absent_indicators
capture meeting-dark.png "$main_window"
set_scheme light 2

stop_app
stop_agent

# --- Recording controls, from the synthetic panel fixture -----------------
rm -f "$XDG_RUNTIME_DIR"/broscribe/broscribed-*.sock
(cd "$harness" && exec python3 -u panel-fixture.py --showcase > "$HOME/logs/panel-fixture.log" 2>&1) &
fixture_pid=$!
socket=""
for _ in {1..100}; do
  alive "$fixture_pid" || fail "panel-fixture.py exited before its socket appeared"
  socket="$(compgen -G "$XDG_RUNTIME_DIR/broscribe/broscribed-*.sock" || :)"
  [[ -S $socket ]] && break
  sleep .1
done
[[ -S $socket ]] || fail "panel-fixture.py did not open its socket within 10 s"
launch_app panel
panel="$(wait_window "recording controls" '^Recording controls - BroScribe$')"
main_window="$(wait_window "main (beside the recording controls)" ' - BroScribe$' "$panel")"
place "$main_window" "$width" "$height" 80 50
read -r main_x main_y < <(origin "$main_window")
read -r panel_w panel_h < <(xwininfo -id "$panel" | awk '/Width:/ {w=$NF} /Height:/ {h=$NF} END {print w, h}')
log "recording controls are ${panel_w}x${panel_h}"
# Settle the main window first, with the controls parked at the screen's
# right edge, clear of the meeting list and heading clicked here.
xdotool windowmove --sync "$panel" $((1440 - panel_w)) 0
click "$main_window" "$featured_title," "list item"
heading="$(a11y locate "$featured_title" --role heading --timeout 20)"
heading_x="$(jq -r '.x + (.width / 2) | floor' <<<"$heading")"
((main_x + heading_x < 1440 - panel_w)) ||
  fail "the heading click at x=$((main_x + heading_x)) would land on the parked recording controls"
settle_focus
# Then place the controls at the main window's right edge, as they sit
# beside a meeting, and bring them to the front.
panel_want_x=$((main_x + width - panel_w - 30))
panel_want_y=$((main_y + 40))
xdotool windowmove --sync "$panel" "$panel_want_x" "$panel_want_y"
xdotool windowactivate --sync "$panel"
xdotool windowraise "$panel"
a11y find "Yes, Elif will email the certificate by Thursday." --contains --timeout 20 > /dev/null
a11y find "Paused" --timeout 20 > /dev/null
absent_indicators
read -r panel_x panel_y < <(origin "$panel")
((panel_x >= main_x && panel_y >= main_y &&
  panel_x + panel_w <= main_x + width && panel_y + panel_h <= main_y + height)) ||
  fail "recording controls at ${panel_x},${panel_y} (${panel_w}x${panel_h}) are not inside the shot at ${main_x},${main_y}"
capture recording.png root "${width}x${height}+${main_x}+${main_y}"
stop_app
kill "$fixture_pid" 2>/dev/null || :

python3 "$harness/shots.py" manifest "$out" --commit "$commit"
chmod 644 "$out/manifest.json"
log "done"
